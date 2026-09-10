//! Columns encoded together instead of one at a time.
//!
//! `spec/06-compression.md` section 6.4 calls this multi-column compression and says it is the
//! mechanism that has no equivalent in DuckDB. The idea is that columns are not independent, so
//! encoding them independently throws away real redundancy. On ClickBench `hits` the obvious case
//! is `URL` and `Referer`, which are both URLs drawn from the same universe, and the less obvious
//! one is that `URL`, `Referer`, `Title` and the referer derived columns are all the same alphabet
//! and could share one symbol table.
//!
//! ## The three strategies
//!
//! `INDEPENDENT` is each column encoded on its own by [`crate::string`]. It is the baseline the
//! other two have to beat, and it is what a group falls back to when they do not.
//!
//! `SHARED_TABLE` is one FSST symbol table trained on a sample of all the columns, with every
//! column compressed against it. Storing one 255 symbol table rather than six saves almost nothing
//! by itself. The effect that matters is that a table trained on the union has more evidence per
//! symbol, so it compresses each column better than a table trained on that column alone would,
//! and the columns that gain most are the small ones that never had enough bytes to train on.
//!
//! `SHARED_DICT` is one dictionary holding the union of the values, with every column becoming an
//! array of codes into it. A value that appears in three columns is stored once rather than three
//! times. The dictionary is itself a string column, so it goes back through the string chooser and
//! comes out FSST compressed, which means a shared dictionary is also a shared symbol table.
//!
//! ## What decides
//!
//! Here, measuring all three and keeping the smallest, for the same reason [`crate::string`] does
//! it that way: M1 is measuring what the format can do rather than how fast a writer can decide.
//! A write path cannot afford this. Section 6.4 says detection is by sampling pairs over a global
//! sample at table level, recorded in the catalog as hints, with each row group checking only the
//! hinted pairs. [`dictionary_groups`] is that pruning step, and it runs on sketches rather than on
//! data, so the 5,460 pairs of a 105 column table cost a Jaccard estimate each rather than a pass
//! over the column.
//!
//! ## What is not here
//!
//! Correlation encodings, which are the third form in section 6.4: column B stored as a function of
//! column A, either as a per dictionary entry lookup for a functional dependency or as B minus f(A)
//! for a numeric one. [`crate::sketch::dependence`] is the detection half of that and the encoding
//! half is its own piece of work.
//!
//! Global dictionaries, which are section 6.5 and are a dictionary across the whole table rather
//! than across a group of columns in one chunk. The two compose, and the reason they are separate
//! is that a global dictionary needs an incremental builder that can spill, which is open question
//! five and is the thing most likely to make the idea impractical.

use rudb_common::{Error, Result};

use crate::fsst::SymbolTable;
use crate::integer;
use crate::reader::Reader;
use crate::sketch::Sketch;
use crate::string;

/// The least a column contributes to a shared sample, however small it is next to the rest of the
/// group. Enough to see an alphabet, not enough to matter against the 64 KB the group gets.
const FLOOR_SAMPLE_BYTES: usize = 2 * 1024;

/// How a column group is encoded. The discriminant is the tag byte and is part of the format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Each column encoded on its own.
    Independent = 0,
    /// One symbol table over all of them.
    SharedTable = 1,
    /// One dictionary over the union of their values.
    SharedDict = 2,
}

impl Strategy {
    fn tag(self) -> u8 {
        self as u8
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Independent),
            1 => Ok(Self::SharedTable),
            2 => Ok(Self::SharedDict),
            other => Err(Error::internal(format!("unknown column group tag {other}"))),
        }
    }

    /// The name that goes in a report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Independent => "INDEPENDENT",
            Self::SharedTable => "SHARED_TABLE",
            Self::SharedDict => "SHARED_DICT",
        }
    }
}

/// Encodes a group of string columns together, choosing whatever comes out smallest.
///
/// The columns do not have to be the same length. Nothing here pairs values up by row, so a group
/// is a set of columns that share an alphabet or a value universe rather than a set of columns from
/// the same table.
///
/// # Errors
///
/// If the group holds more than `u32::MAX` columns or a column holds more than `u32::MAX` values,
/// or if an encoding produces something its own decoder would not accept.
pub fn encode_group(columns: &[&[&[u8]]]) -> Result<Vec<u8>> {
    let mut best: Option<Vec<u8>> = None;
    for strategy in [Strategy::Independent, Strategy::SharedTable, Strategy::SharedDict] {
        let Some(bytes) = encode_as(strategy, columns)? else {
            continue;
        };
        if best.as_ref().is_none_or(|current| bytes.len() < current.len()) {
            best = Some(bytes);
        }
    }
    best.ok_or_else(|| Error::internal("no strategy applied to the column group"))
}

/// Decodes a group written by [`encode_group`].
///
/// # Errors
///
/// If the bytes are truncated, carry an unknown tag, or describe a group whose parts disagree.
pub fn decode_group(bytes: &[u8]) -> Result<Vec<Vec<Vec<u8>>>> {
    let mut reader = Reader::new(bytes);
    let columns = decode_at(&mut reader)?;
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding a column group",
            reader.remaining()
        )));
    }
    Ok(columns)
}

/// The size of every strategy that applies, which is the measurement section 6.4 is asking for.
///
/// # Errors
///
/// As [`encode_group`].
pub fn strategy_sizes(columns: &[&[&[u8]]]) -> Result<Vec<(Strategy, usize)>> {
    let mut sizes = Vec::new();
    for strategy in [Strategy::Independent, Strategy::SharedTable, Strategy::SharedDict] {
        if let Some(bytes) = encode_as(strategy, columns)? {
            sizes.push((strategy, bytes.len()));
        }
    }
    Ok(sizes)
}

/// The shape a group was encoded as, as a line of text.
///
/// # Errors
///
/// As [`decode_group`].
pub fn describe(bytes: &[u8]) -> Result<String> {
    let mut reader = Reader::new(bytes);
    describe_at(&mut reader)
}

/// Which columns should be considered for a shared dictionary, from one sketch per column.
///
/// Two columns are put in the same group when their estimated Jaccard similarity is at least
/// `threshold`, and grouping is transitive: if A overlaps B and B overlaps C then all three end up
/// together, even if A and C do not overlap each other. That is deliberate. A dictionary is a set
/// union, so a chain of overlapping columns still stores fewer values once than separately, and
/// insisting that every pair in a group overlaps would turn this into a clique problem for no
/// benefit.
///
/// Every column appears in exactly one group, and a column that overlaps nothing is a group of one.
/// The groups come back in the order of their lowest column index, and the columns inside a group
/// in index order, so the result does not depend on the order the pairs were tested in.
///
/// This is a pruning step and not a decision. Two columns can overlap heavily and still be better
/// off apart, which is why what comes out of here goes to [`strategy_sizes`] rather than straight
/// into a writer.
///
/// # Errors
///
/// If the sketches were not all built at the same k, since then no estimate over a pair means
/// anything.
pub fn dictionary_groups(sketches: &[Sketch], threshold: f64) -> Result<Vec<Vec<usize>>> {
    let mut parent: Vec<usize> = (0..sketches.len()).collect();
    for left in 0..sketches.len() {
        for right in (left + 1)..sketches.len() {
            if sketches[left].jaccard(&sketches[right])? >= threshold {
                let (a, b) = (find(&mut parent, left), find(&mut parent, right));
                if a != b {
                    // The lower index wins, so the group's root is its first column and the output
                    // order is a function of the columns rather than of the loop.
                    let (low, high) = if a < b { (a, b) } else { (b, a) };
                    parent[high] = low;
                }
            }
        }
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    for column in 0..sketches.len() {
        let root = find(&mut parent, column);
        match roots.iter().position(|seen| *seen == root) {
            Some(at) => groups[at].push(column),
            None => {
                roots.push(root);
                groups.push(vec![column]);
            }
        }
    }
    Ok(groups)
}

fn find(parent: &mut [usize], mut node: usize) -> usize {
    while parent[node] != node {
        parent[node] = parent[parent[node]];
        node = parent[node];
    }
    node
}

fn encode_as(strategy: Strategy, columns: &[&[&[u8]]]) -> Result<Option<Vec<u8>>> {
    let mut out = vec![strategy.tag()];
    put_u32(&mut out, u32::try_from(columns.len()).map_err(|_| too_many(columns.len()))?);
    match strategy {
        Strategy::Independent => {
            for column in columns {
                out.extend_from_slice(&string::encode(column)?);
            }
        }
        Strategy::SharedTable => {
            if columns.len() < 2 {
                return Ok(None);
            }
            let table = SymbolTable::train(&shared_sample(columns));
            if table.is_empty() {
                return Ok(None);
            }
            table.serialize(&mut out);
            for column in columns {
                put_u32(&mut out, u32::try_from(column.len()).map_err(|_| too_many(column.len()))?);
                let mut compressed = Vec::new();
                let mut lengths = Vec::with_capacity(column.len());
                for value in *column {
                    let before = compressed.len();
                    table.compress(value, &mut compressed);
                    lengths.push((compressed.len() - before) as i64);
                }
                out.extend_from_slice(&integer::encode(&lengths)?);
                out.extend_from_slice(&compressed);
            }
        }
        Strategy::SharedDict => {
            if columns.len() < 2 {
                return Ok(None);
            }
            let dictionary = union_values(columns);
            let total: usize = columns.iter().map(|column| column.len()).sum();
            // A dictionary holding as many values as the columns do stores everything once and adds
            // an index on top, so it cannot win and is not worth the encode.
            if dictionary.is_empty() || dictionary.len() >= total {
                return Ok(None);
            }
            let entries: Vec<&[u8]> = dictionary.iter().map(Vec::as_slice).collect();
            out.extend_from_slice(&string::encode(&entries)?);
            for column in columns {
                let codes = codes_over(column, &dictionary);
                out.extend_from_slice(&integer::encode(&codes)?);
            }
        }
    }
    Ok(Some(out))
}

fn decode_at(reader: &mut Reader<'_>) -> Result<Vec<Vec<Vec<u8>>>> {
    let strategy = Strategy::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    let mut columns = Vec::with_capacity(count.min(1024));
    match strategy {
        Strategy::Independent => {
            for _ in 0..count {
                let (values, used) = string::decode_prefix(reader.rest())?;
                reader.skip(used)?;
                columns.push(values);
            }
        }
        Strategy::SharedTable => {
            let (table, used) = SymbolTable::deserialize(reader.rest())?;
            reader.skip(used)?;
            for _ in 0..count {
                let rows = reader.u32()? as usize;
                let (lengths, used) = integer::decode_prefix(reader.rest())?;
                reader.skip(used)?;
                if lengths.len() != rows {
                    return Err(Error::internal(format!(
                        "a column says it holds {rows} values and has {} lengths",
                        lengths.len()
                    )));
                }
                let mut values = Vec::with_capacity(rows);
                for length in lengths {
                    let length = usize::try_from(length)
                        .map_err(|_| Error::internal("a negative compressed length"))?;
                    let compressed = reader.bytes(length)?;
                    let mut value = Vec::new();
                    table.decompress(compressed, &mut value)?;
                    values.push(value);
                }
                columns.push(values);
            }
        }
        Strategy::SharedDict => {
            let (dictionary, used) = string::decode_prefix(reader.rest())?;
            reader.skip(used)?;
            for _ in 0..count {
                let (codes, used) = integer::decode_prefix(reader.rest())?;
                reader.skip(used)?;
                let mut values = Vec::with_capacity(codes.len());
                for code in codes {
                    let entry = usize::try_from(code)
                        .ok()
                        .and_then(|index| dictionary.get(index))
                        .ok_or_else(|| {
                            Error::internal(format!("code {code} is not in the shared dictionary"))
                        })?;
                    values.push(entry.clone());
                }
                columns.push(values);
            }
        }
    }
    Ok(columns)
}

fn describe_at(reader: &mut Reader<'_>) -> Result<String> {
    let strategy = Strategy::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    let mut parts = Vec::with_capacity(count.min(1024));
    let head = match strategy {
        Strategy::Independent => {
            for _ in 0..count {
                let (text, used) = string::describe_prefix(reader.rest())?;
                reader.skip(used)?;
                parts.push(text);
            }
            "INDEPENDENT".to_string()
        }
        Strategy::SharedTable => {
            let (table, used) = SymbolTable::deserialize(reader.rest())?;
            reader.skip(used)?;
            for _ in 0..count {
                let rows = reader.u32()? as usize;
                // The same chunk twice, once for its shape and once for the lengths themselves,
                // which is how the describe knows how far past the payload to step.
                let (text, _) = integer::describe_prefix(reader.rest())?;
                let (lengths, used) = integer::decode_prefix(reader.rest())?;
                reader.skip(used)?;
                if lengths.len() != rows {
                    return Err(Error::internal("a column group disagrees with itself"));
                }
                let bytes: i64 = lengths.iter().sum();
                reader.skip(usize::try_from(bytes).map_err(|_| {
                    Error::internal("a column group has a negative compressed size")
                })?)?;
                parts.push(text);
            }
            format!("SHARED_TABLE[{}]", table.len())
        }
        Strategy::SharedDict => {
            let (text, used) = string::describe_prefix(reader.rest())?;
            reader.skip(used)?;
            for _ in 0..count {
                let (codes, used) = integer::describe_prefix(reader.rest())?;
                reader.skip(used)?;
                parts.push(codes);
            }
            format!("SHARED_DICT({text})")
        }
    };
    Ok(format!("{head}({})", parts.join(", ")))
}

/// A sample of all the columns together, with the byte budget shared out in proportion to how big
/// the columns are and a floor so that a small column still gets looked at.
///
/// The total budget is the same one a single column gets, because a shared table that trained on
/// six times the sample would win partly on the sample size and the report would credit sharing
/// with something sharing did not do.
///
/// Splitting that budget evenly is the obvious thing and it is wrong. A group of one column of
/// 20,000 values and one of 40 is dominated by the first, and giving the first half the budget
/// costs it more than the second gains: measured on exactly that pair, an even split made the
/// shared table 220,755 bytes against 218,099 for encoding the two columns independently, so
/// sharing lost. In proportion, the dominant column keeps nearly all of the budget and its table is
/// nearly the table it would have had alone, while the floor is what buys the small column a table
/// trained on far more evidence than its own 40 values could provide.
fn shared_sample<'a>(columns: &[&[&'a [u8]]]) -> Vec<&'a [u8]> {
    let sizes: Vec<usize> =
        columns.iter().map(|column| column.iter().map(|value| value.len()).sum()).collect();
    let total: usize = sizes.iter().sum();
    let floor = FLOOR_SAMPLE_BYTES;
    let mut sample = Vec::new();
    for (column, bytes) in columns.iter().zip(&sizes) {
        let share = if total == 0 {
            floor
        } else {
            (string::SAMPLE_BYTES as u128 * *bytes as u128 / total as u128) as usize
        };
        sample.extend(string::sample_bytes_of(column, share.max(floor)));
    }
    sample
}

/// The distinct values across all the columns, sorted, which is the shared dictionary.
fn union_values(columns: &[&[&[u8]]]) -> Vec<Vec<u8>> {
    let mut values: Vec<Vec<u8>> =
        columns.iter().flat_map(|column| column.iter().map(|value| value.to_vec())).collect();
    values.sort_unstable();
    values.dedup();
    values
}

fn codes_over(values: &[&[u8]], dictionary: &[Vec<u8>]) -> Vec<i64> {
    values
        .iter()
        .map(|value| {
            dictionary
                .binary_search_by(|entry| entry.as_slice().cmp(value))
                .expect("the dictionary is the union of the columns in this group")
                as i64
        })
        .collect()
}

fn too_many(count: usize) -> Error {
    Error::internal(format!("a column group of {count} is larger than the format allows"))
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// URLs from one host, so two columns built from different hosts share an alphabet and no
    /// values at all.
    fn urls(host: &str, count: usize, from: usize) -> Vec<Vec<u8>> {
        let paths = ["/index.html", "/catalog/item", "/search", "/user/profile/settings"];
        (from..from + count)
            .map(|index| {
                let path = paths[(index / 3) % paths.len()];
                format!("http://{host}{path}?session={}&ref=google", index * 7).into_bytes()
            })
            .collect()
    }

    fn borrow(values: &[Vec<u8>]) -> Vec<&[u8]> {
        values.iter().map(Vec::as_slice).collect()
    }

    fn group<'a>(columns: &'a [Vec<&'a [u8]>]) -> Vec<&'a [&'a [u8]]> {
        columns.iter().map(Vec::as_slice).collect()
    }

    fn round_trip(columns: &[&[&[u8]]]) -> Vec<u8> {
        let bytes = encode_group(columns).unwrap();
        let back = decode_group(&bytes).unwrap();
        assert_eq!(back.len(), columns.len());
        for (decoded, original) in back.iter().zip(columns) {
            assert_eq!(decoded.len(), original.len(), "{}", describe(&bytes).unwrap());
            for (left, right) in decoded.iter().zip(*original) {
                assert_eq!(left.as_slice(), *right, "{}", describe(&bytes).unwrap());
            }
        }
        bytes
    }

    fn strategy_of(bytes: &[u8]) -> Strategy {
        Strategy::from_tag(bytes[0]).unwrap()
    }

    fn size_of(sizes: &[(Strategy, usize)], strategy: Strategy) -> usize {
        sizes
            .iter()
            .find(|(kind, _)| *kind == strategy)
            .map(|(_, size)| *size)
            .unwrap_or_else(|| panic!("{} did not apply", strategy.name()))
    }

    #[test]
    fn two_columns_from_the_same_universe_share_a_dictionary() {
        // The `URL` and `Referer` case. Both columns are URLs and most of the values in one are in
        // the other, so the union is stored once instead of the intersection being stored twice.
        let left = urls("www.example.com", 20_000, 0);
        let right = urls("www.example.com", 20_000, 5_000);
        let columns = [borrow(&left), borrow(&right)];
        let group = group(&columns);
        let bytes = round_trip(&group);
        assert_eq!(strategy_of(&bytes), Strategy::SharedDict);
        let sizes = strategy_sizes(&group).unwrap();
        let independent = size_of(&sizes, Strategy::Independent);
        // Three quarters of the values are in both columns, and the saving is a third rather than
        // a half because the codes still cost something once the values stop being repeated.
        assert!(
            bytes.len() * 4 < independent * 3,
            "{} against {independent} independent",
            bytes.len()
        );
    }

    #[test]
    fn two_columns_of_the_same_alphabet_share_a_symbol_table() {
        // No value appears in both columns, so a dictionary has nothing to share. The alphabet is
        // the same, which is what a symbol table can still share.
        let left = urls("www.example.com", 8_000, 0);
        let right = urls("news.other.example.org", 8_000, 500_000);
        let columns = [borrow(&left), borrow(&right)];
        let group = group(&columns);
        let bytes = round_trip(&group);
        let sizes = strategy_sizes(&group).unwrap();
        let shared = size_of(&sizes, Strategy::SharedTable);
        let independent = size_of(&sizes, Strategy::Independent);
        assert!(shared < independent, "{shared} against {independent} independent");
        assert_eq!(strategy_of(&bytes), Strategy::SharedTable);
    }

    #[test]
    fn a_small_column_gains_most_from_a_shared_table() {
        // The reason sharing a table is worth more than the bytes of the table. A column of 40
        // values has nothing to train on, and a table trained on the group is a table that has.
        let big = urls("www.example.com", 20_000, 0);
        let small = urls("www.example.com", 40, 900_000);
        let columns = [borrow(&big), borrow(&small)];
        let sizes = strategy_sizes(&group(&columns)).unwrap();
        let shared = size_of(&sizes, Strategy::SharedTable);
        let independent = size_of(&sizes, Strategy::Independent);
        assert!(shared < independent, "group {shared} against {independent} apart");
    }

    #[test]
    fn unrelated_columns_are_left_alone() {
        let urls = urls("www.example.com", 4_000, 0);
        let numbers: Vec<Vec<u8>> = (0..4_000)
            .map(|index| format!("{:016x}", index * 2_654_435_761u64).into_bytes())
            .collect();
        let columns = [borrow(&urls), borrow(&numbers)];
        let group = group(&columns);
        let bytes = round_trip(&group);
        assert_eq!(strategy_of(&bytes), Strategy::Independent);
    }

    #[test]
    fn a_group_of_one_is_the_column_on_its_own() {
        let column = urls("www.example.com", 2_000, 0);
        let columns = [borrow(&column)];
        let bytes = round_trip(&group(&columns));
        assert_eq!(strategy_of(&bytes), Strategy::Independent);
        assert_eq!(bytes.len(), 5 + string::encode(&borrow(&column)).unwrap().len());
    }

    #[test]
    fn an_empty_group_round_trips() {
        let bytes = round_trip(&[]);
        assert_eq!(decode_group(&bytes).unwrap().len(), 0);
    }

    #[test]
    fn columns_do_not_have_to_be_the_same_length() {
        let left = urls("www.example.com", 3_000, 0);
        let right = urls("www.example.com", 700, 1_000);
        let columns = [borrow(&left), borrow(&right)];
        round_trip(&group(&columns));
    }

    #[test]
    fn an_empty_column_in_a_group_round_trips() {
        let left = urls("www.example.com", 1_000, 0);
        let empty: Vec<Vec<u8>> = Vec::new();
        let columns = [borrow(&left), borrow(&empty)];
        round_trip(&group(&columns));
    }

    #[test]
    fn every_strategy_that_applies_decodes_to_the_input() {
        let left = urls("www.example.com", 3_000, 0);
        let right = urls("www.example.com", 3_000, 1_000);
        let columns = [borrow(&left), borrow(&right)];
        let group = group(&columns);
        for strategy in [Strategy::Independent, Strategy::SharedTable, Strategy::SharedDict] {
            let bytes = encode_as(strategy, &group).unwrap().unwrap();
            let back = decode_group(&bytes).unwrap();
            assert_eq!(back[0].len(), left.len(), "{}", strategy.name());
            assert_eq!(back[1][7], right[7], "{}", strategy.name());
        }
    }

    #[test]
    fn the_chooser_picks_the_smallest_strategy() {
        let left = urls("www.example.com", 2_000, 0);
        let right = urls("www.example.com", 2_000, 1_000);
        let columns = [borrow(&left), borrow(&right)];
        let group = group(&columns);
        let chosen = encode_group(&group).unwrap();
        for (_, size) in strategy_sizes(&group).unwrap() {
            assert!(chosen.len() <= size);
        }
    }

    #[test]
    fn describe_says_what_every_column_came_out_as() {
        let left = urls("www.example.com", 2_000, 0);
        let right = urls("www.example.com", 2_000, 1_000);
        let columns = [borrow(&left), borrow(&right)];
        let group = group(&columns);
        for strategy in [Strategy::Independent, Strategy::SharedTable, Strategy::SharedDict] {
            let bytes = encode_as(strategy, &group).unwrap().unwrap();
            let shape = describe(&bytes).unwrap();
            assert!(shape.starts_with(strategy.name()), "{shape}");
            assert!(shape.contains(", "), "{shape}");
        }
    }

    #[test]
    fn a_truncated_group_is_an_error_and_not_a_panic() {
        let left = urls("www.example.com", 40, 0);
        let right = urls("www.example.com", 40, 20);
        let columns = [borrow(&left), borrow(&right)];
        let group = group(&columns);
        for strategy in [Strategy::Independent, Strategy::SharedTable, Strategy::SharedDict] {
            let bytes = encode_as(strategy, &group).unwrap().unwrap();
            for len in 0..bytes.len() {
                assert!(
                    decode_group(&bytes[..len]).is_err(),
                    "{} decoded at {len} bytes",
                    strategy.name()
                );
            }
        }
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let column = urls("www.example.com", 10, 0);
        let columns = [borrow(&column)];
        let mut bytes = encode_group(&group(&columns)).unwrap();
        bytes.push(0);
        let error = decode_group(&bytes).unwrap_err();
        assert!(error.message().contains("left over"), "{error}");
    }

    #[test]
    fn an_unknown_tag_is_an_error() {
        let error = decode_group(&[9, 0, 0, 0, 0]).unwrap_err();
        assert!(error.message().contains("unknown column group tag"), "{error}");
    }

    #[test]
    fn a_code_outside_the_shared_dictionary_is_an_error() {
        let mut bytes = vec![Strategy::SharedDict.tag()];
        put_u32(&mut bytes, 1);
        bytes.extend_from_slice(&string::encode(&[b"one".as_slice()]).unwrap());
        bytes.extend_from_slice(&integer::encode(&[4]).unwrap());
        let error = decode_group(&bytes).unwrap_err();
        assert!(error.message().contains("not in the shared dictionary"), "{error}");
    }

    #[test]
    fn overlapping_columns_are_grouped_and_the_rest_are_not() {
        let first = urls("www.example.com", 20_000, 0);
        let second = urls("news.other.example.org", 20_000, 0);
        let third = urls("www.example.com", 20_000, 4_000);
        let fourth = urls("news.other.example.org", 20_000, 4_000);
        let sketches: Vec<Sketch> = [&first, &second, &third, &fourth]
            .iter()
            .map(|column| Sketch::of(&borrow(column)))
            .collect();
        let groups = dictionary_groups(&sketches, 0.5).unwrap();
        assert_eq!(groups, vec![vec![0, 2], vec![1, 3]]);
    }

    #[test]
    fn a_column_that_overlaps_nothing_is_a_group_of_one() {
        let sketches: Vec<Sketch> = (0..4)
            .map(|index| Sketch::of(&borrow(&urls("www.example.com", 5_000, index * 100_000))))
            .collect();
        let groups = dictionary_groups(&sketches, 0.5).unwrap();
        assert_eq!(groups, vec![vec![0], vec![1], vec![2], vec![3]]);
    }

    #[test]
    fn grouping_is_transitive_and_does_not_need_every_pair_to_overlap() {
        // A overlaps B by half, B overlaps C by half, A and C not at all. They still go together,
        // because a dictionary is a union and a chain of overlaps still stores fewer values once
        // than three sets store separately.
        let a = urls("www.example.com", 20_000, 0);
        let b = urls("www.example.com", 20_000, 10_000);
        let c = urls("www.example.com", 20_000, 20_000);
        let sketches: Vec<Sketch> =
            [&a, &b, &c].iter().map(|column| Sketch::of(&borrow(column))).collect();
        assert!(sketches[0].jaccard(&sketches[2]).unwrap() < 0.01);
        let groups = dictionary_groups(&sketches, 0.3).unwrap();
        assert_eq!(groups, vec![vec![0, 1, 2]]);
    }

    #[test]
    fn grouping_needs_sketches_of_the_same_size() {
        let sketches = [Sketch::new(16).unwrap(), Sketch::new(32).unwrap()];
        assert!(dictionary_groups(&sketches, 0.5).is_err());
    }
}

//! The string column, which is offsets, bytes, and the choice between compressing the bytes and
//! not storing most of them at all.
//!
//! ClickBench `hits` is a string dataset before it is anything else. `URL`, `Referer`, `Title` and
//! the referer derived columns are most of the 20.46 GB DuckDB writes for it, so most of what
//! `spec/02-the-goal.md` promises on the resource axis has to come out of this file.
//!
//! ## The five shapes
//!
//! `CONSTANT` when every value is the same. `PLAIN`, which is lengths and raw bytes and is the
//! baseline the others have to beat. `FSST`, which is a symbol table and the same lengths over
//! compressed bytes. `DICT`, which is the distinct values and an array of codes. `FRONT`, which is
//! the length of the prefix each value shares with the one before it and the rest of the value.
//!
//! `DICT_FSST` from the section 6.2 table is not a sixth shape. A dictionary's entries are a string
//! column, and encoding them goes back through the same chooser, so a dictionary whose entries are
//! FSST compressed is what the chooser produces on its own whenever that is smaller. The same
//! recursion gives run length encoding of strings for free, because the codes are an integer chunk
//! and `crate::integer` already knows what to do with a column of long runs.
//!
//! ## Why front coding is here
//!
//! The whole file measurement in M1 says the chooser produces 11.65 GB for `hits` against Parquet's
//! 13.76 GB, and that `URL`, `Referer` and `OriginalURL` are 6.11 GB of it, and that on those three
//! the chooser loses to Parquet's Snappy. The shape it picked on all three was `DICT(FSST[255])`,
//! so the cascade was working and FSST was still losing.
//!
//! The reason is structural. FSST compresses each value on its own against a 255 symbol table, and
//! a block compressor has the previous few kilobytes of the page to point back into. Two URLs that
//! share a host and half a path are most of a back reference to each other and are nothing at all
//! to a symbol table, which can only spend eight bytes of a symbol on the part they share and has
//! to spend it again on every value. On a sorted dictionary of URLs the value before is the closest
//! thing in the column to the value in hand, and the bytes they share are the redundancy Snappy was
//! finding. Front coding is what reaches those bytes, and it composes with everything else here:
//! the suffixes it leaves behind are a string column and go back through the chooser, so
//! `DICT(FRONT(FSST))` is a shape the chooser can arrive at without anyone naming it.
//!
//! The chain has no restarts, so reading entry `n` means walking from entry zero. That is the right
//! trade while a dictionary is decoded whole, which is what `decode` does. When something wants one
//! entry out of a dictionary without materialising the rest, the answer is a restart every so many
//! entries, and it costs one full value per block.
//!
//! ## Lengths, not offsets
//!
//! The usual layout is `n + 1` offsets and Arrow does it that way because a slice of an array has
//! to be free. On disk the offsets are a monotonically increasing sequence whose differences are
//! the lengths, and the differences are what compress: URL lengths in a real column are a few dozen
//! distinct values in a narrow band, which the integer cascade turns into a handful of bits each,
//! while the offsets themselves need enough bits to address the whole chunk. The integer cascade
//! would find that by choosing DELTA, and storing lengths directly gets to the same place without
//! spending a level of the cascade on it. Offsets are a prefix sum away and that is a decode time
//! cost of one add per value.
//!
//! ## What is not here
//!
//! Nulls. A chunk here is N byte strings and an empty string is a value like any other. Validity is
//! a bitmap that belongs to the column rather than to the encoding, per `spec/05-storage.md`, and
//! `ROARING` in the section 6.2 table is what encodes it.
//!
//! Shared symbol tables and shared dictionaries across columns, which are section 6.4 and are the
//! measurement this milestone exists for. Everything here is one column on its own, which is the
//! baseline they get compared against.

use rudb_common::{Error, Result};

use crate::fsst::SymbolTable;
use crate::integer;
use crate::reader::Reader;

/// How deep the recursion goes. A dictionary of a dictionary is not a thing, so this only has to
/// stop the dictionary's own entries from being dictionary encoded again.
const MAX_DEPTH: u8 = 2;

/// How little sharing between neighbours is still worth offering front coding for, as one over
/// this. A twentieth of the column is around where the prefix lengths start paying for themselves,
/// and below it the candidate is an encode of the whole column that loses.
const SHARE_DIVISOR: usize = 20;

/// How many bytes of a column the symbol table is trained on.
///
/// The paper trains on about 16 KB. This is four times that, because training happens once per
/// chunk here rather than once per block, and because the cost of a symbol that is only in the
/// sample by accident is paid on every value in the chunk.
pub(crate) const SAMPLE_BYTES: usize = 64 * 1024;

/// What a string chunk is encoded as. The discriminant is the tag byte and is part of the format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One value repeated.
    Constant = 0,
    /// Lengths and raw bytes.
    Plain = 1,
    /// Lengths, a symbol table, and FSST compressed bytes.
    Fsst = 2,
    /// The distinct values as a string chunk of their own, and codes into it as an integer chunk.
    Dict = 3,
    /// Shared prefix lengths as an integer chunk, and what is left of each value as a string chunk.
    Front = 4,
}

impl Kind {
    fn tag(self) -> u8 {
        self as u8
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Constant),
            1 => Ok(Self::Plain),
            2 => Ok(Self::Fsst),
            3 => Ok(Self::Dict),
            4 => Ok(Self::Front),
            other => Err(Error::internal(format!("unknown string encoding tag {other}"))),
        }
    }

    /// The name that goes in a report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Constant => "CONSTANT",
            Self::Plain => "PLAIN",
            Self::Fsst => "FSST",
            Self::Dict => "DICT",
            Self::Front => "FRONT",
        }
    }
}

/// Encodes a chunk of strings, choosing whatever comes out smallest.
///
/// # Errors
///
/// If the chunk is longer than `u32::MAX` values, or if an encoding produces something its own
/// decoder would not accept.
pub fn encode(values: &[&[u8]]) -> Result<Vec<u8>> {
    encode_at(values, 0)
}

/// Decodes a chunk that sits at the front of a longer buffer, and says how many bytes it took.
///
/// A column group holds one of these per column, and the decoder on that side cannot know where
/// one ends until it has been read.
///
/// # Errors
///
/// As [`decode`], except that trailing bytes are what the caller asked about rather than an error.
pub fn decode_prefix(bytes: &[u8]) -> Result<(Vec<Vec<u8>>, usize)> {
    let mut reader = Reader::new(bytes);
    let values = decode_chunk(&mut reader)?;
    Ok((values, reader.used()))
}

/// [`describe`] over a chunk at the front of a longer buffer, and how many bytes it took.
///
/// # Errors
///
/// As [`decode_prefix`].
pub fn describe_prefix(bytes: &[u8]) -> Result<(String, usize)> {
    let mut reader = Reader::new(bytes);
    let text = describe_chunk(&mut reader)?;
    Ok((text, reader.used()))
}

/// Decodes a chunk written by [`encode`].
///
/// # Errors
///
/// If the bytes are truncated, carry an unknown tag, or describe a chunk whose parts disagree.
pub fn decode(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut reader = Reader::new(bytes);
    let values = decode_chunk(&mut reader)?;
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding a string chunk",
            reader.remaining()
        )));
    }
    Ok(values)
}

/// The size of every candidate that applies, for a report that wants to say what was chosen over
/// what.
///
/// # Errors
///
/// As [`encode`].
pub fn candidate_sizes(values: &[&[u8]]) -> Result<Vec<(Kind, usize)>> {
    let mut sizes = Vec::new();
    for kind in candidates(values, 0) {
        if let Some(bytes) = encode_as(kind, values, 0)? {
            sizes.push((kind, bytes.len()));
        }
    }
    Ok(sizes)
}

/// The shape a chunk was encoded as, as a line of text like `DICT(FSST, RLE(...))`.
///
/// # Errors
///
/// As [`decode`].
pub fn describe(bytes: &[u8]) -> Result<String> {
    let mut reader = Reader::new(bytes);
    describe_chunk(&mut reader)
}

fn encode_at(values: &[&[u8]], depth: u8) -> Result<Vec<u8>> {
    let mut best: Option<Vec<u8>> = None;
    for kind in candidates(values, depth) {
        let Some(bytes) = encode_as(kind, values, depth)? else {
            continue;
        };
        if best.as_ref().is_none_or(|current| bytes.len() < current.len()) {
            best = Some(bytes);
        }
    }
    best.ok_or_else(|| Error::internal("no string encoding applied to the chunk"))
}

fn candidates(values: &[&[u8]], depth: u8) -> Vec<Kind> {
    let mut kinds = vec![Kind::Plain];
    if values.is_empty() {
        return kinds;
    }
    if values.iter().all(|value| *value == values[0]) {
        return vec![Kind::Constant];
    }
    kinds.push(Kind::Fsst);
    if depth < MAX_DEPTH && distinct_values(values).len() < values.len() {
        kinds.push(Kind::Dict);
    }
    if depth < MAX_DEPTH && sharing_of(values) >= total_len(values) / SHARE_DIVISOR {
        kinds.push(Kind::Front);
    }
    kinds
}

/// How many bytes each value shares with the value before it, added up.
///
/// This is a full pass over the column, and it is here rather than on a sample because it is byte
/// comparisons that stop at the first difference, which on a column with nothing to share stops
/// immediately. Against training a symbol table and compressing the whole column, which is what
/// offering the candidate would cost, it is not worth sampling.
fn sharing_of(values: &[&[u8]]) -> usize {
    let mut shared = 0;
    for pair in values.windows(2) {
        shared += shared_prefix(pair[0], pair[1]);
    }
    shared
}

/// Every value split into the bytes it shares with the value before it and the bytes it does not.
///
/// The suffixes point into the values, so this costs the prefix lengths and nothing else. It is
/// shared with [`crate::multi`], which front codes a column before compressing it against a symbol
/// table that belongs to the whole group.
pub(crate) fn front_code<'a>(values: &[&'a [u8]]) -> (Vec<i64>, Vec<&'a [u8]>) {
    let mut prefixes = Vec::with_capacity(values.len());
    let mut suffixes: Vec<&'a [u8]> = Vec::with_capacity(values.len());
    let mut previous: &[u8] = b"";
    for value in values {
        let value: &'a [u8] = value;
        let shared = shared_prefix(previous, value);
        prefixes.push(shared as i64);
        suffixes.push(&value[shared..]);
        previous = value;
    }
    (prefixes, suffixes)
}

/// The other half. The suffixes are consumed because the values are built out of them.
///
/// # Errors
///
/// If a prefix is negative or is longer than the value it is a prefix of, which is what a corrupt
/// or hand written chunk looks like from here.
pub(crate) fn front_decode(prefixes: &[i64], suffixes: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>> {
    let mut values: Vec<Vec<u8>> = Vec::with_capacity(suffixes.len());
    for (index, suffix) in suffixes.into_iter().enumerate() {
        let shared = usize::try_from(prefixes[index])
            .map_err(|_| Error::internal("a negative shared prefix length"))?;
        let previous: &[u8] = if index == 0 { b"" } else { &values[index - 1] };
        if shared > previous.len() {
            return Err(Error::internal(format!(
                "a value shares {shared} bytes with a value {} bytes long",
                previous.len()
            )));
        }
        let mut value = Vec::with_capacity(shared + suffix.len());
        value.extend_from_slice(&previous[..shared]);
        value.extend_from_slice(&suffix);
        values.push(value);
    }
    Ok(values)
}

fn shared_prefix(previous: &[u8], value: &[u8]) -> usize {
    let limit = previous.len().min(value.len());
    let mut shared = 0;
    while shared < limit && previous[shared] == value[shared] {
        shared += 1;
    }
    shared
}

fn total_len(values: &[&[u8]]) -> usize {
    values.iter().map(|value| value.len()).sum()
}

fn encode_as(kind: Kind, values: &[&[u8]], depth: u8) -> Result<Option<Vec<u8>>> {
    let mut out = vec![kind.tag()];
    put_u32(&mut out, u32::try_from(values.len()).map_err(|_| too_long(values.len()))?);
    match kind {
        Kind::Constant => {
            let Some(first) = values.first() else {
                return Ok(None);
            };
            if values.iter().any(|value| value != first) {
                return Ok(None);
            }
            put_u32(&mut out, u32::try_from(first.len()).map_err(|_| too_long(first.len()))?);
            out.extend_from_slice(first);
        }
        Kind::Plain => {
            out.extend_from_slice(&encode_lengths(values)?);
            for value in values {
                out.extend_from_slice(value);
            }
        }
        Kind::Fsst => {
            let sample = sample_of(values);
            let table = SymbolTable::train(&sample);
            if table.is_empty() {
                return Ok(None);
            }
            let mut compressed = Vec::new();
            let mut lengths = Vec::with_capacity(values.len());
            for value in values {
                let before = compressed.len();
                table.compress(value, &mut compressed);
                lengths.push((compressed.len() - before) as i64);
            }
            table.serialize(&mut out);
            out.extend_from_slice(&integer::encode(&lengths)?);
            out.extend_from_slice(&compressed);
        }
        Kind::Dict => {
            let dictionary = distinct_values(values);
            if dictionary.is_empty() {
                return Ok(None);
            }
            let codes = codes_over(values, &dictionary);
            let entries: Vec<&[u8]> = dictionary.iter().map(Vec::as_slice).collect();
            out.extend_from_slice(&encode_at(&entries, depth + 1)?);
            out.extend_from_slice(&integer::encode(&codes)?);
        }
        Kind::Front => {
            let (prefixes, suffixes) = front_code(values);
            out.extend_from_slice(&integer::encode(&prefixes)?);
            out.extend_from_slice(&encode_at(&suffixes, depth + 1)?);
        }
    }
    Ok(Some(out))
}

fn decode_chunk(reader: &mut Reader<'_>) -> Result<Vec<Vec<u8>>> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    match kind {
        Kind::Constant => {
            let len = reader.u32()? as usize;
            let value = reader.bytes(len)?.to_vec();
            Ok(vec![value; count])
        }
        Kind::Plain => {
            let lengths = decode_lengths(reader, count)?;
            let mut values = Vec::with_capacity(count);
            for length in lengths {
                values.push(reader.bytes(length)?.to_vec());
            }
            Ok(values)
        }
        Kind::Fsst => {
            let (table, used) = SymbolTable::deserialize(reader.rest())?;
            reader.skip(used)?;
            let lengths = decode_lengths(reader, count)?;
            let mut values = Vec::with_capacity(count);
            for length in lengths {
                let compressed = reader.bytes(length)?;
                let mut value = Vec::new();
                table.decompress(compressed, &mut value)?;
                values.push(value);
            }
            Ok(values)
        }
        Kind::Dict => {
            let dictionary = decode_chunk(reader)?;
            let codes = decode_integers(reader)?;
            if codes.len() != count {
                return Err(Error::internal(format!(
                    "a dictionary chunk says it holds {count} values and has {} codes",
                    codes.len()
                )));
            }
            let mut values = Vec::with_capacity(count);
            for code in codes {
                let entry =
                    usize::try_from(code).ok().and_then(|index| dictionary.get(index)).ok_or_else(
                        || Error::internal(format!("code {code} is not in the dictionary")),
                    )?;
                values.push(entry.clone());
            }
            Ok(values)
        }
        Kind::Front => {
            let prefixes = decode_integers(reader)?;
            let suffixes = decode_chunk(reader)?;
            if prefixes.len() != count || suffixes.len() != count {
                return Err(Error::internal(format!(
                    "a front coded chunk says it holds {count} values and has {} prefixes and {} suffixes",
                    prefixes.len(),
                    suffixes.len()
                )));
            }
            front_decode(&prefixes, suffixes)
        }
    }
}

fn describe_chunk(reader: &mut Reader<'_>) -> Result<String> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    Ok(match kind {
        Kind::Constant => {
            let len = reader.u32()? as usize;
            reader.bytes(len)?;
            "CONSTANT".to_string()
        }
        Kind::Plain => {
            let (shape, lengths) = describe_lengths(reader, count)?;
            reader.skip(lengths.iter().sum())?;
            format!("PLAIN({shape})")
        }
        Kind::Fsst => {
            let (table, used) = SymbolTable::deserialize(reader.rest())?;
            reader.skip(used)?;
            let (shape, lengths) = describe_lengths(reader, count)?;
            reader.skip(lengths.iter().sum())?;
            format!("FSST[{}]({shape})", table.len())
        }
        Kind::Dict => {
            let entries = describe_chunk(reader)?;
            let codes = describe_integers(reader)?;
            format!("DICT({entries}, {codes})")
        }
        Kind::Front => {
            let prefixes = describe_integers(reader)?;
            let suffixes = describe_chunk(reader)?;
            format!("FRONT({prefixes}, {suffixes})")
        }
    })
}

/// The shape of the length array and the lengths themselves, because a describe has to walk past
/// the payload to leave the reader where the next chunk starts and the payload size is the sum of
/// the lengths.
fn describe_lengths(reader: &mut Reader<'_>, count: usize) -> Result<(String, Vec<usize>)> {
    let (shape, _) = integer::describe_prefix(reader.rest())?;
    let lengths = decode_lengths(reader, count)?;
    Ok((shape, lengths))
}

fn encode_lengths(values: &[&[u8]]) -> Result<Vec<u8>> {
    let lengths: Vec<i64> = values.iter().map(|value| value.len() as i64).collect();
    integer::encode(&lengths)
}

fn decode_lengths(reader: &mut Reader<'_>, count: usize) -> Result<Vec<usize>> {
    let lengths = decode_integers(reader)?;
    if lengths.len() != count {
        return Err(Error::internal(format!(
            "a string chunk says it holds {count} values and has {} lengths",
            lengths.len()
        )));
    }
    lengths
        .into_iter()
        .map(|length| {
            usize::try_from(length).map_err(|_| Error::internal("a negative string length"))
        })
        .collect()
}

/// Reads one nested integer chunk. The integer decoder wants a slice of exactly its own chunk and
/// the reader does not know how long that is, so it decodes from the rest of the buffer and is told
/// afterwards how much it used.
fn decode_integers(reader: &mut Reader<'_>) -> Result<Vec<i64>> {
    let (values, used) = integer::decode_prefix(reader.rest())?;
    reader.skip(used)?;
    Ok(values)
}

fn describe_integers(reader: &mut Reader<'_>) -> Result<String> {
    let (text, used) = integer::describe_prefix(reader.rest())?;
    reader.skip(used)?;
    Ok(text)
}

/// A sample of the column spread across the whole of it, taken at random skips rather than at a
/// fixed stride.
///
/// Section 6.3 makes the point about choosing an encoding from a sample and it applies at least as
/// much to training a symbol table. Column data is frequently sorted or clustered, so the first
/// 64 KB of a URL column is the hosts that sort first and a table trained on it escapes most of the
/// rest of the column.
///
/// The skips are random rather than fixed because a fixed stride aliases. Column data is also
/// frequently periodic, and a stride that shares a factor with the period samples one phase of it
/// and never sees the others. That is not a hypothetical: the first version of this took every
/// `n`th value, and on a test column whose values cycle with a period that the stride happened to
/// divide, the table it trained was 3.4 times worse than one trained on the whole column, because
/// it learned eight byte symbols that only line up with the phase it saw and had no shorter symbols
/// left to fall back on.
///
/// The generator is a fixed seed xorshift, so the sample is a function of the column and encoding
/// the same values twice produces the same bytes.
pub(crate) fn sample_of<'a>(values: &[&'a [u8]]) -> Vec<&'a [u8]> {
    sample_bytes_of(values, SAMPLE_BYTES)
}

/// [`sample_of`] with the byte budget spelled out, for a caller training one table over several
/// columns that has to split the budget between them.
pub(crate) fn sample_bytes_of<'a>(values: &[&'a [u8]], budget: usize) -> Vec<&'a [u8]> {
    let budget = budget.max(1);
    let total: usize = values.iter().map(|value| value.len()).sum();
    if total <= budget {
        return values.to_vec();
    }
    let stride = total.div_ceil(budget).max(1);
    let span = (stride * 2 - 1).max(1) as u64;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut sample = Vec::with_capacity(values.len() / stride + 1);
    let mut at = 0usize;
    while at < values.len() {
        sample.push(values[at]);
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        at += 1 + (state % span) as usize;
    }
    sample
}

/// The distinct values in sorted order, for the same reason the integer dictionary is sorted: an
/// ordered dictionary turns a range predicate into a code range rather than a code set.
fn distinct_values(values: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut distinct: Vec<Vec<u8>> = values.iter().map(|value| value.to_vec()).collect();
    distinct.sort_unstable();
    distinct.dedup();
    distinct
}

fn codes_over(values: &[&[u8]], dictionary: &[Vec<u8>]) -> Vec<i64> {
    values
        .iter()
        .map(|value| {
            dictionary
                .binary_search_by(|entry| entry.as_slice().cmp(value))
                .expect("the dictionary is the distinct values of this chunk") as i64
        })
        .collect()
}

fn too_long(len: usize) -> Error {
    Error::internal(format!("a string chunk of {len} is longer than the format allows"))
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(count: usize) -> Vec<Vec<u8>> {
        let hosts = ["www.example.com", "shop.example.com", "news.other.example.org"];
        let paths = ["/index.html", "/catalog/item", "/search", "/user/profile/settings"];
        (0..count)
            .map(|index| {
                let host = hosts[index % hosts.len()];
                let path = paths[(index / 3) % paths.len()];
                format!("http://{host}{path}?session={}&ref=google", index * 7).into_bytes()
            })
            .collect()
    }

    /// The same values with a scrambled identifier stuck on the front of each, for the tests that
    /// need neighbouring values to have nothing in common. Shuffling the order is not enough,
    /// because two URLs picked at random still agree on a scheme and often on a host.
    fn keyed(values: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let key = (index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) % 1_000_000_007;
                let mut out = format!("{key:010}/").into_bytes();
                out.extend_from_slice(&value);
                out
            })
            .collect()
    }

    fn borrow(values: &[Vec<u8>]) -> Vec<&[u8]> {
        values.iter().map(Vec::as_slice).collect()
    }

    fn round_trip(values: &[Vec<u8>]) -> Vec<u8> {
        let borrowed = borrow(values);
        let bytes = encode(&borrowed).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back, values, "{}", describe(&bytes).unwrap());
        bytes
    }

    fn kind_of(bytes: &[u8]) -> Kind {
        Kind::from_tag(bytes[0]).unwrap()
    }

    fn raw_size(values: &[Vec<u8>]) -> usize {
        values.iter().map(Vec::len).sum::<usize>() + values.len() * 4
    }

    #[test]
    fn an_empty_chunk_round_trips() {
        let bytes = round_trip(&[]);
        assert_eq!(kind_of(&bytes), Kind::Plain);
    }

    #[test]
    fn a_constant_column_costs_what_one_value_costs() {
        let values = vec![b"https://www.example.com/".to_vec(); 100_000];
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Constant);
        assert_eq!(bytes.len(), 9 + 24);
    }

    #[test]
    fn a_url_column_of_unique_values_uses_fsst() {
        // Every value distinct, so a dictionary is the values plus an index and cannot win, and
        // every value starts with an identifier of its own, so neighbours share nothing and front
        // coding cannot win either. What is left is a column with a lot of repeated vocabulary in
        // it and no structure that anything but a symbol table can reach. Section 6.5 says the high
        // cardinality end of `URL` falls back to FSST only and this is that case.
        let values = keyed(urls(20_000));
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Fsst);
        // Eleven bytes of every value are the identifier and a separator and nothing compresses
        // them, so the ratio here is lower than the one FSST gets on the URLs on their own.
        let ratio = raw_size(&values) as f64 / bytes.len() as f64;
        assert!(ratio > 4.0, "{ratio:.2}x");
    }

    #[test]
    fn a_sample_of_a_periodic_column_learns_every_phase_of_it() {
        // This column is periodic and its period is what a fixed stride would have divided. The
        // sample has to see all of it, because a table trained on one phase learns eight byte
        // symbols that only line up with that phase and has nothing shorter to fall back on. The
        // measured cost of getting this wrong was 3.4 times the compressed size.
        let values = urls(20_000);
        let borrowed = borrow(&values);
        let sample = sample_of(&borrowed);
        let mut phases: Vec<&[u8]> = sample
            .iter()
            .map(|value| {
                let query =
                    value.iter().position(|byte| *byte == b'?').expect("every value has a query");
                &value[..query]
            })
            .collect();
        phases.sort_unstable();
        phases.dedup();
        // Three hosts and four paths, and the sample has to contain all twelve of the combinations.
        assert_eq!(phases.len(), 12);
        let whole = SymbolTable::train(&borrowed);
        let sampled = SymbolTable::train(&sample);
        let mut on_whole = Vec::new();
        let mut on_sample = Vec::new();
        for value in &borrowed {
            whole.compress(value, &mut on_whole);
            sampled.compress(value, &mut on_sample);
        }
        // Training on a twentieth of the column is allowed to cost something. It is not allowed to
        // cost a factor.
        assert!(
            on_sample.len() < on_whole.len() * 5 / 4,
            "{} against {}",
            on_sample.len(),
            on_whole.len()
        );
    }

    #[test]
    fn a_repeating_column_becomes_a_dictionary_of_compressed_entries() {
        // The DICT_FSST row of the section 6.2 table, which is not an encoding of its own here: it
        // is a dictionary whose entries went back through the chooser. A dictionary sorts its
        // entries, so what comes back on anything URL shaped is front coding with the leftovers
        // FSST compressed, and nobody had to name that shape for the chooser to arrive at it.
        let distinct = urls(500);
        let values: Vec<Vec<u8>> =
            (0..50_000).map(|index| distinct[index * 7919 % distinct.len()].clone()).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Dict);
        let shape = describe(&bytes).unwrap();
        assert!(shape.starts_with("DICT(FRONT("), "{shape}");
        assert!(shape.contains("FSST"), "{shape}");
        let ratio = raw_size(&values) as f64 / bytes.len() as f64;
        assert!(ratio > 20.0, "{ratio:.2}x, {shape}");
    }

    #[test]
    fn a_column_of_long_runs_costs_almost_nothing() {
        // A dictionary makes the codes an integer chunk, and the integer chunk knows what to do
        // with runs, so run length encoding of strings falls out of the recursion.
        let distinct = urls(50);
        let mut values = Vec::new();
        for entry in &distinct {
            values.extend(std::iter::repeat_n(entry.clone(), 1000));
        }
        let bytes = round_trip(&values);
        let shape = describe(&bytes).unwrap();
        assert!(shape.contains("RLE"), "{shape}");
        assert!(bytes.len() < 2000, "{} bytes: {shape}", bytes.len());
    }

    #[test]
    fn incompressible_strings_stay_close_to_their_own_size() {
        // The case where nothing works. It has to land on PLAIN or on an FSST that is not much
        // worse, rather than on a dictionary of every value in the column.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let values: Vec<Vec<u8>> = (0..2000)
            .map(|_| {
                (0..32)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state as u8
                    })
                    .collect()
            })
            .collect();
        let bytes = round_trip(&values);
        assert!(bytes.len() < 2000 * 32 + 3000, "{} bytes", bytes.len());
    }

    #[test]
    fn lengths_are_stored_rather_than_offsets() {
        // Every value is 24 bytes, so the lengths are a constant chunk and cost 13 bytes for the
        // whole column. Offsets would be 100,000 increasing integers.
        let values: Vec<Vec<u8>> =
            (0..100_000).map(|index| format!("{index:024}").into_bytes()).collect();
        let borrowed = borrow(&values);
        let bytes = encode_as(Kind::Plain, &borrowed, 0).unwrap().unwrap();
        assert_eq!(bytes.len(), 5 + 13 + 100_000 * 24);
    }

    #[test]
    fn empty_strings_are_values_and_not_nulls() {
        let values = vec![Vec::new(), b"a".to_vec(), Vec::new(), b"bb".to_vec()];
        round_trip(&values);
    }

    #[test]
    fn a_chunk_with_one_value_round_trips() {
        round_trip(&[b"only".to_vec()]);
    }

    #[test]
    fn every_candidate_that_applies_decodes_to_the_input() {
        let values = urls(3000);
        let borrowed = borrow(&values);
        let applicable = candidates(&borrowed, 0);
        assert!(applicable.len() >= 2, "{applicable:?}");
        for kind in applicable {
            let bytes = encode_as(kind, &borrowed, 0).unwrap().unwrap();
            assert_eq!(decode(&bytes).unwrap(), values, "{}", kind.name());
        }
    }

    #[test]
    fn the_chooser_picks_the_smallest_candidate() {
        let values = urls(2000);
        let borrowed = borrow(&values);
        let chosen = encode(&borrowed).unwrap();
        for (_, size) in candidate_sizes(&borrowed).unwrap() {
            assert!(chosen.len() <= size);
        }
    }

    #[test]
    fn a_truncated_chunk_is_an_error_and_not_a_panic() {
        let values = urls(40);
        let bytes = encode(&borrow(&values)).unwrap();
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "{len} bytes decoded");
        }
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let mut bytes = encode(&borrow(&urls(10))).unwrap();
        bytes.push(0);
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("left over"), "{error}");
    }

    #[test]
    fn an_unknown_tag_is_an_error() {
        let error = decode(&[99, 0, 0, 0, 0]).unwrap_err();
        assert!(error.message().contains("unknown string encoding tag"), "{error}");
    }

    #[test]
    fn a_dictionary_code_outside_the_dictionary_is_an_error() {
        let mut bytes = vec![Kind::Dict.tag()];
        put_u32(&mut bytes, 1);
        bytes.extend_from_slice(&encode(&[b"one".as_slice()]).unwrap());
        bytes.extend_from_slice(&integer::encode(&[9]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("not in the dictionary"), "{error}");
    }

    #[test]
    fn a_sorted_column_of_urls_is_front_coded() {
        // The M1 finding, in a test. Sorted URLs share a host and most of a path with the URL next
        // to them, FSST cannot reach those bytes because it compresses each value on its own, and
        // front coding is the shape that reaches them.
        let mut values = urls(20_000);
        values.sort();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Front);
        let shape = describe(&bytes).unwrap();
        let mut plain = Vec::new();
        let borrowed = borrow(&values);
        for (kind, size) in candidate_sizes(&borrowed).unwrap() {
            if kind == Kind::Fsst {
                plain.push(size);
            }
        }
        let fsst = plain[0];
        assert!(bytes.len() * 2 < fsst, "{} against FSST {fsst}: {shape}", bytes.len());
    }

    #[test]
    fn a_column_with_nothing_to_share_is_not_offered_front_coding() {
        // The candidate costs an encode of the whole column, so a column whose neighbours have
        // nothing in common must not be paying for it.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let values: Vec<Vec<u8>> = (0..2000)
            .map(|_| {
                (0..24)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        (state % 251) as u8
                    })
                    .collect()
            })
            .collect();
        let borrowed = borrow(&values);
        assert!(!candidates(&borrowed, 0).contains(&Kind::Front));
    }

    #[test]
    fn a_prefix_longer_than_the_value_before_it_is_an_error() {
        let mut bytes = vec![Kind::Front.tag()];
        put_u32(&mut bytes, 2);
        bytes.extend_from_slice(&integer::encode(&[0, 9]).unwrap());
        bytes.extend_from_slice(&encode(&[b"one".as_slice(), b"two".as_slice()]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("shares 9 bytes"), "{error}");
    }

    #[test]
    fn a_negative_prefix_is_an_error() {
        let mut bytes = vec![Kind::Front.tag()];
        put_u32(&mut bytes, 1);
        bytes.extend_from_slice(&integer::encode(&[-1]).unwrap());
        bytes.extend_from_slice(&encode(&[b"one".as_slice()]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("negative shared prefix"), "{error}");
    }

    #[test]
    fn a_negative_length_is_an_error() {
        let mut bytes = vec![Kind::Plain.tag()];
        put_u32(&mut bytes, 1);
        bytes.extend_from_slice(&integer::encode(&[-1]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("negative string length"), "{error}");
    }

    #[test]
    fn the_sample_is_spread_across_the_chunk_and_not_taken_from_the_front() {
        // A sorted column whose first 64 KB says nothing about the rest of it. If the sample were
        // the front, the table would learn `aaaa` and escape every `zzzz`.
        let mut values: Vec<Vec<u8>> = Vec::new();
        for index in 0..20_000 {
            let head = if index < 10_000 { "aaaaaaaaaaaaaaaa" } else { "zzzzzzzzzzzzzzzz" };
            values.push(format!("{head}/{index:08}").into_bytes());
        }
        let borrowed = borrow(&values);
        let sample = sample_of(&borrowed);
        let first_half = sample.iter().filter(|value| value.starts_with(b"aaaa")).count();
        let second_half = sample.len() - first_half;
        assert!(first_half > 0 && second_half > 0, "{first_half} and {second_half}");
        let bytes = round_trip(&values);
        let ratio = raw_size(&values) as f64 / bytes.len() as f64;
        assert!(ratio > 4.0, "{ratio:.2}x");
    }
}

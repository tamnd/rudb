//! A dense code for every row of a wide integer column, and the values the codes stand for.
//!
//! A text column held in a table wide dictionary already has this: a code per row that is a small
//! number, so anything that has to tell its values apart can index an array with it instead of
//! hashing the value. A wide integer column has nothing of the kind. `UserID` in ClickBench has a
//! value per ten rows and every one of them is a scattered sixty four bit number, so `COUNT(DISTINCT
//! UserID) GROUP BY RegionID` has to hash about a million pairs of region and user to throw the
//! repeats away, and on six threads that was two thirds of a query DuckDB runs in thirty
//! milliseconds.
//!
//! With a code per row the pairs are found without a hash. A user belongs to one region in nearly
//! every row that holds it, so an array with a slot per code, holding the first group seen for it,
//! tells a new pair from a repeat with one load, and only the few users seen in a second group go
//! anywhere else. The array for a million users is four megabytes, shared by the threads, and a
//! repeat is a load that finds its own group already there.
//!
//! Built at checkpoint for every signed integer column of a table of at least [`FEWEST_ROWS`] rows
//! whose exact distinct count is at least [`FEWEST_VALUES`] and is below the table's rows, out of its
//! own share of the table's column bytes, cheapest first. A column with fewer values than that is
//! either narrow enough to index by its value or cheap enough to hash, and a column with a value per
//! row has nothing to tell apart. Deleting the section changes no answer, only how a query that
//! could have used it runs.

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering as Atomic};

use rudb_common::bounds::Bound;
use rudb_common::{LogicalType, Result};

use crate::graph::BUDGET_FLOOR;
use crate::section::{self, Attachment};
use crate::{Catalog, Reader, invalid};

/// The share of a table's stored column bytes its value codes may cost together.
pub const VALUE_CODES_SHARE: u64 = 25;

/// Tables with fewer rows than this are not given value codes, since hashing every row of them costs
/// less than a millisecond.
pub const FEWEST_ROWS: usize = 1 << 16;

/// Columns with fewer distinct values than this are not given value codes. A hash table of that
/// many values sits in the second level cache, which is what the codes would have bought.
pub const FEWEST_VALUES: u64 = 1 << 16;

/// What recording one column cost and whether it was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// The column, by position in the table.
    pub column: usize,
    /// How many distinct values it holds.
    pub values: usize,
    /// What the section costs, kept or not.
    pub bytes: usize,
    /// Whether it went into the file.
    pub built: bool,
}

/// Whether the table has a current entry for every column that wants codes, kept or recorded as
/// not kept.
#[must_use]
pub fn current(reader: &Reader) -> bool {
    let table = reader.table();
    wide_columns(reader).all(|column| {
        table.sections().iter().any(|held| {
            held.kind == *section::VALUE_CODES
                && u64::try_from(column) == Ok(held.id)
                && held.current(table.generation())
        })
    })
}

/// Records every wide integer column of a table and attaches them in one commit.
///
/// # Errors
///
/// If the file cannot be opened, a column cannot be read, or the attach fails.
pub fn build_value_codes(path: &Path, table: &str) -> Result<Vec<Built>> {
    build_value_codes_within(path, table, VALUE_CODES_SHARE)
}

/// The same, against a budget of `share` percent of the table's stored column bytes.
///
/// Every wide column gets an entry, and one that does not fit gets one with no bytes, so that
/// [`current`] can tell a column that was decided against from one nobody looked at.
///
/// # Errors
///
/// If the file cannot be opened, a column cannot be read, or the attach fails.
pub fn build_value_codes_within(path: &Path, table: &str, share: u64) -> Result<Vec<Built>> {
    let reader = Catalog::open(path)?.table(table)?;
    let columns = wide_columns(&reader).collect::<Vec<_>>();
    if columns.is_empty() {
        return Ok(Vec::new());
    }
    let allowance = (reader.layout().columns_total().saturating_mul(share) / 100).max(BUDGET_FLOOR);
    let mut report = Vec::with_capacity(columns.len());
    let mut payloads = Vec::with_capacity(columns.len());
    for &column in &columns {
        let payload = encode(&reader, column)?;
        let (values, bytes) =
            payload.as_ref().map_or((0, 0), |(values, bytes)| (*values, bytes.len()));
        report.push(Built { column, values, bytes, built: false });
        payloads.push(payload.map(|(_, bytes)| bytes).unwrap_or_default());
    }
    let mut order = (0..report.len()).filter(|&at| report[at].bytes > 0).collect::<Vec<_>>();
    order.sort_by_key(|&at| report[at].bytes);
    let mut spent = 0_u64;
    for at in order {
        let cost = report[at].bytes as u64;
        if spent.saturating_add(cost) <= allowance {
            spent += cost;
            report[at].built = true;
        }
    }
    drop(reader);
    let attachments = report
        .iter()
        .zip(&payloads)
        .map(|(one, bytes)| {
            Ok(Attachment {
                kind: *section::VALUE_CODES,
                id: u64::try_from(one.column).map_err(|_| invalid("column index overflow"))?,
                flags: 0,
                header_bytes: if one.built {
                    0
                } else {
                    u32::try_from(one.bytes).unwrap_or(u32::MAX)
                },
                bytes: if one.built { bytes } else { &[] },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    crate::attach(path, table, &attachments)?;
    Ok(report)
}

/// The payload of one column and how many distinct values it holds, or `None` for a column that
/// turned out to hold a value per row after all.
///
/// The layout, all little endian: the table's rows as a `u64`, the count `d` of distinct values as
/// a `u32`, the width `w` of a code in bits as a `u32`, then the `d` values in ascending order as
/// `i64`, then the codes as a run of `u64` words, the code of row `r` in bits `r * w` up to
/// `r * w + w` counted from the low bit of the first word, with one word more than the codes need.
/// A null is the code `d`, which is why `w` has room for one more than the values.
fn encode(reader: &Reader, column: usize) -> Result<Option<(usize, Vec<u8>)>> {
    let rows = reader.table().rows();
    let rows_u32 = u32::try_from(rows).map_err(|_| invalid("a table too long for value codes"))?;
    // Every value beside its row, sorted by value, so that a run of equal values is one code and the
    // codes come out in the order of the values. Sixteen bytes a row, which a checkpoint can afford
    // and which is paid once.
    let mut held: Vec<(i64, u32)> = Vec::with_capacity(rows);
    let mut block = Vec::new();
    let mut first = 0_u32;
    for part in 0..reader.parts() {
        let chunk = reader.read(part, &[column])?;
        let vector = chunk.column(0)?;
        let len = reader.part_rows(part);
        if vector.len() != len || !vector.signed_block(&mut block) || block.len() < len {
            return Ok(None);
        }
        let nulls = vector.validity().has_nulls(len);
        for (at, &value) in block[..len].iter().enumerate() {
            if !(nulls && vector.is_null_at(at)) {
                held.push((value, first + at as u32));
            }
        }
        first += u32::try_from(len).map_err(|_| invalid("a part too long for value codes"))?;
    }
    if first != rows_u32 {
        return Err(invalid("value codes found a row count that differs from the table"));
    }
    held.sort_unstable();
    let mut values = Vec::new();
    let mut codes = vec![0_u32; rows];
    for &(value, row) in &held {
        if values.last() != Some(&value) {
            values.push(value);
        }
        codes[row as usize] = (values.len() - 1) as u32;
    }
    if values.len() >= held.len() {
        return Ok(None);
    }
    let null = u32::try_from(values.len()).map_err(|_| invalid("too many values to code"))?;
    // The rows that were not pushed are the nulls, and their code is the one past the values.
    let mut coded = vec![false; rows];
    for &(_, row) in &held {
        coded[row as usize] = true;
    }
    drop(held);
    for (code, coded) in codes.iter_mut().zip(coded) {
        if !coded {
            *code = null;
        }
    }
    let width = u32::BITS - null.leading_zeros();
    let width = width.max(1);
    let words = packed_words(rows, width);
    let mut packed = vec![0_u64; words];
    for (row, &code) in codes.iter().enumerate() {
        let bit = row * width as usize;
        let (word, shift) = (bit / 64, bit % 64);
        packed[word] |= u64::from(code) << shift;
        if shift + width as usize > 64 {
            packed[word + 1] |= u64::from(code) >> (64 - shift);
        }
    }
    let mut bytes = Vec::with_capacity(16 + values.len() * 8 + words * 8);
    bytes.extend_from_slice(&(rows as u64).to_le_bytes());
    bytes.extend_from_slice(&null.to_le_bytes());
    bytes.extend_from_slice(&width.to_le_bytes());
    for value in &values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    for word in &packed {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    Ok(Some((values.len(), bytes)))
}

/// How many words hold `rows` codes of `width` bits, with the one more the reader may load past
/// the last code.
fn packed_words(rows: usize, width: u32) -> usize {
    (rows * width as usize).div_ceil(64) + 1
}

/// One column's value codes, as read out of the file.
#[derive(Debug)]
pub struct ValueCodes {
    rows: usize,
    values: Vec<i64>,
    width: u32,
    packed: Vec<u64>,
}

impl ValueCodes {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let word = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
        let long = |at: usize| Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?));
        let rows = usize::try_from(long(0)?).ok()?;
        let count = word(8)? as usize;
        let width = word(12)?;
        if width == 0 || width > 32 || u64::from(u32::try_from(count).ok()?) >> width != 0 {
            return None;
        }
        let values_at = 16_usize;
        let packed_at = values_at.checked_add(count.checked_mul(8)?)?;
        let words = packed_words(rows, width);
        if bytes.len() != packed_at.checked_add(words.checked_mul(8)?)? {
            return None;
        }
        let values = (0..count)
            .map(|at| long(values_at + at * 8).map(|value| value as i64))
            .collect::<Option<Vec<_>>>()?;
        if values.windows(2).any(|pair| pair[0] >= pair[1]) {
            return None;
        }
        let packed = (0..words).map(|at| long(packed_at + at * 8)).collect::<Option<Vec<_>>>()?;
        Some(Self { rows, values, width, packed })
    }

    /// The table's rows, one code each.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Every distinct value in ascending order, so that a code is the value's place in it.
    #[must_use]
    pub fn values(&self) -> &[i64] {
        &self.values
    }

    /// The code of a null, one past the last value.
    #[must_use]
    pub fn null(&self) -> u32 {
        self.values.len() as u32
    }

    /// The code of one row, which the caller keeps below [`Self::rows`].
    #[inline]
    #[must_use]
    pub fn code_at(&self, row: usize) -> u32 {
        let bit = row * self.width as usize;
        let (word, shift) = (bit / 64, (bit % 64) as u32);
        // The words are padded by one, so the second load is always in the run.
        let pair = u128::from(self.packed[word]) | (u128::from(self.packed[word + 1]) << 64);
        ((pair >> shift) as u32) & (u32::MAX >> (32 - self.width))
    }

    /// The codes of the rows in `rows`, appended to `out`.
    pub fn codes_into(&self, rows: Range<usize>, out: &mut Vec<u32>) {
        out.extend(rows.map(|row| self.code_at(row)));
    }
}

/// The value codes of a column, when the table carries a current section for it.
#[must_use]
pub fn value_codes(reader: &Reader, column: usize) -> Option<ValueCodes> {
    let table = reader.table();
    let id = u64::try_from(column).ok()?;
    let held =
        table.sections().iter().find(|held| held.kind == *section::VALUE_CODES && held.id == id)?;
    if !held.usable(table.generation()) {
        return None;
    }
    let parsed = ValueCodes::parse(&reader.payload(held).ok()?)?;
    (parsed.rows == table.rows()).then_some(parsed)
}

/// The columns that want codes: a signed integer whose exact distinct count is known, is at least
/// [`FEWEST_VALUES`], and is below the rows, on a table of at least [`FEWEST_ROWS`].
fn wide_columns(reader: &Reader) -> impl Iterator<Item = usize> + '_ {
    let table = reader.table();
    let rows = table.rows();
    table.fields().iter().enumerate().filter_map(move |(column, field)| {
        let signed = matches!(
            field.ty,
            LogicalType::TinyInt
                | LogicalType::SmallInt
                | LogicalType::Integer
                | LogicalType::BigInt
        );
        let distinct = reader.distinct_values(column).ok().flatten()?;
        (signed && rows >= FEWEST_ROWS && distinct >= FEWEST_VALUES && distinct < rows as u64)
            .then_some(column)
    })
}

/// How many distinct values of `counted` each value of `group` holds, the way `COUNT(DISTINCT
/// counted) GROUP BY group` counts them, or `None` when the group column is not a signed integer
/// narrow enough to index by.
///
/// Every group comes out, including one whose counted values are all null and so count zero. A
/// null group comes out as `None`.
///
/// One pass over the parts split among `workers` threads. A group is a slot, its distance from the
/// lowest value the stripes record plus one, so no worker waits on another to learn the range. The
/// threads share one array indexed by the code, holding the first slot each code was seen with, and
/// the one that fills a code's place counts the pair. A thread that finds the place taken by another
/// slot puts its pair aside, and the pairs put aside are sorted and told apart once every part is
/// done. A user belongs to one region in nearly every row that holds it, so there are few of them.
///
/// # Errors
///
/// If a part cannot be read.
pub fn distinct_per_group(
    reader: &Reader,
    group: usize,
    counted: &ValueCodes,
    workers: usize,
) -> Result<Option<Vec<(Option<i64>, u64)>>> {
    let rows = reader.table().rows();
    if counted.rows() != rows {
        return Ok(None);
    }
    let Some((low, slots)) = group_slots(reader, group) else { return Ok(None) };
    let parts = reader.parts();
    let mut starts = Vec::with_capacity(parts + 1);
    let mut first = 0;
    for part in 0..parts {
        starts.push(first);
        first += reader.part_rows(part);
    }
    starts.push(first);
    if first != rows {
        return Ok(None);
    }
    let starts = &starts;
    let workers = workers.clamp(1, parts.max(1));
    let next = AtomicUsize::new(0);
    let next = &next;
    // Zero is a code not seen yet and any other value is the slot it was first seen with plus one.
    let firsts = (0..counted.null()).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();
    let firsts = &firsts;
    let done = std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|_| {
                scope.spawn(move || {
                    let mut seen = Seen {
                        present: vec![false; slots],
                        counts: vec![0_u64; slots],
                        again: Vec::new(),
                    };
                    let mut block = Vec::new();
                    loop {
                        let part = next.fetch_add(1, Atomic::Relaxed);
                        if part >= parts {
                            return Ok(Some(seen));
                        }
                        let rows = starts[part]..starts[part + 1];
                        let read = reader.read(part, &[group])?;
                        let vector = read.column(0)?;
                        if vector.len() != rows.len()
                            || !vector.signed_block(&mut block)
                            || block.len() < rows.len()
                        {
                            return Ok(None);
                        }
                        let has_nulls = vector.validity().has_nulls(rows.len());
                        for (at, &value) in block[..rows.len()].iter().enumerate() {
                            let slot = if has_nulls && vector.is_null_at(at) {
                                0
                            } else {
                                match value
                                    .checked_sub(low)
                                    .and_then(|gap| usize::try_from(gap).ok())
                                {
                                    Some(gap) if gap + 1 < slots => gap + 1,
                                    // A value outside the range the stripes record.
                                    _ => return Ok(None),
                                }
                            };
                            seen.present[slot] = true;
                            let code = counted.code_at(rows.start + at);
                            let Some(place) = firsts.get(code as usize) else { continue };
                            let mark = slot as u32 + 1;
                            match place.compare_exchange(0, mark, Atomic::Relaxed, Atomic::Relaxed)
                            {
                                Ok(_) => seen.counts[slot] += 1,
                                Err(held) if held != mark => {
                                    seen.again.push((u64::from(code) << 32) | slot as u64);
                                }
                                Err(_) => {}
                            }
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().map_err(|_| invalid("a distinct count worker panicked"))?)
            .collect::<Result<Vec<_>>>()
    })?;
    let mut present = vec![false; slots];
    let mut counts = vec![0_u64; slots];
    let mut again = Vec::new();
    for one in done {
        let Some(one) = one else { return Ok(None) };
        for (slot, (here, count)) in one.present.into_iter().zip(one.counts).enumerate() {
            present[slot] |= here;
            counts[slot] += count;
        }
        again.extend(one.again);
    }
    // A code seen with a group other than its first, once for each such group.
    again.sort_unstable();
    again.dedup();
    for pair in again {
        counts[pair as u32 as usize] += 1;
    }
    let mut out = Vec::new();
    for (slot, (&here, &count)) in present.iter().zip(&counts).enumerate() {
        if !here {
            continue;
        }
        let value = if slot == 0 { None } else { Some(low + (slot - 1) as i64) };
        out.push((value, count));
    }
    Ok(Some(out))
}

/// The widest range of group values [`distinct_per_group`] indexes, a million slots.
const MOST_GROUP_SLOTS: i128 = 1 << 20;

/// What one worker of [`distinct_per_group`] saw.
struct Seen {
    /// Which slots turned up at all.
    present: Vec<bool>,
    /// The codes each slot was the first to be seen with.
    counts: Vec<u64>,
    /// A code and slot, for a code already seen with another slot.
    again: Vec<u64>,
}

/// The lowest value of `group` over every stripe and the slots from the null one up to the highest,
/// or `None` when a stripe records no integer range for it or the range is too wide to index.
fn group_slots(reader: &Reader, group: usize) -> Option<(i64, usize)> {
    let (mut low, mut high) = (None::<i128>, None::<i128>);
    for stripe in reader.table().stripes() {
        let range = stripe.zone().column(group)?;
        if range.nulls == stripe.rows() {
            continue;
        }
        let (Some(Bound::Int(from)), Some(Bound::Int(to))) = (&range.low, &range.high) else {
            return None;
        };
        low = Some(low.map_or(*from, |low| low.min(*from)));
        high = Some(high.map_or(*to, |high| high.max(*to)));
    }
    let (Some(low), Some(high)) = (low, high) else { return Some((0, 1)) };
    let span = high - low + 2;
    (span <= MOST_GROUP_SLOTS).then_some((i64::try_from(low).ok()?, span as usize))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rudb_common::{Field, Value};
    use rudb_vector::{Chunk, Vector};

    use super::*;
    use crate::Writer;

    fn path(label: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("time advances").as_nanos();
        std::env::temp_dir().join(format!("rudb-codes-{label}-{}-{stamp}.rdb", std::process::id()))
    }

    /// A table of a region drawn from a few values with nulls among them, and a user drawn from a
    /// hundred thousand wide values with nulls among them.
    fn table_of(label: &str, rows: usize) -> (PathBuf, Vec<(Option<i32>, Option<i64>)>) {
        let path = path(label);
        let mut seed = 7_u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            seed >> 33
        };
        let held = (0..rows)
            .map(|_| {
                let region = next() % 50;
                let user = next() % 100_000;
                let region = (region != 0).then_some(region as i32 * 3 - 20);
                let user = (user % 97 != 0).then_some((user as i64) * 1_000_003 - 7_000_000_000);
                (region, user)
            })
            .collect::<Vec<_>>();
        let mut writer = Writer::create(
            &path,
            "hits",
            vec![
                Field::new("region", LogicalType::Integer),
                Field::new("user", LogicalType::BigInt),
            ],
        )
        .expect("new file");
        for part in held.chunks(4096) {
            let regions = part
                .iter()
                .map(|(region, _)| region.map_or(Value::Null, Value::Integer))
                .collect::<Vec<_>>();
            let users = part
                .iter()
                .map(|(_, user)| user.map_or(Value::Null, Value::BigInt))
                .collect::<Vec<_>>();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::Integer, &regions).expect("regions"),
                Vector::from_values(LogicalType::BigInt, &users).expect("users"),
            ])
            .expect("two columns");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");
        (path, held)
    }

    #[test]
    fn a_code_stands_for_the_value_of_its_row() {
        let (path, held) = table_of("codes", 300_000);
        // Two columns are too few for the codes of one to fit a quarter of them.
        let built = build_value_codes_within(&path, "hits", 1000).expect("build");
        assert_eq!(built.len(), 1, "only the user is wide");
        assert!(built[0].built);
        let reader = Catalog::open(&path).expect("reopen").table("hits").expect("the table");
        assert!(current(&reader));
        let codes = value_codes(&reader, 1).expect("the section is in the file");
        let wanted = held.iter().filter_map(|(_, user)| *user).collect::<BTreeSet<_>>();
        assert_eq!(codes.values(), wanted.into_iter().collect::<Vec<_>>().as_slice());
        for (row, (_, user)) in held.iter().enumerate() {
            let code = codes.code_at(row);
            match user {
                Some(user) => assert_eq!(codes.values()[code as usize], *user, "row {row}"),
                None => assert_eq!(code, codes.null(), "row {row}"),
            }
        }
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn the_distinct_values_of_each_group_are_counted_across_every_share() {
        let (path, held) = table_of("groups", 300_000);
        build_value_codes_within(&path, "hits", 1000).expect("build");
        let reader = Catalog::open(&path).expect("reopen").table("hits").expect("the table");
        let codes = value_codes(&reader, 1).expect("the section is in the file");
        let mut wanted = BTreeMap::<Option<i64>, BTreeSet<i64>>::new();
        for (region, user) in &held {
            let users = wanted.entry(region.map(i64::from)).or_default();
            if let Some(user) = user {
                users.insert(*user);
            }
        }
        let wanted = wanted
            .into_iter()
            .map(|(region, users)| (region, users.len() as u64))
            .collect::<Vec<_>>();
        for workers in [1, 3, 8] {
            let mut found = distinct_per_group(&reader, 0, &codes, workers)
                .expect("counted")
                .expect("the region is narrow");
            found.sort_unstable();
            assert_eq!(found, wanted, "{workers} workers");
        }
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_small_table_is_given_no_codes() {
        let (path, _) = table_of("small", 5000);
        let built = build_value_codes(&path, "hits").expect("build");
        assert!(built.is_empty());
        let reader = Catalog::open(&path).expect("reopen").table("hits").expect("the table");
        assert!(current(&reader));
        assert!(value_codes(&reader, 1).is_none());
        fs::remove_file(&path).expect("clean up");
    }
}

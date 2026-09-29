//! The distinct values of a wide integer column in order, each with the rows that hold it.
//!
//! A text column held in a table wide dictionary already has a small number per value, so anything
//! that has to tell its values apart can index an array with it instead of hashing the value. A
//! wide integer column has nothing of the kind. `UserID` in ClickBench has a value per ten rows and
//! every one of them is a scattered sixty four bit number, so `COUNT(DISTINCT UserID) GROUP BY
//! RegionID` has to hash about a million pairs of region and user to throw the repeats away, and on
//! six threads that was two thirds of a query DuckDB runs in thirty milliseconds.
//!
//! With the rows of each value at hand the pairs are found without a hash. A value's place in the
//! order is its code, and the threads split the codes between them, so no two ever look at the
//! same user. A user belongs to one region in nearly every row that holds it, so a user's rows
//! mostly share one region and count once with a compare apiece, and only the few seen in a second
//! region are sorted to be told apart.
//!
//! Built at checkpoint for every signed integer column of a table of at least [`FEWEST_ROWS`] rows
//! whose exact distinct count is at least [`FEWEST_VALUES`] and is below the table's rows, out of its
//! own share of the table's column bytes, cheapest first. A column with fewer values than that is
//! either narrow enough to index by its value or cheap enough to hash, and a column with a value per
//! row has nothing to tell apart. Deleting the section changes no answer, only how a query that
//! could have used it runs.

use std::ops::Range;
use std::path::Path;

use rudb_common::bounds::Bound;
use rudb_common::{LogicalType, Result};

use crate::graph::BUDGET_FLOOR;
use crate::section::{self, Attachment};
use crate::{Catalog, Reader, invalid};

/// The share of a table's stored column bytes its value codes may cost together.
pub const VALUE_CODES_SHARE: u64 = 50;

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
/// a `u32` and a zero `u32`, then the `d` values in ascending order as `i64`, then `d + 1` starts
/// as `u32`, then the rows that are not null as `u32`, those holding the value coded `c` from start
/// `c` up to start `c + 1` in ascending order.
fn encode(reader: &Reader, column: usize) -> Result<Option<(usize, Vec<u8>)>> {
    let rows = reader.table().rows();
    let rows_u32 = u32::try_from(rows).map_err(|_| invalid("a table too long for value codes"))?;
    // Every value beside its row, sorted by value and then row, which is the order the rows are
    // kept in. Twelve bytes a row, which a checkpoint can afford and which is paid once.
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
    let mut starts = Vec::new();
    for (at, &(value, _)) in held.iter().enumerate() {
        if values.last() != Some(&value) {
            values.push(value);
            starts.push(at as u32);
        }
    }
    if values.len() >= held.len() {
        return Ok(None);
    }
    starts.push(held.len() as u32);
    let count = u32::try_from(values.len()).map_err(|_| invalid("too many values to code"))?;
    let mut bytes = Vec::with_capacity(16 + values.len() * 12 + 4 + held.len() * 4);
    bytes.extend_from_slice(&(rows as u64).to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    for value in &values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    for start in &starts {
        bytes.extend_from_slice(&start.to_le_bytes());
    }
    for &(_, row) in &held {
        bytes.extend_from_slice(&row.to_le_bytes());
    }
    Ok(Some((values.len(), bytes)))
}

/// One column's value codes, as read out of the file.
#[derive(Debug)]
pub struct ValueCodes {
    rows: usize,
    values: Vec<i64>,
    starts: Vec<u32>,
    held: Vec<u32>,
}

impl ValueCodes {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let long = |at: usize| Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?));
        let rows = usize::try_from(long(0)?).ok()?;
        let count = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?) as usize;
        let starts_at = 16_usize.checked_add(count.checked_mul(8)?)?;
        let held_at = starts_at.checked_add(count.checked_add(1)?.checked_mul(4)?)?;
        let values = bytes
            .get(16..starts_at)?
            .chunks_exact(8)
            .map(|one| i64::from_le_bytes(one.try_into().expect("eight bytes")))
            .collect::<Vec<_>>();
        let words = |from: &[u8]| {
            from.chunks_exact(4)
                .map(|one| u32::from_le_bytes(one.try_into().expect("four bytes")))
                .collect::<Vec<_>>()
        };
        let starts = words(bytes.get(starts_at..held_at)?);
        let held = words(bytes.get(held_at..)?);
        let in_order = |run: &[u32]| run.windows(2).all(|pair| pair[0] < pair[1]);
        if bytes.len() - held_at != held.len() * 4
            || starts.first() != Some(&0)
            || starts.last().map(|&last| last as usize) != Some(held.len())
            || held.len() > rows
            || !in_order(&starts)
            || values.windows(2).any(|pair| pair[0] >= pair[1])
            || held.iter().any(|&row| row as usize >= rows)
        {
            return None;
        }
        Some(Self { rows, values, starts, held })
    }

    /// The table's rows.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Every distinct value in ascending order, so that a code is the value's place in it.
    #[must_use]
    pub fn values(&self) -> &[i64] {
        &self.values
    }

    /// The rows holding the value coded `code`, in ascending order, for a code below the count of
    /// [`Self::values`].
    #[inline]
    #[must_use]
    pub fn rows_of(&self, code: usize) -> &[u32] {
        &self.held[self.starts[code] as usize..self.starts[code + 1] as usize]
    }

    /// Where the rows of the value coded `code` start among the rows of every value, for a code up
    /// to the count of [`Self::values`].
    #[inline]
    #[must_use]
    pub fn start(&self, code: usize) -> usize {
        self.starts[code] as usize
    }

    /// The rows of each of the codes in `codes` one after the other, which is from
    /// [`Self::start`] of the first up to that of the one past the last.
    #[must_use]
    pub fn rows_of_codes(&self, codes: Range<usize>) -> &[u32] {
        &self.held[self.start(codes.start)..self.start(codes.end)]
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
/// Two passes, each split among `workers` threads. The first reads the group column into a slot
/// per row, the slot being the value's distance from the lowest the stripes record plus one, and
/// zero for a null. The second goes over the codes, a range of them to each thread, and counts the
/// slots of each code's rows once each. A user belongs to one region in nearly every row that
/// holds it, so a code's rows mostly share one slot and are counted with a compare apiece.
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
    let workers = workers.max(1);

    // Every row's slot, each thread writing the rows of its own run of parts.
    let each = parts.div_ceil(workers).max(1);
    let mut of_row = vec![0_u16; rows];
    let read = std::thread::scope(|scope| {
        let mut rest = of_row.as_mut_slice();
        let mut handles = Vec::with_capacity(workers);
        for from in (0..parts).step_by(each) {
            let span = from..(from + each).min(parts);
            let (mine, after) = rest.split_at_mut(starts[span.end] - starts[span.start]);
            rest = after;
            let starts = &starts;
            handles.push(
                scope.spawn(move || read_slots(reader, group, span, starts, low, slots, mine)),
            );
        }
        handles
            .into_iter()
            .map(|handle| handle.join().map_err(|_| invalid("a group read worker panicked"))?)
            .collect::<Result<Vec<_>>>()
    })?;
    let mut present = vec![false; slots];
    for one in read {
        let Some(one) = one else { return Ok(None) };
        for (slot, here) in present.iter_mut().zip(one) {
            *slot |= here;
        }
    }

    // Each code's distinct slots, the codes split among the threads.
    let of_row = &of_row;
    let codes = counted.values().len();
    let each = codes.div_ceil(workers).max(1);
    let counted_shares = std::thread::scope(|scope| {
        let handles = (0..codes)
            .step_by(each)
            .map(|from| {
                let span = from..(from + each).min(codes);
                scope.spawn(move || {
                    // The slots of every row of these codes first, a load apiece that nothing waits
                    // on, so that the misses overlap instead of each one holding up the compare
                    // that follows it.
                    let base = counted.start(span.start);
                    let gathered = counted
                        .rows_of_codes(span.clone())
                        .iter()
                        .map(|&row| of_row[row as usize])
                        .collect::<Vec<_>>();
                    let mut counts = vec![0_u64; slots];
                    let mut others = Vec::new();
                    for code in span {
                        let held =
                            &gathered[counted.start(code) - base..counted.start(code + 1) - base];
                        let Some((&slot, tail)) = held.split_first() else { continue };
                        counts[slot as usize] += 1;
                        for &other in tail {
                            if other != slot {
                                others.push(other);
                            }
                        }
                        if !others.is_empty() {
                            others.sort_unstable();
                            others.dedup();
                            for &other in &others {
                                counts[other as usize] += 1;
                            }
                            others.clear();
                        }
                    }
                    counts
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().map_err(|_| invalid("a code count worker panicked")))
            .collect::<Result<Vec<_>>>()
    })?;
    let mut counts = vec![0_u64; slots];
    for share in counted_shares {
        for (total, count) in counts.iter_mut().zip(share) {
            *total += count;
        }
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

/// The widest range of group values [`distinct_per_group`] indexes, so that a row's slot is two
/// bytes and the slots of a million rows fit in two megabytes of cache.
const MOST_GROUP_SLOTS: i128 = 1 << 16;

/// The slot of every row of `parts` of the group column into `out`, and which slots turned up, or
/// `None` when the column is not read as signed integers or holds a value outside the range the
/// stripes record.
fn read_slots(
    reader: &Reader,
    group: usize,
    parts: Range<usize>,
    starts: &[usize],
    low: i64,
    slots: usize,
    out: &mut [u16],
) -> Result<Option<Vec<bool>>> {
    let mut present = vec![false; slots];
    let mut block = Vec::new();
    let base = starts[parts.start];
    for part in parts {
        let len = starts[part + 1] - starts[part];
        let read = reader.read(part, &[group])?;
        let vector = read.column(0)?;
        if vector.len() != len || !vector.signed_block(&mut block) || block.len() < len {
            return Ok(None);
        }
        let has_nulls = vector.validity().has_nulls(len);
        let out = &mut out[starts[part] - base..starts[part + 1] - base];
        for (at, (&value, slot)) in block[..len].iter().zip(out).enumerate() {
            *slot = if has_nulls && vector.is_null_at(at) {
                0
            } else {
                match value.checked_sub(low).and_then(|gap| usize::try_from(gap).ok()) {
                    Some(gap) if gap + 1 < slots => gap as u16 + 1,
                    _ => return Ok(None),
                }
            };
            present[*slot as usize] = true;
        }
    }
    Ok(Some(present))
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
    fn the_rows_of_a_code_are_the_rows_holding_its_value() {
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
        let mut rows = 0;
        for (code, value) in codes.values().iter().enumerate() {
            for &row in codes.rows_of(code) {
                assert_eq!(held[row as usize].1, Some(*value), "row {row}");
                rows += 1;
            }
        }
        assert_eq!(rows, held.iter().filter(|(_, user)| user.is_some()).count());
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

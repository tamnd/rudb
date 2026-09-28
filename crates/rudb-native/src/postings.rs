//! The rows of each value of a column held in a table wide dictionary.
//!
//! A filter on a coded text column that keeps a few of its values has to decode every code of the
//! column to find the rows that hold them. On JOB `cast_info.note` is 36 million rows, and a filter
//! naming the producer credits keeps 2.4 million of them, so 93 percent of the codes are decoded to
//! be thrown away, in more than a dozen queries. A filter over one column can be asked of the
//! dictionary once, which says which values pass, and this section says where those values are, so
//! the scan reads the rows that can pass and no others.
//!
//! Built at checkpoint for every `VARCHAR` column with a table wide dictionary on a table of at
//! least [`FEWEST_ROWS`] rows, out of its own share of the table's column bytes, cheapest first.
//! Which columns a query filters is not something the file knows, so the rule is the same for every
//! column. A part the dictionary does not code, which is every part after a column is demoted, is
//! recorded as a range of rows that holds any value, so the answer stays a superset of the rows
//! that hold the values asked for. Nulls are not recorded, and a caller whose filter passes a null
//! does not use the section. Deleting the section changes no answer, only how many rows a scan reads.

use std::path::Path;

use rudb_common::{LogicalType, Result};
use rudb_graph::Rids;

use crate::graph::BUDGET_FLOOR;
use crate::section::{self, Attachment};
use crate::{Catalog, Reader, invalid};

/// The share of a table's stored column bytes its value rows may cost together.
///
/// A row costs a byte or two once its row number is written as the gap from the one before, and a
/// coded column costs about the same stored, so a table whose text is all coded spends about what
/// its coded columns cost. A quarter holds `cast_info.note` beside the rest of `cast_info`.
pub const VALUE_ROWS_SHARE: u64 = 25;

/// Tables with fewer rows than this are not given value rows, since reading the whole column costs
/// less than a millisecond.
pub const FEWEST_ROWS: usize = 1 << 16;

/// What recording one column cost and whether it was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// The column, by position in the table.
    pub column: usize,
    /// The rows that hold a coded value.
    pub coded: usize,
    /// What the section costs, kept or not.
    pub bytes: usize,
    /// Whether it went into the file.
    pub built: bool,
}

/// Whether the table has a current entry for every coded text column, kept or recorded as not kept.
#[must_use]
pub fn current(reader: &Reader) -> bool {
    let table = reader.table();
    coded_columns(reader).all(|column| {
        table.sections().iter().any(|held| {
            held.kind == *section::VALUE_ROWS
                && u64::try_from(column) == Ok(held.id)
                && held.current(table.generation())
        })
    })
}

/// Records every coded text column of a table and attaches them in one commit.
///
/// # Errors
///
/// If the file cannot be opened, a column cannot be read, or the attach fails.
pub fn build_value_rows(path: &Path, table: &str) -> Result<Vec<Built>> {
    build_value_rows_within(path, table, VALUE_ROWS_SHARE)
}

/// The same, against a budget of `share` percent of the table's stored column bytes.
///
/// Every coded column gets an entry, and one that does not fit gets one with no bytes, so that
/// [`current`] can tell a column that was decided against from one nobody looked at. When the
/// budget binds the cheapest columns go in first, since the columns that cost least are the ones
/// with the fewest rows per value and so the ones a filter is most likely to keep little of.
///
/// # Errors
///
/// If the file cannot be opened, a column cannot be read, or the attach fails.
pub fn build_value_rows_within(path: &Path, table: &str, share: u64) -> Result<Vec<Built>> {
    let reader = Catalog::open(path)?.table(table)?;
    let columns = coded_columns(&reader).collect::<Vec<_>>();
    if columns.is_empty() {
        return Ok(Vec::new());
    }
    let rows = reader.table().rows();
    let allowance = (reader.layout().columns_total().saturating_mul(share) / 100).max(BUDGET_FLOOR);
    let mut report = Vec::with_capacity(columns.len());
    let mut payloads = Vec::with_capacity(columns.len());
    for &column in &columns {
        let payload = if rows >= FEWEST_ROWS { encode(&reader, column)? } else { None };
        let (coded, bytes) = payload.as_ref().map_or((0, 0), |(coded, bytes)| (*coded, bytes.len()));
        report.push(Built { column, coded, bytes, built: false });
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
                kind: *section::VALUE_ROWS,
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

/// The payload of one column and how many rows hold a coded value, or `None` for a column with no
/// dictionary after all.
///
/// The layout, all little endian: the table's rows as a `u64`, the dictionary's size `d` as a
/// `u32`, the count `w` of uncoded ranges as a `u32`, then `w` pairs of `u64` first row and row
/// count, then `d` counts as `u32`, then `d` end offsets into the row stream as `u64`, then the
/// row stream, which for each value in code order is its rows as the gap from the one before,
/// written seven bits to a byte.
fn encode(reader: &Reader, column: usize) -> Result<Option<(usize, Vec<u8>)>> {
    let Some(dictionary) = reader.global_dictionary(column)? else { return Ok(None) };
    let values = dictionary.len();
    let rows = reader.table().rows();
    let rows_u32 = u32::try_from(rows).map_err(|_| invalid("a table too long for value rows"))?;
    // One code per row, with `u32::MAX` for a null or an uncoded row, then a counting sort into
    // code order. Twice the column's rows in memory, which a checkpoint can afford and a list per
    // value would not, at 700 thousand values.
    let mut codes = vec![u32::MAX; rows];
    let mut whole: Vec<(u64, u64)> = Vec::new();
    let mut first = 0_usize;
    for part in 0..reader.parts() {
        let len = reader.part_rows(part);
        match reader.stable_codes(part, column)? {
            Some(read) if read.len() == len => {
                for (at, code) in read.into_iter().enumerate() {
                    if let Some(code) = code {
                        if code as usize >= values {
                            return Err(invalid("a code past the end of its dictionary"));
                        }
                        codes[first + at] = code;
                    }
                }
            }
            _ => match whole.last_mut() {
                Some((start, count)) if *start + *count == first as u64 => *count += len as u64,
                _ => whole.push((first as u64, len as u64)),
            },
        }
        first += len;
    }
    if first != rows {
        return Err(invalid("value rows found a row count that differs from the table"));
    }
    let mut counts = vec![0_u32; values];
    for &code in &codes {
        if code != u32::MAX {
            counts[code as usize] += 1;
        }
    }
    let mut starts = Vec::with_capacity(values);
    let mut next = 0_usize;
    for &count in &counts {
        starts.push(next);
        next += count as usize;
    }
    let coded = next;
    let mut sorted = vec![0_u32; coded];
    for (row, &code) in (0..rows_u32).zip(&codes) {
        if code != u32::MAX {
            let at = &mut starts[code as usize];
            sorted[*at] = row;
            *at += 1;
        }
    }
    drop(codes);
    let mut stream = Vec::with_capacity(coded * 2);
    let mut ends = Vec::with_capacity(values);
    let mut at = 0;
    for &count in &counts {
        let mut previous = 0_u32;
        for &row in &sorted[at..at + count as usize] {
            put_gap(&mut stream, row - previous);
            previous = row;
        }
        at += count as usize;
        ends.push(stream.len() as u64);
    }
    let head = 16 + whole.len() * 16 + values * 12;
    let mut bytes = Vec::with_capacity(head + stream.len());
    bytes.extend_from_slice(&(rows as u64).to_le_bytes());
    bytes.extend_from_slice(
        &u32::try_from(values).map_err(|_| invalid("dictionary too large"))?.to_le_bytes(),
    );
    bytes.extend_from_slice(
        &u32::try_from(whole.len()).map_err(|_| invalid("too many uncoded ranges"))?.to_le_bytes(),
    );
    for (start, count) in &whole {
        bytes.extend_from_slice(&start.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
    }
    for count in &counts {
        bytes.extend_from_slice(&count.to_le_bytes());
    }
    for end in &ends {
        bytes.extend_from_slice(&end.to_le_bytes());
    }
    bytes.extend_from_slice(&stream);
    Ok(Some((coded, bytes)))
}

fn put_gap(out: &mut Vec<u8>, mut gap: u32) {
    while gap >= 0x80 {
        out.push((gap as u8) | 0x80);
        gap >>= 7;
    }
    out.push(gap as u8);
}

/// One column's value rows, as read out of the file.
#[derive(Debug)]
pub struct ValueRows {
    rows: u64,
    values: usize,
    whole: Vec<(u64, u64)>,
    counts_at: usize,
    ends_at: usize,
    stream_at: usize,
    bytes: Vec<u8>,
}

impl ValueRows {
    fn parse(bytes: Vec<u8>) -> Option<Self> {
        let word = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
        let long = |at: usize| Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?));
        let rows = long(0)?;
        let values = word(8)? as usize;
        let ranges = word(12)? as usize;
        let mut whole = Vec::with_capacity(ranges);
        for at in 0..ranges {
            let start = long(16 + at * 16)?;
            let count = long(24 + at * 16)?;
            if start.checked_add(count)? > rows {
                return None;
            }
            whole.push((start, count));
        }
        let counts_at = 16 + ranges * 16;
        let ends_at = counts_at.checked_add(values.checked_mul(4)?)?;
        let stream_at = ends_at.checked_add(values.checked_mul(8)?)?;
        let stream = bytes.len().checked_sub(stream_at)? as u64;
        if values > 0 && long(ends_at + (values - 1) * 8)? != stream {
            return None;
        }
        Some(Self { rows, values, whole, counts_at, ends_at, stream_at, bytes })
    }

    /// The table's rows, which is what a set of them is over.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// How many values the dictionary holds.
    #[must_use]
    pub fn values(&self) -> usize {
        self.values
    }

    fn count(&self, code: usize) -> u64 {
        let at = self.counts_at + code * 4;
        u64::from(u32::from_le_bytes(self.bytes[at..at + 4].try_into().unwrap_or_default()))
    }

    fn end(&self, code: usize) -> usize {
        let at = self.ends_at + code * 8;
        u64::from_le_bytes(self.bytes[at..at + 8].try_into().unwrap_or_default()) as usize
    }

    /// How many rows [`Self::rows_of`] would hold for these codes, without building the set, or
    /// `None` when a code is past the end of the dictionary.
    #[must_use]
    pub fn held(&self, codes: &[u32]) -> Option<u64> {
        let mut total = self.whole.iter().map(|(_, count)| count).sum::<u64>();
        for &code in codes {
            if code as usize >= self.values {
                return None;
            }
            total += self.count(code as usize);
        }
        Some(total)
    }

    /// Every row that holds one of `codes`, which are distinct, and every row of a part the
    /// dictionary does not code.
    ///
    /// # Errors
    ///
    /// If a code is past the end of the dictionary or the stream does not decode to the counts it
    /// says it holds.
    pub fn rows_of(&self, codes: &[u32]) -> Result<Rids> {
        let total =
            self.held(codes).ok_or_else(|| invalid("a code past the end of its dictionary"))?;
        let dense = total.saturating_mul(64) >= self.rows;
        let mut words = if dense { vec![0_u64; self.rows.div_ceil(64) as usize] } else { Vec::new() };
        let mut members = if dense { Vec::new() } else { Vec::with_capacity(total as usize) };
        let mut put = |row: u64| {
            if dense {
                words[(row / 64) as usize] |= 1 << (row % 64);
            } else {
                members.push(row);
            }
        };
        for &(start, count) in &self.whole {
            for row in start..start + count {
                put(row);
            }
        }
        for &code in codes {
            let code = code as usize;
            let from = self.stream_at + if code == 0 { 0 } else { self.end(code - 1) };
            let to = self.stream_at + self.end(code);
            let stream = self.bytes.get(from..to).ok_or_else(|| invalid("value rows truncated"))?;
            let mut row = 0_u64;
            let mut gap = 0_u64;
            let mut shift = 0;
            let mut seen = 0_u64;
            for &byte in stream {
                gap |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    row += gap;
                    if row >= self.rows {
                        return Err(invalid("a value row past the end of its table"));
                    }
                    put(row);
                    seen += 1;
                    gap = 0;
                    shift = 0;
                } else {
                    shift += 7;
                    if shift > 35 {
                        return Err(invalid("a value row gap is too long"));
                    }
                }
            }
            if seen != self.count(code) || shift != 0 {
                return Err(invalid("value rows do not hold the count they say"));
            }
        }
        if dense {
            Rids::from_words(self.rows, words)
        } else {
            members.sort_unstable();
            members.dedup();
            Rids::from_sorted(self.rows, members)
        }
    }
}

/// The value rows of a column, when the table carries a current section for it.
#[must_use]
pub fn value_rows(reader: &Reader, column: usize) -> Option<ValueRows> {
    let table = reader.table();
    let id = u64::try_from(column).ok()?;
    let held =
        table.sections().iter().find(|held| held.kind == *section::VALUE_ROWS && held.id == id)?;
    if !held.usable(table.generation()) {
        return None;
    }
    let parsed = ValueRows::parse(reader.payload(held).ok()?)?;
    (parsed.rows == table.rows() as u64).then_some(parsed)
}

fn coded_columns(reader: &Reader) -> impl Iterator<Item = usize> + '_ {
    let table = reader.table();
    table.fields().iter().enumerate().filter_map(move |(column, field)| {
        (field.ty == LogicalType::Varchar
            && table.dictionaries.get(column).is_some_and(Option::is_some))
        .then_some(column)
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rudb_common::{Field, Value};
    use rudb_vector::{Chunk, Vector};

    use super::*;
    use crate::Writer;

    fn path(label: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("time advances").as_nanos();
        std::env::temp_dir().join(format!("rudb-postings-{label}-{}-{stamp}.rdb", std::process::id()))
    }

    /// A table of a note drawn from a few values with nulls among them, and a number.
    fn table_of(label: &str, rows: usize) -> (PathBuf, Vec<Option<String>>) {
        let path = path(label);
        let notes = ["(producer)", "(writer)", "(voice)", "(uncredited)", "(archive footage)"];
        let mut seed = 7_u64;
        let values = (0..rows)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let pick = (seed >> 33) as usize % (notes.len() * 4);
                (pick < notes.len() * 2).then(|| notes[pick % notes.len()].to_string())
            })
            .collect::<Vec<_>>();
        let mut writer = Writer::create(
            &path,
            "cast_info",
            vec![Field::new("note", LogicalType::Varchar), Field::new("id", LogicalType::BigInt)],
        )
        .expect("new file");
        for (at, part) in values.chunks(4096).enumerate() {
            let text = part
                .iter()
                .map(|one| one.clone().map_or(Value::Null, Value::Varchar))
                .collect::<Vec<_>>();
            let ids = (0..part.len())
                .map(|row| Value::BigInt((at * 4096 + row) as i64))
                .collect::<Vec<_>>();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::Varchar, &text).expect("notes"),
                Vector::from_values(LogicalType::BigInt, &ids).expect("ids"),
            ])
            .expect("two columns");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");
        (path, values)
    }

    #[test]
    fn the_rows_of_a_value_are_the_rows_that_hold_it() {
        let (path, values) = table_of("rows", 100_000);
        let reader = Catalog::open(&path).expect("open").table("cast_info").expect("the table");
        let dictionary = reader
            .global_dictionary(0)
            .expect("a dictionary read")
            .expect("a few repeated notes are coded table wide");
        drop(reader);
        let built = build_value_rows(&path, "cast_info").expect("build");
        assert_eq!(built.len(), 1, "one coded text column");
        assert!(built[0].built);
        let reader = Catalog::open(&path).expect("reopen").table("cast_info").expect("the table");
        assert!(current(&reader));
        let held = value_rows(&reader, 0).expect("the section is in the file");
        assert_eq!(held.values(), dictionary.len());
        for code in 0..dictionary.len() {
            let Value::Varchar(text) = dictionary.value_at(code) else { continue };
            let rows = held.rows_of(&[code as u32]).expect("rows");
            let wanted = values
                .iter()
                .enumerate()
                .filter(|(_, one)| one.as_deref() == Some(text.as_str()))
                .map(|(row, _)| row as u64)
                .collect::<Vec<_>>();
            assert_eq!(rows.iter().collect::<Vec<_>>(), wanted, "value {text}");
            assert_eq!(held.held(&[code as u32]), Some(wanted.len() as u64));
        }
        let two = held.rows_of(&[0, 1]).expect("two values");
        let one = held.held(&[0]).unwrap_or(0) + held.held(&[1]).unwrap_or(0);
        assert_eq!(two.len(), one);
        assert!(held.rows_of(&[dictionary.len() as u32]).is_err());
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_small_table_is_recorded_without_bytes() {
        let (path, _) = table_of("small", 5000);
        let built = build_value_rows(&path, "cast_info").expect("build");
        assert!(built.iter().all(|one| !one.built));
        let reader = Catalog::open(&path).expect("reopen").table("cast_info").expect("the table");
        assert!(current(&reader));
        assert!(value_rows(&reader, 0).is_none());
        fs::remove_file(&path).expect("clean up");
    }
}

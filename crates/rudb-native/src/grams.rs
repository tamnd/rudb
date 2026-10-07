//! A word per row of a long text column, with a bit for each run of three bytes the value holds.
//!
//! A `LIKE '%special%requests%'` over `o_comment` walks every string's codes to the end, and on
//! TPC-H q13 that walk was half of the query. A string that holds a piece holds every run of three
//! bytes in it, so a row whose word lacks one of the piece's bits is answered without its string
//! being looked at, and the walk is paid only by the rows that might match. The sketch is
//! [`rudb_encoding::sequence::grams`], and the reader side is [`crate::Reader::rows_holding`].
//!
//! Built at checkpoint for every `VARCHAR` column whose values average [`SHORTEST`] bytes or more,
//! out of its own share of the table's column bytes, and the same for every table: which queries
//! ask for a `LIKE` is not something the file knows, and a long text column is where one pays.
//! Deleting the section changes no answer, only how many strings a `LIKE` walks.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rudb_common::{LogicalType, Result};
use rudb_encoding::sequence::grams;
use rudb_vector::{Validity, Vector};

use crate::graph::{BUDGET_FLOOR, by_part, each_part};
use crate::section::{self, Attachment};
use crate::{Catalog, Reader, invalid};

/// The share of a table's stored column bytes its text sketches may cost together.
///
/// A sketch is eight bytes a row whatever the text, and the text it answers for is sixteen or more
/// bytes a row before it compresses, so it is measured against a table that compresses well. On
/// TPC-H SF1 `orders` is 41 MB stored and the sketch of `o_comment` is 12 MB, which a quarter of the
/// table did not hold. Half does, and it holds `l_comment` on `lineitem` beside it.
pub const TEXT_GRAMS_SHARE: u64 = 50;

/// The average length in bytes below which a column is not sketched.
///
/// A short value has few runs of three and a walk over it is a few codes, so its sketch saves
/// little and costs as much as a long one's. Flags, modes and names come in under this and comments
/// come in over it.
pub const SHORTEST: u64 = 16;

/// What sketching one column cost and whether it was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// The column, by position in the table.
    pub column: usize,
    /// The rows sketched, which is the table's rows.
    pub rows: usize,
    /// The bytes the column's values came to, before any compression.
    pub text_bytes: u64,
    /// What the section costs, kept or not.
    pub bytes: usize,
    /// Whether it went into the file.
    pub built: bool,
}

/// Whether the table has a current entry for every text column, kept or recorded as not kept.
///
/// A checkpoint that finds this true has nothing to do, which is what keeps a checkpoint over an
/// unchanged table from reading every text column again.
#[must_use]
pub fn current(reader: &Reader) -> bool {
    let table = reader.table();
    text_columns(reader).all(|column| {
        table.sections().iter().any(|held| {
            held.kind == *section::TEXT_GRAMS
                && u64::try_from(column) == Ok(held.id)
                && held.current(table.generation())
                && (held.extents == 0
                    || reader.extents(held).is_ok_and(|extents| {
                        // A column decided against holds no bytes and states in its header what
                        // the sketch would have cost, which is as many rows as it was decided over.
                        let bytes = match extents.iter().map(|one| u64::from(one.length)).sum() {
                            0 if held.header_bytes == u32::MAX => return true,
                            0 => u64::from(held.header_bytes),
                            bytes => bytes,
                        };
                        crate::postings::enough(bytes / 8, table.rows())
                    }))
        })
    })
}

/// Sketches every text column of a table and attaches them in one commit.
///
/// # Errors
///
/// If the file cannot be opened, a column cannot be read, or the attach fails.
pub fn build_text_grams(path: &Path, table: &str) -> Result<Vec<Built>> {
    build_text_grams_within(path, table, TEXT_GRAMS_SHARE)
}

/// The same, against a budget of `share` percent of the table's stored column bytes.
///
/// Every text column gets an entry, and one that is too short or does not fit gets one with no
/// bytes, for the reason key maps do: the file says what it decided and what that would have cost,
/// and [`current`] can tell a column that was decided against from one nobody looked at.
///
/// When the budget binds, the longest columns are admitted first. Every sketch costs the same eight
/// bytes a row, so what differs is what it saves, and a longer string is a longer walk.
///
/// # Errors
///
/// If the file cannot be opened, a column cannot be read, or the attach fails.
pub fn build_text_grams_within(path: &Path, table: &str, share: u64) -> Result<Vec<Built>> {
    let reader = Catalog::open(path)?.table(table)?;
    within(path, &reader, share)
}

/// The same for a table the caller already has open, which a checkpoint has from deciding which
/// tables need the work. The file at `path` has to hold the rows `reader` does, which it does when
/// all that was written since the reader was opened is sections.
///
/// # Errors
///
/// If a column cannot be read or the attach fails.
pub fn build_text_grams_of(path: &Path, reader: &Reader) -> Result<Vec<Built>> {
    within(path, reader, TEXT_GRAMS_SHARE)
}

fn within(path: &Path, reader: &Reader, share: u64) -> Result<Vec<Built>> {
    let columns = text_columns(reader).collect::<Vec<_>>();
    if columns.is_empty() {
        return Ok(Vec::new());
    }
    let rows = reader.table().rows();
    let allowance = (reader.layout().columns_total().saturating_mul(share) / 100).max(BUDGET_FLOOR);
    let mut report = Vec::with_capacity(columns.len());
    let mut payloads = Vec::with_capacity(columns.len());
    // A column coded against a table wide dictionary is measured first and sketched only once it is
    // kept. The length of a row is the length of the value its code points at, which costs the ends
    // of the dictionary and not its text, and most text columns are short: on JOB `cast_info.note`
    // is 36 million rows at seven bytes a row, and sketching it to throw the sketch away was the
    // largest piece of the checkpoint after the links.
    let mut coded = Vec::with_capacity(columns.len());
    for &column in &columns {
        let dictionary = reader
            .global_dictionary(column)?
            .filter(|values| matches!(values.validity(), Validity::AllValid));
        let (text_bytes, words) = match &dictionary {
            Some(values) => (coded_bytes(reader, column, values)?, Vec::new()),
            None => sketch_rows(reader, column, rows)?,
        };
        report.push(Built { column, rows, text_bytes, bytes: rows * 8, built: false });
        payloads.push(words);
        coded.push(dictionary);
    }
    let mut order = (0..report.len()).collect::<Vec<_>>();
    order.sort_by_key(|&at| std::cmp::Reverse(report[at].text_bytes));
    let mut spent = 0_u64;
    for at in order {
        let long = report[at].text_bytes >= SHORTEST.saturating_mul(rows as u64);
        let cost = report[at].bytes as u64;
        if long && rows > 0 && spent.saturating_add(cost) <= allowance {
            spent += cost;
            report[at].built = true;
        }
    }
    for (at, dictionary) in coded.iter().enumerate() {
        if let (true, Some(values)) = (report[at].built, dictionary) {
            payloads[at] = sketch_codes(reader, report[at].column, values, rows)?;
        }
    }
    let attachments = report
        .iter()
        .zip(&payloads)
        .map(|(one, words)| {
            Ok(Attachment {
                kind: *section::TEXT_GRAMS,
                id: u64::try_from(one.column).map_err(|_| invalid("column index overflow"))?,
                flags: 0,
                header_bytes: if one.built {
                    0
                } else {
                    u32::try_from(one.bytes).unwrap_or(u32::MAX)
                },
                bytes: if one.built { words } else { &[] },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    crate::attach(path, reader.table().name(), &attachments)?;
    Ok(report)
}

/// The sketch of a column in row id order, when the table carries a current one.
///
/// `None` covers every reason there is not one, and a caller that gets it walks every string, which
/// is what it did before the sketch existed.
#[must_use]
pub fn text_grams(reader: &Reader, column: usize) -> Option<Vec<u64>> {
    let table = reader.table();
    let held = current_grams(reader, column)?;
    let bytes = reader.payload(held).ok()?;
    if bytes.is_empty() || bytes.len() % 8 != 0 || bytes.len() > table.rows().checked_mul(8)? {
        return None;
    }
    let mut words = bytes
        .chunks_exact(8)
        .map(|word| u64::from_le_bytes(word.try_into().unwrap_or_default()))
        .collect::<Vec<_>>();
    // Rows after the ones the sketch was built over, which a table extended since has, get a word
    // with every bit set, which rules nothing out and sends them to be walked.
    words.resize(table.rows(), u64::MAX);
    Some(words)
}

/// Whether the table carries a current sketch of the column, found without reading it.
///
/// The sketch of a column is eight bytes a row once read, so a caller that may not need it asks
/// this first. A `LIKE` over a column whose parts are coded with a dictionary never walks a
/// compressed page, and reading the sketch of `URL` anyway held 130 MB on ClickBench q21.
#[must_use]
pub fn has_text_grams(reader: &Reader, column: usize) -> bool {
    current_grams(reader, column).is_some()
}

/// The section holding the current sketch of a column, if the table has one.
fn current_grams(reader: &Reader, column: usize) -> Option<&section::Section> {
    let table = reader.table();
    let id = u64::try_from(column).ok()?;
    table
        .sections()
        .iter()
        .find(|held| held.kind == *section::TEXT_GRAMS && held.id == id)
        .filter(|held| held.usable(table.generation()))
}

/// Sketches a column a part at a time, and answers the bytes its values came to with the words.
fn sketch_rows(reader: &Reader, column: usize, rows: usize) -> Result<(u64, Vec<u8>)> {
    let mut words = vec![0_u8; rows * 8];
    let text_bytes = AtomicU64::new(0);
    by_part(reader, &mut words, 8, &|part, run| {
        let chunk = reader.read(part, &[column])?;
        let values = chunk.column(0)?;
        if chunk.len() * 8 != run.len() {
            return Err(invalid("a text sketch's row count differs from its table"));
        }
        text_bytes.fetch_add(sketch_part(values, run), Ordering::Relaxed);
        Ok(())
    })?;
    Ok((text_bytes.into_inner(), words))
}

/// Sketches a part a row at a time into `run`, and answers the bytes its values came to.
fn sketch_part(values: &Vector, run: &mut [u8]) -> u64 {
    let mut bytes = 0_u64;
    for (row, word) in run.chunks_exact_mut(8).enumerate() {
        let text = values.bytes_at(row).unwrap_or_default();
        bytes += text.len() as u64;
        word.copy_from_slice(&grams(text).to_le_bytes());
    }
    bytes
}

/// The codes of `vector` when they point into `values`, cut to its rows.
///
/// `None` for a part that holds its text some other way, which is read a row at a time.
fn codes_into<'a>(vector: &'a Vector, values: &Arc<Vector>) -> Option<&'a [u32]> {
    match vector.shared_dictionary_parts() {
        Some((codes, held)) if Arc::ptr_eq(held, values) => codes.get(..vector.len()),
        _ => None,
    }
}

/// The bytes a dictionary coded column's values come to, nulls counting none, out of the lengths
/// of the dictionary values its codes point at.
fn coded_bytes(reader: &Reader, column: usize, values: &Arc<Vector>) -> Result<u64> {
    let mut lens = Vec::with_capacity(values.len());
    if !values.try_bytes_lens(&mut lens)? {
        lens.clear();
        for at in 0..values.len() {
            let len = values.try_bytes_len_at(at)?.unwrap_or(0);
            lens.push(i64::try_from(len).unwrap_or(i64::MAX));
        }
    }
    let total = AtomicU64::new(0);
    each_part(reader, &|part| {
        let chunk = reader.read(part, &[column])?;
        let vector = chunk.column(0)?;
        let every = matches!(vector.validity(), Validity::AllValid);
        let mut bytes = 0_u64;
        match codes_into(vector, values) {
            Some(codes) => {
                for (row, &code) in codes.iter().enumerate() {
                    if every || vector.validity().is_valid(row) {
                        let len = lens.get(code as usize).copied().unwrap_or(0);
                        bytes += u64::try_from(len).unwrap_or(0);
                    }
                }
            }
            None => {
                for row in 0..vector.len() {
                    bytes += vector.bytes_at(row).map_or(0, |text| text.len() as u64);
                }
            }
        }
        total.fetch_add(bytes, Ordering::Relaxed);
        Ok(())
    })?;
    Ok(total.into_inner())
}

/// Sketches a dictionary coded column by sketching each dictionary value once and handing every
/// row the sketch of the value its code points at.
fn sketch_codes(
    reader: &Reader,
    column: usize,
    values: &Arc<Vector>,
    rows: usize,
) -> Result<Vec<u8>> {
    let mut sketches = vec![0_u64; values.len()];
    let walked = values.try_visit_text(&mut |at, text| {
        if let Some(sketch) = sketches.get_mut(at) {
            *sketch = grams(text);
        }
        Ok(())
    })?;
    if !walked {
        for (at, sketch) in sketches.iter_mut().enumerate() {
            *sketch = grams(values.bytes_at(at).unwrap_or_default());
        }
    }
    let mut words = vec![0_u8; rows * 8];
    by_part(reader, &mut words, 8, &|part, run| {
        let chunk = reader.read(part, &[column])?;
        let vector = chunk.column(0)?;
        if chunk.len() * 8 != run.len() {
            return Err(invalid("a text sketch's row count differs from its table"));
        }
        let Some(codes) = codes_into(vector, values) else {
            sketch_part(vector, run);
            return Ok(());
        };
        let every = matches!(vector.validity(), Validity::AllValid);
        for (row, (word, &code)) in run.chunks_exact_mut(8).zip(codes).enumerate() {
            let sketch = if every || vector.validity().is_valid(row) {
                sketches.get(code as usize).copied().unwrap_or(u64::MAX)
            } else {
                0
            };
            word.copy_from_slice(&sketch.to_le_bytes());
        }
        Ok(())
    })?;
    Ok(words)
}

fn text_columns(reader: &Reader) -> impl Iterator<Item = usize> + '_ {
    reader
        .table()
        .fields()
        .iter()
        .enumerate()
        .filter(|(_, field)| field.ty == LogicalType::Varchar)
        .map(|(column, _)| column)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rudb_common::{Field, Value};
    use rudb_encoding::sequence::Sequence;
    use rudb_vector::{Chunk, Vector};

    use super::*;
    use crate::Writer;

    fn path(label: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("time advances").as_nanos();
        std::env::temp_dir().join(format!("rudb-grams-{label}-{}-{stamp}.rdb", std::process::id()))
    }

    /// A table of a long comment and a short flag, written a thousand rows to a part.
    fn table_of(label: &str, rows: usize) -> (PathBuf, Vec<String>) {
        let path = path(label);
        let words = [
            "special",
            "requests",
            "pending",
            "deposits",
            "carefully",
            "final",
            "ironic",
            "slyly",
            "quickly",
            "packages",
            "accounts",
            "furiously",
            "express",
            "regular",
            "bold",
            "even",
        ];
        let mut seed = 11_u64;
        let comments = (0..rows)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (0..8)
                    .map(|at| words[(seed >> (8 + at * 6)) as usize % words.len()])
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>();
        let mut writer = Writer::create(
            &path,
            "orders",
            vec![
                Field::new("comment", LogicalType::Varchar),
                Field::new("flag", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        for (at, part) in comments.chunks(1000).enumerate() {
            let text = part.iter().map(|one| Value::Varchar(one.clone())).collect::<Vec<_>>();
            let flags = part
                .iter()
                .map(|_| Value::Varchar(if at % 2 == 0 { "F" } else { "O" }.into()))
                .collect::<Vec<_>>();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::Varchar, &text).expect("comments"),
                Vector::from_values(LogicalType::Varchar, &flags).expect("flags"),
            ])
            .expect("two columns");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");
        (path, comments)
    }

    #[test]
    fn a_long_column_is_sketched_and_a_short_one_is_recorded_without_bytes() {
        let (path, comments) = table_of("kept", 3000);
        let built = build_text_grams(&path, "orders").expect("build");
        assert_eq!(built.len(), 2);
        assert!(built[0].built, "a comment column is long enough to sketch");
        assert!(!built[1].built, "a one byte flag is not");

        let reader = Catalog::open(&path).expect("reopen").table("orders").expect("the table");
        assert!(current(&reader), "both columns have an entry");
        assert!(text_grams(&reader, 1).is_none());
        let sketch = text_grams(&reader, 0).expect("the comment sketch is in the file");
        assert_eq!(sketch.len(), 3000);
        for (word, comment) in sketch.iter().zip(&comments) {
            assert_eq!(*word, grams(comment.as_bytes()));
        }
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_column_coded_against_its_dictionary_is_sketched_value_by_value_the_same() {
        let path = path("coded");
        let phrases = (0..40)
            .map(|at| format!("phrase number {at} of the forty that repeat"))
            .collect::<Vec<_>>();
        let mut writer = Writer::create(
            &path,
            "notes",
            vec![
                Field::new("note", LogicalType::Varchar),
                Field::new("short", LogicalType::Varchar),
            ],
        )
        .expect("new file");
        let mut seed = 7_u64;
        let mut expected = Vec::new();
        let mut total = 0_u64;
        for _ in 0..5 {
            let mut notes = Vec::new();
            let mut shorts = Vec::new();
            for _ in 0..1000 {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let pick = (seed >> 33) as usize % 50;
                if pick < 40 {
                    notes.push(Value::Varchar(phrases[pick].clone()));
                    expected.push(grams(phrases[pick].as_bytes()));
                    total += phrases[pick].len() as u64;
                } else {
                    notes.push(Value::Null);
                    expected.push(0);
                }
                shorts.push(Value::Varchar(format!("{}", pick % 3)));
            }
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::Varchar, &notes).expect("notes"),
                Vector::from_values(LogicalType::Varchar, &shorts).expect("shorts"),
            ])
            .expect("two columns");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");
        let reader = Catalog::open(&path).expect("open").table("notes").expect("the table");
        assert!(reader.global_dictionary(0).expect("read").is_some(), "the notes are coded");
        assert!(reader.global_dictionary(1).expect("read").is_some(), "the digits are coded");
        drop(reader);

        let built = build_text_grams(&path, "notes").expect("build");
        assert_eq!(built[0].text_bytes, total, "a null counts no bytes");
        assert!(built[0].built);
        assert!(!built[1].built, "one digit a row is too short to sketch");
        assert_eq!(built[1].text_bytes, 5000);
        let reader = Catalog::open(&path).expect("reopen").table("notes").expect("the table");
        assert!(current(&reader));
        assert_eq!(text_grams(&reader, 0).expect("the sketch is in the file"), expected);
        assert!(text_grams(&reader, 1).is_none());
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_like_answered_through_the_sketch_keeps_the_rows_it_kept_without_one() {
        let (path, comments) = table_of("like", 6000);
        let before = Catalog::open(&path).expect("open").table("orders").expect("the table");
        let sequence = Sequence::new(&[b"special", b"requests"]).expect("an automaton");
        let mut walked = Vec::new();
        for part in 0..before.parts() {
            walked.push(
                before
                    .rows_holding(part, 0, std::slice::from_ref(&sequence), true)
                    .expect("a part"),
            );
        }
        drop(before);
        build_text_grams(&path, "orders").expect("build");
        let after = Catalog::open(&path).expect("reopen").table("orders").expect("the table");
        let mut kept = 0;
        for (part, walked) in walked.iter().enumerate() {
            let sketched =
                after.rows_holding(part, 0, std::slice::from_ref(&sequence), true).expect("a part");
            assert_eq!(&sketched, walked, "part {part}");
            kept += sketched.map_or(0, |rows| rows.len());
        }
        let wanted = comments
            .iter()
            .filter(|one| !one.find("special").is_some_and(|at| one[at + 7..].contains("requests")))
            .count();
        assert!(walked.iter().all(Option::is_some), "every part is compressed text");
        assert!(wanted > 0 && wanted < comments.len());
        assert_eq!(kept, wanted);
        fs::remove_file(&path).expect("clean up");
    }
}

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

use rudb_common::{LogicalType, Result};
use rudb_encoding::sequence::grams;

use crate::graph::BUDGET_FLOOR;
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
    let columns = text_columns(&reader).collect::<Vec<_>>();
    if columns.is_empty() {
        return Ok(Vec::new());
    }
    let rows = reader.table().rows();
    let allowance = (reader.layout().columns_total().saturating_mul(share) / 100).max(BUDGET_FLOOR);
    let mut report = Vec::with_capacity(columns.len());
    let mut payloads = Vec::with_capacity(columns.len());
    for &column in &columns {
        let mut words = Vec::with_capacity(rows * 8);
        let mut text_bytes = 0_u64;
        for part in 0..reader.parts() {
            let chunk = reader.read(part, &[column])?;
            let values = chunk.column(0)?;
            for row in 0..chunk.len() {
                let text = values.bytes_at(row).unwrap_or_default();
                text_bytes += text.len() as u64;
                words.extend_from_slice(&grams(text).to_le_bytes());
            }
        }
        if words.len() != rows * 8 {
            return Err(invalid("a text sketch's row count differs from its table"));
        }
        report.push(Built { column, rows, text_bytes, bytes: words.len(), built: false });
        payloads.push(words);
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
    drop(reader);
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
    crate::attach(path, table, &attachments)?;
    Ok(report)
}

/// The sketch of a column in row id order, when the table carries a current one.
///
/// `None` covers every reason there is not one, and a caller that gets it walks every string, which
/// is what it did before the sketch existed.
#[must_use]
pub fn text_grams(reader: &Reader, column: usize) -> Option<Vec<u64>> {
    let table = reader.table();
    let id = u64::try_from(column).ok()?;
    let held =
        table.sections().iter().find(|held| held.kind == *section::TEXT_GRAMS && held.id == id)?;
    if !held.usable(table.generation()) {
        return None;
    }
    let bytes = reader.payload(held).ok()?;
    if bytes.len() != table.rows().checked_mul(8)? {
        return None;
    }
    Some(
        bytes
            .chunks_exact(8)
            .map(|word| u64::from_le_bytes(word.try_into().unwrap_or_default()))
            .collect(),
    )
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
    fn a_like_answered_through_the_sketch_keeps_the_rows_it_kept_without_one() {
        let (path, comments) = table_of("like", 6000);
        let before = Catalog::open(&path).expect("open").table("orders").expect("the table");
        let sequence = Sequence::new(&[b"special", b"requests"]).expect("an automaton");
        let mut walked = Vec::new();
        for part in 0..before.parts() {
            walked.push(before.rows_holding(part, 0, &sequence, true).expect("a part"));
        }
        drop(before);
        build_text_grams(&path, "orders").expect("build");
        let after = Catalog::open(&path).expect("reopen").table("orders").expect("the table");
        let mut kept = 0;
        for (part, walked) in walked.iter().enumerate() {
            let sketched = after.rows_holding(part, 0, &sequence, true).expect("a part");
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

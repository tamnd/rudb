//! The table functions that read a file, which today are `read_parquet` and `read_csv`.
//!
//! These are the ones [`crate::table`] cannot finish resolving on its own, because their columns
//! are in the file rather than in a table in this crate. So a caller resolves the call, gets
//! [`Columns::Parquet`] back, and comes here with the path.
//!
//! A read can cover more than one file, because the path can be a pattern, and the two formats
//! settle their schema differently when it does. A Parquet file states its schema in its footer, so
//! the first file's word is taken and a later file that disagrees is cast to it. A CSV file states
//! nothing, so [`csv_fields`] sniffs every file the pattern named and combines the answers, which is
//! what the binary does and is the only way the answer can be right.
//!
//! The file is opened twice for a query that runs, once by the binder to read the schema and once
//! by the executor to read the rows. That is what DuckDB does too and it is not a mistake: the
//! binder has to know the column names before the rest of the statement can bind, and holding an
//! open file between binding and execution would mean a prepared statement holding a descriptor for
//! as long as it lives. The second open re-reads the footer, which is one read of the last few
//! kilobytes of the file.
//!
//! The filesystem is the real one. `rudb-io` has the seam for a second one and nothing reaches it
//! from SQL yet, so plumbing a choice through the binder and the executor before there is a second
//! choice to make would be an argument every caller passes and nobody varies.
//!
//! [`Columns::Parquet`]: crate::table::Columns::Parquet

use std::path::Path;
use std::sync::Arc;

use rudb_common::bounds::Zones;
use rudb_common::{Error, Field, Provenance, Result, Stat, Value};
use rudb_csv::{Given, Reader as CsvReader};
use rudb_io::glob::has_magic;
use rudb_io::{File, Filesystem, OpenMode, RealFilesystem, expand};
use rudb_parquet::Reader;

/// A reader over the Parquet file at `path`, positioned before its first row group.
///
/// # Errors
///
/// When the file is not there, with DuckDB's own wording, and whatever reading the footer reports.
pub fn open_parquet(path: &str) -> Result<Reader> {
    Reader::open(open_file(path)?)
}

/// A reader over the CSV file at `path`, positioned at its first row, with its punctuation and its
/// column types already worked out.
///
/// `given` is whatever the call said about how the file is written, and what it does not say is
/// sniffed. The binder and the executor each open the file and both hand the same thing in, which is
/// what keeps the columns a query was planned against and the columns it reads the same columns.
///
/// # Errors
///
/// When the file is not there, with DuckDB's own wording, and whatever sniffing it reports.
pub fn open_csv(path: &str, given: Given) -> Result<CsvReader> {
    CsvReader::open_with(open_file(path)?, path, given)
}

/// What a call's named parameters say about how a CSV file is written.
///
/// The binder works this out to sniff the file with and the executor works it out again to read it
/// with, both from the list the plan kept, which is what keeps the columns a query was planned
/// against and the columns it reads the same columns. A name this does not know is a name that says
/// nothing about punctuation, such as `all_varchar`, and is somebody else's to act on.
///
/// # Errors
///
/// When a punctuation parameter was given something other than a single byte.
pub fn csv_given(options: &[(&str, Value)]) -> Result<Given> {
    let mut given = Given::default();
    for (name, value) in options {
        match (*name, value) {
            ("header", Value::Boolean(on)) => given.header = Some(*on),
            ("delim" | "sep", Value::Varchar(text)) => {
                given.delimiter = Some(one_byte(name, text)?)
            }
            ("quote", Value::Varchar(text)) => given.quote = Some(one_byte(name, text)?),
            ("escape", Value::Varchar(text)) => given.escape = Some(one_byte(name, text)?),
            _ => {}
        }
    }
    Ok(given)
}

/// The one byte a punctuation parameter was given.
///
/// DuckDB takes a string of any length here and splits on the whole of it, so `delim='||'` is a two
/// byte delimiter there and `delim=''` is a file of one column. The scanner underneath this compares
/// one byte, so anything else is turned away rather than quietly read as the first byte of it, which
/// would be a wrong answer on a file that really is written that way.
fn one_byte(parameter: &str, text: &str) -> Result<u8> {
    match *text.as_bytes() {
        [byte] => Ok(byte),
        _ => Err(Error::not_implemented(format!(
            "the named parameter {parameter} given {} bytes rather than one",
            text.len()
        ))),
    }
}

/// Whether there is a file, rather than a directory, at `path`.
///
/// The replacement scan asks, because a name that looks like a file and is not one is a different
/// answer from a file this build has no reader for. A directory is not a file: DuckDB reports
/// `SELECT * FROM 'some/directory'` as a table that does not exist, which was measured.
#[must_use]
pub fn is_file(path: &str) -> bool {
    let at = Path::new(path);
    let filesystem = RealFilesystem::new();
    filesystem.exists(at) && !filesystem.is_dir(at)
}

/// Whether a path argument stands for a set of files rather than for one.
///
/// The binder asks because the two are named differently. A file gives its columns the stem of its
/// name to answer to and a pattern gives them the whole of what was written, both measured.
#[must_use]
pub fn is_pattern(path: &str) -> bool {
    has_magic(path)
}

/// The files a path argument names, which is one file, or every file a pattern matched.
///
/// Expanded here rather than in the executor because DuckDB expands at bind time: a pattern that
/// matches nothing is an error before the query starts, and the schema comes from the first file, so
/// the binder has to know which file that is.
///
/// # Errors
///
/// When nothing matched, with DuckDB's own wording, which says pattern whether or not one was
/// written because a path that is simply missing and a pattern that matched nothing are the same
/// answer there.
pub fn files(pattern: &str) -> Result<Vec<String>> {
    let found = expand(&RealFilesystem::new(), pattern)?;
    if found.is_empty() {
        return Err(Error::io(format!("No files found that match the pattern \"{pattern}\"")));
    }
    Ok(found)
}

/// The file at `path`, open for reading.
fn open_file(path: &str) -> Result<Box<dyn File>> {
    let filesystem = RealFilesystem::new();
    let at = Path::new(path);
    if !filesystem.exists(at) {
        // DuckDB's message, which says pattern because the argument is a glob there and will be
        // here. A path that is simply missing and a glob that matched nothing are the same answer.
        return Err(Error::io(format!("No files found that match the pattern \"{path}\"")));
    }
    filesystem.open(at, OpenMode::Read)
}

/// What the footers of the Parquet files behind one call said about them.
///
/// Four answers out of the one read. A Parquet footer states the schema, the row count and
/// whatever statistics the writer kept in the same few kilobytes at the end of the file, so a
/// binder that has read one has read all of it, and splitting them into four functions would mean
/// four ways to open the same file.
#[derive(Debug, Clone, Default)]
pub struct Footers {
    /// The columns the call produces, which are the first file's.
    pub fields: Vec<Field>,
    /// How many rows all of the files hold.
    pub rows: Stat<u64>,
    /// How many distinct values each column holds, by name, for the columns that were stated.
    pub distincts: Vec<(String, u64)>,
    /// The minimum and the maximum of every column of every row group, where anything can answer.
    ///
    /// Behind a trait object and not a table of numbers, because the file this matters most on has
    /// a hundred and five columns in eight thousand row groups and the bounds of it are already
    /// parsed and already in memory on the side that read them. What the planner wants out of them
    /// is one number, so the question travels to the bounds rather than the bounds to the question.
    pub zones: Option<Arc<dyn Zones>>,
}

/// The columns a `read_parquet` of `paths` produces, how many rows all of them hold, and how many
/// distinct values their columns hold.
///
/// All of it comes out of the same footer, which is why this is one function and not three. The
/// binder has to read the footer for the schema before the rest of the statement can bind, and the
/// rest of what is in there was being read and dropped. Taking it here costs an extra open of
/// nothing.
///
/// The columns are the first file's, which is the rule for Parquet and is not a choice made here.
/// See [`csv_fields`] for the format where it goes the other way.
///
/// The count is [`Stat::Unknown`] rather than an error when one of the files after the first will
/// not open or the total will not add up. This is the planner's number and not the query's answer:
/// the scan is about to open the same files and will report whatever is wrong with them in the
/// words it has always used, and a bind that began failing here would be a new error on a path that
/// is consulted only to choose between two plans. The first file is the exception, because its
/// footer has to be read for the schema whatever happens to the count.
///
/// A column has a distinct count here only when every row group of every file stated one. Most
/// writers state none, because counting it costs a pass the rest of the statistics do not. DuckDB
/// states one for the columns it already knew the number for, which in practice is the low
/// cardinality ones, which are the ones joins are written on. The number is the largest any one row
/// group stated, capped at the rows. That is the count exactly when the values recur across row
/// groups, which is the ordinary case, and an undercount when the file is sorted on the column and
/// each group holds its own stretch of values. The caller is the optimizer deciding whether a join
/// key is low enough cardinality to make a join produce more rows than its larger side, and an
/// undercount there makes the estimate too large rather than too small. Adding the row groups up
/// would be the other bound and it is useless: a column with the same ten thousand values in every
/// one of fifty groups would come out at five hundred thousand.
///
/// A pattern pays one footer per file for this, against a scan that is about to read all of them in
/// full. The alternative is the first file's count multiplied by the number of files, which is a
/// sample wearing the word exact, and the files of a partitioned export are not the same size.
///
/// The bounds stop at one file, where the rest of this does not. A test names a column by its
/// position in the file's schema, and two files are two schemas as far as this knows: the first
/// file's word is taken for the columns and a later one that disagrees is cast to it, so the same
/// position can be a different column in the second file. Ruling out a row group by comparing the
/// wrong column's bounds drops rows the query wanted, which is a wrong answer and not a slow one,
/// and the way to fix it is to map each file's schema onto the first file's rather than to assume
/// they line up. Until something asks for that, a call over more than one file answers `None` here
/// and the estimate falls back to what it did before.
///
/// # Errors
///
/// Everything [`open_parquet`] reports about the first file.
pub fn parquet_footers(paths: &[String]) -> Result<Footers> {
    let first = paths.first().map_or("", String::as_str);
    let reader = open_parquet(first)?;
    let fields = reader.fields();
    let zones = (paths.len() == 1).then(|| Arc::new(reader.zones()) as Arc<dyn Zones>);
    let mut largest: Vec<(String, Option<u64>)> =
        reader.metadata().schema.iter().map(|column| (column.name.clone(), Some(0))).collect();
    largest_distincts(&reader, &mut largest);
    let counted = |rows: Option<u64>, largest: Vec<(String, Option<u64>)>| {
        let Some(rows) = rows else {
            // Nothing is capped and nothing is claimed. A count that covers some of the files is
            // worse than no count, and a total that did not add up says the set of files is not
            // what this read them as. The bounds are unaffected, since they never covered more
            // than the first file and the first file is the one that opened.
            return Footers {
                fields: fields.clone(),
                rows: Stat::Unknown,
                distincts: Vec::new(),
                zones: zones.clone(),
            };
        };
        let distincts = largest
            .into_iter()
            .filter_map(|(name, count)| count.map(|count| (name, count.min(rows))))
            .collect();
        Footers {
            fields: fields.clone(),
            rows: Stat::exact(rows, Provenance::RowCount),
            distincts,
            zones: zones.clone(),
        }
    };
    let Some(mut total) = reader.rows() else { return Ok(counted(None, largest)) };
    for path in paths.iter().skip(1) {
        let Ok(reader) = open_parquet(path) else { return Ok(counted(None, largest)) };
        let Some(rows) = reader.rows() else { return Ok(counted(None, largest)) };
        let Some(sum) = total.checked_add(rows) else { return Ok(counted(None, largest)) };
        total = sum;
        largest_distincts(&reader, &mut largest);
    }
    Ok(counted(Some(total), largest))
}

/// Folds one file's stated distinct counts into the largest seen for each column so far.
///
/// A column starts at zero and stays a number for as long as every row group of every file states
/// one. One that did not say leaves it at nothing however many others did, because the ones that
/// said nothing could hold anything.
fn largest_distincts(reader: &Reader, largest: &mut [(String, Option<u64>)]) {
    for group in &reader.metadata().row_groups {
        for chunk in &group.columns {
            let Some(held) = largest.get_mut(chunk.column) else {
                continue;
            };
            let stated = chunk.stats.as_ref().and_then(|stats| stats.distinct);
            let stated = stated.and_then(|count| u64::try_from(count).ok());
            held.1 = match (held.1, stated) {
                (Some(held), Some(stated)) => Some(held.max(stated)),
                _ => None,
            };
        }
    }
}

/// The columns a `read_csv` of `paths` produces, sniffed out of the front of every one of them.
///
/// Every file and not only the first, which is the one place this differs from Parquet and is
/// DuckDB's rule rather than a choice made here. It was measured at two, three, four and six files:
/// four files where only the fourth holds a decimal answer DOUBLE, and six where only the sixth
/// holds text answer VARCHAR. A Parquet file states its schema in its footer, so there is a first
/// file's word to take. A CSV file states nothing, so there is not, and a directory of daily exports
/// where one day happens to hold whole numbers in an otherwise decimal column would come out BIGINT
/// or DOUBLE depending on which day sorted first. So all of them are sniffed and the answers are
/// combined by [`rudb_csv::across`].
///
/// That is an open and one sample read per file at bind time. It is what the binary does, it is the
/// only way the answer can be right, and it is a sample against a scan that is about to read all of
/// those files anyway.
///
/// # Errors
///
/// Everything [`open_csv`] reports, and a file that is missing a column the first one has.
pub fn csv_fields(paths: &[String], given: Given) -> Result<Vec<Field>> {
    let mut sniffed = Vec::with_capacity(paths.len());
    for path in paths {
        sniffed.push((path.clone(), open_csv(path, given)?.fields()));
    }
    rudb_csv::across(&sniffed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_is_not_there_is_duckdbs_own_message() {
        let error = open_parquet("/nowhere/at/all.parquet").unwrap_err();
        assert_eq!(
            error.message(),
            "No files found that match the pattern \"/nowhere/at/all.parquet\""
        );
    }

    #[test]
    fn a_file_that_is_there_and_is_not_parquet_fails_on_the_footer_rather_than_on_the_open() {
        // Cargo.toml of this crate, which exists and is not a Parquet file. The distinction
        // matters: a missing file and a file that is not what it claims are different mistakes and
        // a reader that reported both as missing would send somebody looking in the wrong place.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let error = open_parquet(path).unwrap_err();
        assert!(!error.message().contains("No files found"), "{error}");
    }

    #[test]
    fn the_rows_and_the_counts_the_writer_stated_come_out_of_the_one_read() {
        // The fixture is DuckDB written and has two row groups, and DuckDB stated a count for
        // three of its seven columns and nothing for the other four. A column one row group left
        // unstated is left out here however many others stated one.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rudb-parquet/testdata/mixed.parquet");
        let footers = parquet_footers(&[path.to_string()]).expect("the fixture");
        assert_eq!(footers.rows, Stat::exact(4096, Provenance::RowCount));
        assert_eq!(
            footers.distincts,
            vec![("a".to_string(), 97), ("s".to_string(), 5), ("d".to_string(), 64)]
        );
    }
}

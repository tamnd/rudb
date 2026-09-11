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

use rudb_common::{Error, Field, Result};
use rudb_csv::Reader as CsvReader;
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
/// # Errors
///
/// When the file is not there, with DuckDB's own wording, and whatever sniffing it reports.
pub fn open_csv(path: &str) -> Result<CsvReader> {
    CsvReader::open(open_file(path)?, path)
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

/// The columns of the Parquet file at `path`, in the order the file stores them.
///
/// # Errors
///
/// Everything [`open_parquet`] reports.
pub fn parquet_fields(path: &str) -> Result<Vec<Field>> {
    Ok(open_parquet(path)?.fields())
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
pub fn csv_fields(paths: &[String]) -> Result<Vec<Field>> {
    let mut sniffed = Vec::with_capacity(paths.len());
    for path in paths {
        sniffed.push((path.clone(), open_csv(path)?.fields()));
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
}

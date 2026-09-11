//! The table functions that read a file, which today are `read_parquet` and `read_csv`.
//!
//! These are the ones [`crate::table`] cannot finish resolving on its own, because their columns
//! are in the file rather than in a table in this crate. So a caller resolves the call, gets
//! [`Columns::Parquet`] back, and comes here with the path.
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

/// The columns of the CSV file at `path`, sniffed out of its front.
///
/// # Errors
///
/// Everything [`open_csv`] reports.
pub fn csv_fields(path: &str) -> Result<Vec<Field>> {
    Ok(open_csv(path)?.fields())
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

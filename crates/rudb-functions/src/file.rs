//! The table functions that read a file, which today are `read_parquet` and `read_csv`.
//!
//! These are the ones [`crate::table`] cannot finish resolving on its own, because their columns
//! are in the file rather than in a table in this crate. So a caller resolves the call, gets
//! [`Columns::Parquet`] back, and comes here with the path.
//!
//! The path can be a pattern. `read_parquet('data/*.parquet')` is how a directory of files is read
//! and it is what ClickBench's partitioned form is distributed as, so [`files`] turns whatever was
//! written into the list of files it names and everything above here works on that list. One file
//! is a list of one, which is what keeps the two cases from being two code paths.
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
use rudb_io::{File, Filesystem, OpenMode, RealFilesystem, glob, is_pattern};
use rudb_parquet::Reader;

/// Every file the argument to a file reading table function names, in the order they are read.
///
/// One path names one file. A pattern names however many it matches, and matching none of them is
/// the same error as a path that is not there, which is why DuckDB's message says pattern in both
/// cases rather than having two of them.
///
/// The order is sorted rather than whatever the directory hands back. DuckDB takes the directory's
/// order, which on the ext4 box this was measured on is creation order and elsewhere is something
/// else, and a query with no `ORDER BY` is not promised an order by SQL either way. The difference
/// shows up only in a query that is already not asking for one, and an order that is the same on
/// every machine is the one worth having.
///
/// # Errors
///
/// When nothing matches, with DuckDB's own wording.
pub fn files(path: &str) -> Result<Vec<String>> {
    if !is_pattern(path) {
        return if RealFilesystem::new().exists(Path::new(path)) {
            Ok(vec![path.to_string()])
        } else {
            Err(missing(path))
        };
    }
    let found = glob(&RealFilesystem::new(), path);
    if found.is_empty() {
        return Err(missing(path));
    }
    Ok(found.iter().map(|file| file.display().to_string()).collect())
}

/// DuckDB's message for a path that names nothing.
fn missing(path: &str) -> Error {
    Error::io(format!("No files found that match the pattern \"{path}\""))
}

/// A reader over the Parquet file at `path`, positioned before its first row group.
///
/// One file. A caller with a pattern asks [`files`] first and comes back here once per answer.
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

/// Whether `path` names anything, pattern or not.
///
/// The replacement scan asks, because a name that looks like a file and is not one is a different
/// answer from a file this build has no reader for.
#[must_use]
pub fn exists(path: &str) -> bool {
    files(path).is_ok()
}

/// The file at `path`, open for reading.
fn open_file(path: &str) -> Result<Box<dyn File>> {
    let filesystem = RealFilesystem::new();
    let at = Path::new(path);
    if !filesystem.exists(at) {
        return Err(missing(path));
    }
    filesystem.open(at, OpenMode::Read)
}

/// The columns a `read_parquet` of `path` produces, in the order the file stores them.
///
/// The first file settles it when `path` is a pattern, which is DuckDB's rule and is the only one
/// that lets a query bind without opening every file a pattern matched. A later file that does not
/// have one of those columns is caught where it is read rather than here, because that is where
/// there is something to say about which file disagreed.
///
/// # Errors
///
/// Everything [`files`] and [`open_parquet`] report.
pub fn parquet_fields(path: &str) -> Result<Vec<Field>> {
    Ok(open_parquet(&first(path)?)?.fields())
}

/// The columns a `read_csv` of `path` produces, sniffed out of the front of every file it names.
///
/// Every file and not only the first, which is the one place this differs from Parquet and is
/// DuckDB's rule rather than a choice made here. A Parquet file states its schema, so the first
/// file's word is taken and a later file that disagrees is cast to it. A CSV file states nothing,
/// so there is no word to take, and a directory of daily exports where one day happens to hold
/// whole numbers in an otherwise decimal column would come out BIGINT or DOUBLE depending on which
/// day sorted first. So all of them are read and the answers are combined by
/// [`rudb_csv::across`].
///
/// That is an open and a first block read per file at bind time. It is what the binary does, it is
/// the only way the answer can be right, and it is one megabyte per file against a scan that is
/// about to read all of them anyway.
///
/// # Errors
///
/// Everything [`files`] and [`open_csv`] report, and a file that is missing a column the first one
/// has.
pub fn csv_fields(path: &str) -> Result<Vec<Field>> {
    let mut sniffed = Vec::new();
    for file in files(path)? {
        let fields = open_csv(&file)?.fields();
        sniffed.push((file, fields));
    }
    rudb_csv::across(&sniffed)
}

/// The first file `path` names, which is the one the schema is read from.
fn first(path: &str) -> Result<String> {
    // `files` never answers an empty list, so the fallback is unreachable rather than a case.
    files(path)?.into_iter().next().ok_or_else(|| missing(path))
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

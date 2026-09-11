//! The table functions that read a file, which today is `read_parquet`.
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
use rudb_io::{Filesystem, OpenMode, RealFilesystem};
use rudb_parquet::Reader;

/// A reader over the Parquet file at `path`, positioned before its first row group.
///
/// # Errors
///
/// When the file is not there, with DuckDB's own wording, and whatever reading the footer reports.
pub fn open_parquet(path: &str) -> Result<Reader> {
    let filesystem = RealFilesystem::new();
    let at = Path::new(path);
    if !filesystem.exists(at) {
        // DuckDB's message, which says pattern because the argument is a glob there and will be
        // here. A path that is simply missing and a glob that matched nothing are the same answer.
        return Err(Error::io(format!("No files found that match the pattern \"{path}\"")));
    }
    let file = filesystem.open(at, OpenMode::Read)?;
    Reader::open(file)
}

/// The columns of the Parquet file at `path`, in the order the file stores them.
///
/// # Errors
///
/// Everything [`open_parquet`] reports.
pub fn parquet_fields(path: &str) -> Result<Vec<Field>> {
    Ok(open_parquet(path)?.fields())
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

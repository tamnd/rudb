//! What a name that is a file means.
//!
//! Two things in the binder need a file and they are not the same thing. `read_parquet('hits.parquet')`
//! names the reader outright, and `FROM 'hits.parquet'` does not name anything at all: it is a table
//! reference that the catalog has never heard of, and DuckDB turns it into the reader anyway. The
//! second is the replacement scan, and it is why every ClickBench query runs against a file without
//! a single word of the query being changed.
//!
//! Both of them end at the same question, which is what columns the file has, and that question can
//! only be answered by opening the file. The binder has to do it, because a query cannot bind
//! `SELECT UserID FROM 'hits.parquet'` without knowing that the file has a column called `UserID`
//! and what type it is. The scan opens the file again later. Two opens of the same footer is the
//! price of not keeping a reader alive between two passes that do not otherwise know about each
//! other, and it is a few kilobytes of a file that is fourteen gigabytes.
//!
//! # What the replacement scan fires on
//!
//! An extension, or the file existing, in that order, which is what DuckDB does and is observable
//! either way round.
//!
//! A name ending in `.parquet` becomes a `read_parquet` call whether or not the file is there,
//! which is how `FROM 'nope.parquet'` reports a missing file rather than a missing table. A name
//! ending in one of the text formats says which reader it would need, because this build does not
//! have that reader yet and a name that silently fell through to the catalog error would read as
//! though the extension meant nothing. Anything else is only a file if it is on disk, and a file
//! that is on disk with an extension nobody claims is DuckDB's "no extension found" error, which
//! names the three readers rather than leaving a person to guess which one they wanted.
//!
//! Everything that is left is not a file, and the catalog error that sent us here stands. That is
//! the case that matters most, because it is every typo anybody ever makes in a table name.

use rudb_common::{Error, Field, Result};
use rudb_io::{Filesystem, RealFilesystem};

/// The columns a Parquet file has, with the types the binder will give them.
///
/// # Errors
///
/// If the path does not exist, or the file is not one this build reads. Every one of those is a
/// message DuckDB also produces, because a query that names a file that is not there is the most
/// common thing that goes wrong here and the wording is what tells a person which of the two it was.
pub(crate) fn parquet_fields(path: &str) -> Result<Vec<Field>> {
    let reader = rudb_parquet::open_path(path)?;
    Ok(reader.fields())
}

/// Whether a name the catalog does not have should be read as a file, and as what.
///
/// `Ok(Some(path))` is a `read_parquet` call on that path. `Ok(None)` means the name is not a file
/// and the caller's catalog error is the right answer. An error is a name that is definitely a file
/// and definitely not one that can be read.
///
/// # Errors
///
/// When the name is a file this build has no reader for, which is the two text formats it does not
/// have yet and any extension at all that DuckDB does not know either.
pub(crate) fn replacement_scan(name: &str) -> Result<Option<String>> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".parquet") {
        return Ok(Some(name.to_string()));
    }
    for extension in [".csv", ".tsv", ".csv.gz", ".tsv.gz", ".json", ".ndjson", ".json.gz"] {
        if lower.ends_with(extension) {
            let reader = if extension.starts_with(".json") || extension.starts_with(".ndjson") {
                "read_json"
            } else {
                "read_csv"
            };
            return Err(Error::not_implemented(format!(
                "reading the file \"{name}\", which needs {reader} and this build does not have it yet"
            )));
        }
    }
    if RealFilesystem::new().exists(std::path::Path::new(name)) {
        return Err(Error::binder(format!(
            "No extension found that is capable of reading the file \"{name}\"\n* If this file is a supported file format you can explicitly use the reader functions, such as read_csv, read_json or read_parquet"
        )));
    }
    Ok(None)
}

/// The name a replacement scan gives the file it opened.
///
/// DuckDB uses the base name with the extension off, so `FROM '/data/hits.parquet'` is a table
/// called `hits` and `SELECT hits.UserID FROM '/data/hits.parquet'` resolves without an alias
/// being written. It is not the path: a qualifier with slashes in it could not be written down
/// without quoting it, and DuckDB says so when you try, with `Candidate tables: "hits"`.
pub(crate) fn stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parquet_name_is_a_reader_call_whether_or_not_the_file_is_there() {
        assert_eq!(replacement_scan("hits.parquet").unwrap(), Some("hits.parquet".to_string()));
        assert_eq!(replacement_scan("HITS.PARQUET").unwrap(), Some("HITS.PARQUET".to_string()));
        assert_eq!(
            replacement_scan("/data/x.parquet").unwrap(),
            Some("/data/x.parquet".to_string())
        );
    }

    #[test]
    fn a_name_that_is_not_a_file_leaves_the_catalog_error_alone() {
        // The one that matters. Every mistyped table name in the world arrives here, and turning
        // one of them into a file error would be worse than the error it replaced.
        assert_eq!(replacement_scan("hits").unwrap(), None);
        assert_eq!(replacement_scan("no_such_table").unwrap(), None);
    }

    #[test]
    fn a_text_format_says_which_reader_it_would_need() {
        let csv = replacement_scan("hits.csv").unwrap_err();
        assert!(csv.to_string().contains("read_csv"), "{csv}");
        let json = replacement_scan("hits.json").unwrap_err();
        assert!(json.to_string().contains("read_json"), "{json}");
    }

    #[test]
    fn a_file_that_exists_with_an_extension_nobody_claims_says_so() {
        // `Cargo.toml` is in the crate directory the tests run from, so this is a real file with a
        // real extension that no reader wants, which is exactly the case being checked.
        let error = replacement_scan("Cargo.toml").unwrap_err();
        assert!(
            error.to_string().contains("No extension found that is capable of reading the file"),
            "{error}"
        );
        assert!(error.to_string().contains("read_csv, read_json or read_parquet"), "{error}");
    }

    #[test]
    fn the_table_a_replacement_scan_produces_is_named_after_the_file() {
        assert_eq!(stem("/data/hits.parquet"), "hits");
        assert_eq!(stem("hits.parquet"), "hits");
        assert_eq!(stem("hits"), "hits");
    }

    #[test]
    fn a_parquet_file_that_is_not_there_is_a_missing_file_and_not_a_missing_table() {
        let error = parquet_fields("no_such_file_anywhere.parquet").unwrap_err();
        assert_eq!(
            error.to_string(),
            "IO Error: No files found that match the pattern \"no_such_file_anywhere.parquet\""
        );
    }
}

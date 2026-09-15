//! Where the page level bounds are, for the files that have them.
//!
//! Parquet keeps two structures that describe pages rather than chunks. The column index holds a
//! minimum and a maximum per page, and the offset index holds a byte offset and a first row number
//! per page, so a filter answered against the first says which pages to skip and the second says
//! where the rest of them start. Neither is in the footer. The footer records where they are, which
//! is the format doing the right thing, because on `hits` they are larger than everything else in
//! the footer put together and a query wants them for two columns out of a hundred and five.
//!
//! This reads those four numbers and asserts what is and is not there, because what is not there
//! turned out to be the interesting half. Two of the three writers whose files this engine is
//! measured on write no page index at all, and a bound that was never written cannot be read. That
//! is the reason page level skipping is not worth building against ClickBench, and it is worth a
//! test rather than a note, because the day a writer starts emitting one this test changes.

use std::path::{Path, PathBuf};

use rudb_io::{Filesystem, OpenMode, RealFilesystem};
use rudb_parquet::Metadata;

/// The footer of a fixture.
fn footer(name: &str) -> Metadata {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name);
    let fs = RealFilesystem::new();
    let file = fs.open(&path, OpenMode::Read).expect("opens the fixture");
    Metadata::read(file.as_ref()).expect("reads the footer")
}

/// pyarrow writes both structures when it is asked to, and `paged.parquet` asks.
#[test]
fn a_file_written_with_a_page_index_says_where_it_is() {
    let metadata = footer("paged.parquet");
    let group = &metadata.row_groups[0];
    assert_eq!(group.columns.len(), 2, "the fixture is two columns");
    for chunk in &group.columns {
        let name = &metadata.schema[chunk.column].name;
        let index = chunk.column_index.unwrap_or_else(|| panic!("no column index for {name}"));
        let pages = chunk.offset_index.unwrap_or_else(|| panic!("no offset index for {name}"));
        // Both sit after the data and before the footer, which is where the format puts them and
        // the reason reading them is a second read rather than part of opening the file.
        assert!(index.at > chunk.start(), "the column index of {name} is inside the data");
        assert!(pages.at > index.at, "the offset index of {name} comes before the column index");
        assert!(index.len > 0 && pages.len > 0);
    }
}

/// The finding, kept as a test.
///
/// DuckDB v1.4.1 wrote `mixed.parquet` and it writes none. Neither does parquet-cpp 1.5.1, which
/// wrote the `hits.parquet` that ClickBench distributes, checked the same way against its footer.
/// So on every file this engine is benchmarked against, page level bounds do not exist to be read.
#[test]
fn a_file_written_by_duckdb_has_no_page_index_to_read() {
    let metadata = footer("mixed.parquet");
    for group in &metadata.row_groups {
        for chunk in &group.columns {
            assert!(chunk.column_index.is_none(), "DuckDB started writing a column index");
            assert!(chunk.offset_index.is_none(), "DuckDB started writing an offset index");
        }
    }
}

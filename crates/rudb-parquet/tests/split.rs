//! Reading part of a row group, which is how a scan cuts a file into more pieces than it has groups.
//!
//! A row group is the obvious unit of work for a thread and it is far too large a one. DuckDB writes
//! a hundred and twenty two thousand rows into a group, so a million row file has nine, and a scan
//! that hands out nine pieces of work cannot keep more than nine threads busy whatever the machine
//! has. [`Reader::split_rows`] is the way out: a reader over a row range inside a group, which steps
//! over the pages before its first row by reading their headers and none of their bodies.
//!
//! Everything here is the same question asked in different ways. Reading a group in pieces has to
//! give the rows reading it whole gives, in the order it gives them, once each. The fixtures are the
//! ones the rest of the reader's tests use, written by DuckDB and by pyarrow, so a piece boundary
//! lands inside a page rather than on one, which is the case worth being sure about.

use std::path::{Path, PathBuf};

use rudb_common::Value;
use rudb_common::stage::{self, Stage};
use rudb_io::{Filesystem, OpenMode, RealFilesystem};
use rudb_parquet::Reader;

/// A reader over a committed fixture, with every column projected.
fn reader(name: &str) -> Reader {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name);
    let fs = RealFilesystem::new();
    let file = fs.open(&path, OpenMode::Read).expect("opens the fixture");
    Reader::open(file).expect("reads the footer")
}

/// Every row a reader produces, one vector of values per row.
fn rows(reader: &mut Reader) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    while let Some(chunk) = reader.next_chunk().expect("decodes") {
        // row at a time: the test is that the rows come out the same, which is a per row question.
        for at in 0..chunk.len() {
            out.push(chunk.row(at).collect());
        }
    }
    out
}

/// The rows of row group `group` of `name`, read whole.
fn whole(name: &str, group: usize) -> Vec<Vec<Value>> {
    let parent = reader(name);
    let mut reader = parent.split(group..group + 1).expect("splits off one row group");
    rows(&mut reader)
}

/// The rows of `wanted` inside row group `group` of `name`.
fn part(name: &str, group: usize, wanted: std::ops::Range<usize>) -> Vec<Vec<Value>> {
    let parent = reader(name);
    let mut reader = parent.split_rows(group, wanted).expect("splits off part of a row group");
    rows(&mut reader)
}

#[test]
fn a_piece_of_a_row_group_is_the_rows_reading_it_whole_gives() {
    let all = whole("mixed.parquet", 0);
    assert_eq!(all.len(), 2048, "the fixture's row groups are 2048 rows");
    assert_eq!(part("mixed.parquet", 0, 0..2048), all);
    assert_eq!(part("mixed.parquet", 0, 700..900), all[700..900]);
    assert_eq!(part("mixed.parquet", 0, 2047..2048), all[2047..2048]);
}

#[test]
fn the_pieces_of_a_row_group_cover_it_once_each() {
    // Pieces that do not divide the group evenly, so the last one is short and every other boundary
    // lands somewhere a writer had no reason to put a page break.
    let all = whole("zstd.parquet", 1);
    let mut joined = Vec::new();
    for start in (0..all.len()).step_by(300) {
        joined.extend(part("zstd.parquet", 1, start..(start + 300).min(all.len())));
    }
    assert_eq!(joined, all);
}

#[test]
fn a_piece_past_the_end_of_a_row_group_is_no_rows() {
    let all = whole("mixed.parquet", 0);
    assert!(part("mixed.parquet", 0, all.len()..all.len() + 500).is_empty());
    assert_eq!(part("mixed.parquet", 0, all.len() - 2..all.len() + 500), all[all.len() - 2..]);
}

#[test]
fn a_piece_that_starts_inside_a_page_reads_no_more_of_the_file_than_it_has_to() {
    // The point of the whole arrangement. A reader that starts half way through a row group reads
    // the headers of the pages before it and the bodies of none of them, so what it reads is about
    // half of what reading the group whole reads rather than all of it.
    let parent = reader("zstd.parquet");
    let mut all = parent.split(1..2).expect("splits off one row group");
    let total = rows(&mut all).len();
    let read = all.bytes_read();
    let mut second = parent.split_rows(1, total / 2..total).expect("splits off half a row group");
    let half = rows(&mut second).len();
    assert_eq!(half, total - total / 2, "the second half is the rows the first half left");
    assert!(
        second.bytes_read() < read * 3 / 4,
        "reading half a row group read {} bytes of the {read} the whole of it reads",
        second.bytes_read()
    );
}

/// How many bytes of dictionary page this thread has decoded since the clock was last reset.
fn decoded_dictionary() -> u64 {
    stage::here()
        .taken()
        .find(|(stage, _, _)| *stage == Stage::Dictionary)
        .map_or(0, |(_, _, bytes)| bytes)
}

#[test]
fn the_morsels_of_a_row_group_decode_its_dictionary_once() {
    // Without this the cutting costs more than it saves. Every morsel of a row group points into the
    // same dictionary page, so a group cut four ways used to decode it four times, and on a file of
    // wide string columns that is the whole of the read.
    let all = whole("mixed.parquet", 0);
    let cut = all.len() / 2;
    let parent = reader("mixed.parquet");
    stage::reset();
    let mut first = parent.split_rows(0, 0..cut).expect("splits off the first half");
    assert_eq!(rows(&mut first), all[..cut]);
    let once = decoded_dictionary();
    assert!(once > 0, "the fixture has a dictionary encoded column");
    let mut second = parent.split_rows(0, cut..all.len()).expect("splits off the second half");
    assert_eq!(rows(&mut second), all[cut..]);
    assert_eq!(decoded_dictionary(), once, "the second morsel decoded the dictionary again");
}

#[test]
fn every_row_group_of_a_file_can_be_read_in_pieces() {
    // Not `bytes.parquet`, whose blob column this reader refuses whole or in pieces, which is a
    // different thing missing and not this one.
    for name in ["mixed.parquet", "zstd.parquet", "delta.parquet", "lengths.parquet"] {
        let groups = reader(name).metadata().row_groups.len();
        for group in 0..groups {
            let all = whole(name, group);
            let cut = all.len() / 3;
            let mut joined = part(name, group, 0..cut);
            joined.extend(part(name, group, cut..all.len()));
            assert_eq!(joined, all, "{name} row group {group}");
        }
    }
}

//! Reading a real Parquet file, checked against what DuckDB reads from the same bytes.
//!
//! The fixture is `testdata/mixed.parquet`, written by DuckDB. It is 4096 rows in two row groups of
//! 2048, with seven columns that between them cover the shapes the reader has to get right: plain
//! and dictionary encoded pages, a column with nulls, booleans bit packed one to a bit, and a date
//! and a timestamp that are integers in the file and something else in SQL.
//!
//! The expectations are a closed form rather than a dump of values. Every column of the fixture is a
//! function of the row number, and that was checked against DuckDB before it was written down here:
//!
//! ```text
//! select count(*) from (select *, cast(row_number() over () - 1 as integer) i from 'mixed.parquet')
//! where a <> i%97 or b <> cast(i%1000 as bigint)*1000 or d <> (i%64)*1.5 or flag <> (i%2=0)
//!    or day <> date '1970-01-01' + (i%1000)
//!    or t <> timestamp '2013-07-15 10:00:00' + to_seconds(i%900)
//!    or s is distinct from (case when i%7=0 then null else 'tag' || (i%5) end)
//! ```
//!
//! That returns zero, so what is asserted below is DuckDB's reading of the file rather than this
//! reader's. A dump would have been this reader's own output blessed as correct, which tests that it
//! keeps doing whatever it does rather than that it does the right thing.

use std::path::{Path, PathBuf};

use rudb_common::Value;
use rudb_io::{Filesystem, OpenMode, RealFilesystem};
use rudb_parquet::Reader;
use rudb_vector::{Chunk, Form};

/// The fixture DuckDB wrote.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/mixed.parquet")
}

/// A reader over the fixture, with every column projected.
fn reader() -> Reader {
    let fs = RealFilesystem::new();
    let file = fs.open(&fixture(), OpenMode::Read).expect("opens the fixture");
    Reader::open(file).expect("reads the footer")
}

/// Every chunk of the file, in order.
fn chunks(reader: &mut Reader) -> Vec<Chunk> {
    let mut out = Vec::new();
    while let Some(chunk) = reader.next_chunk().expect("decodes") {
        out.push(chunk);
    }
    out
}

/// Seconds from the Unix epoch to `2013-07-15 10:00:00`, which is where the timestamp column starts.
const TIMESTAMP_BASE: i64 = 1_373_882_400;

/// What the `i`th row of the fixture holds, as DuckDB reads it.
fn row(i: i64) -> Vec<Value> {
    vec![
        Value::Integer((i % 97) as i32),
        Value::BigInt((i % 1000) * 1000),
        if i % 7 == 0 { Value::Null } else { Value::Varchar(format!("tag{}", i % 5)) },
        Value::Double((i % 64) as f64 * 1.5),
        Value::Boolean(i % 2 == 0),
        Value::Date((i % 1000) as i32),
        Value::Timestamp((TIMESTAMP_BASE + i % 900) * 1_000_000),
    ]
}

#[test]
fn every_value_of_the_file_is_the_value_duckdb_reads() {
    let mut reader = reader();
    let chunks = chunks(&mut reader);
    let mut i = 0_i64;
    for chunk in &chunks {
        // row at a time: the point is to compare all 4096 rows against DuckDB, not a sample.
        for at in 0..chunk.len() {
            assert_eq!(chunk.row(at).collect::<Vec<_>>(), row(i), "row {i}");
            i += 1;
        }
    }
    assert_eq!(i, 4096, "the file has 4096 rows");
}

#[test]
fn the_schema_is_the_one_the_footer_describes() {
    let fields = reader().fields();
    let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "s", "d", "flag", "day", "t"]);
    let types: Vec<String> = fields.iter().map(|field| field.ty.to_string()).collect();
    assert_eq!(types, ["INTEGER", "BIGINT", "VARCHAR", "DOUBLE", "BOOLEAN", "DATE", "TIMESTAMP"]);
    // DuckDB writes every column optional, so none of them is NOT NULL even though only s has nulls.
    assert!(fields.iter().all(|field| !field.not_null), "{fields:?}");
}

#[test]
fn a_dictionary_encoded_column_reaches_the_chunk_as_a_dictionary() {
    // The three the writer chose RLE_DICTIONARY for, against the four it wrote PLAIN. The pages are
    // 2048 rows and a chunk is 1024, so every one of these is a cut page, and a cut that flattened
    // would leave all seven flat and throw away the form a group by wants.
    let mut reader = reader();
    let chunk = &chunks(&mut reader)[0];
    let forms: Vec<Form> = chunk.columns().iter().map(rudb_vector::Vector::form).collect();
    assert_eq!(
        forms,
        [
            Form::Dictionary,
            Form::Flat,
            Form::Dictionary,
            Form::Dictionary,
            Form::Flat,
            Form::Flat,
            Form::Flat
        ]
    );
}

#[test]
fn a_row_group_comes_back_in_chunks_of_at_most_a_vector() {
    let mut reader = reader();
    let lengths: Vec<usize> = chunks(&mut reader).iter().map(Chunk::len).collect();
    // Two row groups of 2048, each cut into two chunks of 1024, which divides exactly.
    assert_eq!(lengths, [1024, 1024, 1024, 1024]);
}

#[test]
fn a_projection_reads_only_the_columns_it_asked_for() {
    let mut all = reader();
    let _ = chunks(&mut all);
    // Every column chunk of both row groups, which is what the footer says the file is made of.
    assert_eq!(all.bytes_read(), 35262);

    let mut one = reader();
    one.project(&[1]).expect("column 1 exists");
    let chunks = chunks(&mut one);
    assert_eq!(one.bytes_read(), 10871, "the two chunks of column b and nothing else");

    let mut i = 0_i64;
    for chunk in &chunks {
        assert_eq!(chunk.width(), 1, "one column was projected");
        // row at a time: a projected column still has to hold the values it held unprojected.
        for at in 0..chunk.len() {
            assert_eq!(chunk.value_at(at, 0), Value::BigInt((i % 1000) * 1000), "row {i}");
            i += 1;
        }
    }
    assert_eq!(i, 4096);
}

#[test]
fn a_projection_names_its_columns_in_the_order_it_was_given_them() {
    let mut reader = reader();
    reader.project(&[4, 0]).expect("both columns exist");
    let names: Vec<String> = reader.fields().into_iter().map(|field| field.name).collect();
    assert_eq!(names, ["flag", "a"]);
    let chunk = reader.next_chunk().expect("decodes").expect("has a chunk");
    assert_eq!(chunk.value_at(3, 0), Value::Boolean(false));
    assert_eq!(chunk.value_at(3, 1), Value::Integer(3));
}

#[test]
fn counting_the_rows_reads_no_column_data_at_all() {
    let mut reader = reader();
    reader.project(&[]).expect("no columns is a projection");
    let chunks = chunks(&mut reader);
    assert_eq!(chunks.iter().map(Chunk::len).sum::<usize>(), 4096);
    assert!(chunks.iter().all(|chunk| chunk.width() == 0), "and no columns in any of them");
    assert_eq!(reader.bytes_read(), 0, "a count reads the footer and stops");
}

#[test]
fn asking_for_a_column_the_file_does_not_have_is_an_error() {
    let mut reader = reader();
    let error = reader.project(&[7]).expect_err("there are seven columns, numbered 0 to 6");
    assert!(error.to_string().contains("column 7"), "{error}");
}

#[test]
fn reading_the_whole_file_at_once_gives_the_same_rows() {
    let fs = RealFilesystem::new();
    let file = fs.open(&fixture(), OpenMode::Read).expect("opens the fixture");
    let chunks = rudb_parquet::read(file).expect("decodes");
    assert_eq!(chunks.iter().map(Chunk::len).sum::<usize>(), 4096);
    assert_eq!(chunks[0].value_at(1, 2), Value::Varchar("tag1".into()));
    assert_eq!(chunks[0].value_at(7, 2), Value::Null);
}

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
use rudb_vector::{Chunk, Form, VECTOR_SIZE};

/// How many rows are in each row group of the fixture.
const GROUP: usize = 2_048;

/// The chunk lengths one row group of the fixture comes back as.
///
/// A chunk never spans two row groups, so a group is cut into whole vectors and whatever is left
/// over. At a vector of 1024 that is two chunks and at 2048 or more it is one, and writing down the
/// rule rather than either answer is what keeps this test about the reader.
fn cut() -> Vec<usize> {
    let mut left = GROUP;
    let mut lengths = Vec::new();
    while left > 0 {
        let take = left.min(VECTOR_SIZE);
        lengths.push(take);
        left -= take;
    }
    lengths
}

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
    // The three the writer chose RLE_DICTIONARY for, against the four it wrote PLAIN. A read that
    // flattened would leave all seven flat and throw away the form a group by wants.
    //
    // This covered the cut too while a page was 2048 rows and a chunk was 1024. It does not any
    // more, because every fixture here has row groups of at most a vector now, so no page in this
    // directory gets cut. #1081 is the fixture that would bring the cut back.
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
    // Two row groups of 2048, each cut the same way, because a chunk never spans two of them.
    let want: Vec<usize> = cut().into_iter().chain(cut()).collect();
    assert_eq!(lengths, want);
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

#[test]
fn fetching_a_few_rows_by_ordinal_gets_what_a_scan_of_the_whole_file_would() {
    let mut reader = reader();
    let wanted = [0_u64, 7, 2047, 2048, 4095];
    let chunk = reader.rows_at(&wanted).expect("fetches");
    assert_eq!(chunk.len(), wanted.len());
    assert_eq!(chunk.width(), 7);
    for (at, &row_number) in wanted.iter().enumerate() {
        let got: Vec<Value> = (0..chunk.width()).map(|column| chunk.value_at(at, column)).collect();
        assert_eq!(got, row(row_number as i64), "row {row_number}");
    }
}

#[test]
fn fetching_every_row_of_the_file_gets_every_row_of_the_file() {
    // The fetch reads only the positions it was asked for rather than decoding the page and
    // gathering, which means the walk over the values in between is its own piece of code and its
    // own chance to be off by one. Asking for all of them, in one go and in scattered runs, is what
    // catches that: a wrong step shows up as the wrong row rather than as a slow read.
    // A fetch comes back as one chunk, so the whole file takes four of them, which also covers the
    // case of a window that lies inside one row group and one that straddles both.
    for window in 0..4 {
        let mut whole = reader();
        let first = window * 1024;
        let wanted: Vec<u64> = (first..first + 1024).collect();
        let chunk = whole.rows_at(&wanted).expect("fetches");
        assert_eq!(chunk.len(), 1024);
        // row at a time: the point is that every one of the 4096 rows came back right, not a sample.
        for at in 0..chunk.len() {
            let got: Vec<Value> =
                (0..chunk.width()).map(|column| chunk.value_at(at, column)).collect();
            assert_eq!(got, row((first + at as u64) as i64), "row {}", first + at as u64);
        }
    }

    let mut scattered = reader();
    let wanted: Vec<u64> = (0..4096).filter(|row| row % 7 == 3 || row % 101 == 0).collect();
    let chunk = scattered.rows_at(&wanted).expect("fetches");
    assert_eq!(chunk.len(), wanted.len());
    // row at a time: same again, against the ordinal each row was asked for.
    for (at, &row_number) in wanted.iter().enumerate() {
        let got: Vec<Value> = (0..chunk.width()).map(|column| chunk.value_at(at, column)).collect();
        assert_eq!(got, row(row_number as i64), "row {row_number}");
    }
}

#[test]
fn fetching_rows_never_opens_a_row_group_that_holds_none_of_them() {
    // The fixture is two row groups of 2048 and one page per column chunk, so a page here always
    // holds a wanted row and page skipping has nothing to do. What it can show is the level above:
    // a fetch that stays inside the first row group never touches the second.
    let mut both = reader();
    let _ = both.rows_at(&[7, 2048]).expect("fetches");
    assert_eq!(both.bytes_read(), 35262, "one row in each group is every page of the file");

    let mut first = reader();
    let _ = first.rows_at(&[0, 7, 2047]).expect("fetches");
    assert!(
        first.bytes_read() < 35262 / 2 + 512,
        "three rows of the first row group read {} bytes",
        first.bytes_read()
    );
}

#[test]
fn fetching_one_column_of_one_row_is_cheaper_still() {
    let mut reader = reader();
    reader.project(&[1]).expect("column 1 exists");
    let chunk = reader.rows_at(&[4095]).expect("fetches");
    assert_eq!(chunk.len(), 1);
    assert_eq!(chunk.value_at(0, 0), Value::BigInt((4095 % 1000) * 1000));
    // A whole scan of that one column is 10871 bytes. One row of it is a page and a header or two.
    assert!(reader.bytes_read() < 10871, "{}", reader.bytes_read());
}

#[test]
fn fetching_no_rows_at_all_reads_nothing() {
    let mut reader = reader();
    let chunk = reader.rows_at(&[]).expect("fetches nothing");
    assert_eq!(chunk.len(), 0);
    assert_eq!(reader.bytes_read(), 0);
}

#[test]
fn ordinals_out_of_order_or_past_the_end_are_errors_rather_than_wrong_rows() {
    let mut reader = reader();
    let error = reader.rows_at(&[7, 3]).expect_err("not sorted");
    assert!(error.to_string().contains("sorted"), "{error}");
    let error = reader.rows_at(&[3, 3]).expect_err("not strictly increasing");
    assert!(error.to_string().contains("sorted"), "{error}");
    let error = reader.rows_at(&[4096]).expect_err("past the end");
    assert!(error.to_string().contains("past the end"), "{error}");
}

/// The split is what makes a row group a morsel. Two readers over the same open file, each reading
/// one of the fixture's two row groups, have to produce between them exactly the rows one reader
/// over the whole file produces, in the same order, and to read the same bytes doing it.
#[test]
fn two_readers_over_a_row_group_each_read_what_one_reader_over_the_file_reads() {
    let whole = reader();
    let mut first = whole.split(0..1).expect("the file has a first row group");
    let mut second = whole.split(1..2).expect("and a second");
    let mut i = 0_i64;
    for chunk in chunks(&mut first).iter().chain(&chunks(&mut second)) {
        // row at a time: the claim is about every row of both halves and not about a sample.
        for at in 0..chunk.len() {
            assert_eq!(chunk.row(at).collect::<Vec<_>>(), row(i), "row {i}");
            i += 1;
        }
    }
    assert_eq!(i, 4096, "the two halves cover the file");
    let mut all = reader();
    let _ = chunks(&mut all);
    assert_eq!(first.bytes_read() + second.bytes_read(), all.bytes_read());
}

/// A split reader reads nothing outside its own row groups, which is the property the scheduler
/// needs, and the cheapest way to see it is the byte counter rather than the rows.
#[test]
fn a_split_reader_reads_nothing_outside_the_row_groups_it_was_given() {
    let whole = reader();
    let second = 1;
    let mut none = whole.split(second..second).expect("an empty range is a reader with no work");
    assert!(chunks(&mut none).is_empty());
    assert_eq!(none.bytes_read(), 0);

    let mut one = whole.split(0..1).expect("the first row group");
    assert_eq!(chunks(&mut one).len(), cut().len(), "{GROUP} rows in chunks of {VECTOR_SIZE}");
    let mut all = reader();
    let _ = chunks(&mut all);
    assert!(one.bytes_read() < all.bytes_read(), "half a file is fewer bytes than all of it");
}

/// The projection and the `binary_as_string` answer are settled once, on the reader the splits come
/// off, because a caller that had to repeat them on every split would eventually not.
#[test]
fn a_split_carries_the_projection_it_was_split_from() {
    let mut whole = reader();
    whole.project(&[1]).expect("column 1 exists");
    let mut half = whole.split(1..2).expect("the second row group");
    let chunks = chunks(&mut half);
    assert_eq!(half.bytes_read(), 5435, "one chunk of column b and nothing else");
    let mut i = 2048_i64;
    for chunk in &chunks {
        assert_eq!(chunk.width(), 1, "one column was projected");
        // row at a time: a split column still has to hold the values it held unsplit.
        for at in 0..chunk.len() {
            assert_eq!(chunk.value_at(at, 0), Value::BigInt((i % 1000) * 1000), "row {i}");
            i += 1;
        }
    }
    assert_eq!(i, 4096);
}

/// A range the file does not have is a mistake in the caller, so it says so rather than reading
/// whatever happens to be there.
#[test]
fn splitting_past_the_end_of_the_file_is_an_error() {
    let whole = reader();
    let error = whole.split(0..3).expect_err("the file has two row groups");
    assert!(error.to_string().contains("row groups 0..3"), "{error}");
    let (start, end) = (2, 1);
    let error = whole.split(start..end).expect_err("backwards");
    assert!(error.to_string().contains("row groups 2..1"), "{error}");
}

//! Files other people's writers produced, read here and checked against DuckDB's answers.
//!
//! `read.rs` reads a file DuckDB wrote, which proves the reader and DuckDB agree about a file DuckDB
//! chose the shape of. This one is the other half: writers disagree about the parts of the format
//! that are optional, and the parts they disagree about are the ones a reader gets wrong.
//!
//! # The byte array columns nobody annotates
//!
//! `bytes.parquet` is pyarrow writing two `binary` columns, which come out as `BYTE_ARRAY` with no
//! annotation at all. DuckDB reads an unannotated byte array as `BLOB` and so does this reader, and
//! until recently this reader then refused to decode it, because the string column it would go into
//! validates UTF-8.
//!
//! That is not a corner. The ClickBench file has a hundred and five columns, twenty eight of them
//! byte arrays, and not one of the twenty eight is annotated, so reading only `VARCHAR` meant
//! reading none of the strings in the benchmark. All twenty eight million values in the first
//! partition are valid UTF-8. The fixture here has one column like that and one column that is not
//! text at all, so both the reading and the refusing are tested.
//!
//! # The file in the other codec
//!
//! `zstd.parquet` is the same shape as `mixed.parquet` written with `COMPRESSION zstd`, which is
//! what every writer that was configured by somebody rather than left alone produces. It is here
//! rather than in `rudb-compress` because the codec is only worth anything through the reader, and
//! a frame that decompresses in a unit test and a page that reads in a scan are not the same claim.
//!
//! # The benchmark file itself
//!
//! `hits_0.parquet` is 122 MB, which is too big to commit, so the test that reads it runs when
//! `RUDB_CORPUS` names a directory holding it and reports that it was skipped otherwise. The file
//! is one of the hundred partitions of ClickBench's `hits`, written by parquet-cpp 1.5.1, a million
//! rows in two row groups, every chunk Snappy, every page plain or dictionary.

use std::path::{Path, PathBuf};

use rudb_common::{LogicalType, Value};
use rudb_io::{Filesystem, OpenMode, RealFilesystem};
use rudb_parquet::Reader;

/// A reader over a file, with every column projected.
fn reader(path: &Path) -> Reader {
    let fs = RealFilesystem::new();
    let file = fs.open(path, OpenMode::Read).expect("the file opens");
    Reader::open(file).expect("the footer reads")
}

/// One of the committed fixtures.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

/// Every value of the named columns, column by column.
fn columns(reader: &mut Reader, count: usize) -> Vec<Vec<Value>> {
    let mut out = vec![Vec::new(); count];
    while let Some(chunk) = reader.next_chunk().expect("every chunk decodes") {
        for at in 0..chunk.len() {
            for (column, value) in chunk.row(at).enumerate() {
                out[column].push(value);
            }
        }
    }
    out
}

#[test]
fn an_unannotated_byte_array_is_a_blob_and_reads_as_one() {
    // DuckDB reads this fixture as two blob columns, so the types here are the types there. A
    // reader calling them VARCHAR would answer every query about them the same way and still be
    // wrong about what they are, which shows up the moment a query compares one against a string.
    let mut reader = reader(&fixture("bytes.parquet"));
    let fields = reader.fields();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].ty, LogicalType::Blob);
    assert_eq!(fields[1].ty, LogicalType::Blob);

    reader.project(&[0]).expect("the first column is a column");
    let values = columns(&mut reader, 1).remove(0);
    assert_eq!(values.len(), 2048);

    // DuckDB's answers for the same file: 1861 values that are not null, 16749 bytes between them,
    // and those bounds.
    let text: Vec<&[u8]> = values
        .iter()
        .filter_map(|value| match value {
            Value::Blob(bytes) => Some(bytes.as_slice()),
            Value::Null => None,
            other => panic!("a blob column produced {other:?}"),
        })
        .collect();
    assert_eq!(text.len(), 1861);
    assert_eq!(text.iter().map(|bytes| bytes.len()).sum::<usize>(), 16749);
    assert_eq!(text.iter().copied().min(), Some(&b"byte_0000"[..]));
    assert_eq!(text.iter().copied().max(), Some(&b"byte_1023"[..]));

    // pyarrow wrote a null every eleventh row, and where the nulls are is the part a reader that
    // decoded densely and never spread gets wrong while still producing the right count.
    for (at, value) in values.iter().enumerate() {
        assert_eq!(at % 11 == 0, matches!(value, Value::Null), "row {at}");
    }
}

#[test]
fn a_blob_that_is_not_text_is_refused_by_name_rather_than_guessed_at() {
    // The limitation, stated as a test so that it is a decision rather than a surprise. rudb has one
    // variable length column and it holds text, so bytes that are not text have nowhere to go until
    // the byte column arrives with the storage layer. What matters is that this is an error naming
    // the column and not an answer.
    let mut reader = reader(&fixture("bytes.parquet"));
    reader.project(&[1]).expect("the second column is a column");
    let error = reader.next_chunk().expect_err("bytes that are not text have nowhere to go");
    assert!(error.message().contains("raw"), "{}", error.message());
    assert!(error.message().contains("valid UTF-8"), "{}", error.message());
}

#[test]
fn a_file_in_zstd_reads_the_way_the_same_file_in_snappy_would() {
    // The codec is the one thing in a Parquet file that changes nothing about what the file means
    // and decides whether it can be read at all. `zstd.parquet` is DuckDB writing the shape
    // `mixed.parquet` has with the other codec, so this is the reader's whole path over bytes that
    // came out of `rudb-compress`'s zstd rather than its Snappy.
    let mut reader = reader(&fixture("zstd.parquet"));
    assert_eq!(reader.fields().len(), 4);
    let values = columns(&mut reader, 4);
    assert_eq!(values[0].len(), 20000);

    // DuckDB's answers for the same file.
    let ints: Vec<i32> = values[0]
        .iter()
        .map(|value| match value {
            Value::Integer(number) => *number,
            other => panic!("an integer column produced {other:?}"),
        })
        .collect();
    assert_eq!(ints.iter().map(|&number| i64::from(number)).sum::<i64>(), 959_289);
    assert_eq!(ints.iter().min(), Some(&0));
    assert_eq!(ints.iter().max(), Some(&96));

    let longs: i128 = values[1]
        .iter()
        .map(|value| match value {
            Value::BigInt(number) => i128::from(*number),
            other => panic!("a bigint column produced {other:?}"),
        })
        .sum();
    assert_eq!(longs, 9_990_000_000);

    // The string column is the one the codec matters most for, since it is where the bytes are and
    // where a wrong copy distance turns into a plausible looking URL rather than an error.
    let text: Vec<&str> = values[2]
        .iter()
        .filter_map(|value| match value {
            Value::Varchar(text) => Some(text.as_str()),
            Value::Null => None,
            other => panic!("a string column produced {other:?}"),
        })
        .collect();
    assert_eq!(text.len(), 17142);
    assert_eq!(text.iter().copied().min(), Some("https://example.com/page/0"));
    assert_eq!(text.iter().copied().max(), Some("https://example.com/page/999"));
    for (at, value) in values[2].iter().enumerate() {
        assert_eq!(at % 7 == 0, matches!(value, Value::Null), "row {at}");
    }

    let doubles: f64 = values[3]
        .iter()
        .map(|value| match value {
            Value::Double(number) => *number,
            other => panic!("a double column produced {other:?}"),
        })
        .sum();
    assert_eq!(doubles, 944_232.0);
}

#[test]
fn one_bad_column_does_not_stop_the_others() {
    // Worth its own test because it is the difference between a file being unreadable and a column
    // being unreadable. A query that does not select the byte column should not care that the file
    // has one, which is what projection already gives and what would quietly stop being true if the
    // refusal moved up into opening the file.
    let mut reader = reader(&fixture("bytes.parquet"));
    reader.project(&[0]).expect("the readable column is a column");
    let values = columns(&mut reader, 1).remove(0);
    assert_eq!(values.len(), 2048);
}

/// The ClickBench partition, if this machine has one.
///
/// It is 122 MB, so it is not committed, and the test that wants it says so rather than failing on
/// a machine that has not downloaded it.
fn hits() -> Option<PathBuf> {
    let dir = std::env::var_os("RUDB_CORPUS")?;
    let path = Path::new(&dir).join("hits_0.parquet");
    path.exists().then_some(path)
}

#[test]
fn the_clickbench_file_reads_the_way_duckdb_reads_it() {
    let Some(path) = hits() else {
        eprintln!("skipped: set RUDB_CORPUS to a directory holding hits_0.parquet");
        return;
    };
    let mut reader = reader(&path);
    let fields = reader.fields();
    assert_eq!(fields.len(), 105, "the ClickBench schema is 105 columns");

    // WatchID, CounterID and EventDate, by position, checked by name so that a file with the
    // columns in another order fails here rather than comparing the wrong numbers.
    let wanted = ["WatchID", "CounterID", "EventDate"];
    let at: Vec<usize> = wanted
        .iter()
        .map(|name| {
            fields.iter().position(|field| field.name == *name).expect("the column is in the file")
        })
        .collect();
    reader.project(&at).expect("three of a hundred and five");
    let columns = columns(&mut reader, 3);

    assert_eq!(columns[0].len(), 1_000_000, "the partition is a million rows");

    // Every number below is the DuckDB binary reading the same file. The sums are wider than the
    // columns because a million eight byte identifiers overflow one.
    let watch: Vec<i64> = columns[0]
        .iter()
        .map(|value| match value {
            Value::BigInt(number) => *number,
            other => panic!("WatchID produced {other:?}"),
        })
        .collect();
    assert_eq!(
        watch.iter().map(|&value| i128::from(value)).sum::<i128>(),
        6_916_307_057_775_100_222_333_653
    );
    assert_eq!(watch.iter().copied().min(), Some(4_611_687_214_012_840_539));
    assert_eq!(watch.iter().copied().max(), Some(9_223_371_478_009_085_789));

    let counter: Vec<i32> = columns[1]
        .iter()
        .map(|value| match value {
            Value::Integer(number) => *number,
            other => panic!("CounterID produced {other:?}"),
        })
        .collect();
    assert_eq!(counter.iter().map(|&value| i64::from(value)).sum::<i64>(), 49_573_571);
    assert_eq!(counter.iter().copied().min(), Some(17));
    assert_eq!(counter.iter().copied().max(), Some(62));

    // EventDate is a USMALLINT here rather than a date, which is what the ClickBench schema says and
    // not something to fix in the reader.
    let date: Vec<u16> = columns[2]
        .iter()
        .map(|value| match value {
            Value::USmallInt(number) => *number,
            other => panic!("EventDate produced {other:?}"),
        })
        .collect();
    assert_eq!(date.iter().map(|&value| i64::from(value)).sum::<i64>(), 15_901_000_000);
    assert_eq!(date.iter().copied().min(), Some(15901));
    assert_eq!(date.iter().copied().max(), Some(15901));
}

#[test]
fn the_string_columns_of_the_clickbench_file_read_as_blobs() {
    let Some(path) = hits() else {
        eprintln!("skipped: set RUDB_CORPUS to a directory holding hits_0.parquet");
        return;
    };
    let mut reader = reader(&path);
    let fields = reader.fields();
    let blobs = fields.iter().filter(|field| field.ty == LogicalType::Blob).count();
    assert_eq!(blobs, 28, "the ClickBench schema is 28 unannotated byte array columns");

    let at: Vec<usize> = ["URL", "Title"]
        .iter()
        .map(|name| {
            fields.iter().position(|field| field.name == *name).expect("the column is in the file")
        })
        .collect();
    reader.project(&at).expect("two of a hundred and five");
    let columns = columns(&mut reader, 2);

    // DuckDB's answers again. URL has 265 empty values and no nulls, and the total is the number a
    // reader that dropped a page or read one twice cannot match by accident.
    let url: Vec<&[u8]> = columns[0]
        .iter()
        .map(|value| match value {
            Value::Blob(bytes) => bytes.as_slice(),
            other => panic!("URL produced {other:?}"),
        })
        .collect();
    assert_eq!(url.len(), 1_000_000);
    assert_eq!(url.iter().map(|bytes| bytes.len() as u64).sum::<u64>(), 88_562_192);
    assert_eq!(url.iter().filter(|bytes| bytes.is_empty()).count(), 265);
    assert_eq!(url.iter().copied().max(), Some(&b"https://yandex.ru/used/Merce"[..]));

    let title: Vec<&[u8]> = columns[1]
        .iter()
        .map(|value| match value {
            Value::Blob(bytes) => bytes.as_slice(),
            other => panic!("Title produced {other:?}"),
        })
        .collect();
    assert_eq!(title.iter().map(|bytes| bytes.len() as u64).sum::<u64>(), 138_409_995);
    assert_eq!(title.iter().filter(|bytes| bytes.is_empty()).count(), 64282);
}

#[test]
fn reading_three_columns_of_the_clickbench_file_reads_three_columns_of_bytes() {
    // The measure `spec/engine/05-scan.md` section 5.6 asks for, on the file the benchmark uses
    // rather than on a fixture. A scan that reads a column nobody asked for returns the right answer
    // and is invisible in the result, so the bytes are the assertion.
    let Some(path) = hits() else {
        eprintln!("skipped: set RUDB_CORPUS to a directory holding hits_0.parquet");
        return;
    };
    let mut whole = reader(&path);
    let mut rows = 0;
    while let Some(chunk) = whole.next_chunk().expect("the whole file decodes") {
        rows += chunk.len();
    }
    assert_eq!(rows, 1_000_000);
    let all = whole.bytes_read();

    let mut three = reader(&path);
    let fields = three.fields();
    let at: Vec<usize> = ["WatchID", "CounterID", "EventDate"]
        .iter()
        .map(|name| fields.iter().position(|field| field.name == *name).expect("in the file"))
        .collect();
    three.project(&at).expect("three of a hundred and five");
    while three.next_chunk().expect("three columns decode").is_some() {}
    let some = three.bytes_read();

    // Reading three of a hundred and five columns reads a fourteenth of the file rather than all of
    // it. The bound is loose on purpose: the exact number is a property of how ClickHouse wrote this
    // partition and would be a test of the file, while the ratio is a test of the reader.
    assert!(some * 10 < all, "three columns read {some} bytes of the {all} the file holds");
    let mut none = reader(&path);
    none.project(&[]).expect("no columns is a projection");
    while none.next_chunk().expect("counting rows decodes").is_some() {}
    assert_eq!(none.bytes_read(), 0, "counting rows read column data");
}

//! A blob column in a file, coded against a dictionary the way a varchar column is.
//!
//! ClickBench's `hits.parquet` stores its string columns as plain byte arrays and they read back as
//! blobs. Those were written a length and the bytes a row, which made a file four times the size of
//! DuckDB's. They now get a global dictionary and codes like a varchar does, and nothing about the
//! values is allowed to change on the way: bytes that are not UTF-8 come back as they went in, nulls
//! stay null, and a grouping, a filter and an order over the file agree with the same query over
//! the same rows held in memory.

use rudb::Database;
use rudb_common::Value;

/// Sixty thousand rows over forty distinct blobs, every one of them starting with a byte that
/// cannot start UTF-8, and one row in thirteen null. A count comes along to check nothing moved.
const ROWS: &str = "SELECT r::BIGINT AS k, \
                    CASE WHEN r % 13 = 0 THEN NULL \
                    ELSE ('\\xFF\\x00' || (r % 40)::VARCHAR)::BLOB END AS b \
                    FROM range(60000) AS s(r)";

fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).expect("the query ran");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

const QUERIES: &[&str] = &[
    "SELECT b, count(*), sum(k) FROM t GROUP BY b ORDER BY b NULLS FIRST",
    "SELECT count(*) FROM t WHERE b = '\\xFF\\x007'::BLOB",
    "SELECT count(*), count(b), count(DISTINCT b) FROM t",
    "SELECT k, b FROM t WHERE k % 997 = 0 ORDER BY k",
    "SELECT min(b), max(b) FROM t",
];

#[test]
fn a_blob_column_is_coded_in_the_file_and_reads_back_as_the_rows_it_was_given() {
    let path = std::env::temp_dir().join(format!("rudb-blob-coded-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");

    let memory = Database::new();
    memory.execute(&format!("CREATE TABLE t AS {ROWS}")).expect("loads in memory");
    let expected = QUERIES.iter().map(|sql| rows(&memory, sql)).collect::<Vec<_>>();
    assert_eq!(expected[1], vec![vec![Value::BigInt(1385)]], "the filter finds its rows");

    {
        let database = Database::open(name).expect("a file name starts a native database");
        database.execute(&format!("CREATE TABLE t AS {ROWS}")).expect("loads");
        database.execute("CHECKPOINT").expect("commits");
        for (sql, expected) in QUERIES.iter().zip(&expected) {
            assert_eq!(&rows(&database, sql), expected, "{sql}");
        }
        let report =
            rows(&database, "SELECT column_name, compression FROM pragma_storage_info('t')");
        let blob = report
            .iter()
            .filter(|row| row[0] == Value::Varchar("b".into()))
            .map(|row| format!("{:?}", row[1]))
            .collect::<Vec<_>>();
        assert!(!blob.is_empty(), "the blob column is in the report");
        assert!(
            blob.iter().all(|compression| !compression.contains("PLAIN")),
            "forty repeating blobs should be coded, not written out: {blob:?}"
        );
    }

    let reopened = Database::open(name).expect("reopens");
    for (sql, expected) in QUERIES.iter().zip(&expected) {
        assert_eq!(&rows(&reopened, sql), expected, "{sql} after a reopen");
    }
    drop(reopened);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_blob_column_with_too_many_values_for_a_dictionary_still_reads_back_as_it_went_in() {
    let path = std::env::temp_dir().join(format!("rudb-blob-wide-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");
    // Every row its own value, with a long shared tail so the text compressor has something to
    // find, and the same byte that cannot start UTF-8 in front.
    let load = "SELECT r::BIGINT AS k, \
                ('\\xFE' || r::VARCHAR || '/a/long/and/repeated/path/for/the/compressor')::BLOB AS b \
                FROM range(200000) AS s(r)";
    let queries = [
        "SELECT count(*), count(DISTINCT b), min(b), max(b) FROM t",
        "SELECT k, b FROM t WHERE k % 4999 = 0 ORDER BY k",
        "SELECT k FROM t WHERE b = '\\xFE123/a/long/and/repeated/path/for/the/compressor'::BLOB",
    ];
    let memory = Database::new();
    memory.execute(&format!("CREATE TABLE t AS {load}")).expect("loads in memory");
    let expected = queries.iter().map(|sql| rows(&memory, sql)).collect::<Vec<_>>();
    assert_eq!(expected[2], vec![vec![Value::BigInt(123)]], "the filter finds its row");

    let database = Database::open(name).expect("a file name starts a native database");
    database.execute(&format!("CREATE TABLE t AS {load}")).expect("loads");
    database.execute("CHECKPOINT").expect("commits");
    drop(database);
    let reopened = Database::open(name).expect("reopens");
    for (sql, expected) in queries.iter().zip(&expected) {
        assert_eq!(&rows(&reopened, sql), expected, "{sql}");
    }
    let report = rows(&reopened, "SELECT column_name, compression FROM pragma_storage_info('t')");
    let plain = report
        .iter()
        .filter(|row| row[0] == Value::Varchar("b".into()))
        .filter(|row| row[1] == Value::Varchar("PLAIN".into()))
        .count();
    assert_eq!(plain, 0, "a long shared tail should be compressed, not written out: {report:?}");
    drop(reopened);
    let _ = std::fs::remove_file(&path);
}

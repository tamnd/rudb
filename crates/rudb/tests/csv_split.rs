//! One CSV file read on many threads, from SQL.
//!
//! The ranges are made a few kilobytes long so that a file a test can write quickly is cut into
//! dozens of them. That setting is for the whole process, which is why these tests are a test
//! binary of their own rather than part of `tests/csv.rs`.
//!
//! What is asserted is that the threads change nothing: the same rows in the same order and the
//! same error on the same line as one thread reading the whole file. The guessing and the checking
//! have their own tests one layer down.

use std::path::PathBuf;

use rudb::Database;
use rudb_common::Value;

/// Writes `text` to a file of its own and answers its path.
fn written(tag: &str, text: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("rudb-csv-split-{tag}-{}.csv", std::process::id()));
    std::fs::write(&path, text).expect("writes");
    path
}

/// Rows numbered from zero, with quoted values that hold delimiters and line endings, mixed line
/// endings and now and then an empty text value, so that a range often starts inside a quoted
/// value and has to find out it guessed wrong.
fn rows(count: usize, bad: Option<usize>) -> String {
    let mut text = String::from("n,x,s,d\n");
    for n in 0..count {
        let x = if bad == Some(n) { "oops".to_string() } else { format!("{}", n * 7 % 1000) };
        let s = match n % 5 {
            0 => "\"one,\ntwo\"".to_string(),
            1 => format!("plain {n}"),
            2 => "\"a \"\"quoted\"\"\r\nvalue\"".to_string(),
            3 => String::new(),
            _ => "\"\n\n\"".to_string(),
        };
        let ending = if n % 3 == 0 { "\r\n" } else { "\n" };
        text.push_str(&format!("{n},{x},{s},2020-01-{:02}{ending}", 1 + n % 28));
    }
    text
}

/// A database with `threads` threads and ranges small enough to cut these files into many.
fn database(threads: usize) -> Database {
    rudb_csv::split::set_size(4096);
    let database = Database::new();
    database.execute(&format!("SET threads = {threads}")).expect("sets the thread count");
    database
}

#[test]
fn a_file_loaded_on_many_threads_holds_the_rows_one_thread_reads_in_the_same_order() {
    let path = written("rows", &rows(30_000, None));
    let sql = format!("CREATE TABLE t AS SELECT * FROM read_csv('{}')", path.display());
    let many = database(8);
    many.execute(&sql).expect("loads");
    let one = database(1);
    one.execute(&sql).expect("loads");
    std::fs::remove_file(&path).expect("removes");
    for check in [
        "SELECT count(*) FROM t",
        "SELECT sum(x) FROM t",
        "SELECT count(DISTINCT s) FROM t",
        "SELECT count(*) FROM t WHERE s LIKE '%\n%'",
        "SELECT max(d) FROM t",
    ] {
        assert_eq!(many.value(check).expect("runs"), one.value(check).expect("runs"), "{check}");
    }
    assert_eq!(many.value("SELECT count(*) FROM t").expect("runs"), Value::BigInt(30_000));
    // Read back on one thread, so that the rows come out in the order the load stored them.
    many.execute("SET threads = 1").expect("sets the thread count");
    let stored = many.query("SELECT n FROM t").expect("runs");
    let expected: Vec<Value> = (0..30_000).map(Value::BigInt).collect();
    let found: Vec<Value> = (0..stored.len()).map(|row| stored.value_at(row, 0)).collect();
    assert!(found == expected, "the rows were stored out of file order");
}

#[test]
fn a_bad_value_late_in_the_file_is_reported_on_the_line_one_thread_reports() {
    // Past the first megabyte, so the sniffer has not seen it and the column is still a number.
    let path = written("bad", &rows(40_000, Some(36_789)));
    let sql = format!("CREATE TABLE t AS SELECT * FROM read_csv('{}')", path.display());
    let many = database(8).execute(&sql).unwrap_err();
    let one = database(1).execute(&sql).unwrap_err();
    std::fs::remove_file(&path).expect("removes");
    assert_eq!(many.message(), one.message());
    assert!(one.message().contains("Line: 36791"), "{one}");
}

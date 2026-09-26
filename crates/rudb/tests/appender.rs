//! The Appender, `07-the-head.md` section 7.11. The texts are the pin's, from its C API.

use std::path::{Path, PathBuf};

use rudb::{Database, VECTOR_SIZE};
use rudb_common::Value;

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = db.query(sql).expect("the query runs");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

fn count(db: &Database, table: &str) -> Value {
    rows(db, &format!("SELECT count(*) FROM {table}"))[0][0].clone()
}

#[test]
fn rows_show_up_at_the_flush_and_not_before() {
    let db = Database::new();
    db.execute("CREATE TABLE t (id INTEGER, name VARCHAR, price DECIMAL(10, 2), day DATE)")
        .expect("creates");
    let mut appender = db.appender("t").expect("the appender opens");
    assert_eq!(appender.columns().len(), 4);
    appender
        .append_row([
            Value::Integer(1),
            Value::Varchar("one".into()),
            Value::Double(1.5),
            Value::Varchar("2026-01-02".into()),
        ])
        .expect("appends");
    appender.append(Value::BigInt(2)).expect("a BIGINT casts to INTEGER");
    appender.append(Value::Null).expect("appends");
    appender.append(Value::Varchar("3.25".into())).expect("text casts to DECIMAL");
    appender.append(Value::Null).expect("appends");
    appender.end_row().expect("ends");
    assert_eq!(count(&db, "t"), Value::BigInt(0), "nothing is written before the flush");
    appender.flush().expect("flushes");
    assert_eq!(count(&db, "t"), Value::BigInt(2));
    appender.close().expect("closes");
    let got = rows(&db, "SELECT id, name, CAST(price AS VARCHAR), CAST(day AS VARCHAR) FROM t");
    assert_eq!(got[0][1], Value::Varchar("one".into()));
    assert_eq!(got[0][2], Value::Varchar("1.50".into()));
    assert_eq!(got[0][3], Value::Varchar("2026-01-02".into()));
    assert_eq!(got[1][0], Value::Integer(2));
    assert_eq!(got[1][2], Value::Varchar("3.25".into()));
}

#[test]
fn many_rows_cross_chunks_and_keep_their_order() {
    let db = Database::new();
    db.execute("CREATE TABLE t (id BIGINT, tag VARCHAR)").expect("creates");
    let mut appender = db.appender("t").expect("opens");
    let total = 3 * VECTOR_SIZE + 17;
    for id in 0..total {
        let id = i64::try_from(id).expect("small");
        appender
            .append_row([Value::BigInt(id), Value::Varchar(format!("r{id}"))])
            .expect("appends");
    }
    appender.close().expect("closes");
    let got = rows(&db, "SELECT count(*), sum(id), min(tag), max(id) FROM t");
    let n = i64::try_from(total).expect("small");
    assert_eq!(got[0][0], Value::BigInt(n));
    assert_eq!(got[0][3], Value::BigInt(n - 1));
    let got = rows(&db, "SELECT tag FROM t WHERE id = 20000");
    assert_eq!(got[0][0], Value::Varchar("r20000".into()));
}

#[test]
fn a_short_or_long_row_is_refused_with_the_pins_text() {
    let db = Database::new();
    db.execute("CREATE TABLE t (a INTEGER, b INTEGER)").expect("creates");
    let mut appender = db.appender("t").expect("opens");
    appender.append(Value::Integer(1)).expect("appends");
    let error = appender.end_row().expect_err("one value short");
    assert_eq!(error.message(), "Call to EndRow before all columns have been appended to!");
    appender.append(Value::Integer(2)).expect("appends");
    let error = appender.append(Value::Integer(3)).expect_err("one value too many");
    assert!(error.to_string().contains("Too many appends for chunk!"), "{error}");
    appender.end_row().expect("the row is whole now");
    let error = appender.append(Value::Varchar("x".into())).expect_err("not a number");
    assert!(error.to_string().contains("Could not convert string 'x' to INT32"), "{error}");
}

#[test]
fn defaults_fill_in_and_a_missing_table_is_a_catalog_error() {
    let db = Database::new();
    db.execute("CREATE SEQUENCE s").expect("creates");
    db.execute("CREATE TABLE t (id INTEGER DEFAULT nextval('s'), note VARCHAR DEFAULT 'none')")
        .expect("creates");
    let mut appender = db.appender("t").expect("opens");
    for _ in 0..3 {
        appender.append_default().expect("the default");
        appender.append_default().expect("the default");
        appender.end_row().expect("ends");
    }
    appender.close().expect("closes");
    let got = rows(&db, "SELECT id, note FROM t ORDER BY id");
    assert_eq!(got.len(), 3);
    assert_eq!(got[2][0], Value::Integer(3));
    assert_eq!(got[2][1], Value::Varchar("none".into()));
    let error = db.appender("missing").expect_err("no such table");
    assert!(error.to_string().contains("Table with name missing does not exist"), "{error}");
}

#[test]
fn a_flush_that_breaks_a_constraint_keeps_nothing_since_the_last_one() {
    let db = Database::new();
    db.execute("CREATE TABLE t (id INTEGER NOT NULL CHECK (id > 0), name VARCHAR)")
        .expect("creates");
    let mut appender = db.appender("t").expect("opens");
    appender.append_row([Value::Integer(1), Value::Null]).expect("appends");
    appender.flush().expect("flushes");
    appender.append_row([Value::Integer(2), Value::Null]).expect("appends");
    appender.append_row([Value::Null, Value::Null]).expect("appends");
    let error = appender.flush().expect_err("a null in a NOT NULL column");
    assert!(error.to_string().contains("NOT NULL constraint failed: t.id"), "{error}");
    assert_eq!(count(&db, "t"), Value::BigInt(1));
    appender.append_row([Value::Integer(-1), Value::Null]).expect("appends");
    let error = appender.flush().expect_err("the check fails");
    assert!(
        error
            .to_string()
            .contains("CHECK constraint failed on table \"t\" with expression CHECK((id > 0))"),
        "{error}"
    );
    appender.append_row([Value::Integer(3), Value::Null]).expect("appends");
    appender.close().expect("closes");
    let got = rows(&db, "SELECT id FROM t ORDER BY id");
    assert_eq!(got, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
}

fn path(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("rudb-appender-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

fn open(path: &Path) -> Database {
    Database::open(path.to_str().expect("a UTF-8 path")).expect("the database opens")
}

#[test]
fn flushed_rows_survive_a_crash() {
    let path = path("crash");
    let db = open(&path);
    db.execute("CREATE TABLE t (id INTEGER, name VARCHAR)").expect("creates");
    db.execute("INSERT INTO t VALUES (0, 'zero')").expect("inserts");
    let mut appender = db.appender("t").expect("opens");
    for id in 1..=100 {
        appender.append_row([Value::Integer(id), Value::Varchar(format!("n{id}"))]).expect("ok");
    }
    appender.flush().expect("flushes");
    // A crash is a database that is never dropped, and an Appender that never gets to flush.
    std::mem::forget(appender);
    std::mem::forget(db);

    let db = open(&path);
    let got = rows(&db, "SELECT count(*), sum(id), max(name) FROM t");
    assert_eq!(got[0][0], Value::BigInt(101));
    assert_eq!(got[0][1], Value::HugeInt(5050));
    assert_eq!(got[0][2], Value::Varchar("zero".into()));
    db.close().expect("closes");
    let _ = std::fs::remove_file(&path);
}

//! A column type that the file cannot store must not take the database down with it. A `JSON`
//! column and a `JSONB` column go into the file and come back out of it. A type that the file cannot store yet is
//! refused at `CREATE TABLE`, before the catalog changes, so that the writes after it still work.

use std::path::{Path, PathBuf};

use rudb::Database;
use rudb_common::Value;

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-stored-{tag}-{}.rudb", std::process::id()));
    remove(&path);
    path
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
}

fn open(path: &Path) -> Database {
    Database::open(path.to_str().expect("a UTF-8 path")).expect("the database opens")
}

#[test]
fn a_json_column_goes_into_the_file_and_comes_back() {
    let path = path("json");
    let db = open(&path);
    for sql in [
        "CREATE TABLE j (k INTEGER, doc JSON)",
        r#"INSERT INTO j VALUES (1, '{"a": [1, 2]}'), (2, NULL), (3, '"text"')"#,
        "CHECKPOINT",
    ] {
        db.execute(sql).expect(sql);
    }
    drop(db);
    let db = open(&path);
    let rows: Vec<Vec<Value>> =
        db.execute("SELECT k, doc::VARCHAR FROM j ORDER BY k").expect("reads").rows().collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][1], Value::Varchar(r#"{"a": [1, 2]}"#.into()));
    assert_eq!(rows[1][1], Value::Null);
    assert_eq!(rows[2][1], Value::Varchar(r#""text""#.into()));
    db.execute("INSERT INTO j VALUES (4, '[]')").expect("writes after the reopen");
    drop(db);
    remove(&path);
}

#[test]
fn a_jsonb_column_keeps_the_normal_form_through_the_file() {
    let path = path("jsonb");
    let db = open(&path);
    for sql in [
        "CREATE TABLE j (k INTEGER, doc JSONB)",
        r#"INSERT INTO j VALUES (1, '{"bb":1,"a":[1.50,1e2],"bb":2}'), (2, NULL)"#,
        r#"INSERT INTO j SELECT 3, '{"z" : true}'::JSON"#,
        "CHECKPOINT",
    ] {
        db.execute(sql).expect(sql);
    }
    let refused = db.execute("INSERT INTO j VALUES (4, 'nope')").expect_err("not a document");
    assert_eq!(refused.reported_state().as_str(), "22P02");
    drop(db);
    let db = open(&path);
    let rows: Vec<Vec<Value>> =
        db.execute("SELECT k, doc::VARCHAR FROM j ORDER BY k").expect("reads").rows().collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][1], Value::Varchar(r#"{"a": [1.50, 100], "bb": 2}"#.into()));
    assert_eq!(rows[1][1], Value::Null);
    assert_eq!(rows[2][1], Value::Varchar(r#"{"z": true}"#.into()));
    let found = db.execute(r#"SELECT k FROM j WHERE doc = '{ "z":true }'"#).expect("compares");
    assert_eq!(found.rows().collect::<Vec<_>>(), [[Value::Integer(3)]]);
    drop(db);
    remove(&path);
}

#[test]
fn a_type_the_file_cannot_store_is_refused_and_the_writes_after_it_work() {
    let path = path("refused");
    let db = open(&path);
    db.execute("CREATE TABLE t (k INTEGER)").expect("creates");
    for sql in [
        "CREATE TABLE s (k INTEGER, v STRUCT(a INTEGER))",
        "CREATE TABLE m (k INTEGER, v MAP(INTEGER, INTEGER))",
        "CREATE TABLE c AS SELECT {'a': 1} AS v",
    ] {
        let error = db.execute(sql).expect_err(sql);
        assert_eq!(error.reported_state().as_str(), "0A000", "{sql}: {error}");
    }
    db.execute("INSERT INTO t VALUES (1)").expect("the write after the refusal works");
    assert!(db.execute("SELECT * FROM s").is_err(), "the refused table is not there");
    for sql in [
        "ALTER TABLE t ADD COLUMN v STRUCT(a INTEGER)",
        "ALTER TABLE t ALTER COLUMN k TYPE STRUCT(a INTEGER) USING {'a': k}",
    ] {
        let error = db.execute(sql).expect_err(sql);
        assert_eq!(error.reported_state().as_str(), "0A000", "{sql}: {error}");
    }
    db.execute("INSERT INTO t VALUES (2)").expect("the write after the refused change works");
    // A temporary table never goes into the file, so it can have any type.
    db.execute("CREATE TEMP TABLE x (v STRUCT(a INTEGER))").expect("creates a temporary table");
    db.execute("INSERT INTO x VALUES ({'a': 1})").expect("writes the temporary table");
    db.execute("CHECKPOINT").expect("commits");
    drop(db);
    let db = open(&path);
    assert_eq!(db.value("SELECT count(*) FROM t").expect("reads"), Value::BigInt(2));
    drop(db);
    remove(&path);
}

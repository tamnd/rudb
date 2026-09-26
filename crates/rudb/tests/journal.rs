//! A commit is durable when the statement returns, not at the next checkpoint.
//!
//! A crash is a database that is never dropped: `std::mem::forget` skips the write on the way out,
//! so what the next open finds is the file as the last checkpoint left it and the log beside it.
//!
//! An insert into an empty table streams into the file and is not logged, so each test puts a row
//! in first and the inserts after it are the ones the log carries.

use std::path::{Path, PathBuf};

use rudb::{Config, Database};
use rudb_common::Value;

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-journal-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(wal(&path));
    path
}

fn wal(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".wal");
    PathBuf::from(name)
}

fn segments(path: &Path) -> usize {
    std::fs::read_dir(wal(path)).map_or(0, |dir| dir.count())
}

fn open(path: &Path) -> Database {
    Database::open(path.to_str().expect("a UTF-8 path")).expect("the database opens")
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = db.query(sql).expect("the query runs");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

fn crash(db: Database) {
    std::mem::forget(db);
}

#[test]
fn inserted_rows_survive_a_crash_before_any_checkpoint_and_replay_once() {
    let path = path("crash");
    let db = open(&path);
    db.execute(
        "CREATE TABLE t (id INTEGER, name VARCHAR, price DECIMAL(10, 2), day DATE, seen TIMESTAMP)",
    )
    .expect("creates");
    db.execute(
        "INSERT INTO t VALUES (1, 'one', 1.50, DATE '2026-01-02', TIMESTAMP '2026-01-02 03:04:05')",
    )
    .expect("inserts");
    assert_eq!(segments(&path), 0, "the first insert streamed into the file");
    db.execute("INSERT INTO t VALUES (2, NULL, NULL, NULL, NULL), (3, 'three', 3.25, NULL, NULL)")
        .expect("inserts");
    assert!(segments(&path) > 0, "the inserts went to the log");
    crash(db);

    let db = open(&path);
    let want = rows(&db, "SELECT * FROM t ORDER BY id");
    assert_eq!(want.len(), 3);
    assert_eq!(want[0][1], Value::Varchar("one".into()));
    assert_eq!(want[1][1], Value::Null);
    assert_eq!(want[2][1], Value::Varchar("three".into()));
    assert_eq!(segments(&path), 0, "the open checkpointed what it replayed");
    db.execute("INSERT INTO t VALUES (4, 'four', 4.00, NULL, NULL)").expect("inserts");
    crash(db);

    let db = open(&path);
    let ids = rows(&db, "SELECT id FROM t ORDER BY id");
    let ids = ids.into_iter().map(|row| row[0].clone()).collect::<Vec<_>>();
    assert_eq!(ids, (1..=4).map(Value::Integer).collect::<Vec<_>>(), "each row once");
    db.close().expect("closes");
    assert!(!wal(&path).exists(), "a clean close leaves no log");
    let db = open(&path);
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], Value::BigInt(4));
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_rolled_back_insert_is_not_replayed_and_a_committed_one_is() {
    let path = path("rollback");
    let db = open(&path);
    db.execute("CREATE TABLE t (id BIGINT)").expect("creates");
    db.execute("INSERT INTO t VALUES (0)").expect("inserts");
    db.execute("BEGIN").expect("begins");
    db.execute("INSERT INTO t VALUES (1)").expect("inserts");
    db.execute("ROLLBACK").expect("rolls back");
    db.execute("BEGIN").expect("begins");
    db.execute("INSERT INTO t VALUES (2)").expect("inserts");
    db.execute("INSERT INTO t VALUES (3)").expect("inserts");
    db.execute("COMMIT").expect("commits");
    crash(db);

    let db = open(&path);
    assert_eq!(
        rows(&db, "SELECT id FROM t ORDER BY id"),
        vec![vec![Value::BigInt(0)], vec![Value::BigInt(2)], vec![Value::BigInt(3)]]
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_update_or_a_delete_checkpoints_and_the_log_after_it_still_replays() {
    let path = path("update");
    let db = open(&path);
    db.execute("CREATE TABLE t (id INTEGER, v VARCHAR)").expect("creates");
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").expect("inserts");
    db.execute("UPDATE t SET v = 'z' WHERE id = 2").expect("updates");
    db.execute("DELETE FROM t WHERE id = 3").expect("deletes");
    db.execute("INSERT INTO t VALUES (4, 'd')").expect("inserts");
    crash(db);

    let db = open(&path);
    assert_eq!(
        rows(&db, "SELECT id, v FROM t ORDER BY id"),
        vec![
            vec![Value::Integer(1), Value::Varchar("a".into())],
            vec![Value::Integer(2), Value::Varchar("z".into())],
            vec![Value::Integer(4), Value::Varchar("d".into())],
        ]
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rows_appended_through_the_api_are_logged_too() {
    let path = path("api");
    let db = open(&path);
    db.execute("CREATE TABLE t (id BIGINT, name VARCHAR)").expect("creates");
    db.append("t", &[vec![Value::BigInt(1), Value::Varchar("x".into())]]).expect("appends");
    db.append("t", &[vec![Value::Integer(2), Value::Null]]).expect("appends, converting");
    crash(db);

    let db = open(&path);
    assert_eq!(
        rows(&db, "SELECT id, name FROM t ORDER BY id"),
        vec![
            vec![Value::BigInt(1), Value::Varchar("x".into())],
            vec![Value::BigInt(2), Value::Null],
        ]
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_read_only_open_replays_without_touching_the_log() {
    let path = path("readonly");
    let db = open(&path);
    db.execute("CREATE TABLE t (id INTEGER)").expect("creates");
    db.execute("INSERT INTO t VALUES (6)").expect("inserts");
    db.execute("INSERT INTO t VALUES (7)").expect("inserts");
    crash(db);
    let before = segments(&path);
    assert!(before > 0);

    let name = path.to_str().expect("a UTF-8 path");
    let db = Database::open_with(name, Config::new().with_read_only(true)).expect("opens");
    assert_eq!(
        rows(&db, "SELECT id FROM t ORDER BY id"),
        vec![vec![Value::Integer(6)], vec![Value::Integer(7)]]
    );
    drop(db);
    assert_eq!(segments(&path), before, "a read only open leaves the log where it was");
    let db = open(&path);
    assert_eq!(
        rows(&db, "SELECT id FROM t ORDER BY id"),
        vec![vec![Value::Integer(6)], vec![Value::Integer(7)]]
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_log_beside_a_file_that_is_not_its_own_is_ignored() {
    let from = path("stale-from");
    let db = open(&from);
    db.execute("CREATE TABLE t (id INTEGER)").expect("creates");
    db.execute("INSERT INTO t VALUES (0)").expect("inserts");
    db.execute("INSERT INTO t VALUES (1)").expect("inserts");
    crash(db);

    // A log with nothing beside it is left over from a file that was removed.
    let to = path("stale-to");
    std::fs::create_dir_all(wal(&to)).expect("makes the directory");
    for entry in std::fs::read_dir(wal(&from)).expect("lists the log") {
        let entry = entry.expect("an entry");
        std::fs::copy(entry.path(), wal(&to).join(entry.file_name())).expect("copies");
    }
    let db = open(&to);
    db.execute("CREATE TABLE t (id INTEGER)").expect("creates, the stale log gone");
    db.execute("INSERT INTO t VALUES (2)").expect("inserts");
    db.execute("INSERT INTO t VALUES (3)").expect("inserts");
    crash(db);
    let db = open(&to);
    assert_eq!(
        rows(&db, "SELECT id FROM t ORDER BY id"),
        vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]
    );
    drop(db);
    let _ = std::fs::remove_file(&from);
    let _ = std::fs::remove_dir_all(wal(&from));
    let _ = std::fs::remove_file(&to);
}

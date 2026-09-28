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
fn an_update_or_a_delete_is_logged_and_replayed() {
    let path = path("update");
    let db = open(&path);
    db.execute("CREATE TABLE t (id INTEGER, v VARCHAR)").expect("creates");
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").expect("inserts");
    assert_eq!(segments(&path), 0, "the first insert streamed into the file");
    db.execute("UPDATE t SET v = 'z' WHERE id = 2").expect("updates");
    assert!(segments(&path) > 0, "the update went to the log rather than a checkpoint");
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

#[test]
fn changes_across_many_chunks_replay_in_the_order_they_committed() {
    let path = path("chunks");
    let db = open(&path);
    db.execute("CREATE TABLE t (id BIGINT, v VARCHAR, n DOUBLE)").expect("creates");
    db.execute("INSERT INTO t SELECT i, 'r' || i, i / 2 FROM range(10000) r(i)").expect("loads");
    db.execute("DELETE FROM t WHERE id % 3 = 0").expect("deletes");
    db.execute("UPDATE t SET v = 'u' || id, n = NULL WHERE id % 7 = 1").expect("updates");
    db.execute("BEGIN").expect("begins");
    db.execute("INSERT INTO t VALUES (20000, 'new', 1.5)").expect("inserts");
    db.execute("DELETE FROM t WHERE id BETWEEN 4000 AND 4999").expect("deletes");
    db.execute("UPDATE t SET n = -1 WHERE id = 20000 OR id = 9998").expect("updates");
    db.execute("COMMIT").expect("commits");
    db.execute("BEGIN").expect("begins");
    db.execute("DELETE FROM t").expect("deletes");
    db.execute("ROLLBACK").expect("rolls back");
    let query = "SELECT count(*), sum(id), count(n), sum(n), min(v), max(v), \
                 count(*) FILTER (WHERE v LIKE 'u%') FROM t";
    let before = rows(&db, query);
    let order = rows(&db, "SELECT list(id) FROM t");
    crash(db);

    let db = open(&path);
    assert_eq!(rows(&db, query), before);
    assert_eq!(rows(&db, "SELECT list(id) FROM t"), order, "the rows keep their order");
    assert_eq!(
        rows(&db, "SELECT v, n FROM t WHERE id IN (1, 2, 9998, 20000) ORDER BY id"),
        vec![
            vec![Value::Varchar("u1".into()), Value::Null],
            vec![Value::Varchar("r2".into()), Value::Double(1.0)],
            vec![Value::Varchar("r9998".into()), Value::Double(-1.0)],
            vec![Value::Varchar("new".into()), Value::Double(-1.0)],
        ]
    );
    crash(db);

    let db = open(&path);
    assert_eq!(rows(&db, query), before, "a second open replays nothing twice");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn commit_sync_is_a_setting_that_reads_back_and_refuses_what_it_is_not() {
    let db = Database::new();
    let read = |db: &Database| db.setting("commit_sync").expect("reads back");
    assert_eq!(read(&db), "full");
    for (written, read_back) in [("os", "os"), ("NONE", "none"), ("off", "none"), ("normal", "os")]
    {
        db.execute(&format!("SET commit_sync = '{written}'")).expect("sets");
        assert_eq!(read(&db), read_back);
    }
    let error = db.execute("SET commit_sync = 'sometimes'").expect_err("refused");
    assert!(error.to_string().contains("commit_sync is full, os or none"), "{error}");
    db.execute("RESET commit_sync").expect("resets");
    assert_eq!(read(&db), "full");
}

/// Under `os` a commit is written without a sync, and a crash of the process, which is what this
/// can test, loses none of it.
#[test]
fn a_commit_that_skips_the_sync_still_survives_a_crash_of_the_process() {
    let path = path("sync-os");
    let db = open(&path);
    db.execute("SET commit_sync = 'os'").expect("sets");
    db.execute("CREATE TABLE t (id INTEGER, name VARCHAR)").expect("creates");
    db.execute("INSERT INTO t VALUES (0, 'zero')").expect("inserts");
    let insert = db.prepare("INSERT INTO t VALUES (?, ?)").expect("prepares");
    for id in 1..500 {
        insert.execute(&[Value::Integer(id), Value::Varchar(format!("n{id}"))]).expect("inserts");
    }
    drop(insert);
    crash(db);

    let db = open(&path);
    let got = rows(&db, "SELECT count(*), sum(id), max(id) FROM t");
    assert_eq!(
        got[0],
        vec![Value::BigInt(500), Value::HugeInt((0..500).sum()), Value::Integer(499)]
    );
    db.close().expect("closes");
    let _ = std::fs::remove_file(&path);
}

/// Under `none` nothing waits. A close still keeps every row, and a crash keeps a prefix of the
/// commits, the ones written out before it, and never a commit without the ones before it.
#[test]
fn a_commit_that_waits_for_nothing_loses_at_most_the_tail_of_the_log() {
    let path = path("sync-none");
    let db = open(&path);
    db.execute("SET commit_sync = 'none'").expect("sets");
    db.execute("CREATE TABLE t (id INTEGER, name VARCHAR)").expect("creates");
    db.execute("INSERT INTO t VALUES (0, 'zero')").expect("inserts");
    let insert = db.prepare("INSERT INTO t VALUES (?, ?)").expect("prepares");
    let padding = "x".repeat(200);
    for id in 1..20_000 {
        insert.execute(&[Value::Integer(id), Value::Varchar(padding.clone())]).expect("inserts");
    }
    drop(insert);
    crash(db);

    let db = open(&path);
    let got = rows(&db, "SELECT count(*), min(id), max(id) FROM t");
    let Value::BigInt(count) = got[0][0] else { panic!("a count: {got:?}") };
    assert!(count > 1, "more than a megabyte was committed and none of it was written out");
    assert!(count <= 20_000);
    assert_eq!(got[0][1], Value::Integer(0));
    assert_eq!(got[0][2], Value::Integer(i32::try_from(count).expect("small") - 1), "a prefix");
    db.execute("INSERT INTO t VALUES (-1, 'last')").expect("inserts");
    db.close().expect("closes");

    let db = open(&path);
    let after = rows(&db, "SELECT count(*) FROM t");
    assert_eq!(after[0][0], Value::BigInt(count + 1), "a close keeps every row");
    db.close().expect("closes");
    let _ = std::fs::remove_file(&path);
}

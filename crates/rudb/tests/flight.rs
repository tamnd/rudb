//! A checkpoint on a thread of its own: a commit that takes the log past `checkpoint_threshold`
//! starts one and returns, the commits after it go on meanwhile, and the next one after it is done
//! puts what it wrote back into the catalog.
//!
//! A crash is a database that is never dropped, as in `journal.rs`. Each crash here waits a moment
//! first, so the checkpoint a test started is done writing the file before the next open reads it,
//! the way a crash of the whole process would stop it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rudb::Database;
use rudb_common::Value;

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-flight-{tag}-{}.rudb", std::process::id()));
    remove(&path);
    path
}

fn wal(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".wal");
    PathBuf::from(name)
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(wal(path));
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
    std::thread::sleep(Duration::from_millis(500));
    std::mem::forget(db);
}

/// The same statements against a database in memory, for what the file one should hold.
fn both(file: &Database, memory: &Database, sql: &str) {
    file.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    memory.execute(sql).unwrap_or_else(|error| panic!("{sql} in memory: {error}"));
}

const SUMMARY: &str = "SELECT count(*), count(DISTINCT id), sum(id), sum(v), string_agg(s, ',' ORDER BY id) \
                       FROM t";

fn same(file: &Database, memory: &Database, when: &str) {
    assert_eq!(rows(file, SUMMARY), rows(memory, SUMMARY), "{when}");
}

/// Inserts, updates and deletes of a table already in the file, enough of them that the log
/// passes a 4KiB threshold many times over, so checkpoints start while commits change the table
/// they are writing.
fn churn(file: &Database, memory: &Database) {
    both(file, memory, "CREATE TABLE t (id INTEGER, v BIGINT, s VARCHAR)");
    both(file, memory, "INSERT INTO t SELECT range, range * 2, 'r' || range FROM range(5000)");
    file.execute("CHECKPOINT").expect("checkpoints");
    file.execute("SET checkpoint_threshold = '4KiB'").expect("sets");
    for round in 0..300 {
        let id = 5000 + round;
        both(file, memory, &format!("INSERT INTO t VALUES ({id}, {round}, 'n{round}')"));
        if round % 3 == 0 {
            both(
                file,
                memory,
                &format!("UPDATE t SET v = v + 1, s = 'u{round}' WHERE id = {}", round * 7),
            );
        }
        if round % 5 == 0 {
            both(file, memory, &format!("DELETE FROM t WHERE id = {}", round * 11 + 1));
        }
    }
}

#[test]
fn commits_past_the_threshold_go_on_while_a_checkpoint_writes_the_file() {
    let path = path("churn");
    let (file, memory) = (open(&path), Database::new());
    // An open writes nothing until there is something to write.
    let before = std::fs::metadata(&path).map_or(0, |meta| meta.len());
    churn(&file, &memory);
    same(&file, &memory, "while open");
    assert!(
        std::fs::metadata(&path).expect("the file is there").len() != before,
        "the checkpoints wrote the file"
    );
    drop(file);
    assert!(!wal(&path).exists(), "a clean close leaves no log");

    let file = open(&path);
    same(&file, &memory, "after a close");
    drop(file);
    remove(&path);
}

#[test]
fn commits_made_while_a_checkpoint_was_in_flight_survive_a_crash() {
    let path = path("crash");
    let (file, memory) = (open(&path), Database::new());
    churn(&file, &memory);
    // One more round after a pause, so the last checkpoint is done and lands, and then one that
    // the next open has only the log for.
    std::thread::sleep(Duration::from_millis(500));
    both(&file, &memory, "UPDATE t SET v = v * 3 WHERE id % 4 = 1");
    both(&file, &memory, "DELETE FROM t WHERE id % 9 = 2");
    both(&file, &memory, "INSERT INTO t VALUES (-1, -1, 'last')");
    crash(file);

    let file = open(&path);
    same(&file, &memory, "after the crash");
    // And again, from the file the open after the crash checkpointed.
    both(&file, &memory, "INSERT INTO t VALUES (-2, -2, 'after')");
    crash(file);
    let file = open(&path);
    same(&file, &memory, "after a second crash");
    drop(file);
    remove(&path);
}

#[test]
fn a_commit_the_log_cannot_carry_while_a_checkpoint_is_in_flight_keeps_its_rows() {
    let path = path("dirty");
    let (file, memory) = (open(&path), Database::new());
    churn(&file, &memory);
    // Schema changes and a table written whole go through the file rather than the log.
    both(&file, &memory, "ALTER TABLE t ADD COLUMN w INTEGER DEFAULT 7");
    both(&file, &memory, "CREATE TABLE u AS SELECT range AS id FROM range(1000)");
    for round in 0..100 {
        both(
            &file,
            &memory,
            &format!("INSERT INTO t VALUES ({}, {round}, 'd{round}', {round})", 9000 + round),
        );
        both(&file, &memory, &format!("INSERT INTO u VALUES ({})", 1000 + round));
    }
    same(&file, &memory, "while open");
    let summary = "SELECT count(*), sum(w), sum(id) FROM t";
    assert_eq!(rows(&file, summary), rows(&memory, summary));
    crash(file);

    let file = open(&path);
    same(&file, &memory, "after the crash");
    assert_eq!(rows(&file, summary), rows(&memory, summary));
    let summary = "SELECT count(*), sum(id) FROM u";
    assert_eq!(rows(&file, summary), rows(&memory, summary));
    drop(file);
    remove(&path);
}

#[test]
fn background_checkpoint_off_checkpoints_before_the_commit_returns() {
    let path = path("off");
    let db = open(&path);
    assert_eq!(db.setting("background_checkpoint").expect("a setting"), "true");
    db.execute("SET background_checkpoint = false").expect("sets");
    assert_eq!(db.setting("background_checkpoint").expect("a setting"), "false");
    db.execute("SET checkpoint_threshold = '4KiB'").expect("sets");
    db.execute("CREATE TABLE t (id INTEGER, name VARCHAR)").expect("creates");
    db.execute("INSERT INTO t VALUES (0, 'zero')").expect("inserts");
    let before = std::fs::metadata(&path).expect("the file is there").len();
    for id in 1..=200 {
        db.execute(&format!("INSERT INTO t VALUES ({id}, 'name {id}')")).expect("inserts");
    }
    assert!(
        std::fs::metadata(&path).expect("the file is there").len() > before,
        "the commits that passed the threshold checkpointed into the file"
    );
    std::mem::forget(db);

    let db = open(&path);
    assert_eq!(
        rows(&db, "SELECT count(*), count(DISTINCT id), sum(id) FROM t"),
        vec![vec![Value::BigInt(201), Value::BigInt(201), Value::HugeInt(20_100)]]
    );
    drop(db);
    remove(&path);
}

/// Many connections committing at once, so commits queue behind the one that lands a checkpoint.
#[test]
fn connections_committing_at_once_while_checkpoints_run_lose_nothing() {
    let path = path("threads");
    let db = open(&path);
    db.execute("CREATE TABLE t (id INTEGER, v BIGINT)").expect("creates");
    db.execute("INSERT INTO t VALUES (-1, 0)").expect("inserts");
    db.execute("SET checkpoint_threshold = '8KiB'").expect("sets");
    std::thread::scope(|scope| {
        for worker in 0..4 {
            let connection = db.connect();
            scope.spawn(move || {
                for k in 0..250 {
                    let id = worker * 1000 + k;
                    connection
                        .execute(&format!("INSERT INTO t VALUES ({id}, {k})"))
                        .expect("inserts");
                    if k % 10 == 0 {
                        connection
                            .execute(&format!("UPDATE t SET v = v + 1 WHERE id = {}", id - 5))
                            .expect("updates");
                    }
                }
            });
        }
    });
    let want = rows(&db, "SELECT count(*), sum(id), sum(v) FROM t");
    assert_eq!(want[0][0], Value::BigInt(1001));
    crash(db);

    let db = open(&path);
    assert_eq!(rows(&db, "SELECT count(*), sum(id), sum(v) FROM t"), want);
    drop(db);
    remove(&path);
}

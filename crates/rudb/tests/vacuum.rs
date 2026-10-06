//! `VACUUM` and `ANALYZE`: the space a `VACUUM` gives back of a file that deleted rows and tables
//! written again left behind. The forms each dialect takes and its errors are checked where the
//! statement is bound.

use std::path::{Path, PathBuf};

use rudb::Database;
use rudb_common::Value;

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-vacuum-{tag}-{}.rudb", std::process::id()));
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

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).expect("the file is there").len()
}

fn value(db: &Database, sql: &str) -> Value {
    db.connect().value(sql).expect("the query runs")
}

#[test]
fn the_forms_the_pin_takes_succeed_on_a_database_in_memory() {
    let db = Database::new();
    db.execute("CREATE TABLE t (i INTEGER, j VARCHAR)").expect("creates");
    for sql in ["VACUUM", "VACUUM t", "VACUUM t(i)", "ANALYZE", "ANALYZE t"] {
        db.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    db.execute("BEGIN").expect("begins");
    db.execute("INSERT INTO t VALUES (1, 'a')").expect("inserts");
    db.execute("VACUUM t").expect("vacuums in a transaction");
    db.execute("COMMIT").expect("commits");
    assert_eq!(value(&db, "SELECT count(*) FROM t"), Value::BigInt(1));
}

#[test]
fn a_vacuum_after_a_delete_gives_the_space_back_and_keeps_the_rows() {
    let path = path("delete");
    let db = open(&path);
    db.execute("CREATE TABLE t AS SELECT range AS id, hash(range) AS v FROM range(2000000)")
        .expect("creates");
    db.execute("CHECKPOINT").expect("checkpoints");
    let full = size(&path);
    db.execute("DELETE FROM t WHERE id % 2 = 0").expect("deletes");
    // In a transaction it writes nothing, the way the pin succeeds there.
    db.execute("BEGIN").expect("begins");
    db.execute("VACUUM").expect("vacuums");
    db.execute("COMMIT").expect("commits");
    db.execute("VACUUM t").expect("vacuums");
    let after = size(&path);
    assert!(after < full * 3 / 4, "the file went from {full} to {after} bytes");
    // Nothing more to give back, so a second one leaves the file as it is.
    db.execute("VACUUM").expect("vacuums again");
    assert_eq!(size(&path), after);
    assert_eq!(value(&db, "SELECT count(*) FROM t"), Value::BigInt(1_000_000));
    db.execute("INSERT INTO t VALUES (0, 0)").expect("inserts");
    drop(db);

    let db = open(&path);
    assert_eq!(value(&db, "SELECT count(*) FROM t"), Value::BigInt(1_000_001));
    assert_eq!(value(&db, "SELECT count(*) FROM t WHERE id % 2 = 0"), Value::BigInt(1));
    assert_eq!(value(&db, "SELECT sum(id) FROM t"), Value::HugeInt(1_000_000 * 1_000_000));
    drop(db);
    remove(&path);
}

#[test]
fn a_vacuum_gives_back_the_space_a_table_written_again_left() {
    let path = path("again");
    let db = open(&path);
    db.execute("CREATE TABLE a AS SELECT range AS id, hash(range) AS v FROM range(2000000)")
        .expect("creates");
    db.execute("CREATE TABLE b AS SELECT range AS id, hash(range) AS v FROM range(1500000)")
        .expect("creates");
    db.execute("CHECKPOINT").expect("checkpoints");
    let full = size(&path);
    // Every row of `b` changed, so the checkpoint writes it again after the old one.
    db.execute("UPDATE b SET v = v // 2").expect("updates");
    db.execute("CHECKPOINT").expect("checkpoints");
    let grown = size(&path);
    assert!(grown > full + full / 4, "the checkpoint wrote the whole file: {full} to {grown}");
    db.execute("VACUUM").expect("vacuums");
    let after = size(&path);
    assert!(after < full + full / 10, "the file went from {grown} to {after} bytes");
    drop(db);

    let db = open(&path);
    assert_eq!(value(&db, "SELECT count(*) FROM a"), Value::BigInt(2_000_000));
    assert_eq!(
        value(&db, "SELECT count(*) FROM b WHERE v = hash(id) // 2"),
        Value::BigInt(1_500_000)
    );
    drop(db);
    remove(&path);
}

#[test]
fn a_vacuum_of_a_file_with_nothing_to_give_back_is_a_checkpoint() {
    let path = path("clean");
    let db = open(&path);
    db.execute("CREATE TABLE t AS SELECT range AS id FROM range(1000)").expect("creates");
    db.execute("VACUUM").expect("vacuums");
    let written = size(&path);
    db.execute("VACUUM").expect("vacuums");
    db.execute("ANALYZE").expect("analyzes");
    assert_eq!(size(&path), written);
    drop(db);
    let db = open(&path);
    assert_eq!(value(&db, "SELECT count(*) FROM t"), Value::BigInt(1000));
    drop(db);
    remove(&path);
}

#[test]
fn a_table_written_again_at_every_checkpoint_reuses_the_space_its_old_copies_gave_back() {
    let path = path("cycles");
    let db = open(&path);
    db.execute("CREATE TABLE a AS SELECT range AS id, hash(range) AS v FROM range(2000000)")
        .expect("creates");
    db.execute("CREATE TABLE b AS SELECT range AS id, hash(range) AS v FROM range(500000)")
        .expect("creates");
    db.execute("CHECKPOINT").expect("checkpoints");
    let full = size(&path);
    // Every row of `b` changes each round, so each checkpoint writes it again. The first copy
    // after the old one goes on the end, and so does the second, since the runs the first gave
    // back wait two generations. From the third on, each goes where an old copy was.
    db.execute("UPDATE b SET v = v // 2").expect("updates");
    db.execute("CHECKPOINT").expect("checkpoints");
    let copy = size(&path) - full;
    for round in 2..=8 {
        db.execute("UPDATE b SET v = v // 2").expect("updates");
        db.execute("CHECKPOINT").expect("checkpoints");
        let now = size(&path);
        assert!(
            now <= full + copy * 5 / 2,
            "round {round}: the file went from {full} to {now} bytes, a copy of b is {copy}"
        );
    }
    let want = "SELECT count(*) FROM b WHERE v = hash(id) // 256";
    assert_eq!(value(&db, want), Value::BigInt(500_000));
    drop(db);

    let db = open(&path);
    assert_eq!(value(&db, want), Value::BigInt(500_000));
    assert_eq!(value(&db, "SELECT count(*) FROM a WHERE v = hash(id)"), Value::BigInt(2_000_000));
    drop(db);
    remove(&path);
}

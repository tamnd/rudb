//! What a checkpoint writes after an update or a delete: the table that changed, and not the
//! tables beside it that did not.

use std::path::{Path, PathBuf};

use rudb::Database;
use rudb_common::Value;

fn path(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("rudb-checkpoint-{tag}-{}.rudb", std::process::id()));
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
fn a_delete_from_a_small_table_does_not_write_the_large_one_again() {
    let path = path("small");
    let db = open(&path);
    db.execute("CREATE TABLE big AS SELECT range AS id, hash(range) AS v FROM range(2000000)")
        .expect("creates");
    db.execute("CREATE TABLE t AS SELECT range AS id, range AS v FROM range(1000)")
        .expect("creates");
    db.execute("CHECKPOINT").expect("checkpoints");
    let before = size(&path);
    db.execute("DELETE FROM t WHERE id < 10").expect("deletes");
    db.execute("UPDATE t SET v = v + 1 WHERE id >= 990").expect("updates");
    db.execute("CHECKPOINT").expect("checkpoints");
    let after = size(&path);
    assert!(after < before + before / 10, "the file went from {before} to {after} bytes");
    drop(db);

    let db = open(&path);
    assert_eq!(value(&db, "SELECT count(*) FROM big"), Value::BigInt(2_000_000));
    assert_eq!(value(&db, "SELECT count(DISTINCT v) FROM big"), Value::BigInt(2_000_000));
    assert_eq!(value(&db, "SELECT count(*) FROM t"), Value::BigInt(990));
    assert_eq!(value(&db, "SELECT sum(v) FROM t"), Value::HugeInt(499_455 + 10));
    drop(db);
    remove(&path);
}

#[test]
fn the_space_a_table_written_again_leaves_is_given_back() {
    let path = path("space");
    let db = open(&path);
    // Hashed so the columns are most of the file, the way they are in any table worth the question.
    // The table the deletes go to is most of the file too, so that once enough of it is gone for
    // the checkpoint to write it again, the space its old stripes leave is most of the file.
    db.execute("CREATE TABLE big AS SELECT range AS id, hash(range) AS v FROM range(100000)")
        .expect("creates");
    db.execute("CREATE TABLE t AS SELECT range AS id, hash(range) AS v FROM range(600000)")
        .expect("creates");
    db.execute("CHECKPOINT").expect("checkpoints");
    let first = size(&path);
    let mut last = first;
    let mut shrank = false;
    for round in 0..8 {
        db.execute(&format!("DELETE FROM t WHERE id % 10 = {round}")).expect("deletes");
        db.execute("CHECKPOINT").expect("checkpoints");
        let now = size(&path);
        assert!(now <= first * 2, "round {round}: the file grew from {first} to {now} bytes");
        shrank |= now < last;
        last = now;
    }
    assert!(shrank, "no checkpoint wrote the whole file again");
    drop(db);

    let db = open(&path);
    assert_eq!(value(&db, "SELECT count(*) FROM big"), Value::BigInt(100_000));
    assert_eq!(value(&db, "SELECT count(*) FROM t"), Value::BigInt(120_000));
    assert_eq!(value(&db, "SELECT count(*) FROM t WHERE id % 10 < 8"), Value::BigInt(0));
    drop(db);
    remove(&path);
}

/// The format number in the header of the file at `path`.
fn format(path: &Path) -> u32 {
    let bytes = std::fs::read(path).expect("the file is there");
    u32::from_le_bytes(bytes[8..12].try_into().expect("four bytes"))
}

/// Writes `format` into the header of the file at `path`, the way a file an older build wrote
/// would have it.
fn stamp(path: &Path, format: u32) {
    use std::io::{Seek, SeekFrom, Write};
    let mut file =
        std::fs::OpenOptions::new().write(true).open(path).expect("the file opens to write");
    file.seek(SeekFrom::Start(8)).expect("seeks");
    file.write_all(&format.to_le_bytes()).expect("writes");
}

/// A file of the format before this one opens and answers as it is, and takes deletes, updates
/// and inserts and checkpoints them, with no step that moves the file to the new format first.
///
/// The header keeps the older number after the write, which is on purpose: a table this build
/// writes into an older file keeps the older part hash, so the build that wrote the file can still
/// read all of it. A file only moves to this build's format when it is written again whole.
#[test]
fn a_file_of_the_previous_format_takes_writes_with_no_migration_first() {
    let path = path("previous");
    let db = open(&path);
    db.execute("CREATE TABLE big AS SELECT range AS id, hash(range) AS v FROM range(200000)")
        .expect("creates");
    db.execute("CREATE TABLE t AS SELECT range AS id, range AS v FROM range(1000)")
        .expect("creates");
    drop(db);
    let current = format(&path);
    stamp(&path, current - 1);

    let db = open(&path);
    assert_eq!(value(&db, "SELECT count(*) FROM big"), Value::BigInt(200_000));
    assert_eq!(value(&db, "SELECT sum(v) FROM t"), Value::HugeInt(499_500));
    db.execute("DELETE FROM t WHERE id < 10").expect("deletes");
    db.execute("UPDATE t SET v = v + 1 WHERE id >= 990").expect("updates");
    db.execute("INSERT INTO t VALUES (1000, 1000)").expect("inserts");
    db.execute("CHECKPOINT").expect("checkpoints");
    drop(db);
    assert_eq!(format(&path), current - 1, "the write kept the file readable by the older build");

    let db = open(&path);
    assert_eq!(value(&db, "SELECT count(DISTINCT v) FROM big"), Value::BigInt(200_000));
    assert_eq!(value(&db, "SELECT count(*) FROM t"), Value::BigInt(991));
    assert_eq!(value(&db, "SELECT sum(v) FROM t"), Value::HugeInt(499_500 - 45 + 10 + 1000));
    assert_eq!(value(&db, "SELECT min(id) FROM t"), Value::BigInt(10));
    drop(db);
    remove(&path);
}

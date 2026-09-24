//! Loading a schema that was committed empty, one table at a time, when the tables have keys.
//!
//! A loading script creates every table in one statement and fills them in the next ones, often in
//! a process per table. A table with a primary key cannot stream into the file, because the sink
//! does not check keys, so its rows go through memory and reach the file at the checkpoint. The
//! checkpoint used to see a table the file held empty and the catalog held full as a disagreement
//! about which tables exist, and wrote the whole database again to settle it. Loading the JOB
//! schema that way took more than twenty times as long as loading it in one go.
//!
//! What these tests hold down is that the checkpoint appends instead: the file is the same file
//! afterwards, not a new one renamed over it, and every table reads back whole.

#![cfg(unix)]

use std::os::unix::fs::MetadataExt;

use rudb::Database;

/// A path nothing else in this file is using, with no database at it yet.
fn scratch(tag: &str) -> String {
    let path = std::env::temp_dir().join(format!("rudb-keyed-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path.to_str().expect("a UTF-8 temporary path").to_owned()
}

/// The file's inode, which a rewrite renamed over the path changes and an append does not.
fn inode(path: &str) -> u64 {
    std::fs::metadata(path).expect("the database file is there").ino()
}

/// Two keyed tables, created and committed with no rows in them by a process that has gone.
fn schema(path: &str) {
    let database = Database::open(path).expect("a file name starts a native database");
    database
        .execute("CREATE TABLE a (id INTEGER NOT NULL PRIMARY KEY, s VARCHAR)")
        .expect("creates");
    database
        .execute("CREATE TABLE b (id INTEGER NOT NULL PRIMARY KEY, s VARCHAR)")
        .expect("creates");
    database.execute("CHECKPOINT").expect("commits");
}

/// The count and the sum of the key of one table, read by a process that did not write it.
fn read(path: &str, table: &str) -> (String, String) {
    let database = Database::open(path).expect("the written file opens");
    let count = database.value(&format!("SELECT count(*) FROM {table}")).expect("counts");
    let sum = database.value(&format!("SELECT sum(id) FROM {table}")).expect("sums");
    (count.to_string(), sum.to_string())
}

#[test]
fn a_keyed_table_filled_after_its_schema_was_committed_is_appended() {
    let path = scratch("one");
    schema(&path);
    let before = inode(&path);
    {
        let database = Database::open(&path).expect("opens");
        database
            .execute("INSERT INTO a SELECT range::INTEGER, 'a' || range FROM range(5000)")
            .expect("fills a");
        database.execute("CHECKPOINT").expect("commits");
    }
    assert_eq!(inode(&path), before, "filling a rewrote the file instead of appending to it");
    {
        let database = Database::open(&path).expect("opens");
        database.execute("INSERT INTO b VALUES (1, 'one'), (2, 'two')").expect("fills b");
        database.execute("CHECKPOINT").expect("commits");
    }
    assert_eq!(inode(&path), before, "filling b rewrote a as well");
    assert_eq!(read(&path, "a"), ("5000".to_owned(), "12497500".to_owned()));
    assert_eq!(read(&path, "b"), ("2".to_owned(), "3".to_owned()));
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn two_keyed_tables_filled_in_one_transaction_are_appended_together() {
    // Two changed tables in one checkpoint is the second table's writer starting where the first
    // one closed, which is the path that has to step past the empty entry on its own.
    let path = scratch("two");
    schema(&path);
    let before = inode(&path);
    {
        let database = Database::open(&path).expect("opens");
        database.execute("BEGIN").expect("begins");
        database.execute("INSERT INTO a VALUES (1, 'x'), (2, 'y'), (3, 'z')").expect("fills a");
        database.execute("INSERT INTO b VALUES (10, 'ten')").expect("fills b");
        database.execute("COMMIT").expect("commits");
        database.execute("CHECKPOINT").expect("checkpoints");
    }
    assert_eq!(inode(&path), before, "filling both rewrote the file");
    assert_eq!(read(&path, "a"), ("3".to_owned(), "6".to_owned()));
    assert_eq!(read(&path, "b"), ("1".to_owned(), "10".to_owned()));
    std::fs::remove_file(path).expect("removes the temporary database");
}

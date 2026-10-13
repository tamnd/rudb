//! A checkpoint after a table was dropped, on a file that holds the table.
//!
//! A load that sorts each table on the way in copies it into a staging table, inserts it into the
//! real one in order, and drops the staging table. The checkpoint after the drop used to see a file
//! holding a table the catalog no longer had as a disagreement about which tables exist, and wrote
//! the whole database again to settle it. On the JOB clustered load that was a rewrite of a file of
//! several gigabytes for each of the 21 tables.
//!
//! What these tests hold down is that the checkpoint leaves the dropped table out of the next
//! generation instead: the file is the same file afterwards, the table stays dropped when it is
//! opened again, and the space it held is written into by later tables rather than added to.

#![cfg(unix)]

use std::os::unix::fs::MetadataExt;

use rudb::Database;

/// A path nothing else in this file is using, with no database at it yet.
fn scratch(tag: &str) -> String {
    let path = std::env::temp_dir().join(format!("rudb-dropped-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path.to_str().expect("a UTF-8 temporary path").to_owned()
}

/// The file's inode, which a rewrite renamed over the path changes and an append does not.
fn inode(path: &str) -> u64 {
    std::fs::metadata(path).expect("the database file is there").ino()
}

/// The file's length in bytes.
fn size(path: &str) -> u64 {
    std::fs::metadata(path).expect("the database file is there").len()
}

/// A committed table `c` of twenty thousand rows and an empty table for each of `empty`.
fn schema(path: &str, empty: &[&str]) {
    let database = Database::open(path).expect("a file name starts a native database");
    database.execute("CREATE TABLE c (id INTEGER, s VARCHAR)").expect("creates c");
    database
        .execute("INSERT INTO c SELECT range::INTEGER, 'c' || (range * 7919) FROM range(20000)")
        .expect("fills c");
    for table in empty {
        database
            .execute(&format!("CREATE TABLE {table} (id INTEGER, s VARCHAR)"))
            .expect("creates");
    }
    database.execute("CHECKPOINT").expect("commits");
}

/// The count and the sum of the key of one table, read by a process that did not write it.
fn read(path: &str, table: &str) -> (String, String) {
    let database = Database::open(path).expect("the written file opens");
    let count = database.value(&format!("SELECT count(*) FROM {table}")).expect("counts");
    let sum = database.value(&format!("SELECT sum(id) FROM {table}")).expect("sums");
    (count.to_string(), sum.to_string())
}

/// Whether a process that did not write the file finds `table` in it.
fn holds(path: &str, table: &str) -> bool {
    let database = Database::open(path).expect("the written file opens");
    database.value(&format!("SELECT count(*) FROM {table}")).is_ok()
}

#[test]
fn a_table_sorted_through_a_staging_table_is_appended() {
    let path = scratch("staged");
    schema(&path, &["a", "b"]);
    let before = inode(&path);
    {
        let database = Database::open(&path).expect("opens");
        for (table, rows) in [("a", 5000), ("b", 3000)] {
            for sql in [
                format!("CREATE TABLE {table}_in AS SELECT * FROM {table} LIMIT 0"),
                format!(
                    "INSERT INTO {table}_in SELECT range::INTEGER, '{table}' || range \
                     FROM range({rows})"
                ),
                "CHECKPOINT".to_owned(),
                format!("INSERT INTO {table} SELECT * FROM {table}_in ORDER BY id DESC"),
                "CHECKPOINT".to_owned(),
                format!("DROP TABLE {table}_in"),
            ] {
                database.execute(&sql).expect("loads");
            }
        }
        database.execute("CHECKPOINT").expect("commits");
    }
    assert_eq!(inode(&path), before, "a drop rewrote the file instead of appending to it");
    assert_eq!(read(&path, "a"), ("5000".to_owned(), "12497500".to_owned()));
    assert_eq!(read(&path, "b"), ("3000".to_owned(), "4498500".to_owned()));
    assert_eq!(read(&path, "c"), ("20000".to_owned(), "199990000".to_owned()));
    assert!(!holds(&path, "a_in"), "a_in is back after the file was opened again");
    assert!(!holds(&path, "b_in"), "b_in is back after the file was opened again");
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn a_drop_and_nothing_else_commits_a_catalog() {
    let path = scratch("alone");
    schema(&path, &["d"]);
    {
        let database = Database::open(&path).expect("opens");
        database
            .execute("INSERT INTO d SELECT range::INTEGER, 'd' FROM range(100)")
            .expect("fills");
        database.execute("CHECKPOINT").expect("commits");
    }
    let before = inode(&path);
    {
        let database = Database::open(&path).expect("opens");
        database.execute("DROP TABLE d").expect("drops");
        database.execute("CHECKPOINT").expect("commits");
    }
    assert_eq!(inode(&path), before, "a drop alone rewrote the file");
    assert!(!holds(&path, "d"), "d is back after the file was opened again");
    assert_eq!(read(&path, "c"), ("20000".to_owned(), "199990000".to_owned()));
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn a_drop_that_leaves_the_file_mostly_dead_writes_it_again() {
    // Dropping the table that holds most of the file is the case where carrying the rest forward
    // would keep a file mostly made of space nothing names, so the whole file is written again.
    let path = scratch("dead");
    schema(&path, &["e"]);
    let before = inode(&path);
    {
        let database = Database::open(&path).expect("opens");
        database.execute("DROP TABLE c").expect("drops");
        database.execute("INSERT INTO e VALUES (1, 'one')").expect("fills");
        database.execute("CHECKPOINT").expect("commits");
    }
    assert_ne!(inode(&path), before, "the space c held was kept");
    assert!(!holds(&path, "c"), "c is back after the file was opened again");
    assert_eq!(read(&path, "e"), ("1".to_owned(), "1".to_owned()));
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn the_space_a_dropped_table_held_is_written_into_again() {
    // Without the free list each round would add a staging table to the end of the file. With it,
    // a round writes into what a round before it gave back once that generation is two behind.
    let path = scratch("reused");
    schema(&path, &[]);
    let base = size(&path);
    let before = inode(&path);
    let mut first = 0;
    for round in 0..6 {
        let database = Database::open(&path).expect("opens");
        database.execute("CREATE TABLE s (id INTEGER, s VARCHAR)").expect("creates");
        database
            .execute("INSERT INTO s SELECT range::INTEGER, 's' || (range * 7919) FROM range(5000)")
            .expect("fills");
        database.execute("CHECKPOINT").expect("commits");
        if round == 0 {
            first = size(&path);
        }
        database.execute("DROP TABLE s").expect("drops");
        database.execute("CHECKPOINT").expect("commits");
    }
    assert_eq!(inode(&path), before, "a round rewrote the file");
    let staged = first - base;
    assert!(
        size(&path) <= base + 4 * staged,
        "six rounds of {staged} bytes grew the file from {base} to {} bytes",
        size(&path)
    );
    assert!(!holds(&path, "s"), "s is back after the file was opened again");
    assert_eq!(read(&path, "c"), ("20000".to_owned(), "199990000".to_owned()));
    std::fs::remove_file(path).expect("removes the temporary database");
}

//! A table a checkpoint extends or marks rows gone from keeps its extremes and sums in the file but
//! loses its distinct counts, frequencies, sketches and links. Once a tenth of it changed since it
//! was last written whole, the next checkpoint writes it whole again and builds its links again,
//! `engine-v4/10-freezing-and-checkpoint.md` section 10.10. Below a tenth it is extended, which is
//! what keeps a checkpoint the size of what changed.

use std::path::{Path, PathBuf};

use rudb::Database;
use rudb_common::Value;

struct File(PathBuf);

impl File {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-synopses-{tag}-{}.rudb", std::process::id()));
        let file = Self(path);
        file.clear();
        file
    }

    fn clear(&self) {
        let _ = std::fs::remove_file(&self.0);
        let mut wal = self.0.as_os_str().to_owned();
        wal.push(".wal");
        let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    }

    fn open(&self) -> Database {
        Database::open(self.0.to_str().expect("a UTF-8 path")).expect("opens")
    }
}

impl Drop for File {
    fn drop(&mut self) {
        self.clear();
    }
}

/// The distinct count the file holds for column `column` of `table`, and its rows.
fn distinct(path: &Path, table: &str, column: usize) -> (Option<u64>, usize) {
    let native = rudb_native::Catalog::open(path).expect("opens the file");
    let reader = native.table(table).expect("opens the table");
    (reader.distinct_values(column).expect("a column"), reader.table().rows())
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql).expect(sql).rows().collect()
}

#[test]
fn a_table_changed_by_a_tenth_is_written_whole_with_its_distinct_counts() {
    let file = File::new("distinct");
    let db = file.open();
    db.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").expect("creates");
    db.execute("INSERT INTO t SELECT i, 'v' || (i % 7) FROM range(1000) r(i)").expect("inserts");
    db.execute("CHECKPOINT").expect("checkpoints");
    assert_eq!(distinct(&file.0, "t", 0), (Some(1000), 1000));

    // Five in a hundred: extended, and the count it had no longer says what the column holds.
    db.execute("INSERT INTO t SELECT i, 'w' FROM range(1000, 1050) r(i)").expect("inserts");
    db.execute("CHECKPOINT").expect("checkpoints");
    assert_eq!(distinct(&file.0, "t", 0), (None, 1050), "a small append wrote the table whole");

    // Seventy more makes 120 of 1,120 since the table was last written whole.
    db.execute("INSERT INTO t SELECT i, 'w' FROM range(1050, 1120) r(i)").expect("inserts");
    db.execute("CHECKPOINT").expect("checkpoints");
    assert_eq!(distinct(&file.0, "t", 0), (Some(1120), 1120), "the table was not written whole");

    // And it starts again from nought.
    db.execute("INSERT INTO t SELECT i, 'w' FROM range(1120, 1170) r(i)").expect("inserts");
    db.execute("CHECKPOINT").expect("checkpoints");
    assert_eq!(distinct(&file.0, "t", 0), (None, 1170));
    assert_eq!(
        rows(&db, "SELECT count(*), sum(a) FROM t"),
        [[Value::BigInt(1170), Value::HugeInt((0..1170).sum())]]
    );
}

#[test]
fn deletes_count_toward_the_tenth() {
    let file = File::new("deletes");
    let db = file.open();
    db.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    db.execute("INSERT INTO t SELECT i FROM range(1000) r(i)").expect("inserts");
    db.execute("CHECKPOINT").expect("checkpoints");
    db.execute("DELETE FROM t WHERE a < 40").expect("deletes");
    db.execute("CHECKPOINT").expect("checkpoints");
    assert_eq!(distinct(&file.0, "t", 0).1, 1000, "forty deletes wrote the table whole");
    db.execute("DELETE FROM t WHERE a < 100").expect("deletes");
    db.execute("CHECKPOINT").expect("checkpoints");
    assert_eq!(distinct(&file.0, "t", 0), (Some(900), 900), "the table was not written whole");
    assert_eq!(
        rows(&db, "SELECT min(a), count(*) FROM t"),
        [[Value::Integer(100), Value::BigInt(900)]]
    );
}

/// The checkpoint a database takes when it closes builds again the links of a table it wrote
/// whole, which an explicit `CHECKPOINT` does for every table anyway.
#[test]
fn a_closing_checkpoint_builds_the_links_of_a_table_it_wrote_whole() {
    let file = File::new("links");
    {
        let db = file.open();
        for sql in [
            "CREATE TABLE orders(o_orderkey BIGINT PRIMARY KEY, o_total INTEGER)",
            "CREATE TABLE lineitem(l_orderkey BIGINT REFERENCES orders(o_orderkey), l_quantity \
             INTEGER)",
            "INSERT INTO orders SELECT i, CAST(i % 100 AS INTEGER) FROM range(4000) r(i)",
            "INSERT INTO lineitem SELECT i // 4, CAST(i % 50 AS INTEGER) FROM range(16000) r(i)",
            "CHECKPOINT",
        ] {
            db.execute(sql).expect(sql);
        }
    }
    let link = |db: &Database| {
        let listed = rows(db, "SELECT link FROM rudb_links()");
        assert_eq!(listed.len(), 1, "{listed:?}");
        !listed[0][0].is_null()
    };
    {
        let db = file.open();
        assert!(link(&db), "the explicit checkpoint built no link");
        db.execute("INSERT INTO lineitem SELECT i % 4000, 1 FROM range(2000) r(i)").expect("adds");
    }
    let db = file.open();
    assert!(link(&db), "the closing checkpoint lost the link");
    assert_eq!(distinct(&file.0, "lineitem", 1).1, 18000);
    assert!(distinct(&file.0, "lineitem", 1).0.is_some(), "lineitem was not written whole");
    assert_eq!(
        rows(&db, "SELECT count(*) FROM lineitem, orders WHERE l_orderkey = o_orderkey"),
        [[Value::BigInt(18000)]]
    );
}

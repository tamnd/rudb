//! A `DELETE` of a few rows from a table with a key marks them gone beside its file, or takes them
//! out where they are in memory, and takes their keys out of the sets the next write is checked
//! against. Each check runs the same statements on a file and in memory and compares the answers,
//! the errors and the rows.

use std::path::{Path, PathBuf};

use rudb::{Connection, Database};
use rudb_common::Value;

const SETUP: &[&str] = &[
    "CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR UNIQUE, v INTEGER)",
    "INSERT INTO t SELECT i, 'n' || i, (i % 7)::INTEGER FROM range(20000) r(i)",
];

fn path(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("rudb-keyed-delete-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

fn open(path: &Path) -> Database {
    Database::open(path.to_str().expect("a UTF-8 path")).expect("opens")
}

fn rows(db: &Connection, sql: &str) -> Vec<Vec<Value>> {
    db.execute(sql).expect(sql).rows().collect()
}

/// The same statements on a table in its file and one in memory.
struct Pair {
    filed: Database,
    memory: Database,
    a: (Connection, Connection),
    b: (Connection, Connection),
}

impl Pair {
    fn new(filed: Database) -> Self {
        let memory = Database::new();
        for sql in SETUP {
            filed.execute(sql).expect(sql);
            memory.execute(sql).expect(sql);
        }
        filed.execute("CHECKPOINT").expect("checkpoints");
        let a = (filed.connect(), memory.connect());
        let b = (filed.connect(), memory.connect());
        Self { filed, memory, a, b }
    }

    /// Runs `sql` as `a` and checks it works on both.
    fn ok(&self, sql: &str) {
        if let Err(error) = self.answer(false, sql) {
            panic!("{sql}: {error}");
        }
    }

    /// Runs `sql` as `a` and checks it fails the same way on both.
    fn fails(&self, sql: &str) {
        assert!(self.answer(false, sql).is_err(), "{sql} worked");
    }

    /// Runs `sql` as `a` or `b` on both and checks they agree.
    fn answer(&self, b: bool, sql: &str) -> Result<(), String> {
        let (filed, memory) = if b { &self.b } else { &self.a };
        let one = filed.execute(sql).map(|result| result.rows().collect::<Vec<_>>());
        let two = memory.execute(sql).map(|result| result.rows().collect::<Vec<_>>());
        match (one, two) {
            (Ok(one), Ok(two)) => {
                assert_eq!(one, two, "{sql}");
                Ok(())
            }
            (Err(one), Err(two)) => {
                assert_eq!(one.to_string(), two.to_string(), "{sql}");
                Err(one.to_string())
            }
            (one, two) => panic!("{sql}: the file said {one:?} and memory {two:?}"),
        }
    }

    fn same(&self) {
        let all = "SELECT * FROM t ORDER BY id";
        assert_eq!(rows(&self.filed.connect(), all), rows(&self.memory.connect(), all));
    }
}

fn deletes(pair: &Pair) {
    // A key checked once, so the sets are built before the delete has to keep them right.
    pair.fails("INSERT INTO t VALUES (1, 'x', 0)");
    pair.ok("DELETE FROM t WHERE id % 3 = 0");
    pair.same();
    pair.ok("INSERT INTO t VALUES (3, 'x3', 0)");
    pair.fails("INSERT INTO t VALUES (4, 'x4', 0)");
    pair.ok("INSERT INTO t VALUES (100000, 'n6', 0)");
    pair.fails("INSERT INTO t VALUES (100001, 'n7', 0)");
    pair.fails("INSERT INTO t VALUES (3, 'y3', 0)");
    pair.same();

    // Inside a transaction, which shares the sets with the snapshot it can go back to. A refused
    // write would end the transaction, as it does in the pin, so the refusals come after it.
    pair.ok("BEGIN");
    pair.ok("DELETE FROM t WHERE id = 1 OR id = 2");
    pair.ok("INSERT INTO t VALUES (1, 'z1', 0)");
    pair.answer(true, "SELECT count(*) FROM t WHERE id = 2").expect("another connection reads");
    pair.ok("COMMIT");
    pair.same();
    pair.ok("INSERT INTO t VALUES (2, 'z2', 0)");
    pair.fails("INSERT INTO t VALUES (1, 'w1', 0)");
    pair.fails("INSERT INTO t VALUES (5, 'z5', 0)");

    // A rollback leaves the keys the delete took out held.
    pair.ok("BEGIN");
    pair.ok("DELETE FROM t WHERE v = 1");
    pair.ok("INSERT INTO t VALUES (8, 'q8', 0)");
    pair.ok("ROLLBACK");
    pair.fails("INSERT INTO t VALUES (8, 'q8', 0)");
    pair.fails("INSERT INTO t VALUES (200000, 'n8', 0)");
    pair.same();

    // One row at a time with reads by key between, which keep where the keys are built.
    for id in [4_i64, 7, 11, 13, 19_999] {
        pair.ok(&format!("SELECT * FROM t WHERE id = {}", id + 1));
        pair.ok(&format!("DELETE FROM t WHERE id = {id}"));
        pair.ok(&format!("SELECT * FROM t WHERE id = {id}"));
        pair.ok(&format!("SELECT * FROM t WHERE id >= {} ORDER BY id LIMIT 3", id - 2));
    }
    pair.ok("INSERT INTO t VALUES (7, 'n7', 0)");
    pair.ok("SELECT * FROM t WHERE id = 7");
    pair.same();

    // Rows deleted after the table took rows beside its file, and every row deleted.
    pair.ok("DELETE FROM t WHERE id < 1000");
    pair.ok("INSERT INTO t VALUES (10, 'n10', 0)");
    pair.same();
    pair.ok("DELETE FROM t");
    pair.ok("INSERT INTO t VALUES (10, 'n10', 0)");
    pair.same();
}

#[test]
fn a_delete_from_a_keyed_table_in_its_file_keeps_the_keys_right() {
    let file = path("one");
    let pair = Pair::new(open(&file));
    deletes(&pair);
    let Pair { filed, a, b, .. } = pair;
    drop((a, b));
    drop(filed);
    let again = open(&file);
    assert_eq!(
        rows(&again.connect(), "SELECT count(*), max(id) FROM t"),
        vec![vec![Value::BigInt(1), Value::BigInt(10)]]
    );
    assert!(again.execute("INSERT INTO t VALUES (10, 'n11', 0)").is_err());
    assert!(again.execute("INSERT INTO t VALUES (11, 'n11', 0)").is_ok());
    drop(again);
    let _ = std::fs::remove_file(&file);
}

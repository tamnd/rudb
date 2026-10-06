//! A statement with its values written in, as a client sends it in the simple flow, takes the
//! short ways of a prepared statement: an insert of one row, a read, an update or a delete by key,
//! and an upsert. Each check here runs the statement on one database, where it can take the short
//! way, and the same statement with `AND true` after the key on another, where it cannot, and
//! compares the answers, the errors, the columns and then every row of both tables.

use rudb::{Connection, Database};
use rudb_common::Value;

/// Two databases that start from the same statements, one written the short way and one through
/// the plan.
struct Pair {
    short: Connection,
    planned: Connection,
    _databases: [Database; 2],
}

impl Pair {
    fn new(setup: &[&str]) -> Self {
        let (short, planned) = (Database::new(), Database::new());
        for sql in setup {
            short.execute(sql).expect(sql);
            planned.execute(sql).expect(sql);
        }
        Self { short: short.connect(), planned: planned.connect(), _databases: [short, planned] }
    }

    /// Runs `sql` on the short database and `slow` on the other, and checks they agree.
    fn both(&self, sql: &str, slow: &str) {
        match (self.short.execute(sql), self.planned.execute(slow)) {
            (Ok(short), Ok(planned)) => {
                assert_eq!(short.changes(), planned.changes(), "{sql}");
                assert_eq!(short.names(), planned.names(), "{sql}");
                for at in 0..short.names().len() {
                    assert_eq!(short.origin(at), planned.origin(at), "{sql} column {at}");
                }
                assert_eq!(short.rows().collect::<Vec<_>>(), planned.rows().collect::<Vec<_>>());
            }
            (Err(short), Err(planned)) => assert_eq!(short.to_string(), planned.to_string()),
            (short, planned) => {
                panic!("{sql}: the short way said {short:?} and the plan {planned:?}")
            }
        }
    }

    /// Runs `sql`, whose last condition is the key, on both.
    fn keyed(&self, sql: &str) {
        self.both(sql, &format!("{sql} AND true"));
    }

    fn all_same(&self) {
        let sql = "SELECT * FROM t ORDER BY k";
        let short: Vec<Vec<Value>> = self.short.execute(sql).expect(sql).rows().collect();
        assert_eq!(short, self.planned.execute(sql).expect(sql).rows().collect::<Vec<_>>());
    }
}

const SETUP: &[&str] = &[
    "CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT, n INTEGER NOT NULL DEFAULT 0, c VARCHAR(3), \
     big BIGINT)",
    "INSERT INTO t VALUES (1, 'a', 2147483647, 'abc', 9), (2, 'b', 0, NULL, NULL)",
    "INSERT INTO t SELECT i, 'v' || i, i, NULL, i FROM range(10, 3000) r(i)",
];

#[test]
fn a_read_by_key_with_its_value_written_in_reads_what_the_plan_does() {
    let pair = Pair::new(SETUP);
    for sql in [
        "SELECT * FROM t WHERE k = 1",
        "SELECT v AS x, k FROM t WHERE k = 2",
        "SELECT v FROM t WHERE 2047 = k",
        "SELECT v FROM t WHERE k = '2'",
        "SELECT v FROM t WHERE k = 99",
        "SELECT v FROM t WHERE k = NULL",
        "SELECT v FROM t WHERE k = -1",
        "SELECT v FROM t WHERE k = 5000000000",
        "SELECT k, v FROM t WHERE k >= 2990",
        "SELECT k, v FROM t WHERE k < 11",
    ] {
        pair.keyed(sql);
    }
    let read = pair.short.execute("SELECT v, n FROM t WHERE k = 1").expect("reads");
    assert!(read.origin(0).is_some_and(|origin| origin.column == 1), "{:?}", read.origin(0));
    assert!(read.origin(1).is_some_and(|origin| origin.column == 2), "{:?}", read.origin(1));
}

#[test]
fn a_write_by_key_with_its_values_written_in_leaves_the_table_as_the_plan_does() {
    let pair = Pair::new(SETUP);
    for sql in [
        "UPDATE t SET v = 'q' WHERE k = 10",
        "UPDATE t SET v = 'q' WHERE k = 42000",
        "UPDATE t SET n = n + 5 WHERE k = 11",
        "UPDATE t SET n = n + -1 WHERE k = 1",
        "UPDATE t SET n = n + 1 WHERE k = 1",
        "UPDATE t SET n = n - 3 WHERE k = 12",
        "UPDATE t SET n = '7' WHERE k = 13",
        "UPDATE t SET n = NULL WHERE k = 14",
        "UPDATE t SET n = 5000000000 WHERE k = 15",
        "UPDATE t SET big = 5000000000 WHERE k = 16",
        "UPDATE t SET big = -5 WHERE k = 17",
        "UPDATE t SET c = 'xy' WHERE k = 18",
        "UPDATE t SET c = 'abcd' WHERE k = 19",
        "UPDATE t SET v = true WHERE k = 20",
        "UPDATE t SET v = NULL, c = 'z' WHERE k = 21",
        "UPDATE t SET k = 1 WHERE k = 22",
        "DELETE FROM t WHERE k = 23",
        "DELETE FROM t WHERE k = 42000",
        "DELETE FROM t WHERE k = '24'",
    ] {
        pair.keyed(sql);
    }
    for (values, select) in [
        ("(4000, 'new')", "4000, 'new'"),
        ("(1, 'taken')", "1, 'taken'"),
        ("(NULL, 'no key')", "NULL, 'no key'"),
        ("(4001, 'a'), (4002, 'b')", "* FROM (VALUES (4001, 'a'), (4002, 'b'))"),
    ] {
        let sql = format!("INSERT INTO t (k, v) VALUES {values}");
        pair.both(&sql, &format!("INSERT INTO t (k, v) SELECT {select}"));
    }
    pair.all_same();
    for sql in ["SELECT count(*), sum(n), max(big) FROM t", "SELECT k FROM t WHERE c = 'xy'"] {
        let short: Vec<Vec<Value>> = pair.short.execute(sql).expect(sql).rows().collect();
        assert_eq!(short, pair.planned.execute(sql).expect(sql).rows().collect::<Vec<_>>());
    }
}

#[test]
fn an_upsert_with_its_values_written_in_leaves_the_table_as_the_plan_does() {
    let pair = Pair::new(SETUP);
    // A source of `SELECT` is a shape the short way leaves to the plan.
    for (values, rest) in [
        ("(5, 'five')", "DO UPDATE SET v = excluded.v"),
        ("(5, 'FIVE')", "DO UPDATE SET v = excluded.v"),
        ("(5, 'nope')", "DO NOTHING"),
        ("(1, 'one')", "DO UPDATE SET v = excluded.v"),
        ("(6, NULL)", "DO UPDATE SET v = excluded.v"),
        ("(6, 'six')", "DO UPDATE SET n = 1, c = 'ab'"),
        ("(6, 'six')", "DO UPDATE SET c = 'long'"),
    ] {
        let sql = format!("INSERT INTO t (k, v) VALUES {values} ON CONFLICT (k) {rest}");
        let select = values.trim_start_matches('(').trim_end_matches(')');
        let slow = format!("INSERT INTO t (k, v) SELECT {select} ON CONFLICT (k) {rest}");
        pair.both(&sql, &slow);
    }
    let sql = "INSERT INTO t (k, n) VALUES (5, 1) ON CONFLICT (k) DO UPDATE SET n = t.n + 10";
    let slow = "INSERT INTO t (k, n) SELECT 5, 1 ON CONFLICT (k) DO UPDATE SET n = t.n + 10";
    pair.both(sql, slow);
    pair.all_same();
}

#[test]
fn a_write_by_key_in_a_block_that_rolls_back_is_gone() {
    let pair = Pair::new(SETUP);
    for db in [&pair.short, &pair.planned] {
        db.execute("BEGIN").expect("begins");
    }
    pair.keyed("UPDATE t SET v = 'in block' WHERE k = 30");
    pair.keyed("DELETE FROM t WHERE k = 31");
    pair.keyed("SELECT v FROM t WHERE k = 30");
    pair.keyed("SELECT v FROM t WHERE k = 31");
    for db in [&pair.short, &pair.planned] {
        db.execute("ROLLBACK").expect("rolls back");
    }
    pair.keyed("SELECT v FROM t WHERE k = 30");
    pair.keyed("SELECT v FROM t WHERE k = 31");
    pair.all_same();
}

#[test]
fn a_write_to_a_table_with_a_trigger_fires_it() {
    let db = Database::new();
    for sql in [
        "CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT)",
        "CREATE TABLE b (id INTEGER)",
        "CREATE TRIGGER x AFTER INSERT ON a FOR EACH STATEMENT INSERT INTO b VALUES (1)",
        "CREATE TRIGGER y AFTER UPDATE ON a FOR EACH STATEMENT INSERT INTO b VALUES (2)",
        "CREATE TRIGGER z AFTER DELETE ON a FOR EACH STATEMENT INSERT INTO b VALUES (3)",
        "INSERT INTO a VALUES (1, 'one')",
        "UPDATE a SET v = 'uno' WHERE id = 1",
        "DELETE FROM a WHERE id = 1",
    ] {
        db.execute(sql).expect(sql);
    }
    let fired: Vec<Vec<Value>> =
        db.execute("SELECT id FROM b ORDER BY id").expect("reads").rows().collect();
    assert_eq!(fired, [[Value::Integer(1)], [Value::Integer(2)], [Value::Integer(3)]]);
}

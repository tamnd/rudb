//! A prepared `UPDATE ... WHERE key = ?` writes its row where the key finds it inside a
//! transaction too, and while other transactions are open, and has to do what the plan does
//! there: see and write the transaction's own rows, keep the write from everybody else until the
//! commit, take it back on a rollback, and fail a row another transaction holds in the same words.
//! Each check runs the same statements on two databases, one where the update can take the short
//! way and one where `AND true` after the key sends it through the plan, and compares the answers,
//! the errors and the rows each connection sees.

use std::path::PathBuf;

use rudb::{Connection, Database};
use rudb_common::Value;

const SETUP: &[&str] = &[
    "CREATE TABLE t (id BIGINT PRIMARY KEY, f0 VARCHAR, n BIGINT, i INTEGER NOT NULL, d DOUBLE)",
    "INSERT INTO t SELECT i, 'v' || i, i * 3, i::INTEGER, i / 2 FROM range(5000) r(i)",
];

const SET: &str = "UPDATE t SET f0 = ? WHERE id = ?";
const ADD: &str = "UPDATE t SET n = n + ? WHERE id = ?";
const DELETE: &str = "DELETE FROM t WHERE id = ?";

/// One database and two connections to it, `a` and `b`.
struct Side {
    db: Database,
    a: Connection,
    b: Connection,
}

impl Side {
    fn new(db: Database) -> Self {
        for sql in SETUP {
            db.execute(sql).expect(sql);
        }
        let (a, b) = (db.connect(), db.connect());
        Self { db, a, b }
    }

    fn on(&self, who: Who) -> &Connection {
        match who {
            Who::A => &self.a,
            Who::B => &self.b,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Who {
    A,
    B,
}

struct Pair {
    short: Side,
    planned: Side,
}

fn rows(result: &rudb::QueryResult) -> Vec<Vec<Value>> {
    result.rows().collect()
}

impl Pair {
    fn new(short: Database, planned: Database) -> Self {
        Self { short: Side::new(short), planned: Side::new(planned) }
    }

    /// Runs `update` with `values` as `who` on both, and checks the two agree. Says whether it
    /// worked.
    fn update(&self, who: Who, update: &str, values: &[Value]) -> bool {
        let short = self.short.on(who).prepare(update).expect("prepares").execute(values);
        let slow = format!("{update} AND true");
        let planned = self.planned.on(who).prepare(&slow).expect("prepares").execute(values);
        match (short, planned) {
            (Ok(short), Ok(planned)) => {
                assert_eq!(rows(&short), rows(&planned), "{who:?} {update} {values:?}");
                true
            }
            (Err(short), Err(planned)) => {
                assert_eq!(short.to_string(), planned.to_string(), "{who:?} {update} {values:?}");
                false
            }
            (short, planned) => panic!(
                "{who:?} {update} {values:?}: the short way said {short:?} and the plan \
                 {planned:?}"
            ),
        }
    }

    /// Runs `sql` as `who` on both, and checks the two agree. Says whether it worked.
    fn run(&self, who: Who, sql: &str) -> bool {
        let short = self.short.on(who).execute(sql).map_err(|error| error.to_string());
        let planned = self.planned.on(who).execute(sql).map_err(|error| error.to_string());
        assert_eq!(short.is_ok(), planned.is_ok(), "{who:?} {sql}: {short:?} {planned:?}");
        if let (Err(short), Err(planned)) = (&short, &planned) {
            assert_eq!(short, planned, "{who:?} {sql}");
        }
        short.is_ok()
    }

    /// What `who` sees of the table, which has to be the same on both, and what a connection with
    /// no transaction sees, which has to be too.
    fn same(&self, who: Who) {
        let all = "SELECT * FROM t ORDER BY id";
        let seen = |side: &Side| rows(&side.on(who).execute(all).expect("reads"));
        assert_eq!(seen(&self.short), seen(&self.planned), "{who:?}");
        let committed = |side: &Side| rows(&side.db.connect().execute(all).expect("reads"));
        assert_eq!(committed(&self.short), committed(&self.planned), "committed");
    }

    /// The `f0` and `n` of row `id` as `who` sees them, which has to be the same on both. Read on
    /// both, so a transaction takes its snapshot at the same point on each.
    fn row(&self, who: Who, id: i64) -> Vec<Vec<Value>> {
        let sql = format!("SELECT f0, n FROM t WHERE id = {id}");
        let short = rows(&self.short.on(who).execute(&sql).expect("reads"));
        let planned = rows(&self.planned.on(who).execute(&sql).expect("reads"));
        assert_eq!(short, planned, "{who:?} {id}");
        short
    }
}

fn text(value: &str) -> Value {
    Value::Varchar(value.into())
}

fn row_of(f0: &str, n: i64) -> Vec<Vec<Value>> {
    vec![vec![text(f0), Value::BigInt(n)]]
}

/// What a transaction does to its own rows, and the commit that lands it beside a write committed
/// in the meantime.
fn own_rows(pair: &Pair) {
    use Who::{A, B};
    pair.run(A, "BEGIN");
    assert_eq!(
        pair.short.a.prepare(SET).expect("prepares").explain(),
        "UpdateOne t(id) SET f0",
        "inside a transaction"
    );
    assert!(pair.update(A, SET, &[text("a1"), Value::BigInt(1)]));
    assert!(pair.update(A, ADD, &[Value::BigInt(5), Value::BigInt(2)]));
    assert!(pair.update(A, ADD, &[Value::BigInt(5), Value::BigInt(2)]));
    // A row the transaction added, one past a row it took out, and one that is not there.
    assert!(pair.run(A, "INSERT INTO t VALUES (9000, 'x', 1, 1, 1.0)"));
    assert!(pair.update(A, ADD, &[Value::BigInt(5), Value::BigInt(9000)]));
    assert!(pair.run(A, "DELETE FROM t WHERE id = 3"));
    assert!(pair.update(A, SET, &[text("a4000"), Value::BigInt(4000)]));
    assert!(pair.update(A, SET, &[text("none"), Value::BigInt(3)]));
    // The long way inside the same transaction, after the short one.
    assert!(pair.run(A, "UPDATE t SET n = n * 2 WHERE id = 1"));
    assert!(pair.update(A, ADD, &[Value::BigInt(1), Value::BigInt(1)]));
    pair.same(A);
    assert_eq!(pair.row(A, 1), row_of("a1", 7));
    assert_eq!(pair.row(A, 2), row_of("v2", 16));
    assert_eq!(pair.row(A, 9000), row_of("x", 6));
    assert_eq!(pair.row(B, 1), row_of("v1", 3));
    assert_eq!(pair.row(B, 9000), Vec::<Vec<Value>>::new());

    // Somebody else writes another row and commits first.
    assert!(pair.update(B, SET, &[text("b4500"), Value::BigInt(4500)]));
    assert!(pair.run(A, "COMMIT"));
    pair.same(A);
    pair.same(B);
    assert_eq!(pair.row(B, 1), row_of("a1", 7));
    assert_eq!(pair.row(B, 4000), row_of("a4000", 12000));
    assert_eq!(pair.row(B, 4500), row_of("b4500", 13500));
    assert_eq!(pair.row(B, 3), Vec::<Vec<Value>>::new());

    // A rollback takes the writes back.
    pair.run(A, "BEGIN");
    assert!(pair.update(A, SET, &[text("gone"), Value::BigInt(30)]));
    assert!(pair.update(A, ADD, &[Value::BigInt(100), Value::BigInt(31)]));
    assert_eq!(pair.row(A, 30), row_of("gone", 90));
    pair.run(A, "ROLLBACK");
    pair.same(A);
    assert_eq!(pair.row(A, 30), row_of("v30", 90));
    assert_eq!(pair.row(A, 31), row_of("v31", 93));
}

/// Rows another transaction holds or committed since the snapshot.
fn held_rows(pair: &Pair) {
    use Who::{A, B};
    for side in [&pair.short, &pair.planned] {
        for who in [&side.a, &side.b] {
            who.execute("SET lock_timeout = '0'").expect("sets");
        }
    }
    // Two transactions writing one row: the second fails and its transaction with it.
    pair.run(A, "BEGIN");
    assert!(pair.update(A, SET, &[text("a10"), Value::BigInt(10)]));
    pair.run(B, "BEGIN");
    assert!(pair.update(B, SET, &[text("b11"), Value::BigInt(11)]));
    assert!(!pair.update(B, SET, &[text("b10"), Value::BigInt(10)]));
    assert!(!pair.update(B, SET, &[text("b12"), Value::BigInt(12)]));
    pair.run(B, "ROLLBACK");
    // Without a transaction of its own, the row held is refused all the same.
    assert!(!pair.update(B, ADD, &[Value::BigInt(1), Value::BigInt(10)]));
    assert!(pair.update(B, ADD, &[Value::BigInt(1), Value::BigInt(13)]));
    pair.same(B);
    pair.run(A, "COMMIT");
    pair.same(A);
    assert_eq!(pair.row(B, 10), row_of("a10", 30));
    assert_eq!(pair.row(B, 13), row_of("v13", 40));

    // A write committed while a transaction is open, which the transaction does not see and
    // cannot write over.
    pair.run(A, "BEGIN");
    assert_eq!(pair.row(A, 20), row_of("v20", 60));
    for round in 0..20 {
        assert!(pair.update(B, ADD, &[Value::BigInt(1), Value::BigInt(20 + round)]));
    }
    assert_eq!(pair.row(A, 20), row_of("v20", 60));
    assert_eq!(pair.row(B, 20), row_of("v20", 61));
    pair.same(A);
    assert!(!pair.update(A, ADD, &[Value::BigInt(1), Value::BigInt(20)]));
    pair.run(A, "ROLLBACK");
    pair.same(A);
    assert_eq!(pair.row(A, 20), row_of("v20", 61));
}

/// A delete by key, which takes the row out where it is: a row there and one not, a key put back
/// after its row went, in a transaction and its rollback, and a row another transaction holds.
fn deleted_rows(pair: &Pair) {
    use Who::{A, B};
    let id = Value::BigInt;
    assert_eq!(pair.short.a.prepare(DELETE).expect("prepares").explain(), "DeleteOne t(id)");
    assert!(pair.update(A, DELETE, &[id(100)]));
    assert!(pair.update(A, DELETE, &[id(100)]));
    assert_eq!(pair.row(B, 100), Vec::<Vec<Value>>::new());
    assert!(!pair.run(A, "INSERT INTO t VALUES (101, 'x', 1, 1, 1.0)"));
    assert!(pair.run(A, "INSERT INTO t VALUES (100, 'back', 1, 1, 1.0)"));
    assert_eq!(pair.row(B, 100), row_of("back", 1));
    pair.same(A);

    pair.run(A, "BEGIN");
    assert!(pair.update(A, DELETE, &[id(200)]));
    assert!(pair.run(A, "INSERT INTO t VALUES (9100, 'y', 1, 1, 1.0)"));
    assert!(pair.update(A, DELETE, &[id(9100)]));
    assert!(pair.update(A, ADD, &[id(1), id(199)]));
    assert_eq!(pair.row(A, 200), Vec::<Vec<Value>>::new());
    assert_eq!(pair.row(B, 200), row_of("v200", 600));
    assert!(pair.update(B, DELETE, &[id(201)]));
    pair.same(A);
    assert!(pair.run(A, "COMMIT"));
    pair.same(B);
    assert_eq!(pair.row(B, 199), row_of("v199", 598));

    pair.run(A, "BEGIN");
    assert!(pair.update(A, DELETE, &[id(300)]));
    assert_eq!(pair.row(A, 300), Vec::<Vec<Value>>::new());
    pair.run(A, "ROLLBACK");
    assert_eq!(pair.row(A, 300), row_of("v300", 900));

    for side in [&pair.short, &pair.planned] {
        for who in [&side.a, &side.b] {
            who.execute("SET lock_timeout = '0'").expect("sets");
        }
    }
    pair.run(A, "BEGIN");
    assert!(pair.update(A, SET, &[text("a400"), id(400)]));
    assert!(pair.update(A, DELETE, &[id(500)]));
    assert!(!pair.update(B, DELETE, &[id(400)]));
    assert!(!pair.update(B, SET, &[text("b500"), id(500)]));
    assert!(pair.run(A, "COMMIT"));
    assert!(pair.update(B, DELETE, &[id(400)]));
    pair.same(B);
}

#[test]
fn a_write_by_key_inside_a_transaction_does_what_the_plan_does() {
    let pair = Pair::new(Database::new(), Database::new());
    own_rows(&pair);
}

#[test]
fn a_delete_by_key_does_what_the_plan_does() {
    let pair = Pair::new(Database::new(), Database::new());
    deleted_rows(&pair);
}

#[test]
fn a_write_by_key_meets_held_rows_the_way_the_plan_does() {
    let pair = Pair::new(Database::new(), Database::new());
    held_rows(&pair);
}

fn path(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("rudb-point-write-tx-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

#[test]
fn a_write_by_key_inside_a_transaction_is_logged_and_read_back() {
    let (short, planned) = (path("short"), path("planned"));
    let open =
        |path: &PathBuf| Database::open(path.to_str().expect("a UTF-8 path")).expect("opens");
    let pair = Pair::new(open(&short), open(&planned));
    pair.short.db.execute("CHECKPOINT").expect("checkpoints");
    pair.planned.db.execute("CHECKPOINT").expect("checkpoints");
    own_rows(&pair);
    held_rows(&pair);
    deleted_rows(&pair);
    let Pair { short: a, planned: b } = pair;
    std::mem::forget(a);
    std::mem::forget(b);
    let all = "SELECT * FROM t ORDER BY id";
    let (short_db, planned_db) = (open(&short), open(&planned));
    let seen = rows(&short_db.execute(all).expect("reads"));
    assert_eq!(seen, rows(&planned_db.execute(all).expect("reads")));
    assert_eq!(seen.len(), 4996);
    drop((short_db, planned_db));
    for path in [short, planned] {
        let _ = std::fs::remove_file(&path);
    }
}

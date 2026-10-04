//! A prepared `UPDATE ... WHERE key = ?` writes its row where the key finds it rather than through
//! the plan, and has to leave the table exactly as the plan would. Each check here runs the same
//! values through the statement on one database, where it can take the short way, and through the
//! same statement with `AND true` after the key on another, where it cannot, and compares the
//! answers, the errors and then every row of both tables.

use std::path::PathBuf;

use rudb::Database;
use rudb_common::Value;

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.execute(sql).expect(sql).rows().collect()
}

/// Two databases that start from the same statements, one written the short way and one through
/// the plan.
struct Pair {
    short: Database,
    planned: Database,
}

impl Pair {
    fn new(short: Database, planned: Database, setup: &[&str]) -> Self {
        for sql in setup {
            short.execute(sql).expect(sql);
            planned.execute(sql).expect(sql);
        }
        Self { short, planned }
    }

    /// Runs `update`, whose last condition is the key, on both, and checks they agree.
    fn update(&self, update: &str, values: &[Value]) {
        let short = self.short.prepare(update).expect("prepares").execute(values);
        let slow = format!("{update} AND true");
        let planned = self.planned.prepare(&slow).expect("prepares").execute(values);
        match (short, planned) {
            (Ok(short), Ok(planned)) => {
                assert_eq!(short.rows().collect::<Vec<_>>(), planned.rows().collect::<Vec<_>>());
            }
            (Err(short), Err(planned)) => {
                assert_eq!(short.to_string(), planned.to_string(), "{update} {values:?}");
            }
            (short, planned) => {
                panic!("{update} {values:?}: the short way said {short:?} and the plan {planned:?}")
            }
        }
    }

    /// Checks `sql` reads the same from both.
    fn same(&self, sql: &str) -> Vec<Vec<Value>> {
        let short = rows(&self.short, sql);
        assert_eq!(short, rows(&self.planned, sql), "{sql}");
        short
    }

    fn all_same(&self) {
        self.same("SELECT * FROM t ORDER BY id");
        self.same("SELECT count(*), count(f0), sum(n), min(n), max(n), min(f0), max(f0) FROM t");
    }
}

fn path(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("rudb-point-write-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

const SETUP: &[&str] = &[
    "CREATE TABLE t (id BIGINT PRIMARY KEY, f0 VARCHAR, n BIGINT, i INTEGER NOT NULL, d DOUBLE)",
    "INSERT INTO t SELECT i, 'v' || i, i * 3, i::INTEGER, i / 2 FROM range(5000) r(i)",
    "INSERT INTO t VALUES (5000, 'tail', 1, 1, 1.5), (5001, NULL, NULL, 2, NULL)",
];

fn writes(pair: &Pair) {
    let set = "UPDATE t SET f0 = ? WHERE id = ?";
    for id in [0, 1, 2047, 2048, 4999, 5000, 5001, 5002, -1] {
        pair.update(set, &[Value::Varchar(format!("u{id}")), Value::BigInt(id)]);
    }
    pair.update(set, &[Value::Null, Value::BigInt(7)]);
    pair.update(
        set,
        &[Value::Varchar("a long string that does not fit a view".into()), Value::Integer(8)],
    );
    pair.update(set, &[Value::Varchar("cast".into()), Value::Varchar("9".into())]);
    pair.update(set, &[Value::Varchar("none".into()), Value::Null]);
    pair.update(set, &[Value::Integer(5), Value::BigInt(10)]);
    pair.all_same();

    let add = "UPDATE t SET n = n + ? WHERE id = ?";
    for id in [11, 12, 4000, 5000, 5001] {
        pair.update(add, &[Value::BigInt(5), Value::BigInt(id)]);
        pair.update(add, &[Value::Integer(-2), Value::BigInt(id)]);
    }
    pair.update(add, &[Value::BigInt(i64::MAX), Value::BigInt(13)]);
    pair.update(add, &[Value::Null, Value::BigInt(14)]);
    pair.update("UPDATE t SET n = ? + n WHERE id = ?", &[Value::BigInt(1), Value::BigInt(15)]);
    pair.update("UPDATE t SET n = n - ? WHERE id = ?", &[Value::BigInt(100), Value::BigInt(16)]);
    pair.update(
        "UPDATE t SET i = i + ? WHERE id = ?",
        &[Value::Integer(i32::MAX), Value::BigInt(17)],
    );
    pair.update(
        "UPDATE t SET i = i + ? WHERE id = ?",
        &[Value::BigInt(1 << 40), Value::BigInt(17)],
    );
    pair.update("UPDATE t SET i = i - ? WHERE id = ?", &[Value::Integer(3), Value::BigInt(17)]);
    pair.update("UPDATE t SET i = ? WHERE id = ?", &[Value::Null, Value::BigInt(18)]);
    pair.update(
        "UPDATE t SET d = ?, n = ? WHERE id = ?",
        &[Value::Integer(4), Value::BigInt(1_000_000_000_000), Value::BigInt(19)],
    );
    pair.update(
        "UPDATE t AS x SET f0 = ? WHERE x.id = ?",
        &[Value::Varchar("x".into()), Value::BigInt(20)],
    );
    // Shapes the short way leaves to the plan, which have to come out the same all the same.
    pair.update("UPDATE t SET id = ? WHERE id = ?", &[Value::BigInt(-5), Value::BigInt(21)]);
    pair.update("UPDATE t SET id = ? WHERE id = ?", &[Value::BigInt(22), Value::BigInt(23)]);
    pair.update(
        "UPDATE t SET f0 = ?, f0 = ? WHERE id = ?",
        &[Value::Varchar("a".into()), Value::Varchar("b".into()), Value::BigInt(24)],
    );
    pair.update(
        "UPDATE t SET f0 = ? WHERE f0 = ?",
        &[Value::Varchar("c".into()), Value::Varchar("v25".into())],
    );
    pair.all_same();

    // What reads the table afterwards: the key, the zones and the counts.
    for id in [0, 7, 8, 13, 19, 5001, -5] {
        pair.same(&format!("SELECT * FROM t WHERE id = {id}"));
    }
    let lookup = "SELECT f0, n FROM t WHERE id = ?";
    for db in [&pair.short, &pair.planned] {
        let found =
            db.prepare(lookup).expect("prepares").execute(&[Value::BigInt(19)]).expect("runs");
        assert_eq!(
            found.rows().collect::<Vec<_>>(),
            vec![vec![Value::Varchar("v19".into()), Value::BigInt(1_000_000_000_000)]]
        );
    }
    pair.same("SELECT count(*) FROM t WHERE n > 100000000000");
    pair.same("SELECT count(*) FROM t WHERE n < 0");
    pair.same("SELECT count(*) FROM t WHERE f0 IS NULL");
    pair.same("SELECT count(DISTINCT f0), approx_count_distinct(f0) FROM t");
    pair.same("SELECT id FROM t WHERE f0 = 'u2048'");
    pair.same("SELECT count(*) FROM t WHERE d = 4");
}

#[test]
fn a_write_by_key_leaves_the_table_as_the_plan_does() {
    let pair = Pair::new(Database::new(), Database::new(), SETUP);
    writes(&pair);
}

#[test]
fn a_write_by_key_to_a_file_is_logged_and_read_back() {
    let (short, planned) = (path("short"), path("planned"));
    let open =
        |path: &PathBuf| Database::open(path.to_str().expect("a UTF-8 path")).expect("opens");
    let pair = Pair::new(open(&short), open(&planned), SETUP);
    pair.short.execute("CHECKPOINT").expect("checkpoints");
    pair.planned.execute("CHECKPOINT").expect("checkpoints");
    writes(&pair);
    drop(pair);
    // Both again from the file and the log, and then written again past a checkpoint.
    let pair = Pair { short: open(&short), planned: open(&planned) };
    pair.all_same();
    pair.update(
        "UPDATE t SET f0 = ? WHERE id = ?",
        &[Value::Varchar("again".into()), Value::BigInt(30)],
    );
    pair.short.execute("CHECKPOINT").expect("checkpoints");
    pair.planned.execute("CHECKPOINT").expect("checkpoints");
    pair.update("UPDATE t SET n = n + ? WHERE id = ?", &[Value::BigInt(1), Value::BigInt(30)]);
    drop(pair);
    let pair = Pair { short: open(&short), planned: open(&planned) };
    pair.all_same();
}

#[test]
fn a_write_by_key_while_a_transaction_is_open_is_one_it_can_see_the_right_way() {
    let db = Database::new();
    for sql in SETUP {
        db.execute(sql).expect(sql);
    }
    let update = db.prepare("UPDATE t SET f0 = ? WHERE id = ?").expect("prepares");
    let reader = db.connect();
    reader.execute("BEGIN").expect("begins");
    assert_eq!(
        rows(&db, "SELECT f0 FROM t WHERE id = 3"),
        reader.execute("SELECT f0 FROM t WHERE id = 3").expect("reads").rows().collect::<Vec<_>>()
    );
    update.execute(&[Value::Varchar("new".into()), Value::BigInt(3)]).expect("updates");
    let seen = reader.execute("SELECT f0 FROM t WHERE id = 3").expect("reads");
    assert_eq!(seen.rows().collect::<Vec<_>>(), vec![vec![Value::Varchar("v3".into())]]);
    reader.execute("COMMIT").expect("commits");
    assert_eq!(
        rows(&db, "SELECT f0 FROM t WHERE id = 3"),
        vec![vec![Value::Varchar("new".into())]]
    );
    // And with nobody else open, the short way, many times over one row and many rows.
    for round in 0..1000_i64 {
        update
            .execute(&[Value::Varchar(format!("r{round}")), Value::BigInt(round % 50)])
            .expect("updates");
    }
    assert_eq!(
        rows(&db, "SELECT f0 FROM t WHERE id = 49"),
        vec![vec![Value::Varchar("r999".into())]]
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FROM t WHERE f0 LIKE 'r%'"),
        vec![vec![Value::BigInt(50)]]
    );
}

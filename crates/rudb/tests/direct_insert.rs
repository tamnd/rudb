//! A prepared one row `INSERT ... VALUES` goes straight into the table when nothing about the table
//! needs the plan, and has to land exactly what the plan would have. Each check here runs the same
//! values through `INSERT INTO a VALUES (?, ...)`, which can take the short way, and through
//! `INSERT INTO b SELECT ?, ...`, which cannot, and compares the two tables and the two errors.

use std::path::PathBuf;

use rudb::Database;
use rudb_common::Value;

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = db.query(sql).expect("the query runs");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

/// Two tables made by `columns`, the one the short way writes and the one the plan writes.
fn twins(db: &Database, columns: &str) {
    db.execute(&format!("CREATE TABLE a ({columns})")).expect("creates a");
    db.execute(&format!("CREATE TABLE b ({columns})")).expect("creates b");
}

/// Runs `values` through both statements and checks they agree on whether it worked, and on what
/// the error said when `same_text` asks.
fn both(db: &Database, list: &str, values: &[Value], same_text: bool) {
    let marks = vec!["?"; values.len()].join(", ");
    let direct = db.prepare(&format!("INSERT INTO a {list} VALUES ({marks})")).expect("prepares");
    let planned = db.prepare(&format!("INSERT INTO b {list} SELECT {marks}")).expect("prepares");
    let left = direct.execute(values);
    let right = planned.execute(values);
    match (&left, &right) {
        (Ok(left), Ok(right)) => {
            assert_eq!(left.value_at(0, 0), right.value_at(0, 0), "{values:?}");
        }
        (Err(left), Err(right)) => {
            if same_text {
                // The two name their own tables, and otherwise have to say the same thing.
                let left = left.to_string().replace("\"a\"", "\"b\"").replace(" a.", " b.");
                assert_eq!(left, right.to_string(), "{values:?}");
            }
        }
        _ => panic!("{values:?}: the short way said {left:?} and the plan said {right:?}"),
    }
}

fn same_tables(db: &Database) {
    assert_eq!(rows(db, "SELECT * FROM a"), rows(db, "SELECT * FROM b"));
}

#[test]
fn values_of_every_kind_land_as_the_plan_lands_them() {
    let db = Database::new();
    twins(&db, "id BIGINT, name VARCHAR, price DOUBLE, qty INTEGER");
    let cases = [
        vec![Value::BigInt(1), Value::Varchar("one".into()), Value::Double(1.5), Value::Integer(2)],
        vec![Value::Integer(2), Value::Null, Value::Integer(3), Value::BigInt(4)],
        vec![Value::SmallInt(3), Value::Varchar("x".into()), Value::Float(0.25), Value::TinyInt(5)],
        vec![Value::BigInt(4), Value::Integer(9), Value::Varchar("2.5".into()), Value::Double(6.6)],
        vec![Value::BigInt(5), Value::Null, Value::Null, Value::BigInt(i64::MAX)],
        vec![Value::Varchar("6".into()), Value::Null, Value::Null, Value::Varchar("seven".into())],
        vec![
            Value::BigInt(i64::MIN),
            Value::Varchar(String::new()),
            Value::Double(1e300),
            Value::Null,
        ],
    ];
    for values in &cases {
        both(&db, "", values, false);
    }
    same_tables(&db);
    let got = rows(&db, "SELECT count(*), min(id), max(qty) FROM a");
    assert_eq!(got, rows(&db, "SELECT count(*), min(id), max(qty) FROM b"));
}

#[test]
fn a_column_list_leaves_nulls_and_defaults_behind() {
    let db = Database::new();
    db.execute("CREATE SEQUENCE s").expect("creates");
    twins(&db, "id INTEGER, note VARCHAR, day DATE");
    for id in 0..5 {
        both(&db, "(note, id)", &[Value::Varchar(format!("n{id}")), Value::Integer(id)], true);
    }
    both(&db, "(id, id)", &[Value::Integer(1), Value::Integer(2)], true);
    both(&db, "(nope)", &[Value::Integer(1)], true);
    both(&db, "(id)", &[Value::Integer(1), Value::Integer(2)], true);
    same_tables(&db);

    db.execute("DROP TABLE a").expect("drops");
    db.execute("DROP TABLE b").expect("drops");
    twins(&db, "id INTEGER DEFAULT nextval('s'), note VARCHAR DEFAULT 'none'");
    for id in 0..3 {
        both(&db, "(note)", &[Value::Varchar(format!("n{id}"))], true);
    }
    assert_eq!(rows(&db, "SELECT id, note FROM a ORDER BY id")[2][0], Value::Integer(5));
    assert_eq!(rows(&db, "SELECT note FROM a"), rows(&db, "SELECT note FROM b"));
}

#[test]
fn constraints_refuse_in_the_plans_words() {
    let db = Database::new();
    twins(&db, "id INTEGER PRIMARY KEY, qty INTEGER NOT NULL CHECK (qty > 0)");
    both(&db, "", &[Value::Integer(1), Value::Integer(1)], true);
    both(&db, "", &[Value::Integer(1), Value::Integer(2)], true);
    both(&db, "", &[Value::Integer(2), Value::Null], true);
    both(&db, "", &[Value::Integer(3), Value::Integer(-1)], true);
    same_tables(&db);

    let db = Database::new();
    twins(&db, "id INTEGER NOT NULL, small TINYINT");
    both(&db, "", &[Value::Null, Value::Integer(1)], true);
    both(&db, "", &[Value::Integer(1), Value::Integer(1000)], true);
    both(&db, "", &[Value::Integer(2), Value::Integer(100)], true);
    same_tables(&db);
}

#[test]
fn a_view_or_a_missing_table_is_the_plans_error() {
    let db = Database::new();
    db.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    db.execute("CREATE VIEW v AS SELECT a FROM t").expect("creates");
    for name in ["v", "missing"] {
        let prepared = db.prepare(&format!("INSERT INTO {name} VALUES (?)")).expect("prepares");
        let short = prepared.execute(&[Value::Integer(1)]).expect_err("refused");
        let long = db.execute(&format!("INSERT INTO {name} VALUES (1)")).expect_err("refused");
        assert_eq!(short.to_string(), long.to_string());
    }
}

#[test]
fn a_rollback_takes_the_rows_back() {
    let db = Database::new();
    db.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").expect("creates");
    let insert = db.prepare("INSERT INTO t VALUES (?, ?)").expect("prepares");
    insert.execute(&[Value::Integer(0), Value::Null]).expect("inserts");
    db.execute("BEGIN").expect("begins");
    for a in 1..100 {
        insert.execute(&[Value::Integer(a), Value::Varchar(format!("r{a}"))]).expect("inserts");
    }
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], Value::BigInt(100));
    db.execute("ROLLBACK").expect("rolls back");
    assert_eq!(rows(&db, "SELECT count(*) FROM t")[0][0], Value::BigInt(1));

    db.execute("BEGIN TRANSACTION READ ONLY").expect("begins");
    let error = insert.execute(&[Value::Integer(1), Value::Null]).expect_err("read only");
    assert!(error.to_string().contains("read-only mode"), "{error}");
    db.execute("ROLLBACK").expect("rolls back");
}

#[test]
fn a_table_changed_between_executions_is_found_again() {
    let db = Database::new();
    db.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").expect("creates");
    let insert = db.prepare("INSERT INTO t (b, a) VALUES (?, ?)").expect("prepares");
    insert.execute(&[Value::Varchar("x".into()), Value::Integer(1)]).expect("inserts");
    insert.execute(&[Value::Varchar("y".into()), Value::Integer(2)]).expect("inserts");

    // Recreated with the columns the other way round, the list has to land by name again.
    db.execute("DROP TABLE t").expect("drops");
    db.execute("CREATE TABLE t (b VARCHAR, c DOUBLE, a INTEGER)").expect("creates");
    insert.execute(&[Value::Varchar("z".into()), Value::Integer(3)]).expect("inserts");
    assert_eq!(
        rows(&db, "SELECT * FROM t"),
        vec![vec![Value::Varchar("z".into()), Value::Null, Value::Integer(3)]]
    );

    // A default the list leaves out, or a constraint, sends the row the long way.
    db.execute("ALTER TABLE t ALTER COLUMN c SET DEFAULT 2.5").expect("alters");
    insert.execute(&[Value::Varchar("w".into()), Value::Integer(4)]).expect("inserts");
    assert_eq!(rows(&db, "SELECT c FROM t WHERE a = 4"), vec![vec![Value::Double(2.5)]]);

    // A table that shadows the name in `temp` wins from then on.
    db.execute("CREATE TEMPORARY TABLE t (a INTEGER, b VARCHAR)").expect("creates");
    insert.execute(&[Value::Varchar("v".into()), Value::Integer(5)]).expect("inserts");
    assert_eq!(rows(&db, "SELECT a FROM t"), vec![vec![Value::Integer(5)]]);
    db.execute("DROP TABLE temp.t").expect("drops");
    assert_eq!(rows(&db, "SELECT count(*) FROM t"), vec![vec![Value::BigInt(2)]]);
    db.execute("DROP TABLE t").expect("drops");
    let error = insert.execute(&[Value::Varchar("u".into()), Value::Integer(6)]).expect_err("gone");
    assert!(error.to_string().contains("does not exist"), "{error}");
}

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-direct-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

#[test]
fn rows_put_in_the_short_way_survive_a_crash() {
    let path = path("crash");
    let open = || Database::open(path.to_str().expect("a UTF-8 path")).expect("opens");
    let db = open();
    db.execute("CREATE TABLE t (id BIGINT, name VARCHAR, price DOUBLE)").expect("creates");
    let insert = db.prepare("INSERT INTO t VALUES (?, ?, ?)").expect("prepares");
    for id in 0..600_i64 {
        let values = [Value::BigInt(id), Value::Varchar(format!("n{id}")), Value::Integer(1)];
        insert.execute(&values).expect("inserts");
    }
    db.execute("DELETE FROM t WHERE id % 2 = 1").expect("deletes");
    insert.execute(&[Value::BigInt(-1), Value::Null, Value::Null]).expect("inserts");
    drop(insert);
    std::mem::forget(db);

    let db = open();
    let got = rows(&db, "SELECT count(*), sum(id), max(name), sum(price) FROM t");
    assert_eq!(got[0][0], Value::BigInt(301));
    assert_eq!(got[0][1], Value::HugeInt((0..600).step_by(2).sum::<i128>() - 1));
    assert_eq!(got[0][2], Value::Varchar("n98".into()));
    assert_eq!(got[0][3], Value::Double(300.0));
    assert_eq!(rows(&db, "SELECT id FROM t WHERE name IS NULL"), vec![vec![Value::BigInt(-1)]]);
    db.close().expect("closes");
    let _ = std::fs::remove_file(&path);
}

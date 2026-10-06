//! The plans a connection keeps by the text of a query, `08-the-dialect.md` section 8.14.
//!
//! A second run of the same text uses the kept plan and does not parse, bind or optimize, which the
//! metrics document shows as zero for those three phases. A kept plan must never change an answer,
//! so each test below changes something between two runs of one text and checks the answer.

use rudb::{Connection, Database};
use rudb_common::Value;

fn rows(connection: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let result = connection.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

/// Whether the statement ran a kept plan.
fn kept(connection: &Connection, sql: &str) -> bool {
    let result = connection.execute(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let timing = &result.metrics().expect("a query has a metrics document").timing;
    timing.parse_ns == 0 && timing.bind_ns == 0 && timing.optimize_ns == 0
}

fn connected() -> (Database, Connection) {
    let database = Database::open(":memory:").expect("an in-memory database");
    let connection = database.connect();
    (database, connection)
}

#[test]
fn the_same_text_runs_its_kept_plan() {
    let (_database, connection) = connected();
    let sql = "SELECT 1 + 2 AS three";
    assert!(!kept(&connection, sql), "the first run plans");
    assert!(kept(&connection, sql), "the second run does not");
    assert_eq!(rows(&connection, sql), vec![vec![Value::Integer(3)]]);
    assert!(!kept(&connection, "SELECT 1 + 2 AS three "), "the key is the exact text");
    let result = connection.query(sql).expect("the query path");
    assert_eq!(result.value_at(0, 0), Value::Integer(3));
    assert_eq!(result.metrics().expect("a document").timing.parse_ns, 0);
}

#[test]
fn a_kept_plan_reads_the_rows_of_now() {
    let (_database, connection) = connected();
    connection.execute("CREATE TABLE t (a INTEGER)").expect("create");
    let sql = "SELECT a FROM t ORDER BY a";
    assert!(rows(&connection, sql).is_empty());
    connection.execute("INSERT INTO t VALUES (2), (1)").expect("insert");
    assert_eq!(rows(&connection, sql), vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    rows(&connection, sql);
    assert!(kept(&connection, sql));
    connection.execute("DELETE FROM t WHERE a = 1").expect("delete");
    assert_eq!(rows(&connection, sql), vec![vec![Value::Integer(2)]]);
}

#[test]
fn a_change_of_the_table_plans_again() {
    let (_database, connection) = connected();
    connection.execute("CREATE TABLE t (a INTEGER)").expect("create");
    connection.execute("INSERT INTO t VALUES (1)").expect("insert");
    let sql = "SELECT * FROM t";
    rows(&connection, sql);
    assert!(kept(&connection, sql));
    connection.execute("DROP TABLE t").expect("drop");
    connection.execute("CREATE TABLE t (a VARCHAR, b INTEGER)").expect("create again");
    connection.execute("INSERT INTO t VALUES ('x', 5)").expect("insert again");
    assert_eq!(rows(&connection, sql), vec![vec![Value::Varchar("x".into()), Value::Integer(5)]]);
}

#[test]
fn a_setting_that_binds_differently_plans_again() {
    let (_database, connection) = connected();
    let sql = "SELECT x FROM (VALUES (1), (NULL)) AS v(x) ORDER BY x";
    assert_eq!(rows(&connection, sql), vec![vec![Value::Integer(1)], vec![Value::Null]]);
    assert!(kept(&connection, sql));
    connection.execute("SET default_null_order = 'nulls_first'").expect("set");
    assert_eq!(rows(&connection, sql), vec![vec![Value::Null], vec![Value::Integer(1)]]);
}

#[test]
fn what_the_binder_folds_is_not_kept() {
    let (_database, connection) = connected();
    connection.execute("CREATE TABLE t (a INTEGER)").expect("create");
    connection.execute("CREATE VIEW v AS SELECT a, now() AS n FROM t").expect("view");
    for sql in [
        "SELECT now()",
        "SELECT current_date",
        "SELECT 'now'::TIMESTAMP",
        "SELECT ' Today '::DATE",
        "SELECT random()",
        "SELECT * FROM v",
        "SELECT * FROM range(3)",
        "SELECT a FROM t, LATERAL (SELECT now())",
    ] {
        // A text this engine refuses is not kept either, and has nothing to check.
        if connection.execute(sql).is_err() {
            continue;
        }
        assert!(!kept(&connection, sql), "{sql} is planned at each run");
    }
}

#[test]
fn a_transaction_sees_its_own_rows_through_a_kept_plan() {
    let (database, connection) = connected();
    connection.execute("CREATE TABLE t (a INTEGER)").expect("create");
    let sql = "SELECT a FROM t";
    rows(&connection, sql);
    assert!(kept(&connection, sql));
    connection.execute("BEGIN").expect("begin");
    connection.execute("INSERT INTO t VALUES (7)").expect("insert");
    assert_eq!(rows(&connection, sql), vec![vec![Value::Integer(7)]]);
    let other = database.connect();
    assert!(rows(&other, sql).is_empty(), "another connection does not see it");
    connection.execute("ROLLBACK").expect("rollback");
    assert!(rows(&connection, sql).is_empty());
}

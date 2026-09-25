//! `rudb_statement_metrics()` says what each phase of a statement cost, after it ran.
//!
//! Milestone C0 asks for parse, bind, rewrite and optimize to be timed for every statement and to be
//! readable from SQL. These tests run a statement and then read it back out of the table, and they
//! find their own statement by its text, because the ring belongs to the process and the tests in
//! this binary run at the same time as each other.
//!
//! No test asserts a duration. A threshold in nanoseconds would be a threshold about the machine.
//! What is asserted is that the clocks moved, that the four frontend phases add up to the frontend
//! and that the frontend and the rest fit inside the total.

use rudb::Database;
use rudb_common::Value;

/// The row the ring kept for the newest statement whose text is `sql`, as numbers by column name.
fn kept(connection: &rudb::Connection, sql: &str) -> Vec<(String, i64)> {
    let result = connection
        .query("SELECT * FROM rudb_statement_metrics() ORDER BY id DESC")
        .expect("reads the ring");
    let names = result.names().to_vec();
    let row = result
        .rows()
        .find(|row| row[1] == Value::Varchar(sql.to_owned()))
        .unwrap_or_else(|| panic!("{sql} was not kept"));
    names
        .into_iter()
        .zip(row)
        .filter_map(|(name, value)| match value {
            Value::BigInt(number) => Some((name, number)),
            _ => None,
        })
        .collect()
}

fn column(row: &[(String, i64)], name: &str) -> i64 {
    row.iter().find(|(named, _)| named == name).map(|(_, value)| *value).expect(name)
}

#[test]
fn a_query_leaves_its_four_frontend_phases() {
    let database = Database::new();
    let connection = database.connect();
    connection
        .execute("CREATE TABLE phases AS SELECT i AS k, i * 2 AS v FROM range(0, 1000) AS r(i)")
        .expect("builds the table");
    let sql = "SELECT k, SUM(v) FROM phases WHERE k > 10 AND 1 = 1 GROUP BY k ORDER BY 2 LIMIT 5";
    connection.query(sql).expect("runs the query");
    let row = kept(&connection, sql);
    // Not cpu_ns: a thread's CPU clock on Windows ticks in steps longer than this query takes, so
    // it can read nothing there while the wall clocks below cannot.
    for phase in ["parse_ns", "bind_ns", "rewrite_ns", "optimize_ns", "execute_ns"] {
        assert!(column(&row, phase) > 0, "{phase} was charged nothing: {row:?}");
    }
    assert!(column(&row, "cpu_ns") >= 0, "{row:?}");
    let frontend = column(&row, "parse_ns")
        + column(&row, "bind_ns")
        + column(&row, "rewrite_ns")
        + column(&row, "optimize_ns");
    assert_eq!(column(&row, "frontend_ns"), frontend);
    let rest = column(&row, "physical_ns") + column(&row, "execute_ns");
    assert_eq!(column(&row, "total_ns"), frontend + rest, "{row:?}");
}

#[test]
fn a_statement_that_runs_no_plan_is_kept_too() {
    let database = Database::new();
    let connection = database.connect();
    let sql = "CREATE TABLE kept_without_a_plan (a INTEGER)";
    connection.execute(sql).expect("creates the table");
    let row = kept(&connection, sql);
    assert!(column(&row, "parse_ns") > 0, "{row:?}");
    assert_eq!(column(&row, "rewrite_ns") + column(&row, "optimize_ns"), 0, "{row:?}");
    assert!(column(&row, "total_ns") >= column(&row, "frontend_ns"), "{row:?}");
}

#[test]
fn the_document_carries_the_rewrites_inside_the_optimizer() {
    let database = Database::new();
    let connection = database.connect();
    let result = connection
        .query("SELECT x FROM (VALUES (1), (2), (3)) t(x) WHERE x > 1 AND 1 = 1")
        .expect("runs the query");
    let timing = &result.metrics().expect("a query that ran has metrics").timing;
    assert!(timing.rewrite_ns > 0, "the rewrites were charged nothing");
    assert!(timing.rewrite_ns <= timing.optimize_ns, "{timing:?}");
    let json = result.metrics().expect("measured").render();
    assert!(json.contains("\"rewrite_ns\""), "{json}");
}

#[test]
fn explain_analyze_prints_the_phases() {
    let database = Database::new();
    let connection = database.connect();
    let result = connection
        .query("EXPLAIN ANALYZE SELECT x FROM (VALUES (1), (2)) t(x) WHERE x > 1")
        .expect("explains");
    let Value::Varchar(text) = result.value_at(0, 1) else { panic!("the plan is text") };
    assert!(text.contains(" parsing, "), "{text}");
    assert!(text.contains(" rewriting, "), "{text}");
    assert!(text.contains(" optimizing"), "{text}");
}

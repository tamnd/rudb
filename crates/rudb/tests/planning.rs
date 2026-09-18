//! A query says what it spent planning, and the total says planning is part of it.
//!
//! Parsing, binding and optimizing happen on the statement path, above the function that makes the
//! metrics document, so until this landed those three fields were zero in every document the engine
//! ever wrote and `total_ns` was the physical build plus the run. Planning was the one cost of a
//! query that nothing could see.
//!
//! Milestone E1 asks for a column and an assertion about exactly that, and states the failure it is
//! guarding against plainly: a query that plans for four hundred milliseconds and runs for two
//! hundred is a query the optimizer made slower. Nobody finds that out by accident, because nobody
//! profiles the planner, and an optimizer only ever gets added to. The pass that pays for itself on
//! a scan of ten million rows does not pay for itself on a point lookup, and the arithmetic that
//! says so needs a number on both sides.
//!
//! These tests assert that the clock moved and that the total includes it, rather than asserting
//! any particular duration. A threshold in nanoseconds here would be a threshold about the machine
//! the test happened to run on. The budget that is asserted per query lives in the harness, in
//! `baselines/planning.txt` in tamnd/rudb-bench, where a record carries the machine it came from.

use rudb::Database;
use rudb_common::Value;
use rudb_metrics::Document;

/// Runs one query on a fresh database with one small table, and hands back what it reported.
fn measured(sql: &str) -> Document {
    let database = Database::new();
    let connection = database.connect();
    connection
        .execute("CREATE TABLE t AS SELECT i AS k, i * 2 AS v FROM range(0, 1000) AS r(i)")
        .expect("builds the table");
    let result = connection.query(sql).expect("runs the query");
    result.metrics().expect("a query that ran has metrics").clone()
}

#[test]
fn a_query_says_how_long_it_spent_parsing() {
    let timing = measured("SELECT k, SUM(v) FROM t WHERE k > 10 GROUP BY k").timing;
    assert!(timing.parse_ns > 0, "the parse was charged nothing");
}

#[test]
fn a_query_says_how_long_it_spent_binding() {
    let timing = measured("SELECT k, SUM(v) FROM t WHERE k > 10 GROUP BY k").timing;
    assert!(timing.bind_ns > 0, "the bind was charged nothing");
}

/// The one this milestone is actually about. An optimizer whose cost is invisible is an optimizer
/// nobody can tell has become too expensive.
#[test]
fn a_query_says_how_long_it_spent_being_optimized() {
    let timing =
        measured("SELECT k, SUM(v) FROM t WHERE k > 10 GROUP BY k ORDER BY 2 LIMIT 5").timing;
    assert!(timing.optimize_ns > 0, "the optimizer was charged nothing");
}

/// The total was the build plus the run, so planning could grow without any number moving. That is
/// the specific way this was broken and it is worth its own test rather than an assertion inside
/// another one.
#[test]
fn the_total_is_every_phase_and_not_just_the_ones_after_planning() {
    let timing = measured("SELECT k, SUM(v) FROM t GROUP BY k").timing;
    let phases = timing.parse_ns
        + timing.bind_ns
        + timing.optimize_ns
        + timing.physical_ns
        + timing.execute_ns;
    assert_eq!(timing.total_ns, phases);
    assert!(
        timing.total_ns > timing.physical_ns + timing.execute_ns,
        "the total is the same as it was before the planner had a clock on it"
    );
}

/// A statement prepared once and run a thousand times parsed once. Charging the parse to every
/// execution would report the same work a thousand times, and a budget asserted against that number
/// would be a budget against an invented one.
#[test]
fn a_prepared_statement_does_not_charge_its_execution_for_a_parse_it_did_not_do() {
    let database = Database::new();
    let connection = database.connect();
    connection
        .execute("CREATE TABLE t AS SELECT i AS k FROM range(0, 100) AS r(i)")
        .expect("builds the table");
    let prepared = connection.prepare("SELECT k FROM t WHERE k > ?").expect("prepares");
    let result = prepared.execute(&[Value::Integer(10)]).expect("runs the prepared statement");
    let timing = result.metrics().expect("a query that ran has metrics").timing.clone();
    assert_eq!(timing.parse_ns, 0, "an execution was charged for a parse that happened at PREPARE");
    assert!(timing.bind_ns > 0, "the bind is per execution and was charged nothing");
}

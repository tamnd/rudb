//! `EXPLAIN`, end to end through the parser, the binder, the optimizer and back out as a result set.
//!
//! These are about the statement being reachable and answering with the plan that would have run.
//! What the estimate for a given shape is belongs in `rudb-opt`'s own tests, and what the plan text
//! looks like belongs in `rudb-plan`'s. What is here is the join between the three.

use rudb::Database;
use rudb_common::{LogicalType, Value};

/// The explain text for a query against a database with one table of `rows` rows.
fn explained(database: &Database, sql: &str) -> String {
    let result = database.query(sql).expect("the explain ran");
    assert_eq!(result.names(), ["explain_key", "explain_value"], "the shape DuckDB clients expect");
    assert_eq!(result.types(), [LogicalType::Varchar, LogicalType::Varchar]);
    assert_eq!(result.len(), 1, "the whole tree is one value rather than one row per operator");
    match result.value_at(0, 1) {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    }
}

/// A database with a table of the given size, built without a per row `INSERT`.
fn with_rows(count: usize) -> Database {
    let database = Database::new();
    database.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").expect("creates");
    database
        .execute(&format!("INSERT INTO t SELECT r::INTEGER, 'x' FROM range({count}) AS s(r)"))
        .expect("inserts");
    database
}

#[test]
fn explain_answers_with_the_plan_and_an_estimate_on_every_line() {
    let database = with_rows(1000);
    let text = explained(&database, "EXPLAIN SELECT a FROM t WHERE a > 5");
    assert!(text.contains("Get memory.main.t"), "{text}");
    // The scan knows its size because the catalog does, and the filter is a fifth of it.
    assert!(text.contains("[~1000 rows]"), "{text}");
    assert!(text.contains("[~200 rows]"), "{text}");
    for line in text.lines() {
        assert!(line.ends_with(" rows]"), "a line with no estimate on it: {line}");
    }
}

#[test]
fn the_plan_explain_shows_is_the_plan_that_would_have_run() {
    // Not a plan built differently because somebody asked to see it. The optimizer runs over it
    // with the same context, so a pushed down filter is pushed down here too, and an explain that
    // showed the plan before the passes would be showing a plan nothing executes.
    let database = with_rows(100);
    let text = explained(&database, "EXPLAIN SELECT a FROM t WHERE a > 5");
    let ran = database.plan("SELECT a FROM t WHERE a > 5").expect("plans");
    let without_estimates: Vec<String> = text
        .lines()
        .map(|line| line.rsplit_once("  [").map_or(line, |(head, _)| head).to_owned())
        .collect();
    assert_eq!(without_estimates.join("\n"), ran.trim_end(), "{text}\n---\n{ran}");
}

#[test]
fn an_estimate_nobody_can_make_says_so_rather_than_saying_zero() {
    // `range` is a table function and a table function is an open door. Saying nothing here would
    // read as an empty relation, which is the one reading that would be actively misleading.
    let database = Database::new();
    let text = explained(&database, "EXPLAIN SELECT * FROM range(10)");
    assert!(text.contains("rows unknown"), "{text}");
}

#[test]
fn an_ungrouped_count_is_one_row_over_a_table_of_any_size() {
    let database = with_rows(5000);
    let text = explained(&database, "EXPLAIN SELECT count(*) FROM t");
    assert!(text.contains("[~1 rows]"), "{text}");
    assert!(text.contains("[~5000 rows]"), "{text}");
}

#[test]
fn explain_analyze_is_refused_rather_than_answered_with_a_plan_and_no_timings() {
    // A plan is not what was asked for. Per operator timing over a query that actually ran is, and
    // answering the wrong question quietly is worse than saying the feature is not here.
    let database = with_rows(10);
    let error = database.query("EXPLAIN ANALYZE SELECT a FROM t").expect_err("not yet");
    assert_eq!(error.code().duckdb_name(), "Not implemented Error");
    // Named, so that the refusal cannot pass for whatever error happens to come out of this path.
    assert!(error.to_string().contains("ANALYZE"), "{error}");
}

#[test]
fn explaining_something_that_is_not_a_query_is_refused() {
    // Each refusal names the statement it refused, rather than being the generic error a wrong
    // turn somewhere else in this path would also produce.
    let database = with_rows(10);
    let insert = database.query("EXPLAIN INSERT INTO t VALUES (1, 'x')").expect_err("refused");
    assert!(insert.to_string().contains("InsertStatement"), "{insert}");
    let create = database.query("EXPLAIN CREATE TABLE u (a INTEGER)").expect_err("refused");
    assert!(create.to_string().contains("CreateStatement"), "{create}");
    let options = database.query("EXPLAIN (FORMAT JSON) SELECT a FROM t").expect_err("refused");
    assert!(options.to_string().contains("ExplainOptionList"), "{options}");
}

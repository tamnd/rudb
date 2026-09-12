//! `EXPLAIN`, end to end through the parser, the binder, the optimizer and back out as a result set.
//!
//! These are about the statement being reachable and answering with the plan that would have run.
//! What the estimate for a given shape is belongs in `rudb-opt`'s own tests, and what the plan text
//! looks like belongs in `rudb-plan`'s. What is here is the join between the three.

use rudb::Database;
use rudb_common::{LogicalType, Value};

/// The lines of the plan tree, which is everything above the first blank line.
fn tree(text: &str) -> Vec<&str> {
    text.lines().take_while(|line| !line.is_empty()).collect()
}

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
    for line in tree(&text) {
        assert!(line.contains(" rows]"), "a line with no estimate on it: {line}");
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
    let bare: Vec<&str> = tree(&text)
        .into_iter()
        .map(|line| line.rsplit_once("  [").map_or(line, |(head, _)| head))
        .collect();
    assert_eq!(bare.join("\n"), ran.trim_end(), "{text}\n{ran}");
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
fn explain_prints_the_pipelines_a_plan_breaks_into_and_the_edges_between_them() {
    // The same decomposition the executor numbers its operators with, printed before anything runs.
    // A sort is two pipelines, and the one the answer comes out of cannot start until the other has
    // finished, which is the fact somebody reading a slow query is looking for.
    let database = with_rows(100);
    let text = explained(&database, "EXPLAIN SELECT a FROM t ORDER BY a");
    let lines = tree(&text);
    assert!(lines[0].contains("[pipeline 1]"), "{text}");
    assert!(text.contains("  pipeline 0 waits for 1"), "{text}");
    assert!(text.contains("  pipeline 1 waits for nothing"), "{text}");
}

#[test]
fn explain_marks_every_line_that_is_running_a_reference_implementation() {
    // Which at F0 is all of them, and that is the point of printing it. A number measured against
    // the simplest correct version of an operator is not a number to quote as the engine's.
    let database = with_rows(100);
    let text = explained(&database, "EXPLAIN SELECT a FROM t WHERE a > 5");
    for line in tree(&text) {
        assert!(line.ends_with("[reference]"), "a line with no marker on it: {line}");
    }
    assert!(text.contains("\nSeams\n"), "{text}");
    assert!(text.contains("27 seams have nothing registered"), "{text}");
}

#[test]
fn explain_analyze_runs_the_query_and_prints_what_each_operator_actually_did() {
    // The estimate stays where it was and the measurement goes beside it, so that the two are read
    // against each other. A filter that kept everything it was told would keep a fifth is the thing
    // this output exists to make obvious.
    let database = with_rows(1000);
    let text = explained(&database, "EXPLAIN ANALYZE SELECT a FROM t WHERE a > 5");
    for line in tree(&text) {
        assert!(line.contains("[~"), "a line with no estimate on it: {line}");
        assert!(line.contains(" rows, "), "a line with no measurement on it: {line}");
    }
    assert!(text.contains("[994 rows, "), "{text}");
    assert!(text.contains("[1000 rows, "), "{text}");
}

#[test]
fn explain_analyze_counts_the_rows_a_pipeline_breaker_finally_handed_out() {
    // A sort produces nothing until it has seen everything, and the rows come back out of the
    // buffer it filled rather than through the operator, so this is the number that goes missing if
    // nobody counts it there. Zero here is what makes the estimate look a thousand times wrong.
    let database = with_rows(1000);
    let text = explained(&database, "EXPLAIN ANALYZE SELECT a FROM t ORDER BY a");
    let sort = tree(&text)[0];
    assert!(sort.starts_with("Sort "), "{text}");
    assert!(sort.contains("[1000 rows, "), "{sort}");
    assert!(!text.contains("q-error"), "a sort that produced every row it was given: {text}");
    // The same buffer sits under a join, so the same number goes missing there if it is only
    // counted in one of the two places.
    let joined = explained(&database, "EXPLAIN ANALYZE SELECT t.a FROM t JOIN t AS u ON t.a = u.a");
    let join = tree(&joined)
        .into_iter()
        .find(|line| line.trim_start().starts_with("Join "))
        .unwrap_or_else(|| panic!("no join on the plan: {joined}"));
    assert!(join.contains("[1000 rows, "), "{join}");
}

#[test]
fn explain_analyze_reports_the_whole_query_under_its_own_key() {
    // A client reading the result back has to be able to tell a plan that ran from a plan that did
    // not, and the key is the only place that distinction shows up.
    let database = with_rows(100);
    let result = database.query("EXPLAIN ANALYZE SELECT a FROM t").expect("the explain ran");
    assert_eq!(result.value_at(0, 0), Value::Varchar("analyzed_plan".to_owned()));
    let plain = database.query("EXPLAIN SELECT a FROM t").expect("the explain ran");
    assert_eq!(plain.value_at(0, 0), Value::Varchar("logical_plan".to_owned()));
    let text = explained(&database, "EXPLAIN ANALYZE SELECT a FROM t");
    assert!(text.contains("\nPipelines\n"), "{text}");
    assert!(text.contains("\nSeams\n"), "{text}");
    assert!(text.contains("\nTotals\n"), "{text}");
    assert!(text.contains(" building the tree, "), "{text}");
    assert!(text.contains(" of cpu, "), "{text}");
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

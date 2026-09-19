//! `MIN` and `MAX` over a whole column answered out of the footer instead of out of the file.
//!
//! A Parquet file keeps a smallest and a largest value per column chunk, so the smallest value of
//! the whole column is the smallest of those and the query never has to read a page. DuckDB does
//! this in a pass it calls `statistics_propagation` and rudb now does it under the same name, which
//! is what makes `SET disabled_optimizers` mean the same thing on both.
//!
//! The reason it is not simply reading the bound is that a writer is allowed to shorten a long
//! string bound as long as it moves it outward. A shortened minimum is no larger than the smallest
//! value, which keeps every skip correct and makes the bound useless as an answer, because a string
//! that lost its tail is not a string the column holds. So the fold only fires where the bound is
//! the value, and what says it is the value is either the writer's own flag or the physical type
//! being one that cannot be shortened.
//!
//! The fixture is `rudb-parquet`'s, written by DuckDB, which sets both flags to true on all seven of
//! its columns. Every answer asserted here was read out of DuckDB first and the numbers in the
//! comments are its output.

use rudb::Database;
use rudb_common::Value;

/// The path of the fixture, as a SQL string literal.
fn fixture() -> String {
    format!("'{}/../rudb-parquet/testdata/mixed.parquet'", env!("CARGO_MANIFEST_DIR"))
}

/// `SELECT <what> FROM read_parquet(<fixture>)`, as one value.
fn one(what: &str) -> Value {
    let database = Database::new();
    let sql = format!("SELECT {what} FROM read_parquet({})", fixture());
    database.value(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"))
}

/// The plan tree `EXPLAIN` prints for the query, which is everything above the first blank line.
fn explained(what: &str) -> String {
    let database = Database::new();
    let sql = format!("EXPLAIN SELECT {what} FROM read_parquet({})", fixture());
    let result = database.query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    match result.value_at(0, 1) {
        Value::Varchar(text) => text.lines().take_while(|line| !line.is_empty()).collect(),
        other => panic!("the plan came back as {other:?}"),
    }
}

#[test]
fn the_smallest_and_the_largest_of_a_column_are_the_numbers_the_footer_already_holds() {
    // 0 and 96, which is what DuckDB answers over the same file.
    assert_eq!(one("min(a)"), Value::Integer(0));
    assert_eq!(one("max(a)"), Value::Integer(96));
}

#[test]
fn the_file_is_not_read_at_all_because_the_answer_is_a_constant_by_the_time_it_runs() {
    let text = explained("min(a), max(a)");
    assert!(text.contains("Values #1 [min::INTEGER, max::INTEGER]"), "{text}");
    assert!(!text.contains("read_parquet"), "the scan is still there: {text}");
}

#[test]
fn the_query_still_answers_under_the_column_names_it_was_written_with() {
    // The fold replaces the aggregate and not the projection above it, so what comes back is named
    // after the expression the query wrote rather than after the fold.
    let database = Database::new();
    let sql = format!("SELECT min(a) FROM read_parquet({})", fixture());
    let result = database.query(&sql).expect("the query runs");
    assert_eq!(result.names(), ["min(a)"]);
}

#[test]
fn a_date_and_a_timestamp_come_back_as_a_date_and_a_timestamp() {
    // The bound is eight bytes in the file either way and the type it is read back as is the
    // column's, so this is the assertion that the fold did not quietly hand up an integer.
    let text = explained("max(day), min(t)");
    assert!(text.contains("Values #1 [max::DATE, min::TIMESTAMP]"), "{text}");
    assert_eq!(one("max(day)").to_string(), "1972-09-26");
    assert_eq!(one("min(t)").to_string(), "2013-07-15 10:00:00");
}

#[test]
fn a_string_whose_writer_called_its_bounds_exact_is_answered_from_them_too() {
    // DuckDB writes `is_min_value_exact` and `is_max_value_exact` and sets both, so its strings are
    // answers. The ClickBench file states neither flag on any of its twenty three thousand column
    // chunks, so its strings are not, and that is the difference this reads rather than guesses.
    let text = explained("min(s), max(s)");
    assert!(text.contains("rows=[['tag0'::VARCHAR, 'tag4'::VARCHAR]]"), "{text}");
    assert_eq!(one("max(s)"), Value::Varchar("tag4".into()));
}

#[test]
fn a_float_column_keeps_its_scan_however_exact_the_writer_said_the_bounds_were() {
    // The format keeps NaN out of the bounds and this engine sorts NaN above every number, so the
    // largest value of a column holding one is a value the footer never mentions. The fixture holds
    // no NaN and the answer is the same either way, which is the point: it is read rather than
    // folded, and it is right.
    let text = explained("max(d)");
    assert!(text.contains("read_parquet"), "the scan went away: {text}");
    assert_eq!(one("max(d)"), Value::Double(94.5));
}

#[test]
fn a_filter_under_the_aggregate_stops_it_because_the_bounds_cover_the_whole_column() {
    // The smallest value of the rows that survive a filter is not in the footer and cannot be, so
    // the scan runs. DuckDB answers 6 here as well.
    let database = Database::new();
    let sql = format!("SELECT min(a) FROM read_parquet({}) WHERE a > 5", fixture());
    let plan = database.query(&format!("EXPLAIN {sql}")).expect("the plan prints");
    let Value::Varchar(text) = plan.value_at(0, 1) else { panic!("the plan came back wrong") };
    assert!(text.contains("read_parquet"), "the scan went away: {text}");
    assert_eq!(database.value(&sql).expect("the query runs"), Value::Integer(6));
}

#[test]
fn a_group_by_is_left_alone_even_though_every_aggregate_in_it_is_a_minimum() {
    // A bound per column is not a bound per group, so there is nothing here to fold from.
    let database = Database::new();
    let sql = format!("SELECT flag, min(a) FROM read_parquet({}) GROUP BY flag", fixture());
    let plan = database.query(&format!("EXPLAIN {sql}")).expect("the plan prints");
    let Value::Varchar(text) = plan.value_at(0, 1) else { panic!("the plan came back wrong") };
    assert!(text.contains("read_parquet"), "the scan went away: {text}");
}

#[test]
fn one_aggregate_the_footer_cannot_answer_keeps_the_scan_for_all_of_them() {
    // The replacement is the whole node, so it is all of the aggregates or none of them. Counting
    // is not something a bound answers, and the row count that would answer it is a different
    // question read for a different use.
    let text = explained("min(a), count(*)");
    assert!(text.contains("read_parquet"), "the scan went away: {text}");
    assert_eq!(one("count(*)"), Value::BigInt(4096));
}

#[test]
fn the_pass_answers_to_the_name_duckdb_gives_it_and_turning_it_off_puts_the_scan_back() {
    // The same setting on the same name does the same thing on both engines, which is the whole
    // reason it is called `statistics_propagation` here and not something more descriptive.
    let database = Database::new();
    database
        .execute("SET disabled_optimizers = 'statistics_propagation'")
        .expect("the name is a pass now");
    let sql = format!("SELECT min(a) FROM read_parquet({})", fixture());
    let plan = database.query(&format!("EXPLAIN {sql}")).expect("the plan prints");
    let Value::Varchar(text) = plan.value_at(0, 1) else { panic!("the plan came back wrong") };
    assert!(text.contains("read_parquet"), "the fold fired with the pass turned off: {text}");
    assert_eq!(database.value(&sql).expect("the query runs"), Value::Integer(0));
}

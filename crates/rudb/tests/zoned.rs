//! The planner's row count for a filter, held under what the file's zone maps allow.
//!
//! A Parquet writer records the smallest and the largest value of every column of every row group.
//! `pruned.rs` is about a scan walking past the groups a filter rules out, which is the reading
//! side. This is the planning side of the same two numbers: how many rows are left in the groups
//! nothing ruled out, which is a ceiling the answer cannot exceed, and the estimator holds its
//! constant guess under it.
//!
//! The ceiling is only ever applied downwards, so every test here has two halves. One says what the
//! estimate came out as, and one says what the query actually answers, because an estimate below
//! the truth is the failure mode this has and it does not look like anything until a plan is built
//! on it. `sorted.parquet` is written in ascending key order on purpose, which is what makes a
//! filter on that key rule groups out at all, and its README entry says so.

use rudb::Database;
use rudb_common::{LogicalType, Value};

/// The fixture, which is eight row groups of 2048 rows with `k` ascending across them.
fn fixture() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rudb-parquet/testdata/sorted.parquet");
    format!("'{path}'")
}

/// The plan text for a query.
fn explained(database: &Database, sql: &str) -> String {
    let result = database.query(sql).expect("the explain ran");
    assert_eq!(result.types(), [LogicalType::Varchar, LogicalType::Varchar]);
    match result.value_at(0, 1) {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    }
}

/// The line of the plan the filter is on, which is where the estimate for it is printed.
fn filter_line(text: &str) -> String {
    text.lines()
        .find(|line| line.trim_start().starts_with("Filter "))
        .unwrap_or_else(|| panic!("no filter in {text}"))
        .to_string()
}

/// How many rows the filter really keeps.
fn answered(database: &Database, predicate: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM read_parquet({}) WHERE {predicate}", fixture());
    let result = database.query(&sql).expect("the count ran");
    match result.value_at(0, 0) {
        Value::BigInt(count) => count,
        other => panic!("the count came back as {other:?}"),
    }
}

/// The estimate the planner prints for that filter.
fn estimated(database: &Database, predicate: &str) -> String {
    let sql = format!("EXPLAIN SELECT k FROM read_parquet({}) WHERE {predicate}", fixture());
    filter_line(&explained(database, &sql))
}

#[test]
fn a_filter_the_bounds_rule_every_group_out_of_is_exactly_no_rows() {
    // Not an estimate. Every group states a maximum below the constant, so no row of the file can
    // pass, and that is a fact about this file rather than a fraction of it. The one answer here
    // allowed below the one row floor the guess has.
    let database = Database::new();
    let line = estimated(&database, "k > 999999");
    assert!(line.contains("[0 rows exact from zone map]"), "{line}");
    assert_eq!(answered(&database, "k > 999999"), 0, "and it is right");
}

#[test]
fn a_ceiling_under_the_guess_is_what_the_planner_gets() {
    // 16,384 rows, one condition, so the guess is 3,276. The bounds leave one group of 2,048,
    // which is a number no answer to this filter can exceed, so that is the estimate and it is
    // marked as a bound rather than as a guess. The truth is a hundred, so this is still four
    // times too many and it is twenty times closer than the guess was.
    let database = Database::new();
    let line = estimated(&database, "k < 100");
    assert!(line.contains("[2048 rows certified at most 100.00% from zone map]"), "{line}");
    assert_eq!(answered(&database, "k < 100"), 100);
}

#[test]
fn a_ceiling_over_the_guess_leaves_the_guess_where_it_was() {
    // Half the groups survive, which is 8,192 rows, and the guess is 3,276. A ceiling says nothing
    // about how far under it the answer sits, so trading a guess that is already below one for the
    // bound itself would be trading a number for a worse one. Here the guess is the worse number,
    // the truth being 8,192 exactly, and that is the trade being refused: the rule is that reading
    // the bounds can never make an estimate worse, and a rule that is right on this query would
    // have to be wrong on some other one.
    let database = Database::new();
    let line = estimated(&database, "k >= 8192");
    assert!(line.contains("[~3276 rows estimated from default]"), "{line}");
    assert_eq!(answered(&database, "k >= 8192"), 8192);
}

#[test]
fn a_column_whose_groups_all_look_alike_is_estimated_the_way_it_always_was() {
    // The control. `g` is the row number modulo ninety seven, so every group runs from 0 to 96 and
    // no filter on it rules anything out. The ceiling is the whole file and the estimate is the
    // constant, untouched, which is the path almost every query in the world still takes.
    let database = Database::new();
    let line = estimated(&database, "g = 5");
    assert!(line.contains("[~3276 rows estimated from default]"), "{line}");
    assert_eq!(answered(&database, "g = 5"), 169);
}

#[test]
fn the_conjuncts_of_one_filter_are_asked_together_rather_than_one_at_a_time() {
    // A group survives only if no condition rules it out, so these two leave the two groups from 0
    // to 4095, which is 4,096 rows. The guess for two conditions is a fifth of a fifth, 655, which
    // is already under that, so the guess stands. It is six times under the truth of 3,900, which
    // is the compounding this module's own comment calls the part most likely to be wrong, and no
    // pair of bounds is going to fix it.
    let database = Database::new();
    let line = estimated(&database, "k >= 100 AND k < 4000");
    assert!(line.contains("[~655 rows estimated from default]"), "{line}");
    assert_eq!(answered(&database, "k >= 100 AND k < 4000"), 3900);
}

#[test]
fn a_filter_the_bounds_cannot_read_asks_them_nothing() {
    // A comparison of two columns says nothing a minimum and a maximum can answer, because the
    // bounds of one column say nothing about the other's value in the same row. So there is no
    // test to ask with and no ceiling to apply, and the estimate is the constant.
    let database = Database::new();
    let line = estimated(&database, "k = g");
    assert!(line.contains("[~3276 rows estimated from default]"), "{line}");
    assert_eq!(answered(&database, "k = g"), 97, "k is 0 to 16383 and g is k modulo 97");
}

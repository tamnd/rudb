//! Correlated subqueries that no shape rule recognises, from the answers rather than from the plan.
//!
//! Every answer asserted here was read off the pinned duckdb on server2 first, which is
//! v2.0.0-dev84237 at cc7e7bac7f. These are the shapes the rules in `rudb-opt/src/unnest.rs` do not
//! have a pattern for and that the general rule in `rudb-opt/src/domain.rs` answers instead, so what
//! is worth checking is the rows and not which rule produced them. The plan side is tested in
//! `domain.rs` itself.
//!
//! The outer table has a NULL key and the inner table has one too, which is the part a rewrite gets
//! wrong quietly. The domain is joined back with a null safe comparison, so the outer row whose key
//! is NULL gets the subquery's answer about NULL rather than no row at all.

use rudb::Database;
use rudb_common::Value;

/// The two tables every query below reads, with the rows already in them.
fn database() -> Database {
    let database = Database::new();
    for sql in [
        "CREATE TABLE o (k INTEGER, t VARCHAR)",
        "INSERT INTO o VALUES (1, 'a'), (2, 'b'), (3, NULL), (NULL, 'd')",
        "CREATE TABLE i (k INTEGER, w INTEGER, s VARCHAR)",
        "INSERT INTO i VALUES (1, 100, 'x'), (1, 200, 'y'), (2, 300, 'x'), (NULL, 400, 'z')",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// The second column of a query that answers a key and a value, in the order the rows came.
fn answers(database: &Database, sql: &str) -> Vec<Value> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len()).map(|row| result.value_at(row, 1)).collect()
}

/// The one column of keys a query answers with.
fn keys(database: &Database, sql: &str) -> Vec<Value> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len()).map(|row| result.value_at(row, 0)).collect()
}

/// A run of counts, which is what most of these answer with.
fn counts(values: &[i64]) -> Vec<Value> {
    values.iter().map(|&value| Value::BigInt(value)).collect()
}

#[test]
fn a_distinct_inside_a_scalar_subquery_answers_per_outer_row() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT count(*) FROM (SELECT DISTINCT s FROM i WHERE i.k = o.k) AS x) AS c \
             FROM o ORDER BY k"
        ),
        counts(&[2, 1, 0, 0])
    );
}

#[test]
fn a_projection_between_the_filter_and_the_aggregate_is_carried_through() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT max(w) FROM (SELECT w * 2 AS w FROM i WHERE i.k = o.k) AS x) AS c \
             FROM o ORDER BY k"
        ),
        [Value::Integer(400), Value::Integer(600), Value::Null, Value::Null]
    );
}

#[test]
fn a_join_inside_the_subquery_is_pushed_into_the_correlated_side() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT count(*) FROM i JOIN o AS p ON p.k = i.k WHERE i.k = o.k) AS c \
             FROM o ORDER BY k"
        ),
        counts(&[2, 1, 0, 0])
    );
}

#[test]
fn a_cross_product_inside_the_subquery_is_pushed_into_the_correlated_side() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT count(*) FROM i, o AS p WHERE i.k = o.k AND p.k = 1) AS c \
             FROM o ORDER BY k"
        ),
        counts(&[2, 1, 0, 0])
    );
}

#[test]
fn a_left_join_inside_the_subquery_keeps_its_padding_rows() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT count(*) FROM i LEFT JOIN o AS p ON p.k = i.w WHERE i.k = o.k) AS c \
             FROM o ORDER BY k"
        ),
        counts(&[2, 1, 0, 0])
    );
}

#[test]
fn a_sort_inside_the_subquery_does_not_change_what_is_aggregated() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT sum(w) FROM (SELECT w FROM i WHERE i.k = o.k ORDER BY w) AS x) AS c \
             FROM o ORDER BY k"
        ),
        [Value::HugeInt(300), Value::HugeInt(300), Value::Null, Value::Null]
    );
}

#[test]
fn a_grouped_aggregate_inside_the_subquery_groups_per_outer_row() {
    let database = database();
    // One group per distinct s within the rows of one outer key, so the outer key that matched two
    // inner rows with different s answers 2 and not 1.
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT sum(n) FROM (SELECT s, count(*) AS n FROM i WHERE i.k = o.k \
             GROUP BY s) AS x) AS c FROM o ORDER BY k"
        ),
        [Value::HugeInt(2), Value::HugeInt(1), Value::Null, Value::Null]
    );
}

#[test]
fn an_ungrouped_count_over_no_rows_is_zero_and_not_null() {
    let database = database();
    // The count bug. The rewrite groups the inner side by the domain and a key whose share of the
    // inner side is empty has no group at all, so the zero has to be put back.
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT count(*) FROM (SELECT w FROM i WHERE i.k = o.k) AS x) AS c \
             FROM o ORDER BY k"
        ),
        counts(&[2, 1, 0, 0])
    );
}

#[test]
fn an_ungrouped_sum_over_no_rows_is_still_null() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT sum(w) FROM (SELECT w FROM i WHERE i.k = o.k) AS x) AS c \
             FROM o ORDER BY k"
        ),
        [Value::HugeInt(300), Value::HugeInt(300), Value::Null, Value::Null]
    );
}

#[test]
fn an_in_test_against_a_distinct_subquery_answers_the_matching_keys() {
    let database = database();
    assert_eq!(
        keys(
            &database,
            "SELECT k FROM o WHERE k IN (SELECT DISTINCT k FROM i WHERE i.w > 150) ORDER BY k"
        ),
        [Value::Integer(1), Value::Integer(2)]
    );
}

#[test]
fn a_limit_inside_a_correlated_subquery_is_refused() {
    let database = database();
    // No rule for it. A limit inside the subquery is per outer row and pushing the domain under it
    // would make it one limit over the whole inner side, which is a different query.
    let sql = "SELECT k, (SELECT count(*) FROM (SELECT w FROM i WHERE i.k = o.k LIMIT 1) AS x) AS c \
               FROM o";
    let error = database.query(sql).expect_err("no rule for a limit").to_string();
    assert!(error.contains("dependent join"), "{error}");
}

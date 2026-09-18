//! `IN` and `NOT IN` against a subquery, which is the mark join, from the answers.
//!
//! The answer is three valued and the third value is the part a lookup gets wrong quietly. `x IN
//! (SELECT ...)` is true where some row of the subquery equals `x`, false where none does, and NULL
//! where none does but some pair could not be decided, which is where `x` is NULL or where the
//! subquery answered a NULL. A hash table on its own only answers the first two, so every case
//! below is about the third, and every one of them was read off DuckDB v1.5.5 before it was written
//! here.
//!
//! The empty subquery is here because it is the one case where a NULL outer key is still false: no
//! rows means no pairs, and a question about every pair of nothing is answered without looking.

use rudb::Database;
use rudb_common::Value;

/// The outer table and the three subquery tables, with the rows already in them.
///
/// `g` holds a NULL and `h` does not, which is the difference the whole file turns on. `e` is
/// empty.
fn database() -> Database {
    let database = Database::new();
    for sql in [
        "CREATE TABLE d (x BIGINT)",
        "INSERT INTO d VALUES (1), (2), (NULL)",
        "CREATE TABLE g (y BIGINT)",
        "INSERT INTO g VALUES (2), (NULL)",
        "CREATE TABLE h (y BIGINT)",
        "INSERT INTO h VALUES (2), (3)",
        "CREATE TABLE e (y BIGINT)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// The marker column of a query that answers a key and a marker, in key order.
fn markers(database: &Database, sql: &str) -> Vec<Value> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len()).map(|row| result.value_at(row, 1)).collect()
}

/// The three markers a query over `d` answers with, written the short way.
fn three(first: Option<bool>, second: Option<bool>, third: Option<bool>) -> Vec<Value> {
    [first, second, third].into_iter().map(|at| at.map_or(Value::Null, Value::Boolean)).collect()
}

#[test]
fn a_subquery_holding_a_null_answers_null_rather_than_false_for_everything_it_does_not_hold() {
    assert_eq!(
        markers(&database(), "SELECT x, x IN (SELECT y FROM g) AS m FROM d ORDER BY x NULLS LAST"),
        three(None, Some(true), None)
    );
}

#[test]
fn a_subquery_holding_no_null_answers_false_for_what_it_does_not_hold() {
    assert_eq!(
        markers(&database(), "SELECT x, x IN (SELECT y FROM h) AS m FROM d ORDER BY x NULLS LAST"),
        three(Some(false), Some(true), None)
    );
}

#[test]
fn an_outer_null_is_null_against_any_subquery_that_answered_a_row() {
    let database = database();
    for table in ["g", "h"] {
        let sql = format!("SELECT x, x IN (SELECT y FROM {table}) AS m FROM d WHERE x IS NULL");
        assert_eq!(markers(&database, &sql), vec![Value::Null], "against {table}");
    }
}

#[test]
fn an_empty_subquery_is_false_for_every_outer_row_including_the_null_one() {
    assert_eq!(
        markers(&database(), "SELECT x, x IN (SELECT y FROM e) AS m FROM d ORDER BY x NULLS LAST"),
        three(Some(false), Some(false), Some(false))
    );
}

#[test]
fn not_in_against_a_subquery_holding_a_null_is_never_true() {
    assert_eq!(
        markers(
            &database(),
            "SELECT x, x NOT IN (SELECT y FROM g) AS m FROM d ORDER BY x NULLS LAST"
        ),
        three(None, Some(false), None)
    );
}

#[test]
fn not_in_against_a_subquery_holding_no_null_is_the_ordinary_negation() {
    assert_eq!(
        markers(
            &database(),
            "SELECT x, x NOT IN (SELECT y FROM h) AS m FROM d ORDER BY x NULLS LAST"
        ),
        three(Some(true), Some(false), None)
    );
}

#[test]
fn a_subquery_the_outer_row_matches_is_true_whether_or_not_a_null_is_in_it() {
    let database = database();
    for table in ["g", "h"] {
        let sql = format!("SELECT x, x IN (SELECT y FROM {table}) AS m FROM d WHERE x = 2");
        assert_eq!(markers(&database, &sql), vec![Value::Boolean(true)], "against {table}");
    }
}

#[test]
fn a_marker_over_more_rows_than_one_batch_holds_is_the_same_as_over_a_few() {
    let database = database();
    for sql in [
        "CREATE TABLE wide AS SELECT range AS x FROM range(5000)",
        "CREATE TABLE narrow AS SELECT range * 2 AS y FROM range(1000)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    let result = database
        .query("SELECT count(*) FROM wide WHERE x IN (SELECT y FROM narrow)")
        .expect("counted");
    assert_eq!(result.value_at(0, 0), Value::BigInt(1000));
}

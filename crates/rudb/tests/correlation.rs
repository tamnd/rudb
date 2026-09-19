//! A correlated column read more than one level below the query it belongs to.
//!
//! The binder records a correlation into the frame of the query being bound. A name that resolved
//! past the query immediately enclosing belongs to one further out still, and if the frame it lands
//! in is the only one that ever sees it then the query that has to feed it down looks uncorrelated
//! and is planned as though it were. These check that it arrives where it belongs and that the plan
//! it produces answers.
//!
//! Every expected answer here was taken from duckdb v2.0.0-dev84237 and not from rudb.

use rudb::Database;
use rudb_common::Value;

/// Three tables, one to drive the outer query and two to nest the subqueries over.
fn tables() -> Database {
    let database = Database::new();
    database.execute("CREATE TABLE deep_o(x INTEGER)").expect("the outer table");
    database.execute("CREATE TABLE deep_t(c INTEGER)").expect("the middle table");
    database.execute("CREATE TABLE deep_s(c INTEGER, d INTEGER)").expect("the inner table");
    database.execute("INSERT INTO deep_o VALUES (1), (2), (3)").expect("outer rows");
    database.execute("INSERT INTO deep_t VALUES (1), (2)").expect("middle rows");
    database.execute("INSERT INTO deep_s VALUES (2, 20), (3, 30), (2, 21)").expect("inner rows");
    database
}

/// The rows of a query as text, in the order the query asked for.
fn rows(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row: Vec<Value>| {
            row.iter().map(|value| format!("{value}")).collect::<Vec<_>>().join("|")
        })
        .collect()
}

/// The plain shape, where the middle query reads nothing of the outer row itself.
#[test]
fn an_exists_inside_an_exists_reads_the_outer_row_two_levels_up() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT x FROM deep_o o WHERE EXISTS (SELECT 1 FROM deep_t t \
         WHERE EXISTS (SELECT 1 FROM deep_s s WHERE s.c = o.x)) ORDER BY x",
    );
    assert_eq!(answer, vec!["2", "3"], "{answer:?}");
}

/// The same with the middle query read as well, so the inner one is correlated to both levels.
#[test]
fn an_inner_query_reads_the_middle_query_and_the_outer_row_at_once() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT x FROM deep_o o WHERE EXISTS (SELECT 1 FROM deep_t t WHERE t.c = 1 \
         AND EXISTS (SELECT 1 FROM deep_s s WHERE s.c = o.x AND s.c > t.c)) ORDER BY x",
    );
    assert_eq!(answer, vec!["2", "3"], "{answer:?}");
}

#[test]
fn a_scalar_query_holding_a_nested_exists_is_evaluated_per_outer_row() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT o.x, (SELECT count(*) FROM deep_t t \
         WHERE EXISTS (SELECT 1 FROM deep_s s WHERE s.c = o.x)) FROM deep_o o ORDER BY o.x",
    );
    assert_eq!(answer, vec!["1|0", "2|2", "3|2"], "{answer:?}");
}

#[test]
fn a_negated_nested_exists_keeps_the_rows_the_plain_one_dropped() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT x FROM deep_o o WHERE NOT EXISTS (SELECT 1 FROM deep_t t \
         WHERE EXISTS (SELECT 1 FROM deep_s s WHERE s.c = o.x)) ORDER BY x",
    );
    assert_eq!(answer, vec!["1"], "{answer:?}");
}

/// A mark join above, whose body holds the nested correlation.
#[test]
fn an_in_whose_body_holds_a_nested_correlation_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT x FROM deep_o o WHERE o.x IN (SELECT t.c FROM deep_t t \
         WHERE EXISTS (SELECT 1 FROM deep_s s WHERE s.c = o.x)) ORDER BY x",
    );
    assert_eq!(answer, vec!["2"], "{answer:?}");
}

/// A single join underneath a dependent one, which is the case the domain rule used to leave out.
#[test]
fn a_scalar_query_inside_a_scalar_query_reads_the_outer_row() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT o.x, (SELECT max((SELECT max(s.d) FROM deep_s s WHERE s.c = o.x)) \
         FROM deep_t t) FROM deep_o o ORDER BY o.x",
    );
    assert_eq!(answer, vec!["1|NULL", "2|21", "3|30"], "{answer:?}");
}

/// Three levels, so the column is handed up twice rather than once.
#[test]
fn a_correlation_three_levels_down_still_arrives() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT x FROM deep_o o WHERE EXISTS (SELECT 1 FROM deep_t t \
         WHERE EXISTS (SELECT 1 FROM deep_s s \
         WHERE EXISTS (SELECT 1 FROM deep_s s2 WHERE s2.c = o.x AND s2.d = s.d))) ORDER BY x",
    );
    assert_eq!(answer, vec!["2", "3"], "{answer:?}");
}

#[test]
fn a_nested_scalar_query_inside_a_comparison_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT o.x, (SELECT sum(t.c) FROM deep_t t \
         WHERE t.c < (SELECT max(s.c) FROM deep_s s WHERE s.c = o.x)) FROM deep_o o ORDER BY o.x",
    );
    assert_eq!(answer, vec!["1|NULL", "2|1", "3|3"], "{answer:?}");
}

#[test]
fn a_nested_correlation_under_a_comparison_in_a_where_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT o.x FROM deep_o o WHERE (SELECT count(*) FROM deep_t t \
         WHERE EXISTS (SELECT 1 FROM deep_s s WHERE s.c = o.x AND s.d > t.c)) > 1 ORDER BY o.x",
    );
    assert_eq!(answer, vec!["2", "3"], "{answer:?}");
}

//! A subquery written in a join's `ON`, from the SQL down to the rows that come back.
//!
//! A query written in a `WHERE` or a `SELECT` is joined into the rows the whole `FROM` produced,
//! which is above every join in it. A query written in an `ON` cannot go there. The condition it
//! produces columns for is evaluated by the join, over the rows its two inputs handed it, so a
//! column produced above the join is a column the join was never given. That is what these check:
//! the query lands in one of the two inputs, and the answer is the one the reference binary gives.
//!
//! Every expected answer here was taken from duckdb v2.0.0-dev84237 and not from rudb.

use rudb::Database;
use rudb_common::Value;

/// Three tables, two to join and one for the subqueries to read.
fn tables() -> Database {
    let database = Database::new();
    database.execute("CREATE TABLE pair_l(a INTEGER, x INTEGER)").expect("the left table");
    database.execute("CREATE TABLE pair_r(b INTEGER, y INTEGER)").expect("the right table");
    database.execute("CREATE TABLE pair_s(a INTEGER, b INTEGER)").expect("the read table");
    database
        .execute("INSERT INTO pair_l VALUES (1, 10), (1, 11), (2, 20), (3, 30)")
        .expect("left rows");
    database
        .execute("INSERT INTO pair_r VALUES (10, 100), (20, 200), (30, 300), (40, 400)")
        .expect("right rows");
    database
        .execute("INSERT INTO pair_s VALUES (1, 10), (1, 20), (2, 20), (NULL, 30), (3, NULL)")
        .expect("read rows");
    database
}

/// The rows of a query as text, sorted, so an answer compares without depending on the join order.
fn rows(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let mut rows: Vec<String> = result
        .rows()
        .map(|row: Vec<Value>| {
            row.iter().map(|value| format!("{value}")).collect::<Vec<_>>().join("|")
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn a_query_in_a_join_condition_that_reads_nothing_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT l.a, r.b FROM pair_l l LEFT JOIN pair_r r \
         ON EXISTS (SELECT 1 FROM pair_s s WHERE s.a = 1)",
    );
    // The subquery is true for every pair, so this is the product of four left rows and four right
    // ones, and the two left rows that share a value make eight of the sixteen say 1.
    assert_eq!(answer.len(), 16, "{answer:?}");
    assert_eq!(answer.iter().filter(|row| row.starts_with("1|")).count(), 8, "{answer:?}");
}

#[test]
fn a_query_in_a_join_condition_that_reads_the_left_side_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT l.a, r.b FROM pair_l l LEFT JOIN pair_r r \
         ON EXISTS (SELECT 1 FROM pair_s s WHERE s.a = l.a)",
    );
    // Every left value of `a` has a row in `pair_s`, so again every pair passes.
    assert_eq!(answer.len(), 16, "{answer:?}");
}

#[test]
fn a_query_in_a_join_condition_that_reads_the_right_side_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT l.a, r.b FROM pair_l l LEFT JOIN pair_r r \
         ON EXISTS (SELECT 1 FROM pair_s s WHERE s.b = r.b)",
    );
    // `pair_s` has 10, 20 and 30 and not 40, so the right row with 40 matches nothing and the four
    // left rows keep three partners each.
    assert_eq!(answer.len(), 12, "{answer:?}");
    assert!(!answer.iter().any(|row| row.ends_with("|40")), "{answer:?}");
}

/// The right side dropping out of a left join still pads, which is what says the query went into
/// the right input rather than filtering the left rows away with it.
#[test]
fn a_left_join_whose_condition_reads_the_right_side_still_pads() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT l.a, r.b FROM pair_l l LEFT JOIN pair_r r \
         ON r.b = l.x AND EXISTS (SELECT 1 FROM pair_s s WHERE s.b = r.b)",
    );
    assert_eq!(answer, vec!["1|10", "1|NULL", "2|20", "3|30"], "{answer:?}");
}

#[test]
fn an_in_query_in_a_join_condition_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT l.a, r.b FROM pair_l l LEFT JOIN pair_r r ON l.a IN (SELECT s.a FROM pair_s s)",
    );
    // `pair_s` holds 1, 2 and 3 as values of `a`, which is every left value, so nothing is dropped.
    assert_eq!(answer.len(), 16, "{answer:?}");
}

#[test]
fn a_scalar_query_in_a_join_condition_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT l.a, r.b FROM pair_l l JOIN pair_r r \
         ON r.b = (SELECT max(s.b) FROM pair_s s WHERE s.a = l.a)",
    );
    assert_eq!(answer, vec!["1|20", "1|20", "2|20"], "{answer:?}");
}

/// A query that reads both sides, over an inner join, which is a product and a filter.
///
/// It used to reach the executor as a plan whose join condition read a column produced above that
/// join, and came back as an internal error about a column not being in a schema. There is no input
/// that produces a pair of rows, so the join becomes the thing that produces them and the condition
/// moves above it, which is the same query for an inner join and for no other kind.
#[test]
fn a_query_in_an_inner_join_condition_that_reads_both_sides_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT l.a, r.b FROM pair_l l JOIN pair_r r \
         ON EXISTS (SELECT 1 FROM pair_s s WHERE s.a = l.a AND s.b = r.b)",
    );
    assert_eq!(answer, vec!["1|10", "1|10", "1|20", "1|20", "2|20"], "{answer:?}");
}

/// The same through an `IN`, where it is the comparison and not the body that reads the left side.
#[test]
fn an_in_query_split_across_both_sides_of_an_inner_join_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT l.a, r.b FROM pair_l l JOIN pair_r r \
         ON l.a IN (SELECT s.a FROM pair_s s WHERE s.b = r.b)",
    );
    assert_eq!(answer, vec!["1|10", "1|10", "1|20", "1|20", "2|20"], "{answer:?}");
}

/// The negation, which is the pairs the one above dropped rather than a smaller answer.
#[test]
fn a_not_exists_that_reads_both_sides_of_an_inner_join_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT l.a, r.b FROM pair_l l JOIN pair_r r \
         ON NOT EXISTS (SELECT 1 FROM pair_s s WHERE s.a = l.a AND s.b = r.b)",
    );
    assert_eq!(answer.len(), 11, "{answer:?}");
    assert_eq!(answer.iter().filter(|row| row.starts_with("3|")).count(), 4, "{answer:?}");
}

/// A scalar query reading both sides, compared against a column of the right side.
#[test]
fn a_scalar_query_that_reads_both_sides_of_an_inner_join_is_answered() {
    let database = tables();
    let answer = rows(
        &database,
        "SELECT l.a, r.b FROM pair_l l JOIN pair_r r \
         ON r.b = (SELECT max(s.b) FROM pair_s s WHERE s.a = l.a AND s.b <= r.b)",
    );
    assert_eq!(answer, vec!["1|10", "1|10", "1|20", "1|20", "2|20"], "{answer:?}");
}

/// An equality written beside the query, which has to stay an equality the join can build on.
///
/// The product is what the binder writes and not what runs. Filter pushdown already turns a filter
/// over an inner join back into a join condition, so the part of the `ON` that reads a column from
/// each side goes back down and only the part reading the query's output stays above.
#[test]
fn an_equality_beside_a_query_that_reads_both_sides_is_still_a_join_condition() {
    let database = tables();
    let sql = "SELECT l.a, r.b FROM pair_l l JOIN pair_r r \
               ON l.x = r.b AND EXISTS (SELECT 1 FROM pair_s s WHERE s.a = l.a AND s.b = r.b)";
    let answer = rows(&database, sql);
    assert_eq!(answer, vec!["1|10", "2|20"], "{answer:?}");

    let plan = rows(&database, &format!("EXPLAIN {sql}")).join("\n");
    assert!(plan.contains("Join INNER on="), "{plan}");
    assert!(!plan.contains("CrossProduct"), "{plan}");
}

/// A query that reads both sides is still refused over a join that is not an inner one.
///
/// A left join pads the pairs its condition dropped, and a filter above a product has already
/// thrown away which left row a dropped pair came from, so the rewrite the inner join gets is not
/// this query. Upstream plans it as a pair dependent join, which rudb does not have, and #913 stays
/// open for it.
#[test]
fn a_query_that_reads_both_sides_of_an_outer_join_is_refused_by_name() {
    let database = tables();
    let error = database
        .query(
            "SELECT l.a, r.b FROM pair_l l LEFT JOIN pair_r r \
             ON EXISTS (SELECT 1 FROM pair_s s WHERE s.a = l.a AND s.b = r.b)",
        )
        .expect_err("the pair dependent shape has no plan");
    let text = error.to_string();
    assert!(text.contains("Not implemented"), "{text}");
    assert!(text.contains("reads both sides of that join"), "{text}");
}

/// The same refusal through an `IN`, where it is the comparison and not the body that reads a side.
#[test]
fn an_in_query_split_across_both_sides_of_an_outer_join_is_refused_by_name() {
    let database = tables();
    let error = database
        .query(
            "SELECT l.a, r.b FROM pair_l l LEFT JOIN pair_r r \
             ON l.a IN (SELECT s.a FROM pair_s s WHERE s.b = r.b)",
        )
        .expect_err("the pair dependent shape has no plan");
    assert!(error.to_string().contains("reads both sides of that join"), "{error}");
}

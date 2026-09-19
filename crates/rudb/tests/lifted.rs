//! A query written in a clause of a grouped block, which is joined in over the grouping.
//!
//! Every answer asserted here was read off the pinned duckdb first, which is v2.0.0-dev84237 at
//! cc7e7bac7f.
//!
//! A subquery in a select list, a `HAVING` or an `ORDER BY` is one row that has nothing to do with
//! the groups. It is evaluated once for the whole query when it is uncorrelated, so it is not a
//! value that varies within a group and it needs no group of its own. What decides whether it
//! works is where its join goes. Underneath the grouping its column is a column of every row going
//! into the aggregate, which the grouping rule then asks for in the `GROUP BY`, and the aggregate
//! carries nothing but its groups and its aggregates upward, so the projection could not read the
//! column even if the rule let it through. Over the grouping both problems go away.
//!
//! The cases are grouped by which clause wrote the query and by what kind of join it wants, since
//! those are the two things that decide which code path carries it.

use rudb::Database;
use rudb_common::Value;

/// The two tables every query below reads, with the rows already in them.
fn database() -> Database {
    let database = Database::new();
    for sql in [
        "CREATE TABLE t (k INTEGER, w INTEGER)",
        "INSERT INTO t VALUES (1, 100), (1, 200), (2, 300)",
        "CREATE TABLE u (k INTEGER, v INTEGER)",
        "INSERT INTO u VALUES (1, 10), (2, 20), (2, 40)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// Every row of a query, as a row of values per row.
fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| (0..result.width()).map(|at| result.value_at(row, at)).collect())
        .collect()
}

/// The message a query is refused with.
fn refused(database: &Database, sql: &str) -> String {
    match database.query(sql) {
        Ok(_) => panic!("{sql} was expected to be refused"),
        Err(error) => error.to_string(),
    }
}

/// A row of integers, which is what most of these answer with.
fn ints(values: &[i32]) -> Vec<Value> {
    values.iter().map(|&value| Value::Integer(value)).collect()
}

#[test]
fn a_constant_subquery_beside_an_aggregate_answers() {
    let database = database();
    assert_eq!(rows(&database, "SELECT max(w), (SELECT 1) FROM t"), vec![ints(&[300, 1])]);
}

#[test]
fn a_constant_subquery_beside_a_group_key_answers() {
    let database = database();
    assert_eq!(
        rows(&database, "SELECT k, (SELECT 1) FROM t GROUP BY k ORDER BY k"),
        vec![ints(&[1, 1]), ints(&[2, 1])]
    );
}

/// The subquery inside an expression rather than as the whole target, on both sides of the
/// operator, because the walk that rewrites an expression over the aggregate recurses and the two
/// sides are two different arms of it.
#[test]
fn an_aggregate_added_to_a_subquery_answers_either_way_round() {
    let database = database();
    assert_eq!(rows(&database, "SELECT max(w) + (SELECT 1) FROM t"), vec![ints(&[301])]);
    assert_eq!(rows(&database, "SELECT (SELECT 1) + max(w) FROM t"), vec![ints(&[301])]);
}

/// The subquery reads the same table the outer query groups, which is the case where joining it
/// underneath would have doubled the rows going into the aggregate as well as losing the column.
#[test]
fn a_subquery_over_the_grouped_table_answers() {
    let database = database();
    assert_eq!(
        rows(&database, "SELECT count(*), (SELECT max(w) FROM t) FROM t"),
        vec![vec![Value::BigInt(3), Value::Integer(300)]]
    );
}

/// An `IN` binds as a mark join, which carries its comparison rather than the expression carrying
/// it, and that comparison is written over the outer rows. So it needs the same rewrite the
/// expression gets, and here the left side of it is an aggregate.
#[test]
fn an_in_test_over_an_aggregate_answers() {
    let database = database();
    assert_eq!(
        rows(&database, "SELECT max(w) IN (SELECT v FROM u) FROM t"),
        vec![vec![Value::Boolean(false)]]
    );
    assert_eq!(
        rows(&database, "SELECT min(w) / 10 IN (SELECT v FROM u) FROM t"),
        vec![vec![Value::Boolean(true)]]
    );
}

/// An `EXISTS` is a third shape again: a marker column joined in and then compared against NULL.
#[test]
fn an_exists_in_the_select_list_of_a_grouped_query_answers() {
    let database = database();
    assert_eq!(
        rows(
            &database,
            "SELECT k, EXISTS (SELECT 1 FROM u WHERE u.v > 15) FROM t GROUP BY k \
             ORDER BY k"
        ),
        vec![
            vec![Value::Integer(1), Value::Boolean(true)],
            vec![Value::Integer(2), Value::Boolean(true)],
        ]
    );
}

/// Sorted on a query nothing selects, so the key is projected as an extra column and then dropped
/// again, and the query it reads still has to be joined over the grouping.
#[test]
fn a_grouped_query_sorted_on_a_subquery_answers() {
    let database = database();
    assert_eq!(
        rows(&database, "SELECT k FROM t GROUP BY k ORDER BY (SELECT 1), k"),
        vec![ints(&[1]), ints(&[2])]
    );
}

/// A `HAVING` and a select list both writing one, which is the case that says the two clauses put
/// their queries on the same list rather than each on its own.
#[test]
fn a_subquery_in_the_select_list_and_in_the_having_at_once_answers() {
    let database = database();
    assert_eq!(
        rows(
            &database,
            "SELECT k, (SELECT 1), sum(w) FROM t GROUP BY k \
             HAVING sum(w) > (SELECT 50) ORDER BY k"
        ),
        vec![
            vec![Value::Integer(1), Value::Integer(1), Value::HugeInt(300)],
            vec![Value::Integer(2), Value::Integer(1), Value::HugeInt(300)],
        ]
    );
}

/// A grouped block inside a grouped block, each with a query of its own to lift. The inner block
/// finishes first, so an outer block that left its own list where the inner one could clear it
/// would lose it.
#[test]
fn nested_grouped_blocks_each_lift_their_own_subquery() {
    let database = database();
    assert_eq!(
        rows(
            &database,
            "SELECT k, (SELECT 1) FROM t \
             WHERE w IN (SELECT max(w) FROM t GROUP BY k HAVING max(w) > (SELECT 50)) \
             GROUP BY k ORDER BY k"
        ),
        vec![ints(&[1, 1]), ints(&[2, 1])]
    );
}

/// A query written in a `REPLACE` list, which is bound on the star's path through the select list
/// rather than on the ordinary one.
#[test]
fn a_subquery_replacing_a_starred_column_of_a_grouped_query_answers() {
    let database = database();
    assert_eq!(
        rows(
            &database,
            "SELECT * REPLACE ((SELECT 9) AS k) FROM (SELECT k, sum(w) AS z FROM t GROUP BY k) \
             ORDER BY z"
        ),
        vec![
            vec![Value::Integer(9), Value::HugeInt(300)],
            vec![Value::Integer(9), Value::HugeInt(300)],
        ]
    );
}

/// A query written inside the aggregate's own argument, which is the one place in a grouped select
/// list that must not be lifted. It is read once per row going into the aggregate, so over the
/// grouping is exactly where the aggregate that reads it cannot see it. The second query is the
/// same shape with the query inside the argument reading the outer row two levels out, so the
/// grouped block in the middle sees it as uncorrelated and would have lifted it. That is how this
/// was found.
#[test]
fn a_subquery_inside_an_aggregate_argument_stays_underneath() {
    let database = database();
    assert_eq!(rows(&database, "SELECT max((SELECT 1)) FROM t"), vec![ints(&[1])]);
    assert_eq!(
        rows(
            &database,
            "SELECT t.k, (SELECT max((SELECT max(v) FROM u WHERE u.k = t.k)) FROM u) \
             FROM t ORDER BY t.k, t.w"
        ),
        vec![ints(&[1, 10]), ints(&[1, 10]), ints(&[2, 40])]
    );
}

/// The subquery in an ungrouped select list, which is the control: nothing is lifted there because
/// there is no grouping to lift it over, and the answer has to stay what it was.
#[test]
fn a_subquery_in_an_ungrouped_select_list_still_answers() {
    let database = database();
    assert_eq!(
        rows(&database, "SELECT k, (SELECT 1) FROM t ORDER BY k, w"),
        vec![ints(&[1, 1]), ints(&[1, 1]), ints(&[2, 1])]
    );
}

/// A correlated one still goes underneath, because what it correlates to is a column of the rows
/// going into the grouping and there is nothing over the grouping to read. Underneath is where the
/// aggregate cannot carry its column up, so this is refused, and what is asserted is the sentence:
/// the column belongs to no table anybody wrote, so asking for it in a `GROUP BY` names nothing.
/// That is #1032.
#[test]
fn a_subquery_correlated_to_the_group_key_says_what_it_cannot_do() {
    let database = database();
    let message = refused(
        &database,
        "SELECT k, (SELECT max(v) FROM u WHERE u.k = t.k) FROM t GROUP BY k ORDER BY k",
    );
    assert!(message.contains("a correlated subquery over a grouped query"), "{message}");
    assert!(!message.contains("a column must appear"), "{message}");
}

/// A column of the grouped table that is neither grouped nor aggregated is still refused and the
/// message still names it. Lifting a query over the grouping is not the same as letting every
/// column through, and this is the line between the two.
#[test]
fn an_ungrouped_column_is_still_refused_by_name() {
    let database = database();
    let message = refused(&database, "SELECT k, w, (SELECT 1) FROM t GROUP BY k");
    assert!(message.contains("column \"w\" must appear in the GROUP BY clause"), "{message}");
}

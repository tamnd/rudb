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
fn a_limit_inside_a_correlated_subquery_is_one_row_per_outer_row() {
    let database = database();
    // The one that says the limit did not become a limit over the whole inner side. Key 1 has two
    // matches and key 2 has one, and both answer one.
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT count(*) FROM (SELECT w FROM i WHERE i.k = o.k LIMIT 1) AS x) AS c \
             FROM o ORDER BY k"
        ),
        counts(&[1, 1, 0, 0])
    );
}

#[test]
fn a_top_n_inside_a_correlated_subquery_sorts_the_matches_of_each_outer_row() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT w FROM i WHERE i.k = o.k ORDER BY w DESC LIMIT 1) AS c \
             FROM o ORDER BY k"
        ),
        [Value::Integer(200), Value::Integer(300), Value::Null, Value::Null]
    );
}

#[test]
fn an_offset_inside_a_correlated_subquery_skips_rows_of_that_outer_row_only() {
    let database = database();
    // Key 2 has one match, so skipping one leaves it nothing. If the offset were counted over the
    // whole inner side it would have an answer here, which is how this test fails.
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT w FROM i WHERE i.k = o.k ORDER BY w LIMIT 1 OFFSET 1) AS c \
             FROM o ORDER BY k"
        ),
        [Value::Integer(200), Value::Null, Value::Null, Value::Null]
    );
}

#[test]
fn an_offset_with_no_limit_keeps_everything_after_it() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT sum(w) FROM (SELECT w FROM i WHERE i.k = o.k ORDER BY w OFFSET 1) \
             AS x) AS c FROM o ORDER BY k"
        ),
        [Value::HugeInt(200), Value::Null, Value::Null, Value::Null]
    );
}

#[test]
fn an_offset_past_the_end_answers_nothing_for_every_outer_row() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT w FROM i WHERE i.k = o.k ORDER BY w LIMIT 1 OFFSET 5) AS c \
             FROM o ORDER BY k"
        ),
        [Value::Null, Value::Null, Value::Null, Value::Null]
    );
}

#[test]
fn a_lateral_entry_with_a_limit_answers_that_many_rows_per_left_row() {
    let database = database();
    assert_eq!(
        keys(
            &database,
            "SELECT o.k, v.w FROM o, LATERAL (SELECT w FROM i WHERE i.k = o.k ORDER BY w LIMIT 2) \
             AS v ORDER BY o.k, v.w"
        ),
        [Value::Integer(1), Value::Integer(1), Value::Integer(2)]
    );
}

/// A select list that reads the outer row, over a filter that also reads it with an equality.
///
/// The rule in `unnest.rs` that turns a correlated filter into an ordinary join has to stand aside
/// here, because the projection it would move to the right side of that join reads a column of the
/// left side. It used to fire anyway and the query came back as an internal error about a column
/// not being in a schema, which is #993. The general rule answers it.
#[test]
fn a_select_list_that_reads_the_outer_row_is_answered() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT o.k FROM i WHERE i.k = o.k AND i.w = 100) AS c FROM o ORDER BY k"
        ),
        [Value::Integer(1), Value::Null, Value::Null, Value::Null]
    );
}

/// The same with the outer column inside an expression rather than on its own.
#[test]
fn a_select_list_expression_over_both_rows_is_answered() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT o.k + i.w FROM i WHERE i.k = o.k AND i.w = 100) AS c \
             FROM o ORDER BY k"
        ),
        [Value::Integer(101), Value::Null, Value::Null, Value::Null]
    );
}

/// The same over a column the correlation does not read, so the outer row is used twice over.
#[test]
fn a_select_list_that_reads_a_second_outer_column_is_answered() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT o.t FROM i WHERE i.k = o.k AND i.w = 300) AS c FROM o ORDER BY k"
        ),
        [Value::Null, Value::Varchar("b".into()), Value::Null, Value::Null]
    );
}

/// An outer column beside an aggregate, which the grouping rule used to refuse.
///
/// An outer column is one value for the whole of the subquery, because the subquery is evaluated
/// once per outer row, so it is allowed wherever a grouped column is and needs no group of its own.
/// The binder was applying the grouping rule to it as though it came from the subquery's own `FROM`,
/// and the message it produced named no column at all. That is #995.
#[test]
fn an_outer_column_beside_an_aggregate_is_answered() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT max(w) + o.k FROM i WHERE i.k = o.k) AS c FROM o ORDER BY k"
        ),
        [Value::Integer(201), Value::Integer(302), Value::Null, Value::Null]
    );
}

/// An outer column alone in the select list of a query that groups by one of its own.
#[test]
fn an_outer_column_beside_a_group_by_is_answered() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT o.k FROM i WHERE i.k = o.k GROUP BY i.k) AS c FROM o ORDER BY k"
        ),
        [Value::Integer(1), Value::Integer(2), Value::Null, Value::Null]
    );
}

/// The same with no correlation in the `WHERE` at all, so the aggregate is over the whole table.
///
/// This one says the gap was the grouping check rather than the correlated rewrite: there is
/// nothing here for a rewrite to key on and it was refused all the same.
#[test]
fn an_outer_column_beside_an_uncorrelated_aggregate_is_answered() {
    let database = database();
    assert_eq!(
        answers(&database, "SELECT k, (SELECT max(w) + o.k FROM i) AS c FROM o ORDER BY k"),
        [Value::Integer(401), Value::Integer(402), Value::Integer(403), Value::Null]
    );
}

/// An outer column in a `HAVING`, which binds as a filter above the subquery's own aggregate.
///
/// That filter is the one the correlated filter rule matches, so the correlated filter the query
/// really has sits two nodes further down where nothing was looking, and the rule moved a subtree
/// that still read the outer row to the right side of an ordinary join.
#[test]
fn an_outer_column_in_a_having_is_answered() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT max(w) FROM i WHERE i.k = o.k HAVING max(w) > o.k) AS c \
             FROM o ORDER BY k"
        ),
        [Value::Integer(200), Value::Integer(300), Value::Null, Value::Null]
    );
}

/// An outer column beside a count, which takes a different rule again.
#[test]
fn an_outer_column_beside_a_count_is_answered() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT count(*) + o.k FROM i WHERE i.k = o.k) AS c FROM o ORDER BY k"
        ),
        [Value::BigInt(3), Value::BigInt(3), Value::BigInt(3), Value::Null]
    );
}

/// An outer column inside the aggregate's argument and again beside it.
///
/// The argument reading it is what sends this to the domain rule, and the projection reading it is
/// what that rule then has to carry, because the domain it builds is the only thing on the right
/// side of the join that knows the outer value.
#[test]
fn an_outer_column_inside_and_outside_an_aggregate_is_answered() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT max(i.w + o.k) + o.k FROM i WHERE i.k = o.k) AS c \
             FROM o ORDER BY k"
        ),
        [Value::Integer(202), Value::Integer(304), Value::Null, Value::Null]
    );
}

/// An outer column inside the aggregate's own argument, which never reached the check.
///
/// This one always answered, and it is here so that the tests above are read as the rest of a rule
/// rather than as a new one.
#[test]
fn an_outer_column_inside_an_aggregate_argument_is_answered() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT max(w + o.k) FROM i WHERE i.k = o.k) AS c FROM o ORDER BY k"
        ),
        [Value::Integer(201), Value::Integer(302), Value::Null, Value::Null]
    );
}

/// A column of this query's own `FROM` is still refused, and the message names it.
///
/// The slot for the column's name used to be filled with the words `a column` whenever the binding
/// was not in this query's scope, which is every outer column and is how `column a column must
/// appear in the GROUP BY clause` came to be a sentence this engine printed.
#[test]
fn an_ungrouped_column_of_this_query_is_still_refused_by_name() {
    let database = database();
    let error = database
        .query("SELECT t FROM o GROUP BY k")
        .expect_err("t is neither grouped nor aggregated");
    let text = error.to_string();
    assert!(text.contains("column \"t\" must appear in the GROUP BY clause"), "{text}");
}

/// A grouped count answers NULL for an outer row that matches no inner row, not zero.
///
/// The rule that puts a zero there exists because a scalar count over an empty input really is
/// zero, which an ordinary left join would get wrong. With a `GROUP BY` in the subquery that is not
/// what the query means: an empty input produces no groups at all, so the subquery returns no row
/// and a scalar subquery that returns no row is NULL. That is #1013.
#[test]
fn a_grouped_count_answers_null_for_an_outer_row_with_no_match() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT count(*) FROM i WHERE i.k = o.k GROUP BY i.k) AS c FROM o ORDER BY k"
        ),
        [Value::BigInt(2), Value::BigInt(1), Value::Null, Value::Null]
    );
}

/// The same with the group written on the outer column, where the invented group looked real.
///
/// The group expressions are rewritten to read the domain, so a group written on an outer column
/// takes the outer value even on a row the domain padded, which makes the group indistinguishable
/// from one the inner rows produced.
#[test]
fn a_count_grouped_on_the_outer_column_answers_null_with_no_match() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT count(*) FROM i WHERE i.k = o.k GROUP BY o.t) AS c FROM o ORDER BY k"
        ),
        [Value::BigInt(2), Value::BigInt(1), Value::Null, Value::Null]
    );
}

/// An ungrouped count still answers zero, which is the case the padded row is for.
#[test]
fn an_ungrouped_count_still_answers_zero_with_no_match() {
    let database = database();
    assert_eq!(
        answers(
            &database,
            "SELECT k, (SELECT count(*) FROM i WHERE i.k = o.k) AS c FROM o ORDER BY k"
        ),
        [Value::BigInt(2), Value::BigInt(1), Value::BigInt(0), Value::BigInt(0)]
    );
}

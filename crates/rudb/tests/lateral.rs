//! A FROM entry that reads the entries written to its left, which is what LATERAL asks for.
//!
//! Every answer and every message asserted here was read off the pinned duckdb on server2 first,
//! which is v2.0.0-dev84237 at cc7e7bac7f. The word itself changes nothing in rudb because a comma
//! separated FROM already resolves the entries to its left, so the tests are written in both forms
//! where the form is the point and in whichever one reads better where it is not.
//!
//! A lateral entry is bound as a dependent join and then lowered by the general rule in
//! `rudb-opt/src/domain.rs`, so what these check is that the binding produces a plan that rule has
//! an answer for, and that the two things the pinned build refuses are refused in its words.
//!
//! The outer table has a NULL key, because the lowering joins the domain back with a null safe
//! comparison and a row whose key is NULL has to come back with the rest rather than disappear.

use rudb::Database;
use rudb_common::Value;

/// The two tables every query below reads, with the rows already in them.
fn database() -> Database {
    let database = Database::new();
    for sql in [
        "CREATE TABLE o (k INTEGER, n INTEGER)",
        "INSERT INTO o VALUES (1, 2), (2, 3), (3, NULL), (NULL, 1)",
        "CREATE TABLE i (k INTEGER, w INTEGER)",
        "INSERT INTO i VALUES (1, 100), (1, 200), (2, 300), (NULL, 400)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// Both columns of every row, in the order the rows came.
fn pairs(database: &Database, sql: &str) -> Vec<(Value, Value)> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len()).map(|row| (result.value_at(row, 0), result.value_at(row, 1))).collect()
}

/// A pair of integers where neither is null, which most of these answer with.
fn pair(left: i32, right: i32) -> (Value, Value) {
    (Value::Integer(left), Value::Integer(right))
}

#[test]
fn a_lateral_subquery_sees_the_entry_written_to_its_left() {
    let database = database();
    assert_eq!(
        pairs(
            &database,
            "SELECT o.k, v.w FROM o, LATERAL (SELECT i.w FROM i WHERE i.k = o.k) v ORDER BY 1, 2"
        ),
        vec![pair(1, 100), pair(1, 200), pair(2, 300)]
    );
}

#[test]
fn the_word_lateral_changes_nothing_because_the_comma_form_already_does_it() {
    let database = database();
    let written = "SELECT o.k, v.w FROM o, LATERAL (SELECT i.w FROM i WHERE i.k = o.k) v \
                   ORDER BY 1, 2";
    let omitted = "SELECT o.k, v.w FROM o, (SELECT i.w FROM i WHERE i.k = o.k) v ORDER BY 1, 2";
    assert_eq!(pairs(&database, written), pairs(&database, omitted));
}

#[test]
fn an_outer_row_the_lateral_entry_answers_nothing_for_is_dropped() {
    let database = database();
    // The rows whose key is 3 and NULL match no row of `i`, and an inner join over a side with no
    // rows for them is the same as if they were not there.
    let answered: Vec<Value> = pairs(
        &database,
        "SELECT o.k, v.w FROM o, LATERAL (SELECT i.w FROM i WHERE i.k = o.k) v ORDER BY 1, 2",
    )
    .into_iter()
    .map(|(key, _)| key)
    .collect();
    assert_eq!(answered, vec![Value::Integer(1), Value::Integer(1), Value::Integer(2)]);
}

#[test]
fn a_left_join_lateral_keeps_the_outer_row_and_pads_it() {
    let database = database();
    assert_eq!(
        pairs(
            &database,
            "SELECT o.k, v.w FROM o LEFT JOIN LATERAL (SELECT i.w FROM i WHERE i.k = o.k) v \
             ON true ORDER BY 1, 2"
        ),
        vec![
            pair(1, 100),
            pair(1, 200),
            pair(2, 300),
            (Value::Integer(3), Value::Null),
            (Value::Null, Value::Null),
        ]
    );
}

#[test]
fn a_cross_join_lateral_is_the_comma_form_written_out() {
    let database = database();
    assert_eq!(
        pairs(
            &database,
            "SELECT o.k, v.w FROM o CROSS JOIN LATERAL (SELECT i.w FROM i WHERE i.k = o.k) v \
             ORDER BY 1, 2"
        ),
        vec![pair(1, 100), pair(1, 200), pair(2, 300)]
    );
}

#[test]
fn a_values_row_reading_the_outer_row_is_a_projection_over_the_domain() {
    let database = database();
    assert_eq!(
        pairs(&database, "SELECT o.k, v.w FROM o, LATERAL (VALUES (o.k * 3)) v(w) ORDER BY 1"),
        vec![pair(1, 3), pair(2, 6), pair(3, 9), (Value::Null, Value::Null)]
    );
}

#[test]
fn several_values_rows_reading_the_outer_row_each_answer_for_every_outer_row() {
    let database = database();
    assert_eq!(
        pairs(
            &database,
            "SELECT o.k, v.w FROM o, LATERAL (VALUES (o.k * 3), (o.k + 100)) v(w) ORDER BY 1, 2"
        ),
        vec![
            pair(1, 3),
            pair(1, 101),
            pair(2, 6),
            pair(2, 102),
            pair(3, 9),
            pair(3, 103),
            (Value::Null, Value::Null),
            (Value::Null, Value::Null),
        ]
    );
}

#[test]
fn a_values_in_from_without_the_parentheses_reads_the_outer_row_too() {
    let database = database();
    assert_eq!(
        pairs(&database, "SELECT o.k, col0 FROM o, VALUES (o.k * 3) ORDER BY 1"),
        vec![pair(1, 3), pair(2, 6), pair(3, 9), (Value::Null, Value::Null)]
    );
}

#[test]
fn a_lateral_entry_is_visible_to_the_lateral_entry_after_it() {
    let database = database();
    assert_eq!(
        pairs(
            &database,
            "SELECT o.k, y.b FROM o, LATERAL (SELECT o.k + 1 AS a) x, \
             LATERAL (SELECT x.a * 10 AS b) y ORDER BY 1"
        ),
        vec![pair(1, 20), pair(2, 30), pair(3, 40), (Value::Null, Value::Null)]
    );
}

#[test]
fn a_lateral_entry_may_group_by_the_outer_column_it_read() {
    let database = database();
    assert_eq!(
        pairs(
            &database,
            "SELECT o.k, v.c FROM o, \
             LATERAL (SELECT count(*) AS c FROM i WHERE i.k = o.k GROUP BY o.n) v ORDER BY 1"
        ),
        vec![(Value::Integer(1), Value::BigInt(2)), (Value::Integer(2), Value::BigInt(1))]
    );
}

#[test]
fn a_lateral_entry_may_aggregate_over_its_own_columns() {
    let database = database();
    // `max(i.w)` is an aggregate of a column the entry brought in itself, so it is not the thing
    // the refusal below is about, and an ungrouped one answers for every outer row including the
    // ones that matched nothing.
    assert_eq!(
        pairs(
            &database,
            "SELECT o.k, v.w FROM o, LATERAL (SELECT max(i.w) AS w FROM i WHERE i.k = o.k) v \
             ORDER BY 1"
        ),
        vec![
            pair(1, 200),
            pair(2, 300),
            (Value::Integer(3), Value::Null),
            (Value::Null, Value::Null),
        ]
    );
}

#[test]
fn a_lateral_entry_cannot_aggregate_the_column_it_read_from_the_left() {
    let database = database();
    // There is one left row per evaluation of the entry, so a sum over it would be a sum of one
    // value and whoever wrote it meant something else.
    let error = database
        .query("SELECT o.k, v.w FROM o, LATERAL (SELECT sum(o.k) AS w) v")
        .expect_err("an aggregate over a lateral column");
    assert_eq!(error.message(), "LATERAL join cannot contain aggregates!");
}

#[test]
fn a_right_join_lateral_is_refused_because_there_is_no_left_row_to_evaluate_against() {
    let database = database();
    let error = database
        .query("SELECT o.k, v.w FROM o RIGHT JOIN LATERAL (SELECT o.k AS w) v ON true")
        .expect_err("a right join with a lateral right side");
    assert_eq!(
        error.message(),
        "The combining JOIN type must be INNER or LEFT for a LATERAL reference"
    );
}

#[test]
fn a_full_join_lateral_is_refused_for_the_same_reason() {
    let database = database();
    let error = database
        .query("SELECT o.k, v.w FROM o FULL JOIN LATERAL (SELECT o.k AS w) v ON true")
        .expect_err("a full join with a lateral right side");
    assert_eq!(
        error.message(),
        "The combining JOIN type must be INNER or LEFT for a LATERAL reference"
    );
}

#[test]
fn a_lateral_entry_cannot_read_the_entry_written_after_it() {
    let database = database();
    // Left to right and not both ways. The name is looked for in the entries already bound and `o`
    // is not one of them yet.
    let error = database
        .query("SELECT o.k, v.w FROM LATERAL (SELECT o.k AS w) v, o")
        .expect_err("a lateral entry reading forwards");
    assert!(error.message().contains("Referenced table \"o\" not found"), "{error}");
}

#[test]
fn a_table_function_reading_a_lateral_column_says_so() {
    let database = database();
    // The pinned build answers this one. Its arguments are evaluated to produce the rows rather
    // than over rows that already exist, so there is nothing under it for the domain to be pushed
    // into, and this is the message until there is an operator that evaluates a source per value.
    let error = database
        .query("SELECT o.k, g.g FROM o, LATERAL generate_series(1, o.n) g(g)")
        .expect_err("a table function reading a lateral column");
    assert_eq!(error.message(), "a table function reading a LATERAL column");
}

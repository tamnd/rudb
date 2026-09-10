//! End to end tests, from a string of SQL to rows.
//!
//! The layers below have their own tests and this file does not repeat them. `rudb-parse` proves
//! the grammar, `rudb-bind` proves that a query binds to the plan it should, and `rudb-exec` proves
//! that a plan produces the right rows. What is only testable here is the three of them agreeing:
//! a name a query writes has to be the name the binder resolves and the name the executor reads,
//! and a type the binder decided has to be the type the operator produces.

use rudb_common::{Field, LogicalType, Value};

use crate::Database;

/// `t(x INTEGER, s VARCHAR)` with a null in it, plus an empty table to test the degenerate cases.
fn database() -> Database {
    let mut db = Database::new();
    db.create_table(
        "t",
        vec![Field::new("x", LogicalType::Integer), Field::new("s", LogicalType::Varchar)],
    )
    .unwrap();
    db.append(
        "t",
        &[
            vec![Value::Integer(3), Value::Varchar("a".to_string())],
            vec![Value::Integer(1), Value::Null],
            vec![Value::Integer(2), Value::Varchar("c".to_string())],
            vec![Value::Integer(1), Value::Varchar("a".to_string())],
        ],
    )
    .unwrap();
    db.create_table("empty", vec![Field::new("x", LogicalType::Integer)]).unwrap();
    db
}

/// Every row of a query, which is what most of these assert on.
fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql).unwrap().rows().collect()
}

/// The message a query fails with.
fn failure(db: &Database, sql: &str) -> String {
    db.query(sql).unwrap_err().message().to_string()
}

fn integer(value: i32) -> Value {
    Value::Integer(value)
}

fn text(value: &str) -> Value {
    Value::Varchar(value.to_string())
}

#[test]
fn a_star_reads_every_column_in_order() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT * FROM t"),
        vec![
            vec![integer(3), text("a")],
            vec![integer(1), Value::Null],
            vec![integer(2), text("c")],
            vec![integer(1), text("a")],
        ]
    );
}

/// The query from the crate documentation, which is the smallest thing that touches all four
/// layers.
#[test]
fn a_filter_keeps_the_rows_the_predicate_is_true_for() {
    let db = database();
    assert_eq!(rows(&db, "SELECT x FROM t WHERE x > 1"), vec![vec![integer(3)], vec![integer(2)]]);
}

/// A predicate that is null is not a predicate that is true, so the null row goes out. This is the
/// rule that separates SQL from every language whose `if` takes a boolean.
#[test]
fn a_null_predicate_drops_the_row() {
    let db = database();
    assert_eq!(rows(&db, "SELECT x FROM t WHERE s = 'c'"), vec![vec![integer(2)]]);
    assert_eq!(
        rows(&db, "SELECT x FROM t WHERE s <> 'c'"),
        vec![vec![integer(3)], vec![integer(1)]]
    );
}

#[test]
fn a_query_with_no_table_still_runs() {
    let db = database();
    assert_eq!(rows(&db, "SELECT 1 + 1"), vec![vec![integer(2)]]);
}

#[test]
fn an_expression_takes_the_name_it_was_aliased_to() {
    let db = database();
    let result = db.query("SELECT x + 10 AS bumped FROM t WHERE x = 2").unwrap();
    assert_eq!(result.names(), ["bumped"]);
    assert_eq!(result.types(), [LogicalType::Integer]);
    assert_eq!(result.value_at(0, 0), integer(12));
}

#[test]
fn an_aggregate_over_no_rows_is_zero_and_null() {
    let db = database();
    assert_eq!(rows(&db, "SELECT count(*) FROM empty"), vec![vec![Value::BigInt(0)]]);
    assert_eq!(rows(&db, "SELECT sum(x) FROM empty"), vec![vec![Value::Null]]);
}

/// `count(*)` counts rows and `count(s)` counts the rows where `s` is not null. One null in the
/// table is enough to tell them apart.
#[test]
fn count_star_and_count_of_a_column_disagree_about_nulls() {
    let db = database();
    assert_eq!(rows(&db, "SELECT count(*) FROM t"), vec![vec![Value::BigInt(4)]]);
    assert_eq!(rows(&db, "SELECT count(s) FROM t"), vec![vec![Value::BigInt(3)]]);
}

#[test]
fn a_group_by_produces_one_row_per_distinct_value() {
    let db = database();
    let mut answer = rows(&db, "SELECT x, count(*) FROM t GROUP BY x");
    answer.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(
        answer,
        vec![
            vec![integer(1), Value::BigInt(2)],
            vec![integer(2), Value::BigInt(1)],
            vec![integer(3), Value::BigInt(1)],
        ]
    );
}

#[test]
fn a_having_clause_filters_groups_rather_than_rows() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT x, count(*) FROM t GROUP BY x HAVING count(*) > 1"),
        vec![vec![integer(1), Value::BigInt(2)]]
    );
}

/// Direction and null placement are two decisions, and `DESC` alone puts nulls first in DuckDB.
#[test]
fn order_by_puts_nulls_where_the_clause_says() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT s FROM t ORDER BY s NULLS LAST"),
        vec![vec![text("a")], vec![text("a")], vec![text("c")], vec![Value::Null]]
    );
    assert_eq!(
        rows(&db, "SELECT s FROM t ORDER BY s NULLS FIRST"),
        vec![vec![Value::Null], vec![text("a")], vec![text("a")], vec![text("c")]]
    );
}

#[test]
fn order_by_and_limit_and_offset_compose() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT x FROM t ORDER BY x DESC LIMIT 2 OFFSET 1"),
        vec![vec![integer(2)], vec![integer(1)]]
    );
}

#[test]
fn distinct_collapses_equal_rows() {
    let db = database();
    let mut answer = rows(&db, "SELECT DISTINCT x FROM t");
    answer.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(answer, vec![vec![integer(1)], vec![integer(2)], vec![integer(3)]]);
}

#[test]
fn a_join_matches_on_its_condition() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT a.x, b.s FROM t AS a JOIN t AS b ON a.x = b.x WHERE a.x = 2"),
        vec![vec![integer(2), text("c")]]
    );
}

#[test]
fn a_left_join_keeps_the_left_row_and_pads_the_right() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT t.x, e.x FROM t LEFT JOIN empty AS e ON t.x = e.x WHERE t.x = 3"),
        vec![vec![integer(3), Value::Null]]
    );
}

#[test]
fn a_union_deduplicates_and_union_all_does_not() {
    let db = database();
    assert_eq!(rows(&db, "SELECT 1 UNION ALL SELECT 1"), vec![vec![integer(1)], vec![integer(1)]]);
    assert_eq!(rows(&db, "SELECT 1 UNION SELECT 1"), vec![vec![integer(1)]]);
}

#[test]
fn a_subquery_in_the_from_clause_is_a_table() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT inner_query.total FROM (SELECT count(*) AS total FROM t) AS inner_query"),
        vec![vec![Value::BigInt(4)]]
    );
}

#[test]
fn a_case_expression_picks_the_first_arm_that_holds() {
    let db = database();
    assert_eq!(
        rows(&db, "SELECT CASE WHEN x > 2 THEN 'big' ELSE 'small' END FROM t ORDER BY x"),
        vec![vec![text("small")], vec![text("small")], vec![text("small")], vec![text("big")]]
    );
}

#[test]
fn a_cast_that_cannot_hold_the_value_is_an_error_and_try_cast_is_null() {
    let db = database();
    assert!(failure(&db, "SELECT CAST('oops' AS INTEGER)").contains("oops"));
    assert_eq!(rows(&db, "SELECT TRY_CAST('oops' AS INTEGER)"), vec![vec![Value::Null]]);
}

#[test]
fn a_missing_table_names_the_table() {
    let db = database();
    assert!(failure(&db, "SELECT * FROM nope").contains("nope"));
}

#[test]
fn a_missing_column_names_the_column() {
    let db = database();
    assert!(failure(&db, "SELECT nope FROM t").contains("nope"));
}

#[test]
fn a_syntax_error_is_a_parser_error_rather_than_a_panic() {
    let db = database();
    let error = db.query("SELECT FROM WHERE").unwrap_err();
    assert_eq!(error.code(), rudb_common::ErrorCode::Parser);
}

/// The convenience path, and the check that it refuses anything that is not one cell.
#[test]
fn value_returns_one_cell_and_refuses_anything_else() {
    let db = database();
    assert_eq!(db.value("SELECT count(*) FROM t").unwrap(), Value::BigInt(4));
    assert!(db.value("SELECT * FROM t").is_err());
}

/// The plan text is the interface the optimizer tests are written against, so it is worth one test
/// here that a bound query prints in the form `rudb_plan` parses.
#[test]
fn a_plan_prints_parent_before_child() {
    let db = database();
    let text = db.plan("SELECT x FROM t WHERE x > 1").unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("Project"), "{text}");
    assert!(lines[1].trim_start().starts_with("Filter"), "{text}");
    assert!(lines[2].trim_start().starts_with("Get memory.main.t"), "{text}");
}

#[test]
fn creating_a_table_twice_is_an_error_and_dropping_it_makes_room_again() {
    let mut db = Database::new();
    db.create_table("t", vec![Field::new("x", LogicalType::Integer)]).unwrap();
    assert!(db.create_table("t", vec![Field::new("x", LogicalType::Integer)]).is_err());
    db.drop_table("t").unwrap();
    db.create_table("t", vec![Field::new("x", LogicalType::Integer)]).unwrap();
}

/// More rows than one vector holds, so the result is more than one chunk and the row indexing has
/// to walk across the boundary.
#[test]
fn a_result_wider_than_one_vector_reads_across_chunks() {
    let mut db = Database::new();
    db.create_table("big", vec![Field::new("x", LogicalType::Integer)]).unwrap();
    let mut rows = Vec::new();
    for x in 0..2_500 {
        rows.push(vec![Value::Integer(x)]);
    }
    db.append("big", &rows).unwrap();

    let result = db.query("SELECT x FROM big").unwrap();
    assert_eq!(result.len(), 2_500);
    assert!(result.chunks().len() > 1);
    assert_eq!(result.value_at(0, 0), integer(0));
    assert_eq!(result.value_at(1_500, 0), integer(1_500));
    assert_eq!(result.value_at(2_499, 0), integer(2_499));
    assert_eq!(result.row(2_500), None);
    assert_eq!(result.value_at(2_500, 0), Value::Null);
    assert_eq!(db.value("SELECT count(*) FROM big").unwrap(), Value::BigInt(2_500));
}

#[test]
fn a_table_can_be_named_with_its_schema_and_its_catalog() {
    let db = database();
    assert_eq!(db.value("SELECT count(*) FROM main.t").unwrap(), Value::BigInt(4));
    assert_eq!(db.value("SELECT count(*) FROM memory.main.t").unwrap(), Value::BigInt(4));
    assert_eq!(db.table_len("main.t").unwrap(), 4);
}

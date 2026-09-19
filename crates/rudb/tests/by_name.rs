//! `UNION BY NAME`, which lines the two sides up by column name rather than by position.
//!
//! Every answer asserted here was read off the pinned duckdb first, which is v2.0.0-dev84237 at
//! cc7e7bac7f.
//!
//! The result has the left side's columns in the order the left side wrote them, then the right
//! side's columns the left side did not write, in the order the right side wrote them. A column
//! only one side wrote is filled with a null on the other side, which is why the two sides do not
//! have to be the same width here and do have to be everywhere else. Names match without regard to
//! case and the spelling that comes out is the left side's.

use rudb::Database;
use rudb_common::Value;

/// The two tables the wider queries below read, with the rows already in them.
fn database() -> Database {
    let database = Database::new();
    for sql in [
        "CREATE TABLE p (x INTEGER, y INTEGER)",
        "INSERT INTO p VALUES (1, 2), (5, 6)",
        "CREATE TABLE q (y INTEGER, z INTEGER)",
        "INSERT INTO q VALUES (3, 4)",
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

/// The names a query's result columns come out under.
fn names(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result.names().to_vec()
}

/// The message a query is refused with.
fn refused(database: &Database, sql: &str) -> String {
    match database.query(sql) {
        Ok(_) => panic!("{sql} was expected to be refused"),
        Err(error) => error.to_string(),
    }
}

/// A row of integers, with `None` for a null.
fn ints(values: &[Option<i32>]) -> Vec<Value> {
    values.iter().map(|value| value.map_or(Value::Null, Value::Integer)).collect()
}

/// The smallest shape, where both sides wrote the same two names the other way round.
#[test]
fn the_two_sides_line_up_by_name_and_not_by_position() {
    let database = database();
    assert_eq!(
        rows(&database, "SELECT 1 AS a, 2 AS b UNION ALL BY NAME SELECT 3 AS b, 4 AS a ORDER BY a"),
        vec![ints(&[Some(1), Some(2)]), ints(&[Some(4), Some(3)])]
    );
}

/// A name the right side did not write, which it fills with a null.
#[test]
fn a_column_the_right_side_did_not_write_is_filled_with_a_null() {
    let database = database();
    assert_eq!(
        rows(&database, "SELECT 1 AS a, 2 AS b UNION ALL BY NAME SELECT 3 AS a ORDER BY a"),
        vec![ints(&[Some(1), Some(2)]), ints(&[Some(3), None])]
    );
}

/// A name the left side did not write, which goes on the end rather than being sorted in.
#[test]
fn a_column_only_the_right_side_wrote_goes_on_the_end() {
    let database = database();
    let sql = "SELECT 1 AS a UNION ALL BY NAME SELECT 3 AS a, 4 AS c ORDER BY a";
    assert_eq!(names(&database, sql), vec!["a".to_string(), "c".to_string()]);
    assert_eq!(rows(&database, sql), vec![ints(&[Some(1), None]), ints(&[Some(3), Some(4)])]);
}

/// Two sides with no name in common at all, which is the widest the result gets.
#[test]
fn two_sides_that_share_no_name_come_out_as_both_sides_columns() {
    let database = database();
    let sql = "SELECT 1 AS a UNION ALL BY NAME SELECT 4 AS c ORDER BY a NULLS LAST";
    assert_eq!(names(&database, sql), vec!["a".to_string(), "c".to_string()]);
    assert_eq!(rows(&database, sql), vec![ints(&[Some(1), None]), ints(&[None, Some(4)])]);
}

/// Names match without regard to case, and the spelling that comes out is the left side's, which
/// is what the rest of the engine does with an identifier.
#[test]
fn names_match_without_regard_to_case_and_keep_the_left_sides_spelling() {
    let database = database();
    let sql = "SELECT 1 AS a, 2 AS b UNION ALL BY NAME SELECT 3 AS B, 4 AS A ORDER BY a";
    assert_eq!(names(&database, sql), vec!["a".to_string(), "b".to_string()]);
    assert_eq!(rows(&database, sql), vec![ints(&[Some(1), Some(2)]), ints(&[Some(4), Some(3)])]);
    let quoted = "SELECT 1 AS \"Ab\" UNION ALL BY NAME SELECT 2 AS \"aB\" ORDER BY 1";
    assert_eq!(names(&database, quoted), vec!["Ab".to_string()]);
    assert_eq!(rows(&database, quoted), vec![ints(&[Some(1)]), ints(&[Some(2)])]);
}

/// A star on each side, which is the shape somebody actually writes: two tables that share one
/// column and each have one of their own.
#[test]
fn a_star_on_each_side_is_matched_column_by_column() {
    let database = database();
    let sql = "SELECT * FROM p UNION ALL BY NAME SELECT * FROM q ORDER BY y";
    assert_eq!(names(&database, sql), vec!["x".to_string(), "y".to_string(), "z".to_string()]);
    assert_eq!(
        rows(&database, sql),
        vec![
            ints(&[Some(1), Some(2), None]),
            ints(&[None, Some(3), Some(4)]),
            ints(&[Some(5), Some(6), None]),
        ]
    );
}

/// Three branches, which is two operations because the chain is left associative, so the middle
/// result is what the third branch is matched against.
#[test]
fn a_third_branch_is_matched_against_what_the_first_two_came_out_with() {
    let database = database();
    let sql = "SELECT 1 AS a UNION ALL BY NAME SELECT 2 AS b UNION ALL BY NAME SELECT 3 AS c \
               ORDER BY a NULLS LAST, b NULLS LAST";
    assert_eq!(names(&database, sql), vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    assert_eq!(
        rows(&database, sql),
        vec![
            ints(&[Some(1), None, None]),
            ints(&[None, Some(2), None]),
            ints(&[None, None, Some(3)]),
        ]
    );
}

/// `UNION BY NAME` without `ALL` still removes duplicates, and a row whose only difference is a
/// column one side filled with a null is a duplicate of a row that wrote the null itself.
#[test]
fn a_union_by_name_that_is_not_all_removes_duplicates_across_the_filled_nulls() {
    let database = database();
    assert_eq!(
        rows(&database, "SELECT 1 AS a UNION BY NAME SELECT 1 AS a, NULL AS b"),
        vec![ints(&[Some(1), None])]
    );
}

/// An `ORDER BY` and a `LIMIT` over the result, where the sort key is a column only one side
/// wrote, so most of what it sorts is the nulls the other side was filled with.
#[test]
fn the_result_can_be_sorted_and_limited_on_a_column_only_one_side_wrote() {
    let database = database();
    assert_eq!(
        rows(&database, "SELECT * FROM p UNION ALL BY NAME SELECT * FROM q ORDER BY y LIMIT 2"),
        vec![ints(&[Some(1), Some(2), None]), ints(&[None, Some(3), Some(4)])]
    );
}

/// Each side grouped, which is worth a case because the column a grouped side hands up is an
/// aggregate rather than a column of a table and the name it carries is the alias.
#[test]
fn each_side_can_be_a_grouped_query() {
    let database = database();
    assert_eq!(
        rows(
            &database,
            "SELECT sum(x) AS s FROM p UNION ALL BY NAME SELECT max(z) AS m FROM q ORDER BY s"
        ),
        vec![vec![Value::HugeInt(6), Value::Null], vec![Value::Null, Value::Integer(4)]]
    );
}

/// A side that wrote one name twice, which is refused, because matching by name needs the name to
/// say which column and that side has no answer to give. The doubled quotes are the pin's.
#[test]
fn a_side_that_wrote_one_name_twice_is_refused() {
    let database = database();
    for sql in [
        "SELECT 1 AS a, 2 AS a UNION ALL BY NAME SELECT 3 AS a",
        "SELECT 1 AS a UNION ALL BY NAME SELECT 2 AS a, 3 AS a",
        "SELECT * FROM p, q UNION ALL BY NAME SELECT * FROM q",
    ] {
        let message = refused(&database, sql);
        assert!(
            message.contains(
                "UNION (ALL) BY NAME operation doesn't support duplicate names in the SELECT list"
            ),
            "{sql}: {message}"
        );
        assert!(message.contains("occurs multiple times"), "{sql}: {message}");
    }
}

/// `BY NAME` goes with `UNION` and with nothing else. The grammar takes it after `EXCEPT` because
/// the two share a clause, so that pairing is refused by name, and `INTERSECT BY NAME` does not
/// parse at all, which is both of them the way the pin has them.
#[test]
fn by_name_is_refused_on_every_operator_but_union() {
    let database = database();
    let message = refused(&database, "SELECT 1 AS a EXCEPT BY NAME SELECT 1 AS a");
    assert!(message.contains("Invalid combination of EXCEPT and BY NAME"), "{message}");
    let message = refused(&database, "SELECT 1 AS a INTERSECT BY NAME SELECT 1 AS a");
    assert!(message.contains("syntax error"), "{message}");
}

/// An ordinary union is still matched by position, still takes its names from the left side, and
/// still needs the two sides to be the same width. This is the control for the whole file.
#[test]
fn an_ordinary_union_is_still_matched_by_position() {
    let database = database();
    let sql = "SELECT 1 AS a, 2 AS b UNION ALL SELECT 3 AS b, 4 AS a ORDER BY a";
    assert_eq!(names(&database, sql), vec!["a".to_string(), "b".to_string()]);
    assert_eq!(rows(&database, sql), vec![ints(&[Some(1), Some(2)]), ints(&[Some(3), Some(4)])]);
    let message = refused(&database, "SELECT 1 AS a UNION ALL SELECT 2 AS a, 3 AS b");
    assert!(message.contains("same number of result columns"), "{message}");
}

//! What a `LIMIT` and an `OFFSET` are allowed to be written as, and what each one comes to.
//!
//! Every answer asserted here was read off the pinned duckdb first, which is v2.0.0-dev84237 at
//! cc7e7bac7f.
//!
//! The rule is that the clause takes any expression whose value is settled before the first row is
//! read, and that the value is then cast to `BIGINT` whatever it was written as. So `LIMIT 1 + 1`
//! is two rows, `LIMIT '3'` is three, `LIMIT 2.5` is three because the conversion rounds, and
//! `LIMIT DATE '2020-01-01'` is the cast refusing a date rather than a rule of its own.

use rudb::Database;
use rudb_common::Value;

/// Ten rows numbered nought to nine, which every case below limits or offsets.
fn database() -> Database {
    let database = Database::new();
    for sql in [
        "CREATE TABLE t (i INTEGER)",
        "INSERT INTO t VALUES (0), (1), (2), (3), (4), (5), (6), (7), (8), (9)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// The numbers a query answers, in the order it answers them.
fn numbers(database: &Database, sql: &str) -> Vec<i32> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| match result.value_at(row, 0) {
            Value::Integer(number) => number,
            other => panic!("{sql} answered {other:?}"),
        })
        .collect()
}

/// The message a query is refused with.
fn refused(database: &Database, sql: &str) -> String {
    match database.query(sql) {
        Ok(_) => panic!("{sql} was expected to be refused"),
        Err(error) => error.to_string(),
    }
}

/// Arithmetic, which is the shape somebody writes without thinking of it as an expression at all.
#[test]
fn a_limit_and_an_offset_can_be_worked_out_rather_than_written_down() {
    let database = database();
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 1 + 1"), vec![0, 1]);
    assert_eq!(numbers(&database, "SELECT i FROM t OFFSET 3 + 4"), vec![7, 8, 9]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 1 + 1 OFFSET 3 + 4"), vec![7, 8]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT abs(-2)"), vec![0, 1]);
}

/// A cast, which is how a caller that has a count in hand spells the type it wants it read as.
#[test]
fn a_cast_is_worked_out_the_same_way() {
    let database = database();
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT CAST(2 AS BIGINT)"), vec![0, 1]);
    assert_eq!(numbers(&database, "SELECT i FROM t OFFSET CAST(8 AS BIGINT)"), vec![8, 9]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 2::UTINYINT"), vec![0, 1]);
}

/// Whatever the value was written as, it is cast to `BIGINT` and that is the whole of the type rule.
///
/// A string converts, a decimal rounds rather than truncating, and a boolean is one row or none.
/// None of those is a rule about `LIMIT`. They are what casting that value to `BIGINT` does, which
/// is why the clause does no type checking of its own.
#[test]
fn the_value_is_cast_to_a_row_count_whatever_it_was_written_as() {
    let database = database();
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT '3'"), vec![0, 1, 2]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 2.5"), vec![0, 1, 2]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 2.4"), vec![0, 1]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT true"), vec![0]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT false"), Vec::<i32>::new());
}

/// A `CASE`, where the arm that does not fire is not evaluated.
///
/// `1 // 0` in an arm the condition excludes is not a division by zero, here or at run time. It is
/// worth a case of its own because working the value out arm by arm is the only way to get it, and
/// a version that evaluated every arm first would pass everything else in this file.
#[test]
fn an_arm_of_a_case_that_does_not_fire_is_not_evaluated() {
    let database = database();
    let sql = "SELECT i FROM t LIMIT CASE WHEN 1 = 1 THEN 2 ELSE 1 // 0 END";
    assert_eq!(numbers(&database, sql), vec![0, 1]);
    let sql = "SELECT i FROM t LIMIT CASE WHEN 1 = 2 THEN 1 // 0 ELSE 2 END";
    assert_eq!(numbers(&database, sql), vec![0, 1]);
}

/// A null is no limit at all, which is what leaving the clause off already means.
#[test]
fn a_null_limit_or_offset_is_the_same_as_not_writing_one() {
    let database = database();
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT NULL").len(), 10);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT CAST(NULL AS INTEGER)").len(), 10);
    assert_eq!(numbers(&database, "SELECT i FROM t OFFSET NULL").len(), 10);
}

/// A negative count is refused for both clauses, with one message that names both of them.
#[test]
fn a_negative_row_count_is_refused() {
    let database = database();
    for sql in [
        "SELECT i FROM t LIMIT -1",
        "SELECT i FROM t OFFSET -1",
        "SELECT i FROM t LIMIT 1 - 5",
        "SELECT i FROM t OFFSET 1 - 5",
    ] {
        let message = refused(&database, sql);
        assert!(message.contains("LIMIT/OFFSET cannot be negative"), "{sql}: {message}");
    }
}

/// A value the cast cannot take is the cast's own refusal and not a message written here.
#[test]
fn a_value_that_is_not_a_row_count_is_refused_by_the_cast() {
    let database = database();
    let message = refused(&database, "SELECT i FROM t LIMIT DATE '2020-01-01'");
    assert!(message.contains("Unimplemented type for cast (DATE -> BIGINT)"), "{message}");
    let message = refused(&database, "SELECT i FROM t LIMIT INTERVAL 3 DAY");
    assert!(message.contains("Unimplemented type for cast (INTERVAL -> BIGINT)"), "{message}");
    let message = refused(&database, "SELECT i FROM t LIMIT 'abc'");
    assert!(message.contains("Could not convert string 'abc' to INT64"), "{message}");
    let message = refused(&database, "SELECT i FROM t LIMIT 99999999999999999999");
    assert!(message.contains("out of range for the destination type INT64"), "{message}");
}

/// Working the value out can raise, and then the error is the answer.
///
/// This is the one place the binder and the folding pass want opposite things. The pass abandons a
/// fold that raises and leaves the expression for the executor to fail on, because there is a plan
/// either way. Here there is no row count for the node to hold, so the query fails while it is
/// being planned, which is where the pin fails it too.
#[test]
fn a_row_count_that_raises_fails_the_query() {
    let database = database();
    let message = refused(&database, "SELECT i FROM t LIMIT 1 // 0");
    assert!(message.contains("Division by zero"), "{message}");
}

/// A column is refused because there is no `FROM` clause to find it in, which is the ordinary
/// binder error and not one this clause writes.
#[test]
fn a_row_count_naming_a_column_is_refused_like_any_other_unknown_name() {
    let database = database();
    let message = refused(&database, "SELECT i FROM t LIMIT i");
    assert!(message.contains("Referenced column \"i\""), "{message}");
}

/// A subquery is the one shape left over, because the plan node holds a number and not an
/// expression. The pin answers it by reading the value while the query runs.
#[test]
fn a_row_count_holding_a_subquery_says_so() {
    let database = database();
    for sql in ["SELECT i FROM t LIMIT (SELECT 3)", "SELECT i FROM t OFFSET (SELECT 3)"] {
        let message = refused(&database, sql);
        assert!(message.contains("holding a subquery"), "{sql}: {message}");
    }
}

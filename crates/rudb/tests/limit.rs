//! What a `LIMIT` and an `OFFSET` are allowed to be written as, and what each one comes to.
//!
//! Every answer asserted here was read off the pinned duckdb first, which is v2.0.0-dev84237 at
//! cc7e7bac7f.
//!
//! The rule is that the clause takes any expression whose value is settled before the first row is
//! read, and that the value is then cast to `BIGINT` whatever it was written as. So `LIMIT 1 + 1`
//! is two rows, `LIMIT '3'` is three, `LIMIT 2.5` is three because the conversion rounds, and
//! `LIMIT DATE '2020-01-01'` is the cast refusing a date rather than a rule of its own.
//!
//! An expression that is not settled before the first row is read goes down a second path. A
//! subquery has to run before there is a value and a call such as `RANDOM()` answers differently
//! every time it is made, so the binder joins the value in under the limit as a column of every row
//! and the limit reads it off the first chunk that reaches it. The number is read once and used for
//! the rest of the query, and the column goes again above the limit.
//!
//! A percentage is the same evaluation with a different cast at the end of it and a different node
//! under it. The value goes to `DOUBLE`, the row count is a share of the input rounded down, and
//! the offset is applied after the share rather than to it.

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

/// A share of the input rather than a row count, which is a node of its own.
///
/// The count rounds down, which is the whole of the arithmetic: three and a half rows is three, half
/// a row is none, and a hundred percent is every row.
#[test]
fn a_limit_can_be_a_share_of_the_input_rather_than_a_row_count() {
    let database = database();
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 30 PERCENT"), vec![0, 1, 2]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 35 PERCENT"), vec![0, 1, 2]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 5 PERCENT"), Vec::<i32>::new());
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 0 PERCENT"), Vec::<i32>::new());
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 100 PERCENT").len(), 10);
}

/// The share is of the whole input and the offset is applied after it, not to it.
///
/// `LIMIT 30 PERCENT OFFSET 2` over ten rows is three rows starting at the third, so it runs past
/// the point a share of the eight remaining rows would have stopped at.
#[test]
fn the_offset_of_a_share_is_applied_after_the_share_is_worked_out() {
    let database = database();
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 30 PERCENT OFFSET 2"), vec![2, 3, 4]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT 50 PERCENT OFFSET 8"), vec![8, 9]);
    let sql = "SELECT i FROM t LIMIT 30 PERCENT OFFSET 20";
    assert_eq!(numbers(&database, sql), Vec::<i32>::new());
}

/// The `%` sign takes an expression where the `PERCENT` word takes a literal, and the value is cast
/// to `DOUBLE` rather than to `BIGINT`.
///
/// `LIMIT true%` is one percent and not one row, which is the shortest way to see that the two
/// spellings go through different casts.
#[test]
fn a_share_written_with_a_percent_sign_is_worked_out_like_any_other_expression() {
    let database = database();
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT (10 + 20)%"), vec![0, 1, 2]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT '30'%"), vec![0, 1, 2]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT true%"), Vec::<i32>::new());
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT NULL%").len(), 10);
}

/// A share outside nought to a hundred is refused while the query is planned.
///
/// The pin fails an `EXPLAIN` of it, so this is a plan time refusal there too and not something the
/// operator discovers when the rows arrive. `NAN` is outside the range like anything else that is
/// not between the two ends.
#[test]
fn a_share_that_is_not_between_nought_and_a_hundred_is_refused() {
    let database = database();
    for sql in [
        "SELECT i FROM t LIMIT 101 PERCENT",
        "SELECT i FROM t LIMIT 100.5 PERCENT",
        "SELECT i FROM t LIMIT -10%",
        "SELECT i FROM t LIMIT ('nan'::DOUBLE)%",
    ] {
        let message = refused(&database, sql);
        assert!(message.contains("Limit percent out of range"), "{sql}: {message}");
    }
}

/// A subquery is the shape that cannot be worked out while the query is planned, because it has to
/// run first. It is read while the query runs instead, which is what the pin does with it.
#[test]
fn a_row_count_can_be_a_subquery() {
    let database = database();
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT (SELECT 3)"), vec![0, 1, 2]);
    assert_eq!(numbers(&database, "SELECT i FROM t OFFSET (SELECT 8)"), vec![8, 9]);
    let sql = "SELECT i FROM t LIMIT (SELECT 3) OFFSET (SELECT 2)";
    assert_eq!(numbers(&database, sql), vec![2, 3, 4]);
    let sql = "SELECT i FROM t LIMIT (SELECT count(*) FROM t WHERE i < 4)";
    assert_eq!(numbers(&database, sql), vec![0, 1, 2, 3]);
}

/// The column the value is read out of is the query's to see or not, and it is not.
///
/// A star is the case that says so, since a star over a limit holding a subquery would otherwise
/// answer the column the subquery was joined in as well as the columns the table has.
#[test]
fn the_column_the_count_is_read_out_of_is_not_a_column_of_the_answer() {
    let database = database();
    let sql = "SELECT * FROM t LIMIT (SELECT 2)";
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    assert_eq!(result.width(), 1, "{sql} answered an extra column");
    assert_eq!(numbers(&database, sql), vec![0, 1]);
    assert_eq!(numbers(&database, "VALUES (7), (8), (9) LIMIT (SELECT 2)"), vec![7, 8]);
    let sql = "SELECT i FROM t UNION ALL SELECT i FROM t LIMIT (SELECT 3)";
    assert_eq!(numbers(&database, sql), vec![0, 1, 2]);
}

/// A subquery that answers no row is no limit at all, which is the same rule a written null has.
#[test]
fn a_subquery_that_answers_nothing_or_null_leaves_every_row() {
    let database = database();
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT (SELECT NULL)").len(), 10);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT (SELECT i FROM t WHERE false)").len(), 10);
    assert_eq!(numbers(&database, "SELECT i FROM t OFFSET (SELECT NULL)").len(), 10);
}

/// The same cast as a row count written out, and so the same failures.
///
/// The pin casts this one to `UINT64` rather than to `BIGINT` and so writes three different
/// messages here from the three it writes for the same values spelled without the subquery. That is
/// duckdb #9 in our fork rather than something to copy, because a limit that reads differently
/// depending on which of two paths worked it out is a wrong answer waiting to be found.
#[test]
fn a_row_count_read_while_the_query_runs_is_cast_the_way_a_written_one_is() {
    let database = database();
    for (sql, expected) in [
        ("SELECT i FROM t LIMIT (SELECT -1)", "LIMIT/OFFSET cannot be negative"),
        ("SELECT i FROM t OFFSET (SELECT -1)", "LIMIT/OFFSET cannot be negative"),
        ("SELECT i FROM t LIMIT (SELECT 'abc')", "Could not convert string 'abc' to INT64"),
        ("SELECT i FROM t LIMIT (SELECT DATE '2020-01-01')", "(DATE -> BIGINT)"),
    ] {
        let message = refused(&database, sql);
        assert!(message.contains(expected), "{sql}: {message}");
    }
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT (SELECT '3')"), vec![0, 1, 2]);
    assert_eq!(numbers(&database, "SELECT i FROM t LIMIT (SELECT true)"), vec![0]);
}

/// A sort under a limit holding a subquery stays a sort, where one under a written count does not.
///
/// A top n keeps the smallest count plus offset rows it has seen, and there is no such number to
/// keep when it only turns up once the query is running. The rows are the same either way, so what
/// this is pinning is the plan and not the answer, and the pin plans it the same way.
#[test]
fn a_row_count_read_while_the_query_runs_does_not_become_a_top_n() {
    let database = database();
    let sql = "SELECT i FROM t ORDER BY i DESC LIMIT (SELECT 3)";
    assert_eq!(numbers(&database, sql), vec![9, 8, 7]);
    let plan = database.plan(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    assert!(plan.contains("Sort"), "{sql} planned as {plan}");
    assert!(!plan.contains("TopN"), "{sql} planned as {plan}");
}

/// A share written as a subquery is the one shape left over, because the node holds a number.
#[test]
fn a_share_holding_a_subquery_says_so() {
    let database = database();
    for sql in
        ["SELECT i FROM t LIMIT (SELECT 30)%", "SELECT i FROM t LIMIT 30 PERCENT OFFSET (SELECT 2)"]
    {
        let message = refused(&database, sql);
        assert!(message.contains("subquery"), "{sql}: {message}");
    }
}

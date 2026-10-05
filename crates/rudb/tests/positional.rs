//! `#1`, `#2` and so on, which name a column of the `FROM` clause by where it is. Every expected
//! answer here was taken from the pinned duckdb binary, v2.0.0-dev84237, except the macro ones,
//! where the pin disagrees with its own test file and the test file is followed.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let mut rows: Vec<String> = result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect();
    rows.sort();
    rows
}

fn names(database: &Database, sql: &str) -> Vec<String> {
    let result = database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result.names().to_vec()
}

fn refused(database: &Database, sql: &str, expected: &str) {
    let error = database.execute(sql).expect_err(sql);
    assert!(error.to_string().starts_with(expected), "{sql}: {error}");
}

#[test]
fn a_positional_reference_reads_the_column_in_that_place() {
    let database = Database::new();
    let sql = "SELECT #1 + 1, #1, #2 FROM (VALUES (42, 1)) AS t(a, b)";
    assert_eq!(answered(&database, sql), ["43|42|1"]);
    assert_eq!(names(&database, sql), ["(#1 + 1)", "a", "b"]);
    assert_eq!(answered(&database, "SELECT a FROM (SELECT #1 FROM (VALUES (42)) AS t(a))"), ["42"]);
    let sql = "SELECT #2 FROM (VALUES (1)) t(a), (VALUES (2)) u(b)";
    assert_eq!(answered(&database, sql), ["2"]);
    let sql = "SELECT a FROM (VALUES (1), (2)) t(a) WHERE #1 = 1";
    assert_eq!(answered(&database, sql), ["1"]);
    let sql = "SELECT #1, count(*) FROM (VALUES (1), (1)) t(a) GROUP BY #1";
    assert_eq!(answered(&database, sql), ["1|2"]);
}

#[test]
fn both_copies_of_a_column_joined_on_with_using_are_counted() {
    let database = Database::new();
    let using = "FROM (VALUES (1, 3)) t(k, a) JOIN (VALUES (1, 2)) u(k, b) USING (k)";
    assert_eq!(answered(&database, &format!("SELECT #2 {using}")), ["3"]);
    assert_eq!(answered(&database, &format!("SELECT #3 {using}")), ["1"]);
    assert_eq!(answered(&database, &format!("SELECT #4 {using}")), ["2"]);
}

#[test]
fn a_positional_reference_can_be_a_macro_argument() {
    let database = Database::new();
    database.execute("CREATE MACRO subtract_args(x, y) AS x - y").expect("the macro");
    let sql = "SELECT subtract_args(#1, #2), #1 - #2 FROM (VALUES (10, 3)) AS t(y, x)";
    assert_eq!(answered(&database, sql), ["7|7"]);
    let sql = "SELECT subtract_args(y := #1, x := #2) FROM (VALUES (10, 3)) AS t(y, x)";
    assert_eq!(answered(&database, sql), ["-7"]);
    let sql = "SELECT subtract_args(#1, #2) FROM (VALUES (10, 3)) AS t(a, b)";
    assert_eq!(answered(&database, sql), ["7"]);
}

#[test]
fn the_pin_refuses_what_it_refuses_in_its_words() {
    let database = Database::new();
    refused(
        &database,
        "SELECT #3 FROM (VALUES (10, 3)) AS t(y, x)",
        "Binder Error: Positional reference 3 out of range (total 2 columns)",
    );
    refused(
        &database,
        "SELECT #1",
        "Binder Error: Positional reference 1 out of range (total 0 columns)",
    );
    refused(
        &database,
        "SELECT (SELECT #1) FROM (VALUES (1)) t(a)",
        "Binder Error: Positional reference 1 out of range (total 0 columns)",
    );
    refused(
        &database,
        "SELECT #0 FROM (VALUES (42)) AS t(a)",
        "Parser Error: Positional reference node needs to be >= 1",
    );
}

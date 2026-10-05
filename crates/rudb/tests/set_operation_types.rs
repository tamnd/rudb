//! The type a column of a `UNION`, `EXCEPT` or `INTERSECT` comes out as when its two sides wrote
//! types with nothing in common. Every expected answer here was taken from the pinned duckdb
//! binary, v2.0.0-dev84237, which forces the two to the one it ranks higher and lets a row of the
//! other side fail its cast.

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

fn refused(database: &Database, sql: &str, expected: &str) {
    let error = database.execute(sql).expect_err(sql);
    assert!(error.to_string().starts_with(expected), "{sql}: {error}");
}

/// The type the one column of `sql` is described as, without running it.
fn described(database: &Database, sql: &str) -> String {
    answered(database, &format!("SELECT column_type FROM (DESCRIBE {sql})")).join(",")
}

#[test]
fn two_types_with_nothing_in_common_are_forced_to_the_higher_ranked_one() {
    let database = Database::new();
    for (sql, expected) in [
        ("SELECT 1 a UNION ALL SELECT 'x'", "VARCHAR"),
        ("SELECT 'x' a UNION SELECT 1", "VARCHAR"),
        ("SELECT 1 a EXCEPT SELECT 'x'", "VARCHAR"),
        ("SELECT true a UNION ALL SELECT 1", "INTEGER"),
        ("SELECT 1.5 a UNION ALL SELECT true", "DECIMAL(2,1)"),
        ("SELECT 1 a UNION ALL SELECT DATE '2000-01-01'", "DATE"),
        ("SELECT DATE '2000-01-01' a UNION ALL SELECT 'x'", "VARCHAR"),
        ("SELECT 'x'::BLOB a UNION ALL SELECT 'x'", "BLOB"),
        ("SELECT 'x' a UNION ALL SELECT gen_random_uuid()", "UUID"),
        ("SELECT '0101'::BIT a UNION ALL SELECT 'x'::BLOB", "BLOB"),
        ("SELECT '1 day'::INTERVAL a UNION ALL SELECT now()", "INTERVAL"),
        ("SELECT '12:00'::TIMETZ a UNION ALL SELECT '12:00'::TIME", "TIME WITH TIME ZONE"),
        ("SELECT '12:00'::TIME a UNION ALL SELECT '12:00'::TIMETZ", "TIME"),
        ("SELECT [1] a UNION ALL SELECT {'a': 1}", "INTEGER[]"),
        ("SELECT MAP {1: 2} a UNION ALL SELECT [1]", "MAP(INTEGER, INTEGER)"),
        ("SELECT 'a'::ENUM('a') a UNION ALL SELECT 1", "VARCHAR"),
        ("SELECT 1 a UNION ALL SELECT '{}'::JSON", "JSON"),
    ] {
        assert_eq!(described(&database, sql), expected, "{sql}");
    }
}

#[test]
fn nested_types_are_forced_child_by_child() {
    let database = Database::new();
    for (sql, expected) in [
        ("SELECT [1] a UNION ALL SELECT ['x']", "VARCHAR[]"),
        ("SELECT [{'a': 1}] a UNION ALL SELECT [{'a': 'x'}]", "STRUCT(a VARCHAR)[]"),
        ("SELECT MAP {1: 2} a UNION ALL SELECT MAP {'x': 'y'}", "MAP(VARCHAR, VARCHAR)"),
        (
            "SELECT {'a': 1, 'b': 2} a UNION ALL SELECT {'a': 'x', 'c': 2}",
            "STRUCT(a VARCHAR, b INTEGER, c INTEGER)",
        ),
        ("SELECT row(1, 'x') a UNION ALL SELECT row('x', 1)", "TUPLE(VARCHAR, VARCHAR)"),
    ] {
        assert_eq!(described(&database, sql), expected, "{sql}");
    }
    let sql = "SELECT * FROM (SELECT {'a': {'e1': 42, 'e2': 42}} AS c \
               UNION ALL BY NAME SELECT {'a': {'e2': 'hello', 'e3': 'world'}, 'b': '100'} AS c)";
    assert_eq!(
        answered(&database, sql),
        [
            "{'a': {'e1': 42, 'e2': 42, 'e3': NULL}, 'b': NULL}",
            "{'a': {'e1': NULL, 'e2': hello, 'e3': world}, 'b': 100}",
        ]
    );
}

#[test]
fn a_row_that_cannot_be_cast_fails_and_a_side_with_no_rows_does_not() {
    let database = Database::new();
    assert_eq!(answered(&database, "SELECT 1 a UNION ALL SELECT 'asdf'"), ["1", "asdf"]);
    let sql = "SELECT 1 a WHERE false UNION ALL SELECT DATE '2000-01-01'";
    assert_eq!(answered(&database, sql), ["2000-01-01"]);
    let sql = "SELECT count(*) FROM (SELECT NULL::INTEGER a UNION ALL SELECT DATE '2000-01-01')";
    assert_eq!(answered(&database, sql), ["2"]);
    refused(
        &database,
        "SELECT count(*) FROM (SELECT 1 a UNION ALL SELECT DATE '2000-01-01')",
        "Conversion Error: Unimplemented type for cast (INTEGER -> DATE)",
    );
    refused(
        &database,
        "SELECT {'a': 1} a UNION ALL SELECT row(1, 2)",
        "Mismatch Type Error: Type TUPLE(INTEGER, INTEGER) does not match with STRUCT(a INTEGER). \
         Cannot cast STRUCTs of different size",
    );
}

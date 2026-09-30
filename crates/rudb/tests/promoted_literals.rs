//! A string literal handed to `coalesce`, `greatest` or `least` takes the type of the arguments it
//! meets rather than making the call refuse to bind.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

/// The type and the text of the one value a query answers.
fn answer(sql: &str) -> (String, String) {
    let database = Database::new();
    let sql = format!("SELECT typeof({sql}), {sql}");
    let result = database.query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let row = result.rows().next().expect("one row");
    (format!("{}", row[0]), format!("{}", row[1]))
}

fn refused(sql: &str) -> bool {
    Database::new().query(&format!("SELECT {sql}")).is_err()
}

#[test]
fn a_string_literal_takes_the_type_it_meets() {
    let cases = [
        ("coalesce(1, '2')", "INTEGER", "1"),
        ("greatest(1, '2')", "INTEGER", "2"),
        ("coalesce('2', 3, '4')", "INTEGER", "2"),
        ("coalesce(1::BIGINT, '7')", "BIGINT", "1"),
        ("coalesce('1', 2.5)", "DECIMAL(2,1)", "1.0"),
        ("coalesce(3, 2.5, '1')", "DECIMAL(11,1)", "3.0"),
        ("greatest(DATE '2020-01-01', '2021-01-01')", "DATE", "2021-01-01"),
        ("coalesce(true, 'false')", "BOOLEAN", "true"),
        ("coalesce(CAST(NULL AS INT), '2', 3)", "INTEGER", "2"),
        ("coalesce(NULL, '2')", "VARCHAR", "2"),
        ("greatest('a', 'b')", "VARCHAR", "b"),
    ];
    for (sql, ty, value) in cases {
        assert_eq!(answer(sql), (ty.to_string(), value.to_string()), "{sql}");
    }
}

#[test]
fn two_string_literals_or_a_literal_and_a_null_meet_at_varchar() {
    for sql in [
        "coalesce('2', '3', 3)",
        "coalesce('2', NULL, 3)",
        "coalesce(NULL, '2', 3)",
        "greatest(NULL, '2', 3)",
        "coalesce(1, '2'::VARCHAR)",
    ] {
        assert!(refused(sql), "{sql}");
    }
}

#[test]
fn a_literal_that_is_not_the_type_is_only_an_error_when_it_is_read() {
    assert_eq!(answer("coalesce(1, 'x')"), ("INTEGER".to_string(), "1".to_string()));
    let error = Database::new().query("SELECT least('a', 2)").unwrap_err().to_string();
    assert!(error.contains("Could not convert string 'a' to INT32"), "{error}");
}

#[test]
fn types_that_do_not_meet_are_refused_with_the_pins_sentence() {
    let error =
        |sql: &str| Database::new().query(&format!("SELECT {sql}")).unwrap_err().to_string();
    let cases = [
        (
            "coalesce('2', '3', 3)",
            "Cannot mix values of type VARCHAR and INTEGER_LITERAL in COALESCE operator",
        ),
        (
            "coalesce(NULL, '2', 3)",
            "Cannot mix values of type VARCHAR and INTEGER_LITERAL in COALESCE operator",
        ),
        (
            "coalesce(1, '2'::VARCHAR)",
            "Cannot mix values of type INTEGER_LITERAL and VARCHAR in COALESCE operator",
        ),
        (
            "coalesce(1, 2, 'x'::VARCHAR)",
            "Cannot mix values of type INTEGER and VARCHAR in COALESCE operator",
        ),
        (
            "coalesce(1.5, DATE '2020-01-01')",
            "Cannot mix values of type DECIMAL(2,1) and DATE in COALESCE operator",
        ),
        (
            "greatest(1, DATE '2020-01-01')",
            "Cannot combine types of INTEGER_LITERAL and DATE - an explicit cast is required",
        ),
        (
            "least(NULL, '2', 3)",
            "Cannot combine types of VARCHAR and INTEGER_LITERAL - an explicit cast is required",
        ),
    ];
    for (sql, expected) in cases {
        let said = error(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

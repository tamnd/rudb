//! `lambda_syntax` decides whether a lambda may still be written with the arrow, `x -> x + 1`.
//! Every expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

const DEPRECATED: &str = "Binder Error: Deprecated lambda arrow (->) detected. Please transition \
                          to the new lambda syntax, i.e.., lambda x, i: x + i, before DuckDB's \
                          next release.\nUse SET lambda_syntax='ENABLE_SINGLE_ARROW' to revert \
                          to the deprecated behavior.\nFor more information, see \
                          https://duckdb.org/docs/current/sql/functions/lambda.html.";

const INVALID: &str = "Binder Error: Invalid lambda parameters! Parameters must be unqualified \
                       comma-separated names like x or (x, y).";

fn answered(database: &Database, sql: &str) -> String {
    database.value(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}")).to_string()
}

fn refused(database: &Database, sql: &str) -> String {
    database.value(sql).expect_err(sql).to_string()
}

#[test]
fn enable_single_arrow_lets_the_arrow_bind_as_a_lambda() {
    let database = Database::new();
    database.execute("SET lambda_syntax = 'ENABLE_SINGLE_ARROW'").expect("the setting");
    for (sql, expected) in [
        ("SELECT list_transform([1, 2], x -> x + 1)::VARCHAR", "[2, 3]"),
        ("SELECT list_transform([1, 2], (x, i) -> x + i)::VARCHAR", "[2, 4]"),
        ("SELECT list_filter([1, 2], x -> x > 1)::VARCHAR", "[2]"),
        ("SELECT list_reduce([1, 2, 3], (x, y) -> x + y)::VARCHAR", "6"),
        ("SELECT invoke(x -> x * 2, 21)::VARCHAR", "42"),
        ("SELECT list_transform([1, 2], lambda x: x + 1)::VARCHAR", "[2, 3]"),
        ("SELECT ('{\"a\": 1}'::JSON -> 'a')::VARCHAR", "1"),
    ] {
        assert_eq!(answered(&database, sql), expected, "{sql}");
    }
    for sql in [
        "SELECT list_transform([1, 2], x -> x -> 1)",
        "SELECT list_transform([1, 2], 1 -> 1)",
        "SELECT list_transform([1, 2], a.x -> 1)",
    ] {
        assert_eq!(refused(&database, sql), INVALID, "{sql}");
    }
    assert_eq!(
        refused(&database, "SELECT list_transform([1, 2], (x, x) -> 1)"),
        "Binder Error: table \"0_macro_parameters(x, x)\" has duplicate column name \"x\""
    );
    assert_eq!(
        refused(&database, "SELECT list_transform([1, 2], x -> lambda y: y)"),
        "Binder Error: invalid lambda expression"
    );
}

#[test]
fn the_default_and_disable_refuse_the_arrow_once_the_body_has_bound() {
    let database = Database::new();
    for setting in ["RESET lambda_syntax", "SET lambda_syntax = 'DISABLE_SINGLE_ARROW'"] {
        database.execute(setting).expect("the setting");
        assert_eq!(refused(&database, "SELECT list_transform([1, 2], x -> 1)"), DEPRECATED);
        let missing = refused(&database, "SELECT list_transform([1, 2], x -> y + 1)");
        assert!(
            missing.starts_with(
                "Binder Error: Referenced column \"y\" was not found because the FROM clause is \
                 missing"
            ),
            "{setting}: {missing}"
        );
    }
}

#[test]
fn the_setting_keeps_what_was_written_and_refuses_other_words() {
    let database = Database::new();
    database.execute("SET lambda_syntax = 'default'").expect("the setting");
    assert_eq!(answered(&database, "SELECT current_setting('lambda_syntax')"), "default");
    database.execute("SET lambda_syntax = 'enable_single_arrow'").expect("the setting");
    assert_eq!(answered(&database, "SELECT list_transform([1], x -> x)::VARCHAR"), "[1]");
    database.execute("RESET lambda_syntax").expect("the reset");
    assert_eq!(answered(&database, "SELECT current_setting('lambda_syntax')"), "DEFAULT");
    for (written, candidates) in [
        ("enable", "\"DEFAULT\""),
        ("disable_single", "\"ENABLE_SINGLE_ARROW\""),
        ("", "\"DEFAULT\""),
        ("ENABLE_SINGLE_ARROWS", "\"ENABLE_SINGLE_ARROW\", \"DISABLE_SINGLE_ARROW\", \"DEFAULT\""),
    ] {
        let sql = format!("SET lambda_syntax = '{written}'");
        assert_eq!(
            database.execute(&sql).expect_err(&sql).to_string(),
            format!(
                "Not implemented Error: Enum value: unrecognized value \"{written}\" for enum \
                 \"LambdaSyntax\"\n\nCandidates: {candidates}"
            )
        );
    }
}

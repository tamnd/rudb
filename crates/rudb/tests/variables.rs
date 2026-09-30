//! `SET VARIABLE`, `getvariable` and `duckdb_variables`, and the two lookups of the session's
//! surroundings, `in_search_path` and `getenv`.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn rows(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect()
}

/// The answer of the last statement in `sql`, after running the ones before it on one database.
fn after(sql: &[&str]) -> String {
    let database = Database::new();
    let (last, before) = sql.split_last().unwrap();
    for statement in before {
        database.execute(statement).unwrap_or_else(|error| panic!("{statement} failed: {error}"));
    }
    rows(&database, last).join("\n")
}

fn refused(sql: &[&str]) -> String {
    let database = Database::new();
    let (last, before) = sql.split_last().unwrap();
    for statement in before {
        database.execute(statement).unwrap();
    }
    database.execute(last).unwrap_err().to_string()
}

#[test]
fn a_variable_holds_the_value_and_the_type_of_its_expression() {
    let cases: [(&[&str], &str); 13] = [
        (
            &["SET VARIABLE a = 1 + 1", "SELECT getvariable('a'), typeof(getvariable('a'))"],
            "2|INTEGER",
        ),
        (
            &[
                "SET VARIABLE a = 'x'",
                "SET VARIABLE a = [1,2]",
                "SELECT getvariable('a'), typeof(getvariable('a'))",
            ],
            "[1, 2]|INTEGER[]",
        ),
        (&["SET VARIABLE A = 1", "SELECT getvariable('a'), getvariable('A')"], "1|1"),
        (&["SET VARIABLE a = (SELECT 42)", "SELECT getvariable('a')"], "42"),
        (
            &[
                "SET VARIABLE a = (SELECT 1 WHERE false)",
                "SELECT getvariable('a'), typeof(getvariable('a'))",
            ],
            "NULL|INTEGER",
        ),
        (&["SET VARIABLE a = random() < 2", "SELECT getvariable('a')"], "true"),
        (&["SET VARIABLE a TO 5", "SELECT getvariable('a')"], "5"),
        (
            &["SET VARIABLE a = NULL", "SELECT getvariable('a'), typeof(getvariable('a'))"],
            "NULL|\"NULL\"",
        ),
        (&["SET VARIABLE a = 3", "SELECT getvariable('a') + 1, getvariable('a')::VARCHAR"], "4|3"),
        (&["SET VARIABLE a = x", "SELECT getvariable('a'), typeof(getvariable('a'))"], "x|VARCHAR"),
        (&["SET VARIABLE a = 1/0", "SELECT getvariable('a')"], "inf"),
        (&["SET VARIABLE a = getvariable('a')", "SELECT getvariable('a')"], "NULL"),
        (
            &["SET VARIABLE a = 1", "SELECT getvariable(NULL), typeof(getvariable(NULL))"],
            "NULL|\"NULL\"",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(after(sql), expected, "{sql:?}");
    }
    assert_eq!(after(&["SET VARIABLE n = 2", "SELECT * FROM range(getvariable('n'))"]), "0\n1");
    assert_eq!(after(&["SET VARIABLE t = 'abc'", "SELECT length(getvariable('t'))"]), "3");
}

#[test]
fn a_variable_is_reset_and_a_missing_one_is_null() {
    let cases: [(&[&str], &str); 5] = [
        (&["SET VARIABLE a = 1", "RESET VARIABLE a", "SELECT getvariable('a')"], "NULL"),
        (&["RESET VARIABLE nope", "SELECT 1"], "1"),
        (
            &[
                "SET VARIABLE a = 1",
                "SET VARIABLE a = DEFAULT",
                "SELECT getvariable('a'), typeof(getvariable('a'))",
            ],
            "NULL|\"NULL\"",
        ),
        (
            &[
                "SET VARIABLE a = 1",
                "SET VARIABLE a = DEFAULT",
                "SELECT count(*) FROM duckdb_variables()",
            ],
            "0",
        ),
        (&["SELECT getvariable('nope'), typeof(getvariable('nope'))"], "NULL|\"NULL\""),
    ];
    for (sql, expected) in cases {
        assert_eq!(after(sql), expected, "{sql:?}");
    }
}

#[test]
fn duckdb_variables_lists_every_variable_as_text() {
    let cases: [(&[&str], &str); 6] = [
        (
            &[
                "SET VARIABLE a = 1",
                "SET VARIABLE b = 'q'",
                "SELECT * FROM duckdb_variables() ORDER BY name",
            ],
            "a|1|INTEGER\nb|q|VARCHAR",
        ),
        (&["SELECT * FROM duckdb_variables()"], ""),
        (
            &["DESCRIBE SELECT * FROM duckdb_variables()"],
            "name|VARCHAR|YES|NULL|NULL|NULL\nvalue|VARCHAR|YES|NULL|NULL|NULL\ntype|VARCHAR|YES|NULL|NULL|NULL",
        ),
        (
            &["SET VARIABLE a = {'k': 1}", "SELECT * FROM duckdb_variables()"],
            "a|{'k': 1}|STRUCT(k INTEGER)",
        ),
        (
            &[
                "SET VARIABLE a = 1.5",
                "SET VARIABLE b = DATE '2020-01-01'",
                "SET VARIABLE c = 'it''s'",
                "SELECT * FROM duckdb_variables() ORDER BY 1",
            ],
            "a|1.5|DECIMAL(2,1)\nb|2020-01-01|DATE\nc|it's|VARCHAR",
        ),
        (
            &[
                "SET VARIABLE a = 1",
                "SET VARIABLE a = 2",
                "SELECT count(*) FROM duckdb_variables()",
            ],
            "1",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(after(sql), expected, "{sql:?}");
    }
    assert_eq!(
        after(&["SET VARIABLE \"Mixed\" = 1", "SELECT name FROM duckdb_variables()"]),
        "Mixed"
    );
}

#[test]
fn what_the_pin_refuses_about_variables_is_refused_in_its_words() {
    let cases: [(&[&str], &str); 6] = [
        (
            &["SET VARIABLE a = (SELECT i FROM range(3) t(i))"],
            "Invalid Input Error: More than one row returned by a subquery used as an expression - scalar subqueries can only return a single row.\n\nUse \"SET scalar_subquery_error_on_multiple_rows=false\" to revert to previous behavior of returning a random row.",
        ),
        (
            &["SET VARIABLE a = 'x'::INTEGER"],
            "Conversion Error: Could not convert string 'x' to INT32",
        ),
        (&["SET VARIABLE a = 1, b = 2"], "Parser Error: SET can only contain a single value"),
        (
            &["SELECT getvariable('a', 'b')"],
            "Binder Error: No function matches the given name and argument types 'getvariable(STRING_LITERAL, STRING_LITERAL)'. You might need to add explicit type casts.\n\tCandidate functions:\n\tgetvariable(variable_name VARCHAR) -> ANY\n",
        ),
        (
            &["SELECT getvariable(1)"],
            "Binder Error: No function matches the given name and argument types 'getvariable(INTEGER_LITERAL)'. You might need to add explicit type casts.\n\tCandidate functions:\n\tgetvariable(variable_name VARCHAR) -> ANY\n",
        ),
        (
            &["SELECT getvariable(x) FROM (SELECT 'a' x)"],
            "Binder Error: The \"variable_name\" argument in function \"getvariable\" must be a constant expression",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(refused(sql), expected, "{sql:?}");
    }
}

#[test]
fn in_search_path_reads_the_path_the_session_searches() {
    let cases: [(&[&str], &str); 9] = [
        (
            &[
                "SELECT in_search_path('MEMORY', 'MAIN'), in_search_path('system', 'pg_catalog'), in_search_path('system', 'information_schema'), in_search_path('temp', 'pg_catalog')",
            ],
            "true|true|false|false",
        ),
        (
            &[
                "CREATE SCHEMA s",
                "SET search_path = 's'",
                "SELECT in_search_path('memory', 's'), in_search_path('memory', 'main'), in_search_path('temp', 'main')",
            ],
            "true|true|true",
        ),
        (
            &[
                "CREATE SCHEMA s",
                "SET search_path = 'memory.s,main'",
                "SELECT in_search_path('memory', 's'), in_search_path('memory', 'main'), in_search_path('x', 's')",
            ],
            "true|true|false",
        ),
        (&["CREATE SCHEMA s", "SET search_path = 's'", "SELECT in_search_path('', 's')"], "true"),
        (
            &["CREATE SCHEMA s", "SET search_path = 'memory.s'", "SELECT in_search_path('', 's')"],
            "false",
        ),
        (&["ATTACH ':memory:' AS other", "USE other", "SELECT in_search_path('', 'main')"], "true"),
        (
            &[
                "SELECT in_search_path('Memory', 'Main'), in_search_path('memory', ''), in_search_path('', 'main')",
            ],
            "true|false|true",
        ),
        (&["ATTACH ':memory:' AS other", "SELECT in_search_path('other', 'main')"], "false"),
        (
            &[
                "ATTACH ':memory:' AS other",
                "USE other",
                "SELECT in_search_path('other', 'main'), in_search_path('memory', 'main')",
            ],
            "true|false",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(after(sql), expected, "{sql:?}");
    }
    assert_eq!(
        after(&[
            "SELECT in_search_path(c, s) FROM (VALUES ('memory', 'main'), ('memory', 'x'), (NULL, 'main')) t(c, s)"
        ]),
        "true\nfalse\nNULL"
    );
    assert_eq!(
        after(&["SET VARIABLE s = 'main'", "SELECT in_search_path('memory', getvariable('s'))"]),
        "true"
    );
    assert_eq!(
        refused(&["SELECT in_search_path('memory', 1)"]),
        "Binder Error: No function matches the given name and argument types 'in_search_path(STRING_LITERAL, INTEGER_LITERAL)'. You might need to add explicit type casts.\n\tCandidate functions:\n\tin_search_path(col0 VARCHAR, col1 VARCHAR) -> BOOLEAN\n"
    );
}

#[test]
fn getenv_reads_the_environment_and_answers_an_unset_name_with_nothing() {
    let home = std::env::var("HOME").unwrap_or_default();
    assert_eq!(
        after(&["SELECT getenv('HOME'), getenv(''), getenv(x) FROM (VALUES ('HOME')) t(x)"]),
        format!("{home}||{home}")
    );
    assert_eq!(after(&["SELECT getenv('RUDB_SURELY_NOT_SET_ANYWHERE'), getenv(NULL)"]), "|NULL");
    assert_eq!(
        refused(&["SELECT getenv(1)"]),
        "Binder Error: No function matches the given name and argument types 'getenv(INTEGER_LITERAL)'. You might need to add explicit type casts.\n\tCandidate functions:\n\tgetenv(col0 VARCHAR) -> VARCHAR\n"
    );
}

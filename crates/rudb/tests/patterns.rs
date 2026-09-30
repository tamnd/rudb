//! `GLOB` and `LIKE ... ESCAPE`, the two pattern matches beside the plain `LIKE`.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

/// The names and the one row a query answers, with the row as text.
fn answer(sql: &str) -> (Vec<String>, String) {
    let database = Database::new();
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let names = result.names().to_vec();
    let rows: Vec<String> = result
        .rows()
        .map(|row| row.iter().map(|value| format!("{value}")).collect::<Vec<_>>().join("|"))
        .collect();
    (names, rows.join(";"))
}

fn error(sql: &str) -> String {
    Database::new().query(sql).map(|_| String::new()).unwrap_or_else(|error| error.to_string())
}

#[test]
fn glob_is_the_tilde_operator_with_stars_marks_and_brackets() {
    let (names, row) = answer(
        "SELECT 'abc' GLOB 'a*', 'abc' GLOB 'a?c', 'abc' NOT GLOB 'b*', 'a*c' GLOB 'a[*]c', \
         'abc' GLOB '[!a]bc', 'abc' GLOB NULL",
    );
    assert_eq!(
        names,
        [
            "('abc' ~~~ 'a*')",
            "('abc' ~~~ 'a?c')",
            "(NOT ('abc' ~~~ 'b*'))",
            "('a*c' ~~~ 'a[*]c')",
            "('abc' ~~~ '[!a]bc')",
            "('abc' ~~~ NULL)"
        ]
    );
    assert_eq!(row, "true|true|true|true|false|NULL");
}

#[test]
fn a_like_with_an_escape_is_a_like_escape_call() {
    let (names, row) = answer(
        "SELECT 'a%c' LIKE 'a$%c' ESCAPE '$', 'abc' LIKE 'a$%c' ESCAPE '$', 'a_c' NOT LIKE \
         'a$_c' ESCAPE '$', 'A%c' ILIKE 'a$%c' ESCAPE '$', 'abc' LIKE 'a%' ESCAPE ''",
    );
    assert_eq!(
        names,
        [
            "like_escape('a%c', 'a$%c', '$')",
            "like_escape('abc', 'a$%c', '$')",
            "(NOT like_escape('a_c', 'a$_c', '$'))",
            "ilike_escape('A%c', 'a$%c', '$')",
            "like_escape('abc', 'a%', '')"
        ]
    );
    assert_eq!(row, "true|false|false|true|true");
    let (_, row) = answer("SELECT not_like_escape('a%c', 'a$%c', '$'), 'a%' LIKE 'a%' ESCAPE NULL");
    assert_eq!(row, "false|NULL");
    assert!(
        error("SELECT 'abc' LIKE 'a%' ESCAPE 'xy'")
            .contains("Invalid escape string. Escape string must be empty or one character.")
    );
    assert!(
        error("SELECT 'abc' LIKE 'a$' ESCAPE '$'")
            .contains("Like pattern must not end with escape character!")
    );
}

#[test]
fn a_star_takes_glob_and_an_escaped_like() {
    let database = Database::new();
    database.execute("CREATE TABLE integers AS SELECT 42 i, 84 j").expect("the table");
    let names = |sql: &str| database.query(sql).expect(sql).names().to_vec();
    assert_eq!(names("SELECT * GLOB 'i' FROM integers"), ["i"]);
    assert_eq!(names("SELECT * NOT GLOB 'i' FROM integers"), ["j"]);
    let refused = database.query("SELECT * LIKE 'i$%' ESCAPE '$' FROM integers").unwrap_err();
    assert!(
        refused.to_string().contains(
            "COLUMNS(list_filter(['i', 'j'], (lambda __lambda_col: like_escape(__lambda_col, \
             'i$%', '$'))))"
        ),
        "{refused}"
    );
}

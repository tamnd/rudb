//! `TABLE t`, which is `SELECT * FROM t` under a shorter name.
//!
//! Every answer here was read off the pinned duckdb on server2 first, which is v2.0.0-dev84237 at
//! cc7e7bac7f.

use rudb::Database;

/// A database with the statements already run, panicking on the first that does not.
fn ran(statements: &[&str]) -> Database {
    let database = Database::new();
    for sql in statements {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// Every row a query answers with, each as its cells joined with a bar, in the order they came.
fn rows(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| {
            (0..result.width())
                .map(|column| result.text_at(row, column))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

#[test]
fn a_table_statement_reads_every_column_of_the_name() {
    let database = ran(&[
        "CREATE TABLE t AS SELECT 1 AS a, 2 AS b",
        "CREATE TABLE r AS SELECT range AS a FROM range(5)",
        "CREATE SCHEMA s",
        "CREATE TABLE s.t AS SELECT 7 AS z",
    ]);
    assert_eq!(rows(&database, "TABLE t"), ["1|2"]);
    assert_eq!(rows(&database, "TABLE r ORDER BY a DESC LIMIT 2"), ["4", "3"]);
    assert_eq!(rows(&database, "TABLE t UNION ALL TABLE t"), ["1|2", "1|2"]);
    assert_eq!(rows(&database, "WITH c AS (SELECT 42 AS x) TABLE c"), ["42"]);
    assert_eq!(rows(&database, "TABLE s.t"), ["7"]);
    assert_eq!(rows(&database, "SELECT q FROM (TABLE r) AS sub(q) WHERE q = 3"), ["3"]);
    let error = database.query("SELECT * FROM (TABLE nope)").expect_err("no such table");
    assert!(
        error.to_string().starts_with("Catalog Error: Table with name nope does not exist!"),
        "{error}"
    );
}

#[test]
fn a_view_over_a_table_statement_is_written_back_as_a_select() {
    let database = ran(&["CREATE TABLE t AS SELECT 1 AS a, 2 AS b", "CREATE VIEW v AS TABLE t"]);
    assert_eq!(
        rows(&database, "SELECT sql FROM duckdb_views() WHERE view_name = 'v'"),
        ["CREATE VIEW v AS SELECT * FROM t;"]
    );
    assert_eq!(rows(&database, "TABLE v"), ["1|2"]);
}

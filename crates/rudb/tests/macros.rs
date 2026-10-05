//! `CREATE MACRO`, `DROP MACRO` and the calls that expand them. Every expected answer here was
//! taken from the pinned duckdb binary, v2.0.0-dev84237.

use std::path::{Path, PathBuf};

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

#[test]
fn a_scalar_macro_puts_its_arguments_in_place_of_its_parameters() {
    let database = Database::new();
    database.execute("CREATE MACRO m(a, b := 10) AS a + b").expect("the macro");
    assert_eq!(answered(&database, "SELECT m(1)"), ["11"]);
    assert_eq!(answered(&database, "SELECT m(1, 2)"), ["3"]);
    assert_eq!(answered(&database, "SELECT m(1, b := 5)"), ["6"]);
    assert_eq!(answered(&database, "SELECT m(b := 5, a := 2)"), ["7"]);
    database.execute("CREATE TABLE t (x INTEGER)").expect("the table");
    database.execute("INSERT INTO t VALUES (1), (2)").expect("rows");
    assert_eq!(answered(&database, "SELECT m(x) FROM t"), ["11", "12"]);
}

#[test]
fn a_macro_whose_body_aggregates_makes_the_query_a_grouping_one() {
    let database = Database::new();
    database.execute("CREATE MACRO total(x) AS sum(x) + 1").expect("the macro");
    database.execute("CREATE TABLE t (g INTEGER, x INTEGER)").expect("the table");
    database.execute("INSERT INTO t VALUES (1, 1), (1, 2), (2, 5)").expect("rows");
    assert_eq!(answered(&database, "SELECT total(x) FROM t"), ["9"]);
    assert_eq!(answered(&database, "SELECT g, total(x) FROM t GROUP BY g"), ["1|4", "2|6"]);
}

#[test]
fn a_table_macro_is_read_in_a_from_clause() {
    let database = Database::new();
    database
        .execute("CREATE MACRO numbers(n) AS TABLE SELECT range AS v FROM range(n)")
        .expect("the macro");
    assert_eq!(answered(&database, "SELECT * FROM numbers(3)"), ["0", "1", "2"]);
    assert_eq!(answered(&database, "SELECT q.v FROM numbers(2) AS q"), ["0", "1"]);
    assert_eq!(answered(&database, "SELECT w FROM numbers(1) AS q(w)"), ["0"]);
}

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-macros-{tag}-{}.rudb", std::process::id()));
    remove(&path);
    path
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
}

fn open(path: &Path) -> Database {
    Database::open(path.to_str().expect("a UTF-8 path")).expect("the database opens")
}

#[test]
fn a_database_file_keeps_its_macros() {
    let path = path("kept");
    {
        let database = open(&path);
        database.execute("CREATE TABLE t (x INTEGER)").expect("the table");
        database.execute("INSERT INTO t VALUES (1), (2)").expect("rows");
        database.execute("CREATE MACRO total(x) AS sum(x) + 1").expect("the aggregating macro");
        database
            .execute(
                "CREATE MACRO m(\"select\" INTEGER, b := 10) AS \"select\" + b, (s VARCHAR) AS s",
            )
            .expect("the overloaded macro");
        database.execute("CREATE MACRO m() AS TABLE SELECT x FROM t").expect("the table macro");
        database.execute("CREATE MACRO gone(x) AS x").expect("a macro to drop");
        database.execute("DROP MACRO gone").expect("dropped");
    }
    let database = open(&path);
    assert_eq!(answered(&database, "SELECT total(x) FROM t"), ["4"]);
    assert_eq!(answered(&database, "SELECT m(1), m(1, b := 2), m('s')"), ["11|3|s"]);
    assert_eq!(answered(&database, "SELECT * FROM m()"), ["1", "2"]);
    refused(&database, "SELECT gone(1)", "Catalog Error: Scalar Function with name gone");
    drop(database);
    remove(&path);
}

#[test]
fn a_typed_parameter_picks_between_overloads() {
    let database = Database::new();
    database
        .execute("CREATE MACRO m(x INTEGER) AS x + 1, (x VARCHAR) AS x || '!'")
        .expect("the macro");
    assert_eq!(answered(&database, "SELECT m(41)"), ["42"]);
    assert_eq!(answered(&database, "SELECT m('hi')"), ["hi!"]);
}

#[test]
fn duckdb_functions_lists_each_overload() {
    let database = Database::new();
    database.execute("CREATE MACRO m(a, b := 10) AS a + b").expect("the macro");
    database.execute("CREATE MACRO t() AS TABLE SELECT 1 AS x").expect("the table macro");
    assert_eq!(
        answered(
            &database,
            "SELECT function_name, function_type, macro_definition, parameters FROM \
             duckdb_functions() WHERE function_name IN ('m', 't')"
        ),
        ["m|macro|(a + b)|[a, b]", "t|table_macro|SELECT 1 AS x|[]"]
    );
    database.execute("DROP MACRO m").expect("dropped");
    database.execute("DROP MACRO TABLE t").expect("the table macro dropped");
    assert_eq!(
        answered(
            &database,
            "SELECT count(*) FROM duckdb_functions() WHERE function_name IN ('m', 't')"
        ),
        ["0"]
    );
}

#[test]
fn the_pin_refuses_what_it_refuses_in_its_words() {
    let database = Database::new();
    database.execute("CREATE TABLE integers (a INTEGER)").expect("the table");
    database.execute("CREATE MACRO m(a, b := 10) AS a + b").expect("the macro");
    database.execute("CREATE MACRO t() AS TABLE SELECT 1 AS x").expect("the table macro");
    for (sql, expected) in [
        ("CREATE MACRO m(x) AS x", "Catalog Error: Macro Function with name \"m\" already exists!"),
        ("CREATE MACRO r(x) AS r(x)", "Catalog Error: Scalar Function with name r does not exist!"),
        (
            "CREATE MACRO a1(a) AS (SELECT a + a FROM integers)",
            "Binder Error: Conflicting column names for column a!",
        ),
        ("CREATE MACRO w(x) AS lag(x)", "Binder Error: Window functions are not supported here"),
        (
            "SELECT m(b := 6, 3)",
            "Binder Error: Macro \"m\"() has positional argument following named argument",
        ),
        ("SELECT m(1, b := 6, b := 3)", "Binder Error: Macro \"m\"() has named argument repeated"),
        (
            "SELECT m(1, 2, 3)",
            "Binder Error: Macro m() does not support the supplied arguments. You might need to \
             add explicit type casts.\nCandidate macros:\n\tm(a, b := 10)",
        ),
        ("SELECT t()", "Binder Error: Function \"t\" is a table function but it was used as a"),
        ("SELECT * FROM m(1)", "Catalog Error: Table Function with name m does not exist!"),
        ("DROP MACRO TABLE m", "Catalog Error: Table Macro Function with name m does not exist!"),
        ("DROP MACRO nope", "Catalog Error: Macro Function with name nope does not exist!"),
        (
            "CREATE MACRO s() AS TABLE SELECT * FROM suits",
            "Catalog Error: Table with name suits does not exist!",
        ),
        (
            "SELECT * FROM t(x := 1, 2)",
            "Binder Error: Macro \"t\"() has positional argument following named argument",
        ),
    ] {
        refused(&database, sql, expected);
    }
}

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
fn a_file_attached_with_an_older_storage_version_takes_no_typed_parameter() {
    let path = path("older");
    let database = Database::new();
    database
        .execute(&format!(
            "ATTACH '{}' AS older (STORAGE_VERSION 'v1.3.0')",
            path.to_str().expect("a UTF-8 path")
        ))
        .expect("attached");
    database.execute("USE older").expect("used");
    refused(
        &database,
        "CREATE MACRO m(s VARCHAR) AS s || 'c'",
        "Binder Error: Typed macro parameters are only supported for storage versions v1.4.0 and \
         higher.",
    );
    database.execute("CREATE MACRO u(s) AS s || 'c'").expect("an untyped one");
    database.execute("CREATE TEMPORARY MACRO m(s VARCHAR) AS s || 'c'").expect("a temporary one");
    assert_eq!(answered(&database, "SELECT m('ab'), u('ab')"), ["abc|abc"]);
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
fn the_overload_with_the_cheapest_casts_is_called_and_its_arguments_are_cast() {
    let database = Database::new();
    database
        .execute(
            "CREATE MACRO m (a tinyint) AS 1, (a smallint) AS 2, (a integer) AS 3, \
             (a bigint) AS 4, (a hugeint) AS 5, (a) AS 10",
        )
        .expect("the macro");
    assert_eq!(
        answered(
            &database,
            "SELECT m(0::tinyint), m(0::utinyint), m(0::uinteger), m(0::ubigint), \
             m(0::uhugeint), m(0::double)"
        ),
        ["1|4|4|5|10|10"]
    );
    database.execute("CREATE MACRO t(a bigint) AS typeof(a)").expect("the cast one");
    assert_eq!(answered(&database, "SELECT t(42::integer), t(NULL)"), ["BIGINT|BIGINT"]);
    database.execute("CREATE MACRO p(a varchar) AS 'v', (a) AS 'any'").expect("the untyped one");
    assert_eq!(answered(&database, "SELECT p(1), p(NULL), p('x')"), ["any|v|v"]);
    assert_eq!(
        answered(
            &database,
            "SELECT parameter_types[1] FROM duckdb_functions() WHERE function_name = 'm'"
        ),
        ["BIGINT", "HUGEINT", "INTEGER", "NULL", "SMALLINT", "TINYINT"]
    );
}

#[test]
fn a_typed_default_is_checked_when_the_macro_is_made() {
    let database = Database::new();
    database
        .execute(
            "CREATE MACRO d(i tinyint := 1, u utinyint := 255, n tinyint := NULL) AS \
             typeof(i) || typeof(u) || typeof(n)",
        )
        .expect("defaults that fit");
    assert_eq!(answered(&database, "SELECT d()"), ["TINYINTUTINYINTTINYINT"]);
    database.execute("CREATE MACRO w(i bigint := 42::integer) AS typeof(i)").expect("widened");
    assert_eq!(answered(&database, "SELECT w()"), ["BIGINT"]);
    for (sql, expected) in [
        (
            "CREATE MACRO b(i tinyint := 128) AS i",
            "Binder Error: Default value '128' for parameter '\"i\"' cannot be implicitly cast to \
             'TINYINT'. Please add an explicit type cast.",
        ),
        (
            "CREATE MACRO b(i tinyint := 64 + 63) AS i",
            "Binder Error: Default value '127' for parameter '\"i\"' cannot be implicitly cast",
        ),
        (
            "CREATE MACRO b(s varchar := 'a' || 'b', i integer := 'x') AS i",
            "Binder Error: Default value ''x'' for parameter '\"i\"' cannot be implicitly cast",
        ),
        (
            "CREATE MACRO b(i tinyint := NULL::integer) AS i",
            "Binder Error: Default value 'NULL::INTEGER' for parameter '\"i\"' cannot be",
        ),
        (
            "CREATE MACRO b(i := random()) AS i",
            "Binder Error: Default value 'random()' for parameter '\"i\"' is not a constant \
             expression.",
        ),
        (
            "CREATE MACRO b(i integer := (SELECT 1)) AS i",
            "Binder Error: Default value for parameter \"i\" cannot contain subqueries",
        ),
    ] {
        refused(&database, sql, expected);
    }
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
    database.execute("CREATE MACRO tie(a, b := 1) AS 1, (a) AS 2").expect("overloads that tie");
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
            "CREATE MACRO twice(a INTEGER, b) AS a, (c INTEGER, d := 1) AS c",
            "Binder Error: Ambiguity in macro overloads - macro twice() has multiple definitions \
             with the same parameters",
        ),
        (
            "SELECT tie(1)",
            "Binder Error: Macro tie() has multiple overloads that match the supplied arguments.",
        ),
        (
            "SELECT * FROM t(x := 1, 2)",
            "Binder Error: Macro \"t\"() has positional argument following named argument",
        ),
    ] {
        refused(&database, sql, expected);
    }
}

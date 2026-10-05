//! `query` and `query_table`, the table functions whose rows are those of a query handed to them as
//! text or as table names. Every expected answer here was taken from the pinned duckdb binary,
//! v2.0.0-dev84237, except the argument types in a call that matches no overload, where the test
//! file it ships spells a string literal `STRING_LITERAL` and the pin spells it `VARCHAR`.

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
fn query_runs_the_select_it_is_given() {
    let database = Database::new();
    assert_eq!(answered(&database, "SELECT * FROM query('SELECT 42 AS a') t(b)"), ["42"]);
    assert_eq!(answered(&database, "SELECT * FROM query('SELECT 42;;;--- hello;')"), ["42"]);
    assert_eq!(answered(&database, "SELECT * FROM query('SELECT 4' || '2')"), ["42"]);
    let sql = "SELECT * FROM query('SELECT 1 AS a UNION ALL SELECT 2')";
    assert_eq!(answered(&database, sql), ["1", "2"]);
    assert_eq!(answered(&database, "SELECT * FROM main.query('SELECT 1')"), ["1"]);
    let single = "Parser Error: Expected a single SELECT statement";
    refused(&database, "SELECT * FROM query(' ')", single);
    refused(&database, "SELECT * FROM query('SELECT 1; SELECT 2')", single);
    refused(&database, "SELECT * FROM query('CREATE TABLE tbl (a INT)')", single);
    refused(
        &database,
        "SELECT * FROM query(42)",
        "Binder Error: No function matches the given name and argument types 'query(INTEGER)'.",
    );
}

#[test]
fn query_table_reads_one_table_or_stacks_several() {
    let database = Database::new();
    database.execute("CREATE TABLE t1 AS SELECT 1 a, 2 b").expect("t1");
    database.execute("CREATE TABLE t2 AS SELECT 3 b, 4 a").expect("t2");
    assert_eq!(answered(&database, "FROM query_table('t1')"), ["1|2"]);
    assert_eq!(answered(&database, "SELECT x.a FROM query_table(t1) x"), ["1"]);
    assert_eq!(answered(&database, "FROM query_table(['t1', 't2'])"), ["1|2", "3|4"]);
    assert_eq!(answered(&database, "FROM query_table([t1, t2], true)"), ["1|2", "4|3"]);
    assert_eq!(answered(&database, "FROM query_table(['t1', NULL])"), ["1|2"]);
    database.execute("CREATE TABLE \"(SELECT 17 + 25)\"(i INT)").expect("an odd name");
    database.execute("INSERT INTO \"(SELECT 17 + 25)\" VALUES (100)").expect("a row");
    assert_eq!(answered(&database, "FROM query_table('(SELECT 17 + 25)')"), ["100"]);
}

#[test]
fn a_macro_can_hand_query_table_its_parameter() {
    let database = Database::new();
    database.execute("CREATE TABLE integers AS FROM range(100) t(i)").expect("the table");
    database
        .execute(
            "CREATE MACRO min_from_tbl(tbl, col) AS \
             (SELECT min(col) FROM query_table(tbl::VARCHAR))",
        )
        .expect("the macro");
    assert_eq!(answered(&database, "SELECT min_from_tbl(integers, i)"), ["0"]);
    refused(
        &database,
        "SELECT min_from_tbl(integers2, i)",
        "Catalog Error: Table with name integers2 does not exist!",
    );
}

#[test]
fn query_table_refuses_what_the_pin_refuses_in_its_words() {
    let database = Database::new();
    for (sql, expected) in [
        (
            "FROM query_table()",
            "Binder Error: No function matches the given name and argument types \
             'query_table()'.",
        ),
        (
            "FROM query_table('a', 'b', 'c')",
            "Binder Error: No function matches the given name and argument types \
             'query_table(STRING_LITERAL, STRING_LITERAL, STRING_LITERAL)'.",
        ),
        ("FROM query_table(NULL)", "Binder Error: Cannot use NULL as function argument"),
        ("FROM query_table(NULL::VARCHAR)", "Binder Error: Cannot use NULL as function argument"),
        (
            "FROM query_table([NULL])",
            "Invalid Input Error: Expected a table or a list with tables as input",
        ),
        ("FROM query_table([''])", "Parser Error: syntax error at or near \"FROM\""),
        (
            "FROM query_table('FROM query(\"select 1 + 2;\")')",
            "Parser Error: Unexpected quote in the middle of a qualified name component! \
             (input: FROM query(\"select 1 + 2;\"))",
        ),
        (
            "FROM query_table('SELECT 4 + 2')",
            "Catalog Error: Table with name SELECT 4 + 2 does not exist!",
        ),
        (
            "FROM query_table('/nope/x.parquet')",
            "IO Error: No files found that match the pattern \"/nope/x.parquet\"",
        ),
    ] {
        refused(&database, sql, expected);
    }
}

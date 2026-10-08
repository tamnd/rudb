//! A table name written where a value goes, which is the whole row of the table as a struct.
//!
//! The cases are the ones of DuckDB's `test/sql/binder/test_implicit_struct_pack.test` that do not
//! name a schema, and a few more whose answers were taken from a duckdb binary and not from rudb.

use rudb::Database;

fn rows(database: &Database, sql: &str) -> String {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let rows: Vec<String> = result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect();
    rows.join("\n")
}

#[test]
fn a_table_name_is_its_row_when_no_column_or_field_has_the_name() {
    let database = Database::new();
    database.execute("CREATE TABLE test AS SELECT range i FROM range(3)").unwrap();
    let cases = [
        ("SELECT test FROM test", "{'i': 0}\n{'i': 1}\n{'i': 2}"),
        ("SELECT t FROM test AS t", "{'i': 0}\n{'i': 1}\n{'i': 2}"),
        ("SELECT t FROM (SELECT * FROM test) AS t", "{'i': 0}\n{'i': 1}\n{'i': 2}"),
        (
            "WITH data AS (SELECT 1 AS a, 2 AS b, 3 AS c) SELECT d FROM data d",
            "{'a': 1, 'b': 2, 'c': 3}",
        ),
        ("SELECT typeof(test) FROM test LIMIT 1", "STRUCT(i BIGINT)"),
        ("SELECT test.i FROM test WHERE test = {'i': 1}", "1"),
        ("SELECT (SELECT test FROM range(1)) FROM test WHERE i = 2", "{'i': 2}"),
    ];
    for (sql, expected) in cases {
        assert_eq!(rows(&database, sql), expected, "{sql}");
    }
    let error = database.query("SELECT test FROM test t").unwrap_err().to_string();
    assert!(error.contains("\"test\""), "{error}");

    // A column of the name comes first, and then a field of a struct of the name.
    database.execute("CREATE TABLE main AS SELECT 3 test").unwrap();
    assert_eq!(rows(&database, "SELECT main.test FROM main, test"), "3\n3\n3");
    assert_eq!(rows(&database, "SELECT test FROM main, test"), "3\n3\n3");
    database.execute("CREATE TABLE structs AS SELECT {test: 4} main").unwrap();
    assert_eq!(rows(&database, "SELECT main.test FROM structs, test"), "4\n4\n4");
}

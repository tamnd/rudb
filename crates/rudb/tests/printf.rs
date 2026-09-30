//! `format` and `printf`, and how their arguments are bound.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn answered(sql: &str) -> Vec<String> {
    let database = Database::new();
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join(","))
        .collect()
}

fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

#[test]
fn every_argument_is_cast_to_a_kind_the_formatter_reads() {
    let cases = [
        ("SELECT printf('%d|%s|%.2f', 42, 'x', 1.5)", "42|x|1.50"),
        ("SELECT format('{} {}', 1::TINYINT, 2::UTINYINT)", "1 2"),
        ("SELECT printf('%x', 255::UINTEGER)", "ff"),
        ("SELECT format('{}', 1.5::FLOAT)", "1.5"),
        ("SELECT format('{}', 0.1::FLOAT)", "0.10000000149011612"),
        ("SELECT format('{}', 12.50::DECIMAL(4,2))", "12.5"),
        ("SELECT format('{:.2f}', 2.675)", "2.67"),
        ("SELECT format('{}', DATE '2024-01-02')", "2024-01-02"),
        ("SELECT format('{:>12}|', DATE '2024-01-02')", "  2024-01-02|"),
        ("SELECT format('{}', [1, 2])", "[1, 2]"),
        ("SELECT format('{}', {'a': 1})", "{'a': 1}"),
        ("SELECT format('{}', INTERVAL 1 DAY)", "1 day"),
        (
            "SELECT format('{}', '00000000-0000-0000-0000-000000000001'::UUID)",
            "00000000-0000-0000-0000-000000000001",
        ),
        (
            "SELECT format('{}', 170141183460469231731687303715884105727::HUGEINT)",
            "170141183460469231731687303715884105727",
        ),
        ("SELECT printf('%d', 18446744073709551615::UBIGINT)", "18446744073709551615"),
        ("SELECT printf('%s', true)", "true"),
        ("SELECT format('{}', true)", "true"),
        ("SELECT format('plain')", "plain"),
        ("SELECT typeof(format('{}', 1))", "VARCHAR"),
        ("SELECT format('{}', NULL)", "NULL"),
        ("SELECT format(NULL, 1)", "NULL"),
        ("SELECT printf(NULL)", "NULL"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), [expected], "{sql}");
    }
}

#[test]
fn a_column_is_formatted_row_by_row() {
    assert_eq!(answered("SELECT printf('%d', x) FROM range(3) t(x)"), ["0", "1", "2"]);
    let rows = answered(
        "SELECT format('{}-{}', a, b) FROM (VALUES (1, 'x'), (NULL, 'y'), (3, NULL)) v(a, b)",
    );
    assert_eq!(rows, ["1-x", "NULL", "NULL"]);
    let rows = answered("SELECT printf(f, 7) FROM (VALUES ('%d'), ('%05d'), ('%x')) v(f)");
    assert_eq!(rows, ["7", "00007", "7"]);
}

#[test]
fn what_the_pin_refuses_is_refused_in_its_words() {
    let cases = [
        (
            "SELECT printf(1)",
            "No function matches the given name and argument types 'printf(INTEGER_LITERAL)'",
        ),
        ("SELECT printf(1)", "printf(col0 VARCHAR, [ANY...]) -> VARCHAR"),
        ("SELECT format(1, 2)", "format(col0 VARCHAR, [ANY...]) -> VARCHAR"),
        ("SELECT printf()", "'printf()'"),
        ("SELECT format('{}')", "Invalid Input Error: Argument index \"0\" out of range"),
        (
            "SELECT printf('%d', 'x')",
            "Invalid type specifier \"d\" for formatting a value of type string",
        ),
        (
            "SELECT printf('%s', 1.5::DECIMAL(3,1))",
            "Invalid type specifier \"s\" for formatting a value of type float",
        ),
        (
            "SELECT printf('%c', 200)",
            "Invalid UTF8 produced by format string \"%c\" - note that %c writes a single byte",
        ),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

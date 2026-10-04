//! `-0.0` and `0.0` are equal and are not the same constant: `signbit` and the printed form tell
//! them apart. Every expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect()
}

#[test]
fn equal_constants_written_differently_are_kept_apart() {
    let database = Database::new();
    for (sql, expected) in [
        (
            "SELECT signbit(x) FROM (VALUES ('-0'::DOUBLE) UNION ALL SELECT '0'::DOUBLE) v(x)",
            vec!["true", "false"],
        ),
        (
            "SELECT x FROM (VALUES ('-0'::DOUBLE) UNION ALL SELECT '0'::DOUBLE) v(x)",
            vec!["-0.0", "0.0"],
        ),
        (
            "SELECT signbit(x), signbit(y) FROM (SELECT '-0'::DOUBLE AS x, '0'::DOUBLE AS y)",
            vec!["true|false"],
        ),
    ] {
        assert_eq!(answered(&database, sql), expected, "{sql}");
    }
}

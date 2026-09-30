//! `format_bytes`, its spellings, and `parse_formatted_bytes`.
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
fn a_count_of_bytes_is_written_and_read_the_way_the_pin_does_it() {
    let cases = [
        ("SELECT format_bytes(1023)", "1023 bytes"),
        ("SELECT format_bytes('1')", "1 byte"),
        ("SELECT format_bytes(1::TINYINT)", "1 byte"),
        ("SELECT format_bytes(1::UINTEGER)", "1 byte"),
        ("SELECT pg_size_pretty(1500)", "1.4 KiB"),
        ("SELECT formatReadableSize(500*1000*1000)", "476.8 MiB"),
        ("SELECT formatReadableDecimalSize(500*1000*1000)", "500.0 MB"),
        ("SELECT format_bytes(-9223372036854775808)", "-8192.0 PiB"),
        ("SELECT format_bytes(NULL)", "NULL"),
        ("SELECT typeof(parse_formatted_bytes('1b'))", "UBIGINT"),
        ("SELECT parse_formatted_bytes('  2  mib  ')", "2097152"),
        ("SELECT parse_formatted_bytes('1 KB extra')", "1000"),
        ("SELECT parse_formatted_bytes('18446744073709551615 b')", "9223372036854775808"),
        ("SELECT parse_formatted_bytes(NULL)", "NULL"),
        ("SELECT try(parse_formatted_bytes('x'))", "NULL"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), [expected], "{sql}");
    }
}

#[test]
fn what_the_pin_refuses_is_refused_in_its_words() {
    let cases = [
        (
            "SELECT format_bytes(1.5)",
            "No function matches the given name and argument types 'format_bytes(DECIMAL(2,1))'",
        ),
        ("SELECT format_bytes(1::UBIGINT)", "format_bytes(col0 BIGINT) -> VARCHAR"),
        ("SELECT pg_size_pretty(1.5)", "'format_bytes(DECIMAL(2,1))'"),
        ("SELECT formatreadablesize(1.5)", "formatReadableSize(col0 BIGINT) -> VARCHAR"),
        ("SELECT parse_formatted_bytes(1)", "parse_formatted_bytes(col0 VARCHAR) -> UBIGINT"),
        ("SELECT parse_formatted_bytes('-1 b')", "Invalid Input Error: Memory cannot be negative"),
        ("SELECT parse_formatted_bytes('5')", "Unknown unit for memory: ''"),
        ("SELECT parse_formatted_bytes('abc')", "Memory must have a number (e.g. 1GB)"),
        ("SELECT format_bytes('x')", "Could not convert string 'x' to INT64"),
        ("SELECT format_bytes('1'::VARCHAR)", "'format_bytes(VARCHAR)'"),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

#[test]
fn a_column_is_named_by_the_spelling_that_was_written() {
    let cases = [
        ("SELECT pg_size_pretty(1500)", "pg_size_pretty(1500)"),
        ("SELECT formatReadableSize(1500)", "formatreadablesize(1500)"),
        ("SELECT format_bytes(1::TINYINT)", "format_bytes(CAST(1 AS TINYINT))"),
    ];
    for (sql, expected) in cases {
        let result = Database::new().query(sql).unwrap();
        assert_eq!(result.names(), [expected], "{sql}");
    }
}

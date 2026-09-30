//! `make_time`, `make_timestamp` and `make_timestamp_ns`, which build a moment out of numbers.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn made(sql: &str) -> String {
    let database = Database::new();
    let sql = format!("SELECT {sql}");
    let result = database.query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let row = result.rows().next().expect("one row");
    row.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
}

fn refused(sql: &str) -> String {
    Database::new().query(&format!("SELECT {sql}")).unwrap_err().to_string()
}

#[test]
fn a_time_is_built_out_of_its_fields() {
    let cases = [
        (
            "make_time(10, 11, 12.5), make_time(10, 0, 59.9999999), make_time(24, 0, 0), make_time(10, 0, 60), typeof(make_time(1,2,3))",
            "10:11:12.5,10:01:00,24:00:00,10:01:00,TIME",
        ),
        ("make_time(10, 0, 60.5), make_time(10, 0, 59.9999996)", "10:01:00.5,10:01:00"),
        (
            "make_time(10, 0, 12.3456785), make_time(10, 0, 12.0000005), make_time(NULL, 0, 0)",
            "10:00:12.345679,10:00:12.000001,NULL",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(made(sql), expected, "{sql}");
    }
}

#[test]
fn a_timestamp_is_built_out_of_its_fields_or_a_count() {
    let cases = [
        (
            "make_timestamp(2020, 1, 5, 10, 11, 12.5), make_timestamp(1600000000000000), typeof(make_timestamp(1)), make_timestamp(-1)",
            "2020-01-05 10:11:12.5,2020-09-13 12:26:40,TIMESTAMP,1969-12-31 23:59:59.999999",
        ),
        (
            "make_timestamp(2020, 1, 1, 24, 0, 0), make_timestamp(2020, 1, 1, 23, 59, 59.9999999)",
            "2020-01-02 00:00:00,2020-01-02 00:00:00",
        ),
        (
            "make_timestamp_ns(1600000000000000123), typeof(make_timestamp_ns(1)), make_timestamp_ns(-1)",
            "2020-09-13 12:26:40.000000123,TIMESTAMP_NS,1969-12-31 23:59:59.999999999",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(made(sql), expected, "{sql}");
    }
}

#[test]
fn a_field_out_of_range_is_refused_in_the_pins_words() {
    let cases = [
        ("make_time(-1, 0, 0)", "Conversion Error: Time out of range: -1:0:0.0"),
        ("make_time(10, 60, 0)", "Time out of range: 10:60:0.0"),
        ("make_time(10, 0, -0.5)", "Time out of range: 10:0:0.-500000"),
        ("make_time(24, 0, 0.5)", "Time out of range: 24:0:0.500000"),
        ("make_time(10, 0, 'nan'::DOUBLE)", "Time out of range: 10:0:-2147483648.-2147483648"),
        (
            "make_time(10, 0, 1e20)",
            "Invalid Input Error: Type DOUBLE with value 1e+20 can't be cast because the value is out of range for the destination type INT32",
        ),
        (
            "make_time(10000000000, 0, 0)",
            "Invalid Input Error: Type INT64 with value 10000000000 can't be cast because the value is out of range for the destination type INT32",
        ),
        (
            "make_timestamp(2020, 2, 30, 10, 11, 12.5)",
            "Conversion Error: Date out of range: 2020-2-30",
        ),
        ("make_timestamp(2020, 13, 1, 0, 0, 0)", "Date out of range: 2020-13-1"),
        (
            "make_timestamp(300000, 1, 1, 0, 0, 0)",
            "Conversion Error: Date and time not in timestamp range",
        ),
        ("make_timestamp(2020, 1, 1, 25, 0, 0)", "Time out of range: 25:0:0.0"),
        (
            "make_timestamp(3000000000, 1, 1, 0, 0, 0)",
            "Type INT64 with value 3000000000 can't be cast because the value is out of range for the destination type INT32",
        ),
        (
            "make_timestamp(9223372036854775807)",
            "Conversion Error: Timestamp microseconds out of range: 9223372036854775807",
        ),
        (
            "make_timestamp(-9223372036854775807)",
            "Timestamp microseconds out of range: -9223372036854775807",
        ),
        (
            "make_timestamp_ns(9223372036854775807)",
            "Timestamp microseconds out of range: 9223372036854775807",
        ),
        (
            "make_timestamp(1, 2)",
            "\tmake_timestamp(col0 BIGINT, col1 BIGINT, col2 BIGINT, col3 BIGINT, col4 BIGINT, col5 DOUBLE) -> TIMESTAMP\n\tmake_timestamp(col0 BIGINT) -> TIMESTAMP",
        ),
        ("make_time(1, 2)", "\tmake_time(col0 BIGINT, col1 BIGINT, col2 DOUBLE) -> TIME"),
        ("make_timestamp_ns(1.5)", "\tmake_timestamp_ns(col0 BIGINT) -> TIMESTAMP_NS"),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

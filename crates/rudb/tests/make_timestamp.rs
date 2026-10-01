//! `make_time`, `make_timestamp` and the rest of the calls that build a moment out of numbers.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb. The calls
//! that answer an instant are run in a zone the test sets, since the default is the zone of the
//! machine running the test.

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

/// Every row of `SELECT {sql}` under `zone`, one row per line and the cells joined with a bar.
fn under(zone: &str, sql: &str) -> String {
    let database = Database::new();
    database.execute(&format!("SET TimeZone = '{zone}'")).expect("a known zone");
    let sql = format!("SELECT {sql}");
    let result = database.query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let width = result.rows().next().map_or(0, |row| row.len());
    (0..result.len())
        .map(|row| {
            (0..width).map(|column| result.text_at(row, column)).collect::<Vec<_>>().join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn refused_under(zone: &str, sql: &str) -> String {
    let database = Database::new();
    database.execute(&format!("SET TimeZone = '{zone}'")).expect("a known zone");
    database.query(&format!("SELECT {sql}")).unwrap_err().to_string()
}

const NEW_YORK: &str = "America/New_York";

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

#[test]
fn an_instant_is_built_out_of_a_wall_clock_in_the_session_zone_or_a_named_one() {
    assert_eq!(
        under(
            NEW_YORK,
            "make_timestamptz(2020, 7, 1, 12, 0, 0.5), make_timestamptz(2020, 7, 1, 12, 0, 0, 'Asia/Tokyo'), make_timestamptz(1593604800000000), typeof(make_timestamptz(0))"
        ),
        "2020-07-01 12:00:00.5-04|2020-06-30 23:00:00-04|2020-07-01 08:00:00-04|TIMESTAMP WITH TIME ZONE"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "make_timestamptz(2020, 3, 8, 2, 30, 0), make_timestamptz(2020, 11, 1, 1, 30, 0), make_timestamptz(2020, 13, 1, 0, 0, 0), make_timestamptz(2020, -13, 1, 0, 0, 0)"
        ),
        "2020-03-08 03:30:00-04|2020-11-01 01:30:00-05|2021-01-01 00:00:00-05|2018-11-01 00:00:00-04"
    );
    assert_eq!(
        under(
            "UTC",
            "make_timestamptz(x, 1, 1, 0, 0, 0, z) FROM (VALUES (2020, 'Asia/Tokyo'), (2021, 'Europe/Berlin'), (2022, NULL)) v(x, z)"
        ),
        "2019-12-31 15:00:00+00\n2020-12-31 23:00:00+00\nNULL"
    );
}

#[test]
fn the_fields_of_a_wall_clock_carry_into_each_other() {
    assert_eq!(
        under(
            NEW_YORK,
            "make_timestamptz(2020, 1, 0, 0, 0, 0), make_timestamptz(2020, 0, 1, 0, 0, 0), make_timestamptz(2020, 1, 1, 25, 61, 61.5), make_timestamptz(2020, 1, 1, -1, -1, -1.5), make_timestamptz(2020, 2, 30, 0, 0, 0)"
        ),
        "2019-12-31 00:00:00-05|2019-12-01 00:00:00-05|2020-01-02 02:02:01.5-05|2019-12-31 22:58:58.5-05|2020-03-01 00:00:00-05"
    );
    assert_eq!(
        under(
            "UTC",
            "make_timestamptz(-1, 1, 1, 0, 0, 0), make_timestamptz(0, 1, 1, 0, 0, 0), make_timestamptz(-44, 3, 15, 0, 0, 0), make_timestamptz(1582, 10, 10, 0, 0, 0)"
        ),
        "0001-01-01 (BC) 00:00:00+00|0001-01-01 (BC) 00:00:00+00|0044-03-15 (BC) 00:00:00+00|1582-10-10 00:00:00+00"
    );
    assert_eq!(
        under(
            "UTC",
            "make_timestamptz(2020, 1, 1, 0, 0, 59.9999999), make_timestamptz(2020, 1, 1, 0, 0, 0.0015), make_timestamptz(2020, 1, 1, 0, 0, 0.0000005)"
        ),
        "2020-01-01 00:01:00+00|2020-01-01 00:00:00.0015+00|2020-01-01 00:00:00.000001+00"
    );
}

#[test]
fn a_wall_clock_that_cannot_be_built_is_refused_in_the_pins_words() {
    let cases = [
        (
            "make_timestamptz(2020, 1, 1, 0, 0, 'nan'::DOUBLE)",
            "Invalid Input Error: Type DOUBLE with value nan can't be cast because the value is out of range for the destination type INT32",
        ),
        (
            "make_timestamptz(2020, 1, 1, 0, 0, 3e9)",
            "Invalid Input Error: Type DOUBLE with value 3000000000.0 can't be cast because the value is out of range for the destination type INT32",
        ),
        (
            "make_timestamptz(5000000000, 1, 1, 0, 0, 0)",
            "Invalid Input Error: Type INT64 with value 5000000000 can't be cast because the value is out of range for the destination type INT32",
        ),
        (
            "make_timestamptz(300000, 1, 1, 0, 0, 0)",
            "Conversion Error: ICU date overflows timestamp range",
        ),
        (
            "make_timestamptz(9223372036854775807)",
            "Conversion Error: Timestamp microseconds out of range: 9223372036854775807",
        ),
        (
            "make_timestamptz(2020, 7, 1, 12, 0, 0, 'Nope')",
            "Not implemented Error: Unknown TimeZone 'Nope'!",
        ),
    ];
    for (sql, expected) in cases {
        let said = refused_under("UTC", sql);
        assert!(said.starts_with(expected), "{sql}: {said}");
    }
}

#[test]
fn a_count_of_seconds_or_milliseconds_is_a_moment() {
    assert_eq!(
        under(
            "UTC",
            "to_timestamp(1284352323.5), to_timestamp(-1.25), to_timestamp(1.0000015), to_timestamp(-0.0000005), to_timestamp(NULL), to_timestamp(9223372036854.774)"
        ),
        "2010-09-13 04:32:03.5+00|1969-12-31 23:59:58.75+00|1970-01-01 00:00:01.000002+00|1970-01-01 00:00:00+00|NULL|294247-01-10 04:00:54.77376+00"
    );
    assert_eq!(
        under(
            "UTC",
            "make_timestamp_ms(1593604800123), typeof(make_timestamp_ms(0)), make_timestamp_ms(-1), make_timestamp_ms(9223372036854775)"
        ),
        "2020-07-01 12:00:00.123|TIMESTAMP|1969-12-31 23:59:59.999|294247-01-10 04:00:54.775"
    );
    for sql in
        ["to_timestamp(1e20)", "to_timestamp('nan'::DOUBLE)", "to_timestamp(9223372036854.775807)"]
    {
        assert_eq!(
            refused(sql),
            "Conversion Error: Epoch seconds out of range for TIMESTAMP WITH TIME ZONE",
            "{sql}"
        );
    }
    assert_eq!(
        refused("make_timestamp_ms(9223372036854776)"),
        "Conversion Error: Could not convert Timestamp(MS) to Timestamp(US)"
    );
}

#[test]
fn an_interval_is_normalized_into_months_and_days_that_are_never_negative() {
    assert_eq!(
        under(
            "UTC",
            "normalized_interval(INTERVAL '35 days 30 hours'), normalized_interval(INTERVAL '-35 days 30 hours'), normalized_interval(INTERVAL '1 month 40 days 25 hours 70 minutes'), normalized_interval(INTERVAL '0 days -1 microsecond')"
        ),
        "1 month 6 days 06:00:00|-2 months 26 days 06:00:00|2 months 11 days 02:10:00|-1 month 29 days 23:59:59.999999"
    );
    assert_eq!(
        under(
            "UTC",
            "normalized_interval(INTERVAL '-1 month 50 days'), normalized_interval(INTERVAL '400 days'), normalized_interval(INTERVAL '-30 days -25 hours')"
        ),
        "20 days|1 year 1 month 10 days|-2 months 28 days 23:00:00"
    );
    assert_eq!(
        under(
            "UTC",
            "normalized_interval(to_months(2147483647) + INTERVAL 40 days), normalized_interval(to_days(2147483647) + to_microseconds(9223372036854775807)), normalized_interval(to_months(-2147483647) - INTERVAL 40 days)"
        ),
        "178956970 years 7 months 40 days|6261765 years 7 months 28 days 04:00:54.775807|-178956970 years -8 months -10 days"
    );
}

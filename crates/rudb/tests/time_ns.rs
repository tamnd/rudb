//! `TIME_NS` and `TIMESTAMPTZ_NS`, the time of day and the zoned moment counted in nanoseconds.
//! Every expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237. Every test
//! sets the zone itself, since the default is the zone of the machine running the test.

use rudb::Database;

fn zoned(zone: &str) -> Database {
    let database = Database::new();
    database.execute(&format!("SET TimeZone = '{zone}'")).expect("a known zone");
    database
}

/// Every row of `sql` as the shell writes it, which is where a zoned value takes the session zone.
fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let width = result.rows().next().map_or(0, |row| row.len());
    (0..result.len())
        .map(|row| {
            (0..width).map(|column| result.text_at(row, column)).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn refused(database: &Database, sql: &str, expected: &str) {
    let error = database.execute(sql).expect_err(sql);
    assert!(error.to_string().starts_with(expected), "{sql}: {error}");
}

#[test]
fn a_time_ns_keeps_nine_digits() {
    let database = zoned("UTC");
    for (sql, expected) in [
        (
            "SELECT '15:30:00.123456789'::TIME_NS, '24:00:00'::TIME_NS, '1:2:3'::TIME_NS, \
             '2020-01-01 12:00:00.5+05'::TIME_NS",
            "15:30:00.123456789|24:00:00|01:02:03|12:00:00.5",
        ),
        (
            "SELECT '12:00:00.9999999'::TIME_NS::TIME, '23:59:59.999999999'::TIME_NS::TIME, \
             TIME '12:34:56.123456'::TIME_NS",
            "12:00:01|24:00:00|12:34:56.123456",
        ),
        ("SELECT TIMESTAMP_NS '2020-01-01 12:34:56.123456789'::TIME_NS", "12:34:56.123456789"),
        ("SELECT TRY_CAST('abc' AS TIME_NS), typeof('12:00:00'::TIME_NS)", "NULL|TIME_NS"),
        (
            "SELECT '12:00:00'::TIME_NS = '12:00:00'::TIME_NS, \
             '12:00:00.000000001'::TIME_NS > '12:00:00', '12:00:00'::TIME_NS::VARCHAR",
            "true|true|12:00:00",
        ),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
    refused(
        &database,
        "SELECT 'abc'::TIME_NS",
        "Conversion Error: time field value out of range: \"abc\", expected format is \
         ([YYYY-MM-DD ]HH:MM:SS[.MS])",
    );
    refused(
        &database,
        "SELECT '12:00:00'::TIME_NS::TIMETZ",
        "Conversion Error: Unimplemented type for cast (TIME_NS -> TIME WITH TIME ZONE)",
    );
}

#[test]
fn a_time_ns_answers_the_parts_of_a_time() {
    let database = zoned("UTC");
    for (sql, expected) in [
        (
            "SELECT date_part('hour', '23:59:59.123456789'::TIME_NS), \
             date_part('epoch', '23:59:59.123456789'::TIME_NS), \
             nanosecond('23:59:59.123456789'::TIME_NS), epoch_ns('23:59:59.123456789'::TIME_NS)",
            "23|86399.123456|59123456789|86399123456789",
        ),
        (
            "SELECT date_part(['minute', 'epoch'], '23:59:59.123456789'::TIME_NS), \
             hour('13:00:00'::TIME_NS), microsecond('13:00:01.123456789'::TIME_NS)",
            "{'minute': 59, 'epoch': 86399.123456}|13|1123456",
        ),
        (
            "SELECT date_part('hour', '24:00:00'::TIME_NS), date_part('hour', TIME '24:00:00'), \
             TRY_CAST(TIMESTAMP 'infinity' AS TIMETZ)",
            "24|24|NULL",
        ),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
    refused(
        &database,
        "SELECT date_part('year', '23:59:59.123456789'::TIME_NS)",
        "Not implemented Error: \"time_ns\" units \"year\" not recognized",
    );
    refused(
        &database,
        "SELECT CAST(TIMESTAMP 'infinity' AS TIMETZ)",
        "Conversion Error: Can't get TIME of infinite TIMESTAMP",
    );
}

#[test]
fn values_order_group_and_round_trip() {
    let database = zoned("UTC");
    for (sql, expected) in [
        (
            "SELECT min(x), max(x), count(DISTINCT x) FROM (VALUES ('12:00:00.000000002'::TIME_NS), \
             ('12:00:00.000000001'::TIME_NS), ('12:00:00.000000001'::TIME_NS)) t(x)",
            "12:00:00.000000001|12:00:00.000000002|2",
        ),
        (
            "SELECT max(x), min(x) FROM (VALUES \
             ('2020-01-01 00:00:00.000000001'::TIMESTAMPTZ_NS), \
             ('2020-01-01 00:00:00.000000002'::TIMESTAMPTZ_NS)) t(x)",
            "2020-01-01 00:00:00.000000002+00|2020-01-01 00:00:00.000000001+00",
        ),
        ("SELECT histogram(x) FROM (VALUES ('12:00:00.5'::TIME_NS)) t(x)", "{'12:00:00.5'=1}"),
        (
            "SELECT '12:00:00.000000001'::TIME_NS::VARIANT, \
             variant_typeof('12:00:00'::TIME_NS::VARIANT), \
             variant_typeof('2020-01-01'::TIMESTAMPTZ_NS::VARIANT)",
            "12:00:00.000000001|TIME_NANOS|TIMESTAMP_NANOS_TZ",
        ),
        (
            "SELECT '2020-01-01 12:00:00.000000001'::TIMESTAMPTZ_NS::VARIANT::TIMESTAMPTZ_NS",
            "2020-01-01 12:00:00.000000001+00",
        ),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
    database.execute("CREATE TABLE t(a TIME_NS, b TIMESTAMPTZ_NS)").expect("creates");
    database
        .execute(
            "INSERT INTO t VALUES ('23:59:59.999999999', '2020-01-01 12:34:56.123456789'), \
             ('00:00:00.000000001', NULL)",
        )
        .expect("inserts");
    assert_eq!(
        answered(&database, "SELECT a, b FROM t ORDER BY a"),
        ["00:00:00.000000001|NULL", "23:59:59.999999999|2020-01-01 12:34:56.123456789+00"]
    );
}

#[test]
fn a_timestamptz_ns_is_read_and_written_in_the_session_zone() {
    let database = zoned("Asia/Kolkata");
    for (sql, expected) in [
        (
            "SELECT '2020-01-01 12:34:56.123456789'::TIMESTAMPTZ_NS, \
             typeof('2020-01-01'::TIMESTAMPTZ_NS)",
            "2020-01-01 12:34:56.123456789+05:30|TIMESTAMPTZ_NS",
        ),
        (
            "SELECT '2020-01-01 12:34:56.123456789+02'::TIMESTAMPTZ_NS, \
             '2020-01-01T12:00:00Z'::TIMESTAMPTZ_NS",
            "2020-01-01 16:04:56.123456789+05:30|2020-01-01 17:30:00+05:30",
        ),
        (
            "SELECT '2020-01-01 12:34:56.123456789'::TIMESTAMPTZ_NS::TIMESTAMPTZ, \
             '2020-01-01 12:34:56.123456789'::TIMESTAMPTZ_NS::TIMESTAMP_NS",
            "2020-01-01 12:34:56.123457+05:30|2020-01-01 12:34:56.123456789",
        ),
        (
            "SELECT '2020-01-01 12:34:56.123456789'::TIMESTAMPTZ_NS::TIMETZ, \
             'infinity'::TIMESTAMPTZ_NS",
            "07:04:56.123457+00|infinity",
        ),
        (
            "SELECT DATE '2020-01-01'::TIMESTAMPTZ_NS, \
             TIMESTAMP '2020-01-01 01:02:03'::TIMESTAMPTZ_NS, \
             TIMESTAMP_NS '2020-01-01 01:02:03.000000004'::TIMESTAMPTZ_NS",
            "2020-01-01 05:30:00+05:30|2020-01-01 06:32:03+05:30|2020-01-01 \
             01:02:03.000000004+05:30",
        ),
        (
            "SELECT nanosecond('2020-01-01 12:34:56.123456789'::TIMESTAMPTZ_NS), \
             epoch_ns('2020-01-01 12:34:56.123456789'::TIMESTAMPTZ_NS)",
            "56123456789|1577862296123456789",
        ),
        ("SELECT TRY_CAST('2300-01-01' AS TIMESTAMPTZ_NS)", "NULL"),
        (
            "SELECT to_json('2020-01-01 12:34:56.123456789'::TIMESTAMPTZ_NS), \
             ['2020-01-01'::TIMESTAMPTZ_NS]",
            "\"2020-01-01 12:34:56.123456789+05:30\"|['2020-01-01 00:00:00+05:30']",
        ),
        (
            "SELECT typeof([TIMESTAMP '2020-01-01', '2020-01-01'::TIMESTAMPTZ_NS]), \
             greatest(DATE '2020-01-02', '2020-01-01'::TIMESTAMPTZ_NS)",
            "TIMESTAMPTZ_NS[]|2020-01-02 05:30:00+05:30",
        ),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
    refused(
        &database,
        "SELECT '2300-01-01'::TIMESTAMPTZ_NS",
        "Conversion Error: timestamp field value out of range: \"2300-01-01\"",
    );
    refused(
        &database,
        "SELECT 'abc'::TIMESTAMPTZ_NS",
        "Conversion Error: invalid timestamp field format: \"abc\", expected format is \
         (YYYY-MM-DD HH:MM[:SS[.US]][±HH[:MM[:SS]]| ZONE])",
    );
    refused(
        &database,
        "SELECT '12:00:00'::TIME_NS < TIME '12:00:00'",
        "Binder Error: Cannot compare values of type TIME_NS and type TIME - an explicit cast is \
         required",
    );
}

//! `time_bucket`, which rounds a date, a timestamp or a time down to the start of its bucket.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn bucketed(sql: &str) -> String {
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
fn a_moment_rounds_down_from_the_timescale_origins() {
    let cases = [
        (
            "time_bucket(INTERVAL '2 days', DATE '2019-04-05'), time_bucket(INTERVAL '1 month', DATE '2019-04-05'), time_bucket(INTERVAL '3 hours', TIMESTAMP '2019-04-05 10:11:12')",
            "2019-04-05,2019-04-01,2019-04-05 09:00:00",
        ),
        (
            "time_bucket(INTERVAL '3 months', TIMESTAMP '1965-02-03 10:00:00'), time_bucket(INTERVAL '7 days', DATE '1965-02-03')",
            "1965-01-01 00:00:00,1965-02-01",
        ),
        (
            "time_bucket(INTERVAL '30 minutes', TIME '10:47:00'), time_bucket(INTERVAL '1 month', TIME '10:47:00'), time_bucket(INTERVAL '1 hour', TIME '10:47:00', TIME '00:15:00')",
            "10:30:00,00:00:00,10:15:00",
        ),
        (
            "time_bucket(INTERVAL '1 day', TIMESTAMP_NS '2019-04-05 10:00:00'), typeof(time_bucket(INTERVAL '1 day', TIMESTAMP_NS '2019-04-05 10:00:00'))",
            "2019-04-05 00:00:00,TIMESTAMP",
        ),
        ("time_bucket('1 day', DATE '2019-04-05')", "2019-04-05"),
    ];
    for (sql, expected) in cases {
        assert_eq!(bucketed(sql), expected, "{sql}");
    }
}

#[test]
fn an_origin_or_an_offset_moves_the_buckets() {
    let cases = [
        (
            "time_bucket(INTERVAL '1 week', TIMESTAMP '2019-04-05 10:11:12', INTERVAL '1 day'), time_bucket(INTERVAL '2 months', DATE '2019-04-05', DATE '2000-02-01')",
            "2019-04-02 00:00:00,2019-04-01",
        ),
        (
            "time_bucket(INTERVAL '1 day', DATE '2019-04-05', INTERVAL '-3 hours'), typeof(time_bucket(INTERVAL '1 day', DATE '2019-04-05', INTERVAL '-3 hours'))",
            "2019-04-04,DATE",
        ),
        (
            "time_bucket(INTERVAL '1 day', DATE '2019-04-05', TIMESTAMP '2000-01-01 10:00:00'), typeof(time_bucket(INTERVAL '1 day', DATE '2019-04-05', TIMESTAMP '2000-01-01 10:00:00'))",
            "2019-04-04 10:00:00,TIMESTAMP",
        ),
        (
            "time_bucket(INTERVAL '1 day', TIMESTAMP '2019-04-05 12:00:00', DATE '2000-01-01')",
            "2019-04-05 00:00:00",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(bucketed(sql), expected, "{sql}");
    }
}

#[test]
fn a_null_or_an_infinity_has_no_bucket() {
    assert_eq!(
        bucketed(
            "time_bucket(NULL::INTERVAL, DATE '2019-04-05'), time_bucket(INTERVAL '1 day', NULL::DATE), time_bucket(INTERVAL '1 day', DATE '2019-04-05', NULL::DATE)"
        ),
        "NULL,NULL,NULL"
    );
    assert_eq!(
        bucketed(
            "time_bucket(INTERVAL '1 day', 'infinity'::DATE), time_bucket(INTERVAL '1 day', DATE '2019-04-05', 'infinity'::DATE), time_bucket(INTERVAL '1 month', '-infinity'::TIMESTAMP)"
        ),
        "infinity,NULL,-infinity"
    );
}

#[test]
fn a_bad_width_is_refused_in_the_pins_words() {
    let cases = [
        ("time_bucket(INTERVAL '0 days', DATE '2019-04-05')", "Period must be greater than 0"),
        ("time_bucket(INTERVAL '-1 month', DATE '2019-04-05')", "Period must be greater than 0"),
        (
            "time_bucket(INTERVAL '1 month 1 day', DATE '2019-04-05')",
            "Month intervals cannot have day or time component",
        ),
        (
            "time_bucket(i, d) FROM (VALUES (INTERVAL '1 month 1 day', DATE '2019-04-05')) t(i, d)",
            "Month intervals cannot have day or time component",
        ),
        (
            "time_bucket(INTERVAL '106751991' DAY, TIMESTAMP '1800-01-01 00:00:00', TIMESTAMP '1900-01-01 00:00:00')",
            "Overflow in addition of INT64 (-9223372022400000000 + -2208988800000000)!",
        ),
        (
            "time_bucket(INTERVAL '2147483647' MONTH, DATE '1700-01-01', DATE '1800-01-01')",
            "Overflow in addition of INT32 (-2147483647 + -2040)!",
        ),
        (
            "time_bucket(INTERVAL '1 day', 5::INTEGER)",
            "time_bucket(col0 INTERVAL, col1 TIMESTAMP WITH TIME ZONE, col2 VARCHAR) -> TIMESTAMP WITH TIME ZONE",
        ),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

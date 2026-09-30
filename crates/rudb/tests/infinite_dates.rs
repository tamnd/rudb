//! The two infinities a date or a timestamp can hold, from the text that spells them through the
//! casts and the arithmetic that keep them and the calendar functions that have nothing to say
//! about them.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

/// The one row a query answers, as text with a comma between the columns.
fn answer(sql: &str) -> String {
    let database = Database::new();
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let row = result.rows().next().expect("one row");
    row.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
}

fn error(sql: &str) -> String {
    Database::new().query(sql).expect_err(sql).to_string()
}

#[test]
fn the_text_of_an_infinity_is_read_the_way_the_pin_reads_it() {
    let cases = [
        ("SELECT 'infinity'::DATE, '-infinity'::DATE", "infinity,-infinity"),
        ("SELECT 'Infinity'::TIMESTAMP, ' -INFINITY'::TIMESTAMPTZ", "infinity,-infinity"),
        ("SELECT 'inf'::DATE, '-inf'::TIMESTAMP", "infinity,-infinity"),
        (
            "SELECT 'infinity  '::DATE, 'epoch'::DATE, '-epoch'::TIMESTAMP",
            "infinity,1970-01-01,1970-01-01 00:00:00",
        ),
        ("SELECT '-infinity'::TIMESTAMP::VARCHAR, 'infinity'::DATE::VARCHAR", "-infinity,infinity"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answer(sql), expected, "{sql}");
    }
    for sql in ["SELECT '+infinity'::DATE", "SELECT 'inf '::DATE", "SELECT 'infin'::DATE"] {
        assert!(error(sql).contains("Conversion Error"), "{sql}");
    }
}

#[test]
fn a_cast_between_dates_and_timestamps_keeps_the_infinity() {
    let cases = [
        (
            "SELECT 'infinity'::DATE::TIMESTAMP, '-infinity'::DATE::TIMESTAMPTZ",
            "infinity,-infinity",
        ),
        ("SELECT 'infinity'::TIMESTAMP::DATE, '-infinity'::TIMESTAMP::DATE", "infinity,-infinity"),
        (
            "SELECT 'infinity'::DATE::TIMESTAMP_NS, 'infinity'::TIMESTAMP_S::DATE",
            "infinity,infinity",
        ),
        (
            "SELECT 'infinity'::DATE::TIMESTAMP_S, '-infinity'::TIMESTAMP_MS::TIMESTAMP",
            "infinity,-infinity",
        ),
        ("SELECT 'infinity'::DATE::TIMESTAMP = 'infinity'::TIMESTAMP", "true"),
        ("SELECT 'infinity'::DATE < 'infinity'::TIMESTAMP", "false"),
        ("SELECT 'infinity'::DATE > DATE '2020-01-01'", "true"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answer(sql), expected, "{sql}");
    }
    assert!(
        error("SELECT 'infinity'::TIMESTAMP::TIME")
            .contains("Can't get TIME of infinite TIMESTAMP")
    );
}

#[test]
fn arithmetic_leaves_an_infinity_where_it_is() {
    let cases = [
        ("SELECT 'infinity'::DATE + 1, '-infinity'::DATE - 1", "infinity,-infinity"),
        ("SELECT 'infinity'::DATE - DATE '2020-01-01'", "2147465385"),
        (
            "SELECT 'infinity'::TIMESTAMP + INTERVAL 1 DAY, '-infinity'::TIMESTAMP - INTERVAL 1 DAY",
            "infinity,-infinity",
        ),
        (
            "SELECT 'infinity'::DATE + INTERVAL 1 DAY, typeof('infinity'::DATE + INTERVAL 1 DAY)",
            "infinity,TIMESTAMP",
        ),
        ("SELECT '-infinity'::DATE + TIME '10:00:00'", "-infinity"),
        ("SELECT age('infinity'::TIMESTAMP, TIMESTAMP '2020-01-01')", "NULL"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answer(sql), expected, "{sql}");
    }
    let said = error("SELECT 'infinity'::TIMESTAMP - TIMESTAMP '2020-01-01'");
    assert!(said.contains("Cannot subtract infinite timestamps"), "{said}");
}

#[test]
fn an_infinity_has_no_parts_and_truncates_to_itself() {
    let cases = [
        (
            "SELECT date_part('year', 'infinity'::DATE), date_part('epoch', 'infinity'::TIMESTAMP), extract(month FROM '-infinity'::DATE)",
            "NULL,NULL,NULL",
        ),
        (
            "SELECT date_trunc('month', 'infinity'::TIMESTAMP), date_trunc('day', '-infinity'::TIMESTAMP)",
            "infinity,-infinity",
        ),
        (
            "SELECT isinf('infinity'::DATE), isfinite('-infinity'::TIMESTAMP), isinf(DATE '2020-01-01'), isfinite('infinity'::TIMESTAMPTZ)",
            "true,false,false,false",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answer(sql), expected, "{sql}");
    }
}

#[test]
fn a_column_with_an_infinity_in_it_answers_row_by_row() {
    let database = Database::new();
    database
        .execute("CREATE TABLE t AS SELECT * FROM (VALUES (DATE '2020-05-05', TIMESTAMP '2020-05-05 10:11:12'), ('infinity'::DATE, '-infinity'::TIMESTAMP)) v(d, ts)")
        .unwrap();
    let result = database
        .query("SELECT extract(year FROM d), date_part('minute', ts), date_trunc('hour', ts) FROM t ORDER BY d")
        .unwrap();
    let rows: Vec<String> = result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join(","))
        .collect();
    assert_eq!(rows, ["2020,11,2020-05-05 10:00:00", "NULL,NULL,-infinity"]);
}

#[test]
fn make_date_refuses_the_day_that_is_the_infinity() {
    let said = error("SELECT make_date(5881580, 7, 11)");
    assert!(said.contains("Date out of range: 5881580-7-11"), "{said}");
}

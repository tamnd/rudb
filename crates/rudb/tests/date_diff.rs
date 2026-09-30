//! `date_diff` and `date_sub`, which count a part between two moments in two different ways.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn counted(sql: &str) -> String {
    let database = Database::new();
    let sql = format!("SELECT {sql}");
    let result = database.query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let row = result.rows().next().expect("one row");
    row.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
}

fn refused(sql: &str) -> String {
    Database::new().query(&format!("SELECT {sql}")).unwrap_err().to_string()
}

fn column(sql: &str) -> Vec<String> {
    let database = Database::new();
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result.rows().map(|row| row[0].to_string()).collect()
}

#[test]
fn date_diff_counts_the_boundaries_crossed() {
    let cases = [
        (
            "date_diff('year', DATE '2019-12-31', DATE '2020-01-01'), date_diff('month', DATE '2020-01-31', DATE '2020-02-01'), date_diff('day', DATE '2020-01-01', DATE '2020-03-01'), date_diff('week', DATE '2020-01-01', DATE '2020-01-14'), date_diff('quarter', DATE '2020-03-31', DATE '2020-04-01')",
            "1,1,60,1,1",
        ),
        (
            "date_diff('decade', DATE '2019-12-31', DATE '2020-01-01'), date_diff('century', DATE '1999-12-31', DATE '2000-01-01'), date_diff('millennium', DATE '1999-12-31', DATE '2000-01-01'), date_diff('isoyear', DATE '2020-12-31', DATE '2021-01-04')",
            "1,1,1,1",
        ),
        (
            "date_diff('hour', DATE '2020-01-01', DATE '2020-01-02'), date_diff('minute', DATE '2020-01-01', DATE '2020-01-02'), date_diff('second', DATE '2020-01-01', DATE '2020-01-02'), date_diff('millisecond', DATE '2020-01-01', DATE '2020-01-02'), date_diff('microsecond', DATE '2020-01-01', DATE '2020-01-02'), date_diff('epoch', DATE '2020-01-01', DATE '2020-01-02')",
            "24,1440,86400,86400000,86400000000,86400",
        ),
        (
            "date_diff('hour', TIMESTAMP '2020-01-01 10:59:59', TIMESTAMP '2020-01-01 11:00:00'), date_diff('second', TIMESTAMP '1969-12-31 23:59:59.5', TIMESTAMP '1970-01-01 00:00:00'), date_diff('millisecond', TIMESTAMP '1969-12-31 23:59:59.9995', TIMESTAMP '1970-01-01 00:00:00')",
            "1,1,1",
        ),
        (
            "date_diff('minute', DATE '1969-12-31', DATE '1970-01-01'), date_diff('hour', DATE '1960-01-01', DATE '1960-01-02'), date_diff('week', DATE '2020-01-14', DATE '2020-01-01')",
            "1440,24,-1",
        ),
        (
            "date_diff('hour', TIME '10:00:00', TIME '12:30:00'), date_diff('minute', TIME '10:00:00', TIME '12:30:00'), date_diff('second', TIME '10:00:00', TIME '10:00:01.9'), date_diff('microsecond', TIME '10:00:00', TIME '10:00:01'), date_diff('epoch', TIME '10:00:00', TIME '11:00:00')",
            "2,150,1,1000000,3600",
        ),
        (
            "date_diff('dow', DATE '2020-01-01', DATE '2020-01-05'), date_diff('doy', DATE '2020-01-01', DATE '2020-01-05'), date_diff('julian', DATE '2020-01-01', DATE '2020-01-05'), date_diff('yearweek', DATE '2020-01-01', DATE '2020-01-15'), date_diff('isodow', DATE '2020-01-01', DATE '2020-01-05')",
            "4,4,4,2,4",
        ),
        (
            "date_diff('day', DATE '2020-01-01', TIMESTAMP '2020-01-02 10:00:00'), date_diff('hour', DATE '2020-01-01', TIMESTAMP '2020-01-02 10:00:00'), datediff('day', DATE '2020-01-01', DATE '2020-01-05')",
            "1,34,4",
        ),
        (
            "date_diff('day', TIMESTAMP_NS '2020-01-01 10:00:00', TIMESTAMP_NS '2020-01-03 09:00:00'), date_diff('ms', TIMESTAMP_S '2020-01-01 10:00:00', DATE '2020-01-03')",
            "2,136800000",
        ),
        (
            "date_diff('quarter', DATE '-0001-01-01', DATE '0001-01-01'), date_diff('century', DATE '-0150-01-01', DATE '0150-01-01'), date_diff('isoyear', DATE '2020-01-01', DATE '2021-01-01')",
            "8,2,0",
        ),
        ("date_diff('second', DATE '5000000-01-01', DATE '2000-01-01')", "-157721646096000"),
    ];
    for (sql, expected) in cases {
        assert_eq!(counted(sql), expected, "{sql}");
    }
}

#[test]
fn date_sub_counts_the_whole_parts_that_fit() {
    let cases = [
        (
            "date_sub('month', DATE '2020-01-31', DATE '2020-02-29'), date_sub('month', DATE '2020-01-31', DATE '2020-02-28'), date_sub('month', DATE '2020-01-15', DATE '2020-02-14'), date_sub('year', DATE '2019-02-28', DATE '2020-02-28'), date_sub('month', DATE '2020-03-31', DATE '2020-01-31')",
            "1,0,0,1,-2",
        ),
        (
            "date_sub('month', TIMESTAMP '2020-01-31 10:00:00', TIMESTAMP '2020-02-29 09:00:00'), date_sub('month', TIMESTAMP '2020-01-31 10:00:00', TIMESTAMP '2020-02-29 11:00:00'), date_sub('month', TIMESTAMP '2020-01-29 10:00:00', TIMESTAMP '2020-02-29 09:00:00')",
            "0,1,0",
        ),
        (
            "date_sub('day', TIMESTAMP '2020-01-01 10:00:00', TIMESTAMP '2020-01-02 09:00:00'), date_sub('hour', TIMESTAMP '2020-01-02 10:00:00', TIMESTAMP '2020-01-01 10:30:00'), date_sub('week', DATE '2020-01-01', DATE '2020-01-14'), date_sub('quarter', DATE '2020-01-01', DATE '2020-12-31'), date_sub('decade', DATE '2000-01-01', DATE '2019-12-31')",
            "0,-23,1,3,1",
        ),
        (
            "date_sub('isoyear', DATE '2019-01-01', DATE '2020-01-01'), date_sub('dow', DATE '2019-01-01', DATE '2019-01-05'), date_sub('century', DATE '1900-01-01', DATE '2100-01-01'), date_sub('millennium', DATE '1000-01-01', DATE '2100-01-01'), date_sub('epoch', DATE '2019-01-01', DATE '2019-01-02')",
            "1,4,2,1,86400",
        ),
        (
            "date_sub('hour', TIME '10:00:00', TIME '12:30:00'), date_sub('second', TIME '10:00:01', TIME '10:00:00.5'), date_sub('millisecond', TIME '10:00:00', TIME '10:00:01.5')",
            "2,0,1500",
        ),
        (
            "date_sub('month', TIMESTAMP '2020-03-31 10:00:00', TIMESTAMP '2020-02-29 12:00:00'), date_sub('year', DATE '2020-02-29', DATE '2021-02-28'), date_sub('year', DATE '2021-02-28', DATE '2020-02-29'), date_sub('quarter', DATE '2020-05-31', DATE '2020-02-29')",
            "-1,1,-1,-1",
        ),
        (
            "date_sub('month', DATE '2020-02-29', DATE '2020-01-31'), date_sub('month', DATE '2020-03-31', DATE '2020-02-29'), date_sub('month', DATE '2021-02-28', DATE '2020-02-29')",
            "-1,-1,-12",
        ),
        (
            "datesub('day', 'infinity'::TIMESTAMP, TIMESTAMP '2020-01-01'), typeof(datesub('day', DATE '2020-01-01', DATE '2020-01-02'))",
            "NULL,BIGINT",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(counted(sql), expected, "{sql}");
    }
}

#[test]
fn a_null_or_an_infinity_has_no_count() {
    assert_eq!(
        counted(
            "date_diff('day', 'infinity'::DATE, DATE '2020-01-01'), date_diff(NULL, DATE '2020-01-01', DATE '2020-01-02'), date_diff('day', NULL, DATE '2020-01-02'), typeof(date_diff('day', DATE '2020-01-01', DATE '2020-01-02'))"
        ),
        "NULL,NULL,NULL,BIGINT"
    );
    assert_eq!(
        counted(
            "date_diff('day', DATE '2020-01-01', NULL), date_diff('day', '2020-01-01', DATE '2020-01-05'), date_diff('hour', TIME '10:00:00', '12:00:00')"
        ),
        "NULL,4,2"
    );
    assert_eq!(
        column(
            "SELECT date_diff('day', d, d + 40) FROM (VALUES (DATE '2020-01-01'), ('infinity'::DATE), (NULL)) t(d)"
        ),
        ["40", "NULL", "NULL"]
    );
    assert_eq!(
        column(
            "SELECT date_diff(p, DATE '2020-01-01', DATE '2021-03-05') FROM (VALUES ('year'), ('month'), (NULL), ('day')) t(p)"
        ),
        ["1", "14", "NULL", "429"]
    );
    assert_eq!(
        column("SELECT date_diff('day', TIME '10:00:00', t) FROM (VALUES (NULL::TIME)) t(t)"),
        ["NULL"]
    );
}

#[test]
fn a_part_that_cannot_be_counted_is_refused_in_the_pins_words() {
    let cases = [
        (
            "date_diff('day', TIME '10:00:00', TIME '12:30:00')",
            "Not implemented Error: \"time\" units \"day\" not recognized",
        ),
        (
            "date_diff('isoyear', TIME '10:00:00', TIME '12:30:00')",
            "\"time\" units \"isoyear\" not recognized",
        ),
        (
            "date_sub('isoyear', TIME '10:00:00', TIME '12:30:00')",
            "\"time\" units \"year\" not recognized",
        ),
        (
            "date_sub('yearweek', TIME '10:00:00', TIME '12:00:00')",
            "\"time\" units \"week\" not recognized",
        ),
        (
            "date_diff('julian', TIME '10:00:00', TIME '12:00:00')",
            "\"time\" units \"day\" not recognized",
        ),
        (
            "date_diff('era', DATE '2020-01-01', DATE '2020-01-02')",
            "Not implemented Error: Specifier type not implemented for DATEDIFF",
        ),
        (
            "date_diff('timezone_hour', DATE '2020-01-01', DATE '2020-01-02')",
            "Specifier type not implemented for DATEDIFF",
        ),
        (
            "date_sub('era', DATE '2020-01-01', DATE '2020-01-02')",
            "Specifier type not implemented for DATESUB",
        ),
        (
            "date_diff('nanosecond', DATE '2020-01-01', DATE '2020-01-02')",
            "Conversion Error: extract specifier \"nanosecond\" not recognized",
        ),
        (
            "date_diff(p, DATE '2020-01-01', DATE '2021-03-05') FROM (VALUES ('year'), ('bogus')) t(p)",
            "extract specifier \"bogus\" not recognized",
        ),
        (
            "date_diff('microsecond', TIMESTAMP '290000-01-01', TIMESTAMP '-290000-01-01')",
            "Out of Range Error: Overflow in subtraction of INT64 (-9213683299200000000 - 9089348860800000000)!",
        ),
        (
            "date_sub('microsecond', TIMESTAMP '290000-01-01', TIMESTAMP '-290000-01-01')",
            "Overflow in subtraction of INT64 (-9213683299200000000 - 9089348860800000000)!",
        ),
        (
            "date_diff('microsecond', DATE '5000000-01-01', DATE '2000-01-01')",
            "Conversion Error: Could not convert DATE (5000000-01-01) to microseconds",
        ),
        (
            "date_sub('day', DATE '5000000-01-01', DATE '2000-01-01')",
            "Conversion Error: Date and time not in timestamp range",
        ),
        (
            "datediff('day', 1, 2)",
            "\tdatediff(col0 VARCHAR, col1 TIMESTAMPTZ_NS, col2 TIMESTAMPTZ_NS) -> BIGINT",
        ),
        (
            "date_sub('day', 1, 2)",
            "\tdate_sub(col0 VARCHAR, col1 TIMESTAMP WITH TIME ZONE, col2 TIMESTAMP WITH TIME ZONE) -> BIGINT\n",
        ),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

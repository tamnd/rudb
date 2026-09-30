//! `dayname`, `monthname`, `last_day`, `nanosecond` and the three epoch readers, and `date_trunc`
//! of a date, which is a timestamp.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

/// The type and the text of the one value a query answers.
fn answer(sql: &str) -> (String, String) {
    let database = Database::new();
    let sql = format!("SELECT typeof({sql}), {sql}");
    let result = database.query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let row = result.rows().next().expect("one row");
    (format!("{}", row[0]), format!("{}", row[1]))
}

fn check(cases: &[(&str, &str, &str)]) {
    for (sql, ty, value) in cases {
        assert_eq!(answer(sql), (ty.to_string(), value.to_string()), "{sql}");
    }
}

#[test]
fn a_day_and_a_month_have_names_and_a_month_has_a_last_day() {
    check(&[
        ("dayname(DATE '2020-05-05')", "VARCHAR", "Tuesday"),
        ("dayname(DATE '2021-01-31')", "VARCHAR", "Sunday"),
        ("monthname(TIMESTAMP '2020-12-05 10:00:00')", "VARCHAR", "December"),
        ("monthname(DATE '1900-02-15')", "VARCHAR", "February"),
        ("last_day(DATE '2020-02-10')", "DATE", "2020-02-29"),
        ("last_day(DATE '1900-02-15')", "DATE", "1900-02-28"),
        ("last_day(TIMESTAMP '2020-02-10 10:00:00')", "DATE", "2020-02-29"),
        ("last_day(TIMESTAMP_NS '2020-02-10 10:00:00')", "DATE", "2020-02-29"),
    ]);
}

#[test]
fn the_epochs_count_from_1970_in_their_own_unit() {
    check(&[
        ("epoch_us(TIMESTAMP '2020-05-05 10:11:12.345')", "BIGINT", "1588673472345000"),
        ("epoch_ms(TIMESTAMP '2020-05-05 10:11:12.345')", "BIGINT", "1588673472345"),
        ("epoch_ns(DATE '2020-05-05')", "BIGINT", "1588636800000000000"),
        ("epoch_ms(DATE '2020-05-05')", "BIGINT", "1588636800000"),
        ("epoch_us(DATE '2020-05-05')", "BIGINT", "1588636800000000"),
        ("epoch_us(TIME '00:00:01')", "BIGINT", "1000000"),
        ("epoch_ms(TIME '10:00:00')", "BIGINT", "36000000"),
        ("epoch_ns(TIME '10:11:12.5')", "BIGINT", "36672500000000"),
        ("epoch_ms(INTERVAL 1 SECOND)", "BIGINT", "1000"),
        ("epoch_ns(INTERVAL '1 month 1 day 1 second')", "BIGINT", "2678401000000000"),
        ("epoch_us(INTERVAL '1 month')", "BIGINT", "2592000000000"),
        ("epoch_ms(INTERVAL '-1 microsecond')", "BIGINT", "0"),
        ("epoch_us(INTERVAL '-1 microsecond')", "BIGINT", "-1"),
        ("epoch_ms(TIMESTAMP '1969-12-31 23:59:59.9999')", "BIGINT", "0"),
        ("epoch_ns(TIMESTAMP '1969-12-31 23:59:59.999999')", "BIGINT", "-1000"),
        ("epoch_ns(TIMESTAMP_NS '2020-02-10 10:00:00.123456789')", "BIGINT", "1581328800123456789"),
        ("epoch_ms(TIMESTAMP_S '2020-02-10 10:00:00')", "BIGINT", "1581328800000"),
        ("epoch_ms(1588636800000)", "TIMESTAMP", "2020-05-05 00:00:00"),
    ]);
}

#[test]
fn a_nanosecond_carries_the_seconds_of_its_minute() {
    check(&[
        ("nanosecond(TIMESTAMP '2020-05-05 10:11:12.345')", "BIGINT", "12345000000"),
        ("nanosecond(DATE '2020-05-05')", "BIGINT", "0"),
        ("nanosecond(TIME '10:11:12.5')", "BIGINT", "12500000000"),
        ("nanosecond(INTERVAL 1 SECOND)", "BIGINT", "1000000000"),
        ("nanosecond(INTERVAL '1 minute 1.5 second')", "BIGINT", "1500000000"),
    ]);
}

#[test]
fn an_infinity_has_none_of_these() {
    let database = Database::new();
    let sql = "SELECT dayname('infinity'::TIMESTAMP), last_day('-infinity'::DATE), \
               epoch_us('infinity'::DATE), nanosecond('infinity'::TIMESTAMP), \
               epoch_ms('infinity'::DATE)";
    let result = database.query(sql).expect("the infinities");
    let row = result.rows().next().expect("one row");
    for value in row.iter() {
        assert!(value.is_null(), "{value}");
    }
}

#[test]
fn a_column_reads_the_same_as_a_constant() {
    let database = Database::new();
    database.execute("CREATE TABLE t(d DATE)").expect("the table");
    database
        .execute("INSERT INTO t VALUES (DATE '2021-01-31'), (NULL), (DATE '1900-02-15')")
        .expect("the rows");
    let result = database
        .query("SELECT dayname(d), monthname(d), last_day(d), epoch_us(d), nanosecond(d) FROM t")
        .expect("the names");
    let rows: Vec<String> = result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join(","))
        .collect();
    assert_eq!(
        rows,
        [
            "Sunday,January,2021-01-31,1612051200000000,0",
            "NULL,NULL,NULL,NULL,NULL",
            "Thursday,February,1900-02-28,-2205100800000000,0",
        ]
    );
}

#[test]
fn a_date_truncates_to_the_midnight_of_a_timestamp() {
    check(&[
        ("date_trunc('month', DATE '2020-05-05')", "TIMESTAMP", "2020-05-01 00:00:00"),
        ("date_trunc('week', DATE '2020-05-07')", "TIMESTAMP", "2020-05-04 00:00:00"),
        ("date_trunc('month', 'infinity'::DATE)", "TIMESTAMP", "infinity"),
    ]);
    let database = Database::new();
    database.execute("CREATE TABLE t(d DATE)").expect("the table");
    database.execute("INSERT INTO t VALUES (DATE '2020-05-07'), (NULL)").expect("the rows");
    let result =
        database.query("SELECT typeof(date_trunc('year', d)), date_trunc('year', d) FROM t");
    let rows: Vec<String> = result
        .expect("the truncation")
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join(","))
        .collect();
    assert_eq!(rows, ["TIMESTAMP,2020-01-01 00:00:00", "TIMESTAMP,NULL"]);
}

#[test]
fn what_is_not_a_moment_is_refused_with_the_pins_list() {
    let error =
        |sql: &str| Database::new().query(&format!("SELECT {sql}")).unwrap_err().to_string();
    let said = error("dayname(1::INTEGER)");
    assert!(said.contains("'dayname(INTEGER)'"), "{said}");
    assert!(said.contains("dayname(col0 TIMESTAMP WITH TIME ZONE) -> VARCHAR"), "{said}");
    let said = error("epoch_ms(true)");
    assert!(said.contains("'epoch_ms(BOOLEAN)'"), "{said}");
    assert!(said.contains("epoch_ms(col0 BIGINT) -> TIMESTAMP"), "{said}");
    let said = error("last_day(true)");
    assert!(said.contains("last_day(col0 DATE) -> DATE"), "{said}");
}

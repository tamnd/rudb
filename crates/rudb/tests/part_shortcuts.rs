//! `year(x)` and the other names that read one part of a date or a timestamp, which the pin answers
//! the way `date_part` answers with that part.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

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
fn each_name_reads_the_part_it_is_named_for() {
    let cases = [
        (
            "SELECT year(DATE '2020-05-05'), month(TIMESTAMP '2020-05-05 10:11:12'), epoch(DATE '2020-05-05'), typeof(epoch(DATE '2020-05-05'))",
            "2020,5,1588636800.0,DOUBLE",
        ),
        ("SELECT year(INTERVAL 14 MONTH), second(TIMESTAMP '2020-05-05 10:11:12.5')", "1,12"),
        (
            "SELECT weekday(DATE '2020-05-05'), weekofyear(DATE '2020-05-05'), dayofmonth(DATE '2020-05-05'), isodow(DATE '2020-05-10'), era(DATE '2020-05-05'), julian(DATE '2020-05-05')",
            "2,19,5,7,1,2458975.0",
        ),
        (
            "SELECT year(TIMESTAMP_NS '2020-05-05 10:11:12'), year(TIMESTAMP_S '2020-05-05 10:11:12')",
            "2020,2020",
        ),
        (
            "SELECT millisecond(TIMESTAMP '2020-05-05 10:11:12.345'), microsecond(TIMESTAMP '2020-05-05 10:11:12.345')",
            "12345,12345000",
        ),
        (
            "SELECT yearweek(DATE '2020-05-05'), isoyear(DATE '2021-01-01'), century(DATE '2020-05-05'), millennium(DATE '2020-05-05'), decade(DATE '2020-05-05'), quarter(DATE '2020-05-05'), dayofyear(DATE '2020-05-05'), dayofweek(DATE '2020-05-05'), week(DATE '2020-05-05'), day(DATE '2020-05-05'), minute(TIMESTAMP '2020-05-05 10:11:12')",
            "202019,2020,21,3,202,2,126,2,19,5,11",
        ),
        ("SELECT datepart('year', DATE '2020-05-05')", "2020"),
        ("SELECT year('infinity'::DATE), epoch('-infinity'::TIMESTAMP)", "NULL,NULL"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answer(sql), expected, "{sql}");
    }
}

#[test]
fn a_time_of_day_has_six_parts() {
    let cases = [
        (
            "SELECT date_part('epoch', TIME '10:11:12.5'), date_part('millisecond', TIME '10:11:12.5'), date_part('microsecond', TIME '10:11:12.5'), date_part('second', TIME '10:11:12.5')",
            "36672.5,12500,12500000,12",
        ),
        (
            "SELECT hour(TIME '10:11:12'), second(TIME '10:11:12.5'), epoch(TIME '10:00:00')",
            "10,12,36000.0",
        ),
        (
            "SELECT extract(hour FROM TIME '23:59:59.999999'), extract(epoch FROM TIME '23:59:59.999999')",
            "23,86399.999999",
        ),
        ("SELECT date_part('minutes', TIME '10:11:12')", "11"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answer(sql), expected, "{sql}");
    }
    let said = error("SELECT date_part('year', TIME '10:11:12')");
    assert!(said.contains("\"time\" units \"year\" not recognized"), "{said}");
}

#[test]
fn a_column_reads_the_same_parts_as_a_constant() {
    let database = Database::new();
    let sql = "SELECT year(d), quarter(d), epoch(d) FROM (VALUES (DATE '2020-05-05'), (DATE '1999-12-31')) v(d) ORDER BY d";
    let result = database.query(sql).unwrap();
    let rows: Vec<String> = result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join(","))
        .collect();
    assert_eq!(rows, ["1999,4,946598400.0", "2020,2,1588636800.0"]);
}

#[test]
fn something_that_is_not_a_date_is_refused_under_the_name_written() {
    let said = error("SELECT year(1)");
    assert!(
        said.contains(
            "No function matches the given name and argument types 'year(INTEGER_LITERAL)'"
        ),
        "{said}"
    );
    assert!(said.contains("\"year\"(col0 TIMESTAMP WITH TIME ZONE) -> BIGINT"), "{said}");
    let said = error("SELECT julian(INTERVAL 1 DAY)");
    assert!(said.contains("\n\tjulian(col0 DATE) -> DOUBLE"), "{said}");
    let said = error("SELECT hour(1)");
    assert!(said.contains("\"hour\"(col0 TIME) -> BIGINT"), "{said}");
    let said = error("SELECT year(TIME '10:11:12')");
    assert!(said.contains("'year(TIME)'"), "{said}");
    let said = error("SELECT year(1, 2)");
    assert!(said.contains("'year(INTEGER_LITERAL, INTEGER_LITERAL)'"), "{said}");
    for sql in ["SELECT year('2020-01-01')", "SELECT year(NULL)"] {
        let said = error(sql);
        assert!(said.contains("Could not choose a best candidate function"), "{sql}: {said}");
    }
}

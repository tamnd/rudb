//! `strftime`, which writes a date or a timestamp out in a format.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb, except
//! the one that says what `%n` should have written, which the pin gets wrong (tamnd/duckdb#22).

use rudb::Database;

fn written(sql: &str) -> String {
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
fn every_specifier_writes_what_the_pin_writes() {
    let cases = [
        (
            "strftime(DATE '1992-01-01', '%a %A %w %u %d %-d %b %h %B %m %-m %y %-y %Y %G %V %U %W %j %-j')",
            "Wed Wednesday 3 3 01 1 Jan Jan January 01 1 92 92 1992 1992 01 00 00 001 1",
        ),
        (
            "strftime(TIMESTAMP '1992-01-01 13:04:05.123456', '%H %-H %I %-I %p %M %-M %S %-S %f %g %n')",
            "13 13 01 1 PM 04 4 05 5 123456 123 123456000",
        ),
        ("strftime(TIMESTAMP '1992-01-01 13:04:05.123456', '[%z][%Z]')", "[+00][]"),
        (
            "strftime(TIMESTAMP '1992-01-01 13:04:05.123456', '%c|%x|%X|%T|%%|x')",
            "1992-01-01 13:04:05|1992-01-01|13:04:05|13:04:05|%|x",
        ),
        ("strftime(DATE '2020-01-01', '%H:%M:%S.%f %p %I')", "00:00:00.000000 AM 12"),
        (
            "strftime(TIMESTAMP '2021-01-03 00:00:00', '%U %W %V %G %u %w'), strftime(DATE '2020-12-31', '%U %W %V %G %j')",
            "01 00 53 2020 7 0,52 52 53 2020 366",
        ),
        (
            "strftime(DATE '12345-01-01', '%Y'), strftime(DATE '0005-01-01', '%Y %G')",
            "12345,0005 0004",
        ),
        ("strftime(TIMESTAMP_NS '2020-01-01 00:00:00.123456789', '%n')", "123456789"),
        ("strftime(TIMESTAMP_NS '2020-01-01 00:00:00.123456789', '%f')", "123456"),
        (
            "strftime(TIMESTAMP_NS '2020-01-01 00:00:00.123456789', '%n %f %g')",
            "123456789 123456 123",
        ),
        ("strftime(TIMESTAMP '2020-01-01 00:00:00', 'plain')", "plain"),
    ];
    for (sql, expected) in cases {
        assert_eq!(written(sql), expected, "{sql}");
    }
}

#[test]
fn the_format_can_come_first_and_can_be_folded() {
    assert_eq!(written("strftime('%Y/%m/%d', DATE '1992-03-02')"), "1992/03/02");
    assert_eq!(written("typeof(strftime('%Y', DATE '1992-03-02'))"), "VARCHAR");
    assert_eq!(written("strftime(DATE '2020-01-01', '%Y'::VARCHAR)"), "2020");
    assert_eq!(written("strftime(DATE '2020-01-01', '%Y' || '-%m')"), "2020-01");
}

#[test]
fn a_null_or_an_infinity_is_not_formatted() {
    assert_eq!(
        written(
            "strftime(DATE '1992-01-01', NULL), strftime(NULL::DATE, '%Y'), strftime(NULL, '%Y')"
        ),
        "NULL,NULL,NULL"
    );
    assert_eq!(
        written("strftime('infinity'::DATE, '%Y'), strftime('-infinity'::TIMESTAMP, '%Y')"),
        "infinity,-infinity"
    );
}

#[test]
fn a_column_is_written_row_by_row() {
    let database = Database::new();
    database.execute("CREATE TABLE t(d DATE)").expect("the table");
    database
        .execute("INSERT INTO t VALUES (DATE '2021-01-31'), (NULL), (DATE '1900-02-15')")
        .expect("the rows");
    let result = database.query("SELECT strftime(d, '%A %-d %B %Y') FROM t").expect("written");
    let rows: Vec<String> = result.rows().map(|row| row[0].to_string()).collect();
    assert_eq!(rows, ["Sunday 31 January 2021", "NULL", "Thursday 15 February 1900"]);
}

#[test]
fn a_bad_format_or_argument_is_refused_in_the_pins_words() {
    let cases = [
        (
            "strftime(DATE '1992-01-01', '%Q')",
            "Failed to parse format specifier %Q: Unrecognized format for strftime/strptime: %Q",
        ),
        (
            "strftime(DATE '1992-01-01', '')",
            "Failed to parse format specifier : Empty format string",
        ),
        (
            "strftime(DATE '1992-01-01', '%')",
            "Failed to parse format specifier %: Trailing format character %",
        ),
        (
            "strftime(DATE '1992-01-01', '%-Q')",
            "Failed to parse format specifier %-Q: Unrecognized format for strftime/strptime: %-Q",
        ),
        (
            "strftime(d, f) FROM (VALUES (DATE '1992-01-01', '%Y')) t(d, f)",
            "The \"format\" argument in function \"strftime\" must be a constant expression",
        ),
        ("strftime(TIME '10:00:00', '%H')", "strftime(\"data\" DATE, format VARCHAR) -> VARCHAR"),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

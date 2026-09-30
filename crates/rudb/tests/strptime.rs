//! `strptime` and `try_strptime`, which read a timestamp out of text in a format.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn read(sql: &str) -> String {
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
fn a_text_is_read_in_its_format() {
    let cases = [
        (
            "strptime('2020-01-05 10:11:12', '%Y-%m-%d %H:%M:%S'), typeof(strptime('2020-01-05', '%Y-%m-%d'))",
            "2020-01-05 10:11:12,TIMESTAMP",
        ),
        ("strptime('  2020-1-5', '%Y-%m-%d')", "2020-01-05 00:00:00"),
        (
            "strptime('Mon, 5 January 2020 1:02:03 PM', '%a, %-d %B %Y %-I:%M:%S %p'), strptime('jan 5 2020 12:00 am', '%b %d %Y %I:%M %p')",
            "2020-01-05 13:02:03,2020-01-05 00:00:00",
        ),
        (
            "strptime('20 45', '%y %j'), strptime('2020 10 3', '%Y %U %w'), strptime('2020 10 3', '%Y %W %w'), strptime('2020 10 3', '%G %V %u')",
            "2020-02-14 00:00:00,2020-03-11 00:00:00,2020-03-11 00:00:00,2020-03-04 00:00:00",
        ),
        (
            "strptime('10:11:12.5', '%H:%M:%S.%f'), strptime('10:11:12.5', '%H:%M:%S.%g')",
            "1900-01-01 10:11:12.5,1900-01-01 10:11:12.5",
        ),
        ("strptime('2020-01-05 UTC', '%Y-%m-%d %Z')", "2020-01-05 00:00:00"),
        (
            "strptime('infinity', '%Y'), strptime('-infinity', '%Y'), strptime('epoch', '%Y'), strptime('Infinity  ', '%Y')",
            "infinity,-infinity,1970-01-01 00:00:00,infinity",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(read(sql), expected, "{sql}");
    }
}

#[test]
fn an_offset_or_nanoseconds_change_the_type() {
    let cases = [
        (
            "strptime('2020-01-05 10:11:12 +0230', '%Y-%m-%d %H:%M:%S %z'), typeof(strptime('2020-01-05 +02', '%Y-%m-%d %z'))",
            "2020-01-05 07:41:12+00,TIMESTAMP WITH TIME ZONE",
        ),
        (
            "strptime('2020-01-05 +02:30:15', '%Y-%m-%d %z'), strptime('2020-01-05 -0100', '%Y-%m-%d %z')",
            "2020-01-04 21:29:45+00,2020-01-05 01:00:00+00",
        ),
        (
            "strptime('2020-01-05 10:11:12.123456789', '%Y-%m-%d %H:%M:%S.%n'), typeof(strptime('2020-01-05 10:11:12.123456789', '%Y-%m-%d %H:%M:%S.%n'))",
            "2020-01-05 10:11:12.123456789,TIMESTAMP_NS",
        ),
        ("typeof(strptime('2020', ['%Y', '%z']))", "TIMESTAMP WITH TIME ZONE"),
    ];
    for (sql, expected) in cases {
        assert_eq!(read(sql), expected, "{sql}");
    }
}

#[test]
fn a_list_of_formats_is_tried_in_order() {
    assert_eq!(
        read(
            "strptime('2020-01-05', ['%d/%m/%Y', '%Y-%m-%d']), strptime('05/01/2020', ['%d/%m/%Y', '%Y-%m-%d'])"
        ),
        "2020-01-05 00:00:00,2020-01-05 00:00:00"
    );
    assert_eq!(
        column(
            "SELECT strptime(s, ['%d/%m/%Y', '%Y-%m-%d']) FROM (VALUES ('05/01/2020'), ('2021-02-03'), (NULL)) t(s)"
        ),
        ["2020-01-05 00:00:00", "2021-02-03 00:00:00", "NULL"]
    );
}

#[test]
fn try_strptime_answers_null_where_strptime_refuses() {
    assert_eq!(
        read(
            "try_strptime('2020-02-30', '%Y-%m-%d'), try_strptime('nope', '%Y'), try_strptime('2020', '%Y')"
        ),
        "NULL,NULL,2020-01-01 00:00:00"
    );
    // The try path never looks at a special and reads the date it started from.
    assert_eq!(
        read("try_strptime('infinity', '%Y'), try_strptime('2020-01-05 +02', '%Y-%m-%d %z')"),
        "1900-01-01 00:00:00,2020-01-04 22:00:00+00"
    );
    assert_eq!(
        column(
            "SELECT try_strptime(s, '%d/%m/%Y') FROM (VALUES ('05/01/2020'), ('bad'), (NULL), ('31/02/2020')) t(s)"
        ),
        ["2020-01-05 00:00:00", "NULL", "NULL", "NULL"]
    );
    assert_eq!(
        read("strptime('2020', NULL), strptime(NULL, '%Y'), try_strptime('2020', NULL)"),
        "NULL,NULL,NULL"
    );
}

#[test]
fn a_text_that_does_not_fit_is_refused_in_the_pins_words() {
    let cases = [
        (
            "strptime('2020-01-05x', '%Y-%m-%d')",
            "Could not parse string \"2020-01-05x\" according to format specifier \"%Y-%m-%d\"\n2020-01-05x\n          ^\nError: Full specifier did not match: trailing characters",
        ),
        ("strptime('2020-02-30', '%Y-%m-%d')", "Date out of range: 2020-2-30"),
        ("strptime('x', ['%d/%m/%Y', '%Y-%m-%d'])", "\"%d/%m/%Y\"\nx\n^\nError: Expected a number"),
        (
            "strptime('13 PM', '%H %p')",
            "\"%H %p\"\n\nError: Invalid hour: 13 AM/PM, expected an hour within the range [0..12]",
        ),
        (
            "strptime('2020-13-05', '%Y-%m-%d')",
            "2020-13-05\n     ^\nError: Month out of range, expected a value between 1 and 12",
        ),
        (
            "strptime('12345-1-5', '%Y-%m-%d')",
            "12345-1-5\n     ^\nError: Literal does not match, expected -",
        ),
        (
            "strptime('2020-01-05', '%Y-%m-%d %H')",
            "2020-01-05\n          ^\nError: Space does not match, expected  ",
        ),
        (
            "strptime('2020-01-05 x', '%Y-%m-%d %a')",
            "Error: Expected an abbreviated day name (Mon, Tue, Wed, Thu, Fri, Sat, Sun)",
        ),
        (
            "strptime('10:11:12.1234565', '%H:%M:%S.%f')",
            "Error: Full specifier did not match: trailing characters",
        ),
        (
            "strptime('2020-01-05', '%Q')",
            "Failed to parse format specifier %Q: Unrecognized format for strftime/strptime: %Q",
        ),
        ("strptime('2020-01-05', []::VARCHAR[])", "strptime format list must not be empty"),
        (
            "strptime(s, f) FROM (VALUES ('2020', '%Y')) t(s, f)",
            "The \"format\" argument in function \"strptime\" must be a constant expression",
        ),
        ("strptime(1::INTEGER, '%Y')", "strptime(\"text\" VARCHAR, format VARCHAR[]) -> TIMESTAMP"),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

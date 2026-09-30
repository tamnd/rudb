//! `date_part` of a list of parts, which answers a struct, and `make_date` of that struct.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn answered(sql: &str) -> String {
    let database = Database::new();
    let sql = format!("SELECT {sql}");
    let result = database.query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let row = result.rows().next().expect("one row");
    row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|")
}

fn refused(sql: &str) -> String {
    Database::new().query(&format!("SELECT {sql}")).unwrap_err().to_string()
}

#[test]
fn a_list_of_parts_is_a_struct_with_a_field_for_each() {
    let cases = [
        (
            "date_part(['year', 'month', 'day'], DATE '1992-02-03')",
            "{'year': 1992, 'month': 2, 'day': 3}",
        ),
        (
            "date_part(['epoch', 'second', 'julian'], TIMESTAMP '1992-02-03 01:02:03.5')",
            "{'epoch': 697078923.5, 'second': 3, 'julian': 2448656.0430960646}",
        ),
        ("date_part(['hour', 'minute'], TIME '01:02:03')", "{'hour': 1, 'minute': 2}"),
        ("date_part(['hour', 'minute'], INTERVAL '3 hours 4 minutes')", "{'hour': 3, 'minute': 4}"),
        ("date_part(['year', 'month'], DATE 'infinity')", "{'year': NULL, 'month': NULL}"),
        ("date_part(['year'], NULL::DATE)", "NULL"),
        (
            "date_part(['era', 'isoyear', 'dow'], TIMESTAMP '1992-02-03 01:02:03')",
            "{'era': 1, 'isoyear': 1992, 'dow': 1}",
        ),
        ("date_part(['Year', 'EPOCH'], DATE '1992-02-03')", "{'Year': 1992, 'EPOCH': 697075200.0}"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
}

#[test]
fn a_bad_list_of_parts_is_refused_in_the_pins_words() {
    let cases = [
        (
            "date_part(['year', 'year'], DATE '1992-02-03')",
            "Binder Error: Duplicate struct entry name \"year\" in \"date_part\"",
        ),
        (
            "date_part(['yearx'], DATE '1992-02-03')",
            "Conversion Error: extract specifier \"yearx\" not recognized",
        ),
        (
            "date_part([], DATE '1992-02-03')",
            "Binder Error: \"date_part\" requires non-empty lists of part names",
        ),
        (
            "date_part(['year', NULL], DATE '1992-02-03')",
            "Binder Error: NULL struct entry name in \"date_part\"",
        ),
        (
            "date_part(p, DATE '1992-02-03') FROM (VALUES (['year'])) t(p)",
            "Binder Error: The \"part_list\" argument in function \"date_part\" must be a constant expression",
        ),
        (
            "date_part(['era'], TIME '01:02:03')",
            "Not implemented Error: \"time\" units \"era\" not recognized",
        ),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

#[test]
fn a_date_is_made_out_of_a_struct_of_its_fields() {
    let cases = [
        ("make_date(date_part(['year', 'month', 'day'], DATE '1992-02-03'))", "1992-02-03"),
        ("make_date({'year': 2020, 'month': 1, 'day': 5})", "2020-01-05"),
        ("make_date({'day': 5, 'month': 1, 'year': 2020})", "2020-01-05"),
        ("make_date({'YEAR': 2020, 'Month': 1, 'day': 5})", "2020-01-05"),
        ("make_date({'year': NULL, 'month': 1, 'day': 5})", "NULL"),
        ("make_date(NULL::STRUCT(year BIGINT, month BIGINT, day BIGINT))", "NULL"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
    let cases = [
        (
            "make_date({'year': 2020, 'month': 13, 'day': 5})",
            "Conversion Error: Date out of range: 2020-13-5",
        ),
        (
            "make_date({'year': 20000000000, 'month': 1, 'day': 5})",
            "Invalid Input Error: Type INT64 with value 20000000000 can't be cast because the value is out of range for the destination type INT32",
        ),
        (
            "make_date({'y': 2020, 'm': 1, 'd': 5})",
            "\tmake_date(col0 STRUCT(\"year\" BIGINT, \"month\" BIGINT, \"day\" BIGINT)) -> DATE",
        ),
        (
            "make_date({'year': 2020, 'month': 1})",
            "No function matches the given name and argument types 'make_date(STRUCT(",
        ),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

#[test]
fn a_moment_without_a_zone_is_no_distance_from_utc() {
    let cases = [
        (
            "date_part('timezone', TIMESTAMP '1992-02-03 01:02:03'), date_part('timezone_hour', TIMESTAMP '1992-02-03 01:02:03'), date_part('timezone_minute', TIMESTAMP '1992-02-03')",
            "0|0|0",
        ),
        (
            "date_part('timezone', TIME '01:02:03'), date_part('timezone_hour', TIME '01:02:03')",
            "0|0",
        ),
        (
            "date_part('timezone', TIMESTAMP 'infinity'), date_part('timezone_hour', DATE '-infinity')",
            "NULL|NULL",
        ),
        (
            "timezone_hour(TIMESTAMP '1992-02-03 01:02:03'), timezone_minute(TIME '01:02:03'), timezone(TIMESTAMP '1992-02-03 01:02:03'), typeof(timezone_minute(TIMESTAMP '1992-02-03'))",
            "0|0|0|BIGINT",
        ),
        ("date_part(['timezone'], TIMESTAMP '1992-02-03 01:02:03')", "{'timezone': 0}"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
    let cases = [
        (
            "date_part('TIMEZONE', DATE '1992-02-03')",
            "Not implemented Error: \"date\" units \"timezone\" not recognized",
        ),
        (
            "date_part('Timezone_Hour', INTERVAL '1 hour')",
            "Not implemented Error: \"interval\" units \"Timezone_Hour\" not recognized",
        ),
        (
            "timezone_hour(DATE '1992-02-03')",
            "Not implemented Error: \"date\" units \"timezone_hour\" not recognized",
        ),
        ("timezone_hour(INTERVAL '1 hour')", "\"interval\" units \"timezone_hour\" not recognized"),
        (
            "date_trunc('timezone', DATE '1992-02-03')",
            "Not implemented Error: Specifier type not implemented for DATETRUNC",
        ),
        (
            "date_trunc('timezone', TIMESTAMP '1992-02-03 01:02:03')",
            "Not implemented Error: Specifier type not implemented for DATETRUNC",
        ),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

//! `TIME WITH TIME ZONE`, which is a time of day and the offset it was read at.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb. Every test
//! sets the zone itself, and none of them reads a time without an offset in a zone that moves its
//! clocks, since that takes the offset the zone is on today.

use rudb::Database;

/// Every row of `sql` under `zone`, one row per line and the cells joined with a bar.
fn under(zone: &str, sql: &str) -> String {
    let database = Database::new();
    database.execute(&format!("SET TimeZone = '{zone}'")).expect("a known zone");
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let width = result.rows().next().map_or(0, |row| row.len());
    (0..result.len())
        .map(|row| {
            (0..width).map(|column| result.text_at(row, column)).collect::<Vec<_>>().join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn refused(zone: &str, sql: &str) -> String {
    let database = Database::new();
    database.execute(&format!("SET TimeZone = '{zone}'")).expect("a known zone");
    database.query(sql).unwrap_err().to_string()
}

const NEW_YORK: &str = "America/New_York";

#[test]
fn a_zoned_time_prints_with_its_own_offset_in_any_zone() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT '12:00:00+05'::TIMETZ, '12:00:00-05:30'::TIMETZ, '12:00:00+05:30:15'::TIMETZ, '1:2:3.5 +0530'::TIMETZ, '12:00:00+05abc'::TIMETZ"
        ),
        "12:00:00+05|12:00:00-05:30|12:00:00+05:30:15|01:02:03.5+05:30|12:00:00+05"
    );
    assert_eq!(
        under(NEW_YORK, "SELECT [x], {'t': x} FROM (SELECT '12:00:00+05'::TIMETZ x)"),
        "['12:00:00+05']|{'t': '12:00:00+05'}"
    );
}

#[test]
fn text_without_an_offset_or_with_a_date_takes_the_session_offset() {
    assert_eq!(
        under(
            "UTC",
            "SELECT '12:00:00'::TIMETZ, '2020-01-01 12:00:00+05'::TIMETZ, '2020-07-01 23:00:00-05'::TIMETZ, TIME '10:00'::TIMETZ"
        ),
        "12:00:00+00|07:00:00+00|04:00:00+00|10:00:00+00"
    );
}

#[test]
fn an_offset_is_read_strictly() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT TRY_CAST('12:00+05' AS TIMETZ), TRY_CAST('12:00:00+16' AS TIMETZ), TRY_CAST('12:00:00Z' AS TIMETZ), TRY_CAST('25:00:00+00' AS TIMETZ)"
        ),
        "NULL|NULL|NULL|NULL"
    );
    assert_eq!(
        refused(NEW_YORK, "SELECT '12:00:00+16'::TIMETZ"),
        "Conversion Error: time field value out of range: \"12:00:00+16\", expected format is ([YYYY-MM-DD ]HH:MM:SS[.MS])"
    );
}

#[test]
fn zoned_times_sort_by_the_instant_and_then_the_larger_offset() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT x FROM (VALUES ('12:00:00+00'::TIMETZ), ('11:00:00-01'::TIMETZ), ('13:00:00+01'::TIMETZ), ('11:30:00+00'::TIMETZ)) v(x) ORDER BY x"
        ),
        "11:30:00+00\n13:00:00+01\n12:00:00+00\n11:00:00-01"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT count(DISTINCT x), '12:00:00+00'::TIMETZ = '13:00:00+01'::TIMETZ, '12:00:00+00'::TIMETZ < '13:00:00+01'::TIMETZ FROM (VALUES ('12:00:00+00'::TIMETZ), ('13:00:00+01'::TIMETZ), ('12:00:00+00'::TIMETZ)) v(x)"
        ),
        "2|false|false"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT a.x FROM (VALUES ('12:00:00+00'::TIMETZ), ('13:00:00+01'::TIMETZ)) a(x) JOIN (VALUES ('13:00:00+01'::TIMETZ)) b(x) ON a.x = b.x"
        ),
        "13:00:00+01"
    );
}

#[test]
fn the_extremes_go_by_the_time_of_day_and_not_the_instant() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT min(x), max(x), arg_min(x, x), arg_max(x, x) FROM (VALUES ('06:00:00+00'::TIMETZ), ('01:00:00-05'::TIMETZ), ('12:00:00+05'::TIMETZ)) v(x)"
        ),
        "01:00:00-05|12:00:00+05|01:00:00-05|12:00:00+05"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT g, min(x), max(x) FROM (VALUES (1, '06:00:00+00'::TIMETZ), (1, '01:00:00-05'::TIMETZ), (2, '13:00:00+01'::TIMETZ), (2, '12:00:00+00'::TIMETZ)) v(g, x) GROUP BY g ORDER BY g"
        ),
        "1|01:00:00-05|06:00:00+00\n2|12:00:00+00|13:00:00+01"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT greatest('06:00:00+00'::TIMETZ, '01:00:00-05'::TIMETZ), least('06:00:00+00'::TIMETZ, '01:00:00-05'::TIMETZ), avg(x) FROM (VALUES ('12:00:00+05'::TIMETZ), ('14:00:00+00'::TIMETZ)) v(x)"
        ),
        "06:00:00+00|01:00:00-05|10:30:00+00"
    );
}

#[test]
fn casts_keep_the_time_and_take_the_offset_from_where_it_was() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT TIMESTAMPTZ '2020-07-01 12:00:00+00'::TIMETZ, TIMESTAMP '2020-01-01 12:00:00'::TIMETZ, '12:00:00+05'::TIMETZ::TIME, TIMESTAMPTZ 'infinity'::TIMETZ"
        ),
        "08:00:00-04|12:00:00+00|12:00:00|NULL"
    );
}

#[test]
fn an_interval_moves_the_clock_and_a_date_makes_an_instant() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT '23:30:00+05'::TIMETZ + INTERVAL 1 hour, '00:30:00-03'::TIMETZ - INTERVAL '1 day 1 hour', INTERVAL 90 minutes + '12:00:00+05'::TIMETZ, DATE '2020-07-01' + '12:00:00+05'::TIMETZ"
        ),
        "00:30:00+05|23:30:00-03|13:30:00+05|2020-07-01 03:00:00-04"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT timezone('Asia/Tokyo', '12:00:00+05'::TIMETZ), timezone(INTERVAL '-90 minutes', '00:30:00+00'::TIMETZ), '12:00:00+05'::TIMETZ AT TIME ZONE 'UTC'"
        ),
        "16:00:00+09|23:00:00-01:30|07:00:00+00"
    );
}

#[test]
fn the_parts_of_a_zoned_time_are_its_clock_and_its_offset() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT hour(x), minute(x), second(x), millisecond(x), microsecond(x), epoch(x), epoch_ms(x), nanosecond(x), timezone(x), timezone_hour(x), timezone_minute(x) FROM (SELECT '23:59:58.123456-05:30'::TIMETZ x)"
        ),
        "23|59|58|58123|58123456|86398.123456|86398123|58123456000|-19800|-5|-30"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT date_part(['hour','timezone'], '01:00:00+05'::TIMETZ), date_part('epoch', '24:00:00+05'::TIMETZ), date_part('hour', '24:00:00+05'::TIMETZ), typeof(date_part('hour', '01:00:00+05'::TIMETZ))"
        ),
        "{'hour': 1, 'timezone': 18000}|86400.0|24|BIGINT"
    );
    assert_eq!(
        refused(NEW_YORK, "SELECT date_part('day', '01:00:00+05'::TIMETZ)"),
        "Not implemented Error: \"time with time zone\" units \"day\" not recognized"
    );
}

#[test]
fn a_zoned_time_hashes_as_the_pin_stores_it() {
    assert_eq!(
        under(NEW_YORK, "SELECT hash('12:00:00+05'::TIMETZ), hash('07:00:00+00'::TIMETZ)"),
        "14941587672634741675|11754314777078766622"
    );
}

#[test]
fn a_time_equals_a_zoned_time_in_the_type_on_the_left() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT TIME '12:00:00' = '12:00:00+00'::TIMETZ, TIME '12:00:00' IS NOT DISTINCT FROM '12:00:00+00'::TIMETZ, TIME '12:00:00' IN ('12:00:00+00'::TIMETZ)"
        ),
        "true|true|true"
    );
    assert_eq!(
        refused(NEW_YORK, "SELECT TIME '12:00:00' < '12:00:00+01'::TIMETZ"),
        "Binder Error: Cannot compare values of type TIME and type TIME WITH TIME ZONE - an explicit cast is required"
    );
    assert_eq!(
        refused(NEW_YORK, "SELECT 1 = DATE '2020-01-01'"),
        "Conversion Error: Unimplemented type for cast (INTEGER -> DATE)"
    );
}

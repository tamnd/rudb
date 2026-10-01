//! Casts into and out of `TIMESTAMPTZ`, which read and write a wall clock in the session zone.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb. Every test
//! sets the zone itself, since the default is the zone of the machine running the test.

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

#[test]
fn text_is_read_in_the_zone_it_names_or_else_the_session_zone() {
    assert_eq!(
        under(
            "America/New_York",
            "SELECT '2020-07-01 00:00'::TIMESTAMPTZ, '2020-07-01 00:00+05:30'::TIMESTAMPTZ, '2020-07-01 00:00Z'::TIMESTAMPTZ, '2020-07-01 00:00 Europe/Berlin'::TIMESTAMPTZ, '2020-07-01 00:00 asia/tokyo'::TIMESTAMPTZ"
        ),
        "2020-07-01 00:00:00-04|2020-06-30 14:30:00-04|2020-06-30 20:00:00-04|2020-06-30 18:00:00-04|2020-06-30 11:00:00-04"
    );
    assert_eq!(
        under(
            "America/New_York",
            "SELECT 'epoch'::TIMESTAMPTZ, 'infinity'::TIMESTAMPTZ, '-infinity'::TIMESTAMPTZ, TIMESTAMPTZ 'infinity'::TIMESTAMP"
        ),
        "1969-12-31 19:00:00-05|infinity|-infinity|infinity"
    );
}

#[test]
fn a_wall_clock_becomes_an_instant_in_the_session_zone_and_back() {
    assert_eq!(
        under(
            "America/New_York",
            "SELECT DATE '2020-07-01'::TIMESTAMPTZ, TIMESTAMP '2020-01-01 10:00'::TIMESTAMPTZ, TIMESTAMPTZ '2020-07-01 02:00:00+00'::TIMESTAMP, TIMESTAMPTZ '2020-07-01 02:00:00+00'::DATE"
        ),
        "2020-07-01 00:00:00-04|2020-01-01 10:00:00-05|2020-06-30 22:00:00|2020-06-30"
    );
    assert_eq!(
        under(
            "America/New_York",
            "SELECT TIMESTAMP_S '2020-07-01 00:00:00'::TIMESTAMPTZ, TIMESTAMP_MS '2020-07-01 00:00:00'::TIMESTAMPTZ, TIMESTAMPTZ '2020-07-01 12:00:00+00'::TIMESTAMP_S"
        ),
        "2020-07-01 00:00:00-04|2020-07-01 00:00:00-04|2020-07-01 08:00:00"
    );
    assert_eq!(
        under(
            "America/New_York",
            "SELECT DATE '2020-07-01' = TIMESTAMPTZ '2020-07-01 04:00:00+00', DATE '2020-07-01' = TIMESTAMPTZ '2020-07-01 00:00:00+00'"
        ),
        "true|false"
    );
}

#[test]
fn a_column_is_cast_the_same_way_as_a_constant() {
    let database = Database::new();
    database.execute("SET TimeZone = 'America/New_York'").expect("a known zone");
    database
        .execute(
            "CREATE TABLE t AS SELECT * FROM (VALUES (TIMESTAMP '2020-07-01 00:00'), (TIMESTAMP '2020-01-01 00:00'), (NULL)) v(x)",
        )
        .expect("creates");
    let result =
        database.query("SELECT x::TIMESTAMPTZ, x::TIMESTAMPTZ::TIMESTAMP FROM t").expect("runs");
    let rows: Vec<String> = (0..result.len())
        .map(|row| format!("{}|{}", result.text_at(row, 0), result.text_at(row, 1)))
        .collect();
    assert_eq!(
        rows,
        [
            "2020-07-01 00:00:00-04|2020-07-01 00:00:00",
            "2020-01-01 00:00:00-05|2020-01-01 00:00:00",
            "NULL|NULL"
        ]
    );
}

#[test]
fn a_skipped_reading_moves_forward_and_a_repeated_one_takes_the_later_instant() {
    assert_eq!(
        under(
            "America/New_York",
            "SELECT TIMESTAMP '2020-03-08 02:30'::TIMESTAMPTZ, TIMESTAMP '2020-11-01 01:30'::TIMESTAMPTZ"
        ),
        "2020-03-08 03:30:00-04|2020-11-01 01:30:00-05"
    );
}

#[test]
fn summer_time_carries_on_past_the_end_of_the_tables() {
    assert_eq!(
        under(
            "America/New_York",
            "SELECT TIMESTAMP '2100-07-01 12:00'::TIMESTAMPTZ, TIMESTAMP '2100-01-01 12:00'::TIMESTAMPTZ"
        ),
        "2100-07-01 12:00:00-04|2100-01-01 12:00:00-05"
    );
    assert_eq!(
        under(
            "Australia/Sydney",
            "SELECT TIMESTAMP '2150-01-01 12:00'::TIMESTAMPTZ, TIMESTAMP '2150-07-01 12:00'::TIMESTAMPTZ"
        ),
        "2150-01-01 12:00:00+11|2150-07-01 12:00:00+10"
    );
    assert_eq!(
        under(
            "America/New_York",
            "SELECT CAST(TIMESTAMP '294247-01-01 00:00' AS TIMESTAMPTZ), TRY_CAST('2020-01-01 zzz' AS TIMESTAMPTZ)"
        ),
        "294247-01-01 00:00:00-05|NULL"
    );
}

#[test]
fn an_offset_is_shown_in_whole_minutes() {
    assert_eq!(
        under("Europe/Amsterdam", "SELECT TIMESTAMPTZ '1900-01-01 00:00:00+00'"),
        "1900-01-01 00:00:00+00"
    );
}

#[test]
fn a_cast_that_cannot_be_answered_says_why() {
    assert_eq!(
        refused("Asia/Tokyo", "SELECT CAST(TIMESTAMPTZ '294247-01-10 03:00:00+00' AS TIMESTAMP)"),
        "Conversion Error: Unable to convert TIMESTAMPTZ to local TIMESTAMP"
    );
    assert_eq!(
        refused("America/New_York", "SELECT CAST(TIMESTAMPTZ '2020-07-01 00:00+00' AS TIME)"),
        "Conversion Error: Unimplemented type for cast (TIMESTAMP WITH TIME ZONE -> TIME)"
    );
    assert_eq!(
        refused("UTC", "SELECT '2020-01-01 10:00+05:3'::TIMESTAMPTZ"),
        "Conversion Error: invalid timestamp field format: \"2020-01-01 10:00+05:3\", expected format is (YYYY-MM-DD HH:MM[:SS[.US]][±HH[:MM[:SS]]| ZONE])"
    );
    assert_eq!(
        refused("UTC", "SELECT '2020-13-01 10:00'::TIMESTAMPTZ"),
        "Conversion Error: timestamp field value out of range: \"2020-13-01 10:00\""
    );
}

#[test]
fn a_zone_name_is_matched_without_regard_to_case_and_kept_as_found() {
    assert_eq!(under("America/new_york", "SELECT current_setting('TimeZone')"), "America/New_York");
    let database = Database::new();
    let error = database.execute("SET TimeZone = 'nope'").unwrap_err().to_string();
    assert!(error.starts_with("Not implemented Error: Unknown TimeZone 'nope'!"), "{error}");
}

#[test]
fn the_last_instant_prints_its_wall_clock_even_past_the_last_timestamp() {
    assert_eq!(
        under(
            "Europe/Berlin",
            "SELECT '294247-01-10 04:00:54.775806+00'::TIMESTAMPTZ, '294247-01-10 03:30:00+00'::TIMESTAMPTZ::VARCHAR, ['294247-01-10 04:00:54.775806+00'::TIMESTAMPTZ]"
        ),
        "294247-01-10 05:00:54.775806+01|294247-01-10 04:30:00+01|['294247-01-10 05:00:54.775806+01']"
    );
    assert_eq!(
        under("America/New_York", "SELECT make_timestamptz(9223372036854775806)"),
        "294247-01-09 23:00:54.775806-05"
    );
}

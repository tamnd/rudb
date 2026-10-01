//! Date functions over `TIMESTAMPTZ`, which read the wall clock in the session zone.
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

const NEW_YORK: &str = "America/New_York";

#[test]
fn a_part_is_read_from_the_wall_clock_and_the_epoch_from_the_instant() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT hour(TIMESTAMPTZ '2020-07-01 02:00+00'), day(TIMESTAMPTZ '2020-07-01 02:00+00'), date_part('epoch', TIMESTAMPTZ '2020-07-01 02:00+00'), date_part('timezone', TIMESTAMPTZ '2020-07-01 02:00+00'), dayname(TIMESTAMPTZ '2020-07-01 02:00+00'), last_day(TIMESTAMPTZ '2020-07-01 02:00+00')"
        ),
        "22|30|1593568800.0|-14400|Tuesday|2020-06-30"
    );
    assert_eq!(
        under(
            "America/St_Johns",
            "SELECT timezone(TIMESTAMPTZ '2020-01-01 00:00+00'), timezone_hour(TIMESTAMPTZ '2020-01-01 00:00+00'), timezone_minute(TIMESTAMPTZ '2020-01-01 00:00+00')"
        ),
        "-12600|-3|-30"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT date_part(['hour', 'epoch', 'timezone_hour'], TIMESTAMPTZ '2020-07-01 02:00+00')"
        ),
        "{'hour': 22, 'epoch': 1593568800.0, 'timezone_hour': -4}"
    );
}

#[test]
fn truncating_cuts_the_wall_clock_and_a_repeated_hour_takes_the_later_instant() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT date_trunc('day', TIMESTAMPTZ '2020-07-01 02:00+00'), date_trunc('month', TIMESTAMPTZ '2020-07-01 02:00+00'), date_trunc('hour', TIMESTAMPTZ '2020-11-01 06:30+00')"
        ),
        "2020-06-30 00:00:00-04|2020-06-01 00:00:00-04|2020-11-01 01:00:00-05"
    );
}

#[test]
fn strftime_writes_the_offset_and_the_name_of_the_zone() {
    assert_eq!(
        under(
            "Asia/Kolkata",
            "SELECT strftime(TIMESTAMPTZ '2020-07-01 02:00+00', '%Y-%m-%d %H:%M %z %Z')"
        ),
        "2020-07-01 07:30 +05:30 Asia/Kolkata"
    );
    assert_eq!(
        under("UTC", "SELECT strftime(TIMESTAMPTZ '2020-07-01 02:00+00', '%H %z %Z')"),
        "02 +00 UTC"
    );
}

#[test]
fn a_day_moves_the_calendar_and_an_hour_is_elapsed_time() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT TIMESTAMPTZ '2020-03-07 12:00-05' + INTERVAL 1 day, TIMESTAMPTZ '2020-03-07 12:00-05' + INTERVAL 24 hour, TIMESTAMPTZ '2020-03-09 12:00-04' - INTERVAL '1 day 1 hour'"
        ),
        "2020-03-08 12:00:00-04|2020-03-08 13:00:00-04|2020-03-08 11:00:00-04"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT TIMESTAMPTZ '2020-03-09 12:00-04' - TIMESTAMPTZ '2020-03-07 12:00-05', TIMESTAMPTZ '2020-03-07 12:00-05' - TIMESTAMPTZ '2020-03-09 13:00-04', age(TIMESTAMPTZ '2020-03-09 12:00-04', TIMESTAMPTZ '2020-01-07 12:00-05')"
        ),
        "2 days|-2 days -01:00:00|2 months 2 days"
    );
    let database = Database::new();
    database.execute("SET TimeZone = 'America/New_York'").expect("a known zone");
    let error = database
        .query("SELECT TIMESTAMPTZ 'infinity' - TIMESTAMPTZ '2020-01-01 00:00+00'")
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Invalid Input Error: Cannot subtract infinite timestamps");
}

#[test]
fn a_column_is_read_a_row_at_a_time_in_its_own_offset() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT hour(x), date_trunc('day', x), strftime(x, '%H:%M %z') FROM (VALUES (TIMESTAMPTZ '2020-03-08 05:00+00'), (TIMESTAMPTZ '2020-03-08 09:00+00'), (NULL)) v(x)"
        ),
        "0|2020-03-08 00:00:00-05|00:00 -05\n5|2020-03-08 00:00:00-05|05:00 -04\nNULL|NULL|NULL"
    );
}

#[test]
fn a_difference_counts_days_on_the_calendar_and_hours_as_elapsed_time() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT date_diff('day', TIMESTAMPTZ '2020-07-01 02:00+00', TIMESTAMPTZ '2020-07-02 05:00+00'), date_diff('hour', TIMESTAMPTZ '2020-03-07 12:00-05', TIMESTAMPTZ '2020-03-09 12:00-04'), date_sub('hour', TIMESTAMPTZ '2020-03-07 12:00-05', TIMESTAMPTZ '2020-03-09 12:00-04'), date_sub('month', TIMESTAMPTZ '2020-07-01 02:00+00', TIMESTAMPTZ '2020-08-01 03:00+00')"
        ),
        "2|47|47|1"
    );
}

#[test]
fn a_bucket_is_cut_in_utc_unless_the_call_names_a_zone() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT time_bucket(INTERVAL 1 day, TIMESTAMPTZ '2020-07-01 02:00+00'), time_bucket(INTERVAL 1 hour, TIMESTAMPTZ '2020-07-01 02:30+00'), time_bucket(INTERVAL 1 month, TIMESTAMPTZ '2020-07-01 02:00+00')"
        ),
        "2020-06-30 20:00:00-04|2020-06-30 22:00:00-04|2020-06-30 20:00:00-04"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT time_bucket(INTERVAL 1 day, TIMESTAMPTZ '2020-07-01 02:00+00', 'America/New_York'), time_bucket(INTERVAL 7 minutes, TIMESTAMPTZ '2020-07-01 02:30+00', 'Asia/Kolkata'), time_bucket(INTERVAL 1 month, TIMESTAMPTZ '2020-07-01 02:00+00', 'America/New_York')"
        ),
        "2020-06-30 00:00:00-04|2020-06-30 22:30:00-04|2020-06-01 00:00:00-04"
    );
}

#[test]
fn a_series_steps_its_days_on_the_wall_clock_and_its_hours_in_elapsed_time() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT range(TIMESTAMPTZ '2020-03-07 00:00', TIMESTAMPTZ '2020-03-10 00:00', INTERVAL 1 day)"
        ),
        "['2020-03-07 00:00:00-05', '2020-03-08 00:00:00-05', '2020-03-09 00:00:00-04']"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT generate_series(TIMESTAMPTZ '2020-03-08 00:00', TIMESTAMPTZ '2020-03-08 04:00', INTERVAL 1 hour)"
        ),
        "['2020-03-08 00:00:00-05', '2020-03-08 01:00:00-05', '2020-03-08 03:00:00-04', '2020-03-08 04:00:00-04']"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT generate_series(TIMESTAMPTZ '2020-01-31 00:00', TIMESTAMPTZ '2020-05-01 00:00', INTERVAL 1 month)"
        ),
        "['2020-01-31 00:00:00-05', '2020-02-29 00:00:00-05', '2020-03-29 00:00:00-04', '2020-04-29 00:00:00-04']"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT * FROM generate_series(TIMESTAMPTZ '2020-03-07 00:00', TIMESTAMPTZ '2020-03-10 00:00', INTERVAL 1 day)"
        ),
        "2020-03-07 00:00:00-05\n2020-03-08 00:00:00-05\n2020-03-09 00:00:00-04\n2020-03-10 00:00:00-04"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT x, g FROM (VALUES (TIMESTAMPTZ '2020-10-31 00:00')) v(x), generate_series(x, x + INTERVAL 2 day, INTERVAL 1 day) t(g) ORDER BY g"
        ),
        "2020-10-31 00:00:00-04|2020-10-31 00:00:00-04\n2020-10-31 00:00:00-04|2020-11-01 00:00:00-04\n2020-10-31 00:00:00-04|2020-11-02 00:00:00-05"
    );
}

#[test]
fn a_zoned_value_inside_a_nested_one_is_written_in_the_session_zone() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT [TIMESTAMPTZ '2020-03-07 00:00'], {'a': TIMESTAMPTZ '2020-03-07 00:00'}, [TIMESTAMPTZ '2020-03-07 00:00']::VARCHAR, MAP {'k': TIMESTAMPTZ '2020-03-07 00:00'}"
        ),
        "['2020-03-07 00:00:00-05']|{'a': '2020-03-07 00:00:00-05'}|['2020-03-07 00:00:00-05']|{k='2020-03-07 00:00:00-05'}"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT [TIMESTAMPTZ '2020-03-07 00:00', NULL]::TIMESTAMP[], [TIMESTAMP '2020-03-07 00:00']::TIMESTAMPTZ[]"
        ),
        "['2020-03-07 00:00:00', NULL]|['2020-03-07 00:00:00-05']"
    );
}

#[test]
fn at_time_zone_reads_an_instant_on_a_named_wall_clock_and_a_wall_clock_as_an_instant() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT TIMESTAMPTZ '2020-07-01 02:00+00' AT TIME ZONE 'Asia/Tokyo', typeof(TIMESTAMPTZ '2020-07-01 02:00+00' AT TIME ZONE 'Asia/Tokyo')"
        ),
        "2020-07-01 11:00:00|TIMESTAMP"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT TIMESTAMP '2020-07-01 02:00' AT TIME ZONE 'Asia/Tokyo', typeof(TIMESTAMP '2020-07-01 02:00' AT TIME ZONE 'Asia/Tokyo')"
        ),
        "2020-06-30 13:00:00-04|TIMESTAMP WITH TIME ZONE"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT timezone('UTC', TIMESTAMPTZ '2020-07-01 02:00+00'), timezone('UTC', TIMESTAMP '2020-07-01 02:00'), DATE '2020-07-01' AT TIME ZONE 'UTC'"
        ),
        "2020-07-01 02:00:00|2020-06-30 22:00:00-04|2020-06-30 20:00:00-04"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT TIMESTAMP '2020-01-01 00:00' AT TIME ZONE 'utc', TIMESTAMP '2020-01-01 00:00' AT TIME ZONE 'EST', TIMESTAMP '2020-01-01 00:00' AT TIME ZONE 'asia/tokyo'"
        ),
        "2019-12-31 19:00:00-05|2020-01-01 00:00:00-05|2019-12-31 10:00:00-05"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT TIMESTAMP '2020-03-08 02:30' AT TIME ZONE 'America/New_York', TIMESTAMP '2020-11-01 01:30' AT TIME ZONE 'America/New_York'"
        ),
        "2020-03-08 03:30:00-04|2020-11-01 01:30:00-05"
    );
}

#[test]
fn at_time_zone_takes_its_zone_from_each_row_and_passes_nulls_and_infinities_through() {
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT TIMESTAMP '2020-07-01 02:00' AT TIME ZONE z FROM (VALUES ('Asia/Tokyo'), ('UTC'), (NULL)) v(z)"
        ),
        "2020-06-30 13:00:00-04\n2020-06-30 22:00:00-04\nNULL"
    );
    assert_eq!(
        under(
            NEW_YORK,
            "SELECT TIMESTAMPTZ 'infinity' AT TIME ZONE 'UTC', TIMESTAMP 'infinity' AT TIME ZONE 'UTC', NULL AT TIME ZONE 'UTC', TIMESTAMP '2020-01-01' AT TIME ZONE NULL"
        ),
        "infinity|infinity|NULL|NULL"
    );
    assert_eq!(under(NEW_YORK, "SELECT typeof(NULL AT TIME ZONE 'UTC')"), "TIME WITH TIME ZONE");
    let database = Database::new();
    let error = database.query("SELECT TIMESTAMP '2020-01-01' AT TIME ZONE 'zzz'").unwrap_err();
    assert!(
        error.to_string().starts_with("Not implemented Error: Unknown TimeZone 'zzz'!"),
        "{error}"
    );
}

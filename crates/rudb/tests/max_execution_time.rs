//! `max_execution_time` stops a statement that runs longer than it allows, in milliseconds. Every
//! expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

const SLOW: &str = "SELECT count(*) FROM range(1000000000) t(a), range(10) u(b) WHERE a + b < 0";

#[test]
fn a_statement_over_the_limit_is_stopped_in_the_pins_words() {
    let database = Database::new();
    database.execute("SET max_execution_time = 1").expect("the setting");
    let error = database.query(SLOW).expect_err("too slow for a millisecond");
    assert_eq!(error.to_string(), "INTERRUPT Error: Query exceeded maximum execution time");
    // The limit is on each statement, so a quick one after it still runs.
    assert_eq!(database.value("SELECT 42").expect("a quick query").to_string(), "42");
}

#[test]
fn a_limit_set_earlier_in_a_script_holds_for_the_statements_after_it() {
    let database = Database::new();
    let error = database
        .execute(&format!("SET max_execution_time = 1; {SLOW}"))
        .expect_err("too slow for a millisecond");
    assert_eq!(error.to_string(), "INTERRUPT Error: Query exceeded maximum execution time");
}

#[test]
fn the_setting_reads_back_and_zero_or_less_is_no_limit() {
    let database = Database::new();
    for (set, expected) in [
        ("SET max_execution_time = -1", "-1"),
        ("SET max_execution_time = 5000", "5000"),
        ("RESET max_execution_time", "0"),
    ] {
        database.execute(set).unwrap_or_else(|error| panic!("{set} failed: {error}"));
        let read = database.value("SELECT current_setting('max_execution_time')").expect("a read");
        assert_eq!(read.to_string(), expected, "{set}");
    }
    let error = database.execute("SET max_execution_time = 'abc'").expect_err("not a number");
    assert_eq!(
        error.to_string(),
        "Invalid Input Error: Failed to cast value: Could not convert string 'abc' to INT64"
    );
    database.execute("SET max_execution_time = -1").expect("the setting");
    assert_eq!(database.value("SELECT 42").expect("no limit").to_string(), "42");
}

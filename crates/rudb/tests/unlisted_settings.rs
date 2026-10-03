//! The settings the pin takes and reads back but does not list in `duckdb_settings()`. Every
//! expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

fn value(database: &Database, sql: &str) -> String {
    database.value(sql).expect(sql).to_string()
}

fn refused(database: &Database, sql: &str) -> String {
    database.execute(sql).expect_err(sql).to_string()
}

#[test]
fn they_answer_by_name_and_are_not_rows() {
    let database = Database::new();
    assert_eq!(value(&database, "SELECT count(*) FROM duckdb_settings()"), "192");
    assert_eq!(
        value(
            &database,
            "SELECT count(*) FROM duckdb_settings() WHERE name IN ('enable_caching_operators', \
             'profiling_mode', 'force_bitpacking_mode', 'old_implicit_casting')"
        ),
        "0"
    );
    assert_eq!(
        value(
            &database,
            "SELECT current_setting('enable_caching_operators') || ' ' || \
             typeof(current_setting('enable_caching_operators'))"
        ),
        "true BOOLEAN"
    );
    assert_eq!(value(&database, "SELECT current_setting('force_bitpacking_mode')"), "AUTO");
    assert_eq!(value(&database, "SELECT current_setting('profiling_mode') IS NULL"), "true");
    assert_eq!(value(&database, "SELECT current_setting('extension_directory')"), "");
}

#[test]
fn a_knob_keeps_what_it_was_set_to() {
    let database = Database::new();
    database.execute("SET enable_caching_operators = 0").expect("a boolean");
    assert_eq!(value(&database, "SELECT current_setting('enable_caching_operators')"), "false");
    database.execute("RESET enable_caching_operators").expect("a reset");
    assert_eq!(value(&database, "SELECT current_setting('enable_caching_operators')"), "true");
    database.execute("SET force_bitpacking_mode = 'constant'").expect("a mode");
    assert_eq!(value(&database, "SELECT current_setting('force_bitpacking_mode')"), "constant");
    assert_eq!(
        refused(&database, "SET force_bitpacking_mode = 'zz'"),
        "Not implemented Error: Enum value: unrecognized value \"zz\" for enum \"BitpackingMode\"\n\nCandidates: \"AUTO\""
    );
    assert_eq!(
        refused(&database, "SET force_update_to_del_and_insert = 'zz'"),
        "Invalid Input Error: Failed to cast value: Could not convert string 'zz' to BOOL"
    );
    database.execute("SET force_update_to_del_and_insert = true").expect("a boolean");
    assert_eq!(
        value(&database, "SELECT current_setting('force_update_to_del_and_insert')"),
        "true"
    );
}

#[test]
fn profiling_mode_reads_back_standard_and_turns_profiling_on() {
    let database = Database::new();
    database.execute("SET profiling_mode = 'DETAILED'").expect("a mode");
    assert_eq!(value(&database, "SELECT current_setting('profiling_mode')"), "standard");
    assert_eq!(value(&database, "SELECT current_setting('enable_profiling')"), "query_tree");
    assert_eq!(
        refused(&database, "SET profiling_mode = 'zz'"),
        "Parser Error: Unrecognized profiling mode \"zz\", supported formats: [standard, detailed, all]"
    );
    // A format already chosen stays the one chosen.
    let database = Database::new();
    database.execute("PRAGMA enable_profiling = 'json'").expect("a format");
    database.execute("SET profiling_mode = 'all'").expect("a mode");
    assert_eq!(value(&database, "SELECT current_setting('enable_profiling')"), "json");
}

#[test]
fn a_misspelling_is_offered_an_unlisted_name() {
    let database = Database::new();
    assert_eq!(
        refused(&database, "SET enable_cachng_operators = 0"),
        "Catalog Error: unrecognized configuration parameter \"enable_cachng_operators\"\n\nDid you mean: \"enable_caching_operators\""
    );
}

#[test]
fn old_implicit_casting_stays_off() {
    let database = Database::new();
    database.execute("SET old_implicit_casting = false").expect("the default");
    let error = refused(&database, "SET old_implicit_casting = true");
    assert!(
        error.starts_with("Not implemented Error: SET old_implicit_casting = 'true'"),
        "{error}"
    );
}

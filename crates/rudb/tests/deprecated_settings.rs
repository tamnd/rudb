//! A deprecated setting or pragma warns when it is used, and with `warnings_as_errors` on the
//! warning is the error. Every expected answer here was taken from the pinned duckdb binary,
//! v2.0.0-dev84237.

use rudb::Database;

fn refused(database: &Database, sql: &str) -> String {
    database.execute(sql).expect_err(sql).to_string()
}

fn deprecated(name: &str) -> String {
    format!(
        "Invalid Input Error: The '{name}' setting is deprecated and will be removed in a future release."
    )
}

#[test]
fn a_deprecated_setting_is_refused_and_not_kept() {
    let database = Database::new();
    database.execute("SET warnings_as_errors = true").expect("the logger is on");
    for (sql, name) in [
        ("SET delim_join_as_cte = false", "delim_join_as_cte"),
        ("SET legacy_disable_null_type = true", "legacy_disable_null_type"),
        ("SET null_on_division_by_zero = true", "null_on_division_by_zero"),
        ("SET regex_match_operator_semantics = 'full'", "regex_match_operator_semantics"),
        ("SET GLOBAL experimental_metadata_reuse = false", "experimental_metadata_reuse"),
        ("SET old_implicit_casting = true", "old_implicit_casting"),
        ("SET extension_directory = 'x'", "extension_directory"),
    ] {
        assert_eq!(refused(&database, sql), deprecated(name), "{sql}");
    }
    assert_eq!(
        refused(&database, "SET profiling_mode = 'standard'"),
        "Invalid Input Error: the profiling_mode setting is deprecated: detailed profiling information is always collected - use \"PRAGMA enable_profiling\" to enable profiling instead"
    );
    assert_eq!(
        refused(&database, "PRAGMA enable_object_cache"),
        "Invalid Input Error: The 'enable_object_cache' pragma no longer has any effect; it is deprecated and will be removed in a future release."
    );
    // A value of the wrong type is refused for its type first.
    assert_eq!(
        refused(&database, "SET delim_join_as_cte = 'zz'"),
        "Invalid Input Error: Failed to cast value: Could not convert string 'zz' to BOOL"
    );
    database.execute("SET threads = 2").expect("not deprecated");
    database.execute("RESET delim_join_as_cte").expect("a reset does not warn");
    database.execute("PRAGMA enable_profiling").expect("not deprecated");
    database.execute("SET warnings_as_errors = false").expect("warnings again");
    assert_eq!(
        database.value("SELECT current_setting('delim_join_as_cte')").expect("a value").to_string(),
        "true"
    );
    database.execute("SET delim_join_as_cte = false").expect("only a warning now");
}

#[test]
fn warnings_as_errors_needs_the_logger_only_when_it_is_turned_on() {
    let database = Database::new();
    database.execute("SET warnings_as_errors = true").expect("the logger is on");
    database.execute("SET enable_logging = false").expect("the logger off");
    assert_eq!(
        database
            .value("SELECT current_setting('warnings_as_errors')")
            .expect("a value")
            .to_string(),
        "true"
    );
    database.execute("SET warnings_as_errors = false").expect("off");
    assert_eq!(
        refused(&database, "SET warnings_as_errors = true"),
        "Settings Error: Can not set 'warnings_as_errors=true'; no logger is available. To solve, run: 'SET enable_logging=true;'"
    );
}

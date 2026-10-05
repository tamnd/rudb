//! `CALL enable_logging(...)` and `CALL disable_logging()` write the logging settings and answer
//! no rows. Every expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

/// The logging settings as `name=value` pairs, in name order.
fn logging(database: &Database) -> String {
    let mut out = Vec::new();
    for name in
        ["enable_logging", "enabled_log_types", "logging_level", "logging_mode", "logging_storage"]
    {
        let value = database
            .value(&format!("SELECT value FROM duckdb_settings() WHERE name = '{name}'"))
            .expect("a setting");
        out.push(format!("{name}={value}"));
    }
    out.join(" ")
}

#[test]
fn a_type_turns_on_that_type_at_its_own_level() {
    let database = Database::new();
    database.execute("CALL enable_logging('PhysicalOperator')").expect("the call");
    assert_eq!(
        logging(&database),
        "enable_logging=1 enabled_log_types=PhysicalOperator logging_level=DEBUG \
         logging_mode=ENABLE_SELECTED logging_storage=shell_log_storage"
    );
    // A level written next to a type is not the level that is used.
    database.execute("CALL enable_logging('filesystem', level = 'error')").expect("the call");
    assert_eq!(
        logging(&database),
        "enable_logging=1 enabled_log_types=FileSystem logging_level=TRACE \
         logging_mode=ENABLE_SELECTED logging_storage=shell_log_storage"
    );
    database
        .execute(
            "CALL enable_logging(['QueryLog', 'HTTP', 'AdaptiveFilter', 'Metrics', 'querylog'])",
        )
        .expect("the call");
    assert_eq!(
        logging(&database),
        "enable_logging=1 enabled_log_types=Metrics,AdaptiveFilter,HTTP,QueryLog \
         logging_level=DEBUG logging_mode=ENABLE_SELECTED logging_storage=shell_log_storage"
    );
}

#[test]
fn no_type_turns_on_every_type_at_the_level_asked_for() {
    let database = Database::new();
    database.execute("CALL enable_logging('QueryLog')").expect("the call");
    database.execute("CALL enable_logging(level = 'trace', storage = 'memory')").expect("the call");
    assert_eq!(
        logging(&database),
        "enable_logging=1 enabled_log_types= logging_level=TRACE logging_mode=LEVEL_ONLY \
         logging_storage=memory"
    );
    // A call that names no storage keeps the one in use, and no level is INFO.
    database.execute("CALL enable_logging()").expect("the call");
    assert_eq!(
        logging(&database),
        "enable_logging=1 enabled_log_types= logging_level=INFO logging_mode=LEVEL_ONLY \
         logging_storage=memory"
    );
    // Turning it off leaves everything else where it was.
    database.execute("CALL disable_logging()").expect("the call");
    assert_eq!(
        logging(&database),
        "enable_logging=0 enabled_log_types= logging_level=INFO logging_mode=LEVEL_ONLY \
         logging_storage=memory"
    );
}

#[test]
fn the_call_answers_no_rows_on_the_query_path_as_well() {
    let database = Database::new();
    let result = database.query("CALL enable_logging()").expect("the call");
    assert!(result.is_empty());
}

#[test]
fn what_the_pin_refuses_is_refused_in_its_words() {
    let database = Database::new();
    for (call, expected) in [
        (
            "CALL enable_logging(level = 'warn')",
            "Not implemented Error: Enum value: unrecognized value \"warn\" for enum \"LogLevel\"\n\nCandidates: \"DEBUG\"",
        ),
        ("CALL enable_logging('Quack')", "Invalid Input Error: Unknown log type: 'Quack'"),
        (
            "CALL enable_logging('QueryLog', 'HTTP')",
            "Invalid Input Error: EnableLogging: expected 0 or 1 parameter",
        ),
        (
            "CALL enable_logging(1)",
            "Binder Error: Unexpected type positional parameter to enable_logging",
        ),
        (
            "CALL enable_logging(storage = 'bogus')",
            "Invalid Input Error: Log storage 'bogus' is not yet registered",
        ),
        (
            "CALL enable_logging(storage = 'memory', storage_config = 'hi')",
            "Invalid Input Error: EnableLogging: storage_config must be a struct",
        ),
        (
            "CALL enable_logging(storage = 'memory', storage_config = {'path': 'x', 'bla': 1})",
            "Invalid Input Error: Unrecognized log storage config option for storage: 'InMemoryLogStorage': 'bla'",
        ),
        (
            "CALL enable_logging(storage_config = {'buffer_size': 10})",
            "Invalid Input Error: Log storage 'ShellLogStorage' does not support passing configuration",
        ),
        (
            "CALL enable_logging(storage = 'file')",
            "Invalid Input Error: Cannot enable 'file' log storage without a valid path. Provide one via storage_path, e.g. CALL enable_logging(storage='file', storage_path='mylog.csv');",
        ),
        (
            "CALL disable_logging('a', 2)",
            "Binder Error: No function matches the given name and argument types 'disable_logging(VARCHAR, INTEGER)'. You might need to add explicit type casts.\n\tCandidate functions:\n\t\"disable_logging\"()\n",
        ),
    ] {
        let error = database.execute(call).expect_err(call);
        assert_eq!(error.to_string(), expected, "{call}");
    }
    // None of them changed anything.
    assert_eq!(
        logging(&database),
        "enable_logging=1 enabled_log_types= logging_level=WARNING logging_mode=LEVEL_ONLY \
         logging_storage=shell_log_storage"
    );
}

#[test]
fn the_settings_take_the_words_the_pin_takes() {
    let database = Database::new();
    database.execute("SET logging_level = 'debug'").expect("a level");
    database.execute("SET logging_mode = 'enable_selected'").expect("a mode");
    database.execute("SET logging_storage = 'MEMORY'").expect("a storage");
    database.execute("SET enable_logging = false").expect("off");
    assert_eq!(
        logging(&database),
        "enable_logging=0 enabled_log_types= logging_level=DEBUG logging_mode=ENABLE_SELECTED \
         logging_storage=memory"
    );
    let error = database.execute("SET logging_mode = 'zz'").expect_err("not a mode");
    assert_eq!(
        error.to_string(),
        "Not implemented Error: Enum value: unrecognized value \"zz\" for enum \"LogMode\"\n\nCandidates: \"DISABLE_SELECTED\""
    );
    let error = database.execute("SET logging_storage = 'bogus'").expect_err("not a storage");
    assert_eq!(error.to_string(), "Invalid Input Error: Log storage 'bogus' is not yet registered");
}

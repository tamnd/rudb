//! Three settings the corpus sets in its preambles that cannot change what rudb returns, so they
//! are taken and kept rather than refused. Every expected answer here was taken from the pinned
//! duckdb binary, v2.0.0-dev84237.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> String {
    database.value(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}")).to_string()
}

#[test]
fn a_knob_reads_back_what_was_written() {
    let database = Database::new();
    for (set, name, expected) in [
        ("SET preserve_insertion_order = false", "preserve_insertion_order", "false"),
        ("SET debug_disable_optimizer = true", "debug_disable_optimizer", "true"),
        ("PRAGMA explain_output = 'optimized_only'", "explain_output", "optimized_only"),
        ("SET explain_output = 'ALL'", "explain_output", "ALL"),
    ] {
        database.execute(set).unwrap_or_else(|error| panic!("{set} failed: {error}"));
        let read = format!("SELECT current_setting('{name}')::VARCHAR");
        assert_eq!(answered(&database, &read), expected, "{set}");
    }
    database.execute("RESET explain_output").expect("the reset");
    assert_eq!(answered(&database, "SELECT current_setting('explain_output')"), "PHYSICAL_ONLY");
}

#[test]
fn explain_output_refuses_a_word_that_is_not_one_of_its_three() {
    let database = Database::new();
    for (written, candidates) in
        [("optimized", "\"ALL\""), ("OPTIMIZED", "\"OPTIMIZED_ONLY\""), ("1", "\"ALL\"")]
    {
        let sql = format!("SET explain_output = '{written}'");
        assert_eq!(
            database.execute(&sql).expect_err(&sql).to_string(),
            format!(
                "Not implemented Error: Enum value: unrecognized value \"{written}\" for enum \
                 \"ExplainOutputType\"\n\nCandidates: {candidates}"
            )
        );
    }
}

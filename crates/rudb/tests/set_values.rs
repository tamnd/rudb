//! The value of a `SET` is an expression and not only a literal. The pin evaluates it when it binds,
//! so a map, a list or `1 + 1` is as good as the constant it comes to, and only a subquery is
//! refused. Every expected answer here was taken from the pinned duckdb binary.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> String {
    database.value(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}")).to_string()
}

#[test]
fn a_setting_takes_a_value_that_folds_to_a_constant() {
    let database = Database::new();
    for (set, read, expected) in [
        (
            "SET profiling_renderer_settings = MAP {'operator_casing': 'upper'}",
            "SELECT current_setting('profiling_renderer_settings')::VARCHAR",
            "{operator_casing=upper}",
        ),
        (
            "SET tracked_metrics = ['query.*']",
            "SELECT current_setting('tracked_metrics')::VARCHAR",
            "[query.*]",
        ),
        ("SET threads = 1 + 1", "SELECT current_setting('threads')::VARCHAR", "2"),
        ("SET memory_limit = '1' || 'GB'", "SELECT current_setting('memory_limit')", "953.6 MiB"),
    ] {
        database.execute(set).unwrap_or_else(|error| panic!("{set} failed: {error}"));
        assert_eq!(answered(&database, read), expected, "{set}");
    }
}

#[test]
fn a_setting_refuses_a_subquery_for_its_value() {
    let error = Database::new().execute("SET threads = (SELECT 2)").unwrap_err();
    assert_eq!(error.to_string(), "Binder Error: SET value cannot contain subqueries");
}

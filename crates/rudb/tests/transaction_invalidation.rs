//! `default_transaction_invalidation_policy` decides which errors abort the transaction they happen
//! in. Every expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

const ABORTED: &str = "TransactionContext Error: Current transaction is aborted (please ROLLBACK)";

fn answered(database: &Database, sql: &str) -> String {
    database.value(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}")).to_string()
}

fn refused(database: &Database, sql: &str) -> String {
    database.execute(sql).expect_err(sql).to_string()
}

#[test]
fn by_default_any_error_aborts_the_transaction() {
    let database = Database::new();
    database.execute("BEGIN").expect("a transaction");
    assert!(refused(&database, "SELECT * FROM nosuch").starts_with("Catalog Error"));
    assert_eq!(refused(&database, "SELECT 42"), ABORTED);
    database.execute("ROLLBACK").expect("the rollback");
}

#[test]
fn syntactic_errors_leave_the_transaction_open_and_others_still_abort_it() {
    let database = Database::new();
    database
        .execute(
            "SET default_transaction_invalidation_policy = 'SYNTACTIC_ERRORS_DO_NOT_INVALIDATE'",
        )
        .expect("the setting");
    database.execute("CREATE TABLE t (a INTEGER)").expect("a table");
    database.execute("BEGIN").expect("a transaction");
    for (failing, after) in [
        ("SELECT * FROM nosuch", "SELECT 43"),
        ("SELEC 1", "SELECT 44"),
        ("SELECT nosuchfn(1)", "SELECT 46"),
        ("CREATE TABLE t (a INTEGER)", "SELECT 50"),
        ("INSERT INTO t VALUES (1, 2)", "SELECT 55"),
    ] {
        refused(&database, failing);
        assert_eq!(answered(&database, after), &after[7..], "after {failing}");
    }
    assert!(refused(&database, "SELECT 'a'::INTEGER").starts_with("Conversion Error"));
    assert_eq!(refused(&database, "SELECT 45"), ABORTED);
    database.execute("ROLLBACK").expect("the rollback");
}

#[test]
fn the_policy_keeps_what_was_written_and_refuses_other_words() {
    let database = Database::new();
    database
        .execute(
            "SET default_transaction_invalidation_policy = 'syntactic_errors_do_not_invalidate'",
        )
        .expect("the setting");
    assert_eq!(
        answered(&database, "SELECT current_setting('default_transaction_invalidation_policy')"),
        "syntactic_errors_do_not_invalidate"
    );
    assert_eq!(
        refused(&database, "SET default_transaction_invalidation_policy = 'bogus'"),
        "Not implemented Error: Enum value: unrecognized value \"bogus\" for enum \
         \"TransactionInvalidationPolicy\"\n\nCandidates: \"ALL_ERRORS_INVALIDATE_TRANSACTION\""
    );
    database.execute("RESET default_transaction_invalidation_policy").expect("the reset");
    assert_eq!(
        answered(&database, "SELECT current_setting('default_transaction_invalidation_policy')"),
        "ALL_ERRORS_INVALIDATE_TRANSACTION"
    );
}

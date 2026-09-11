//! `CREATE VIEW`, from both ends: the catalog rules and what a view does when it is selected from.
//!
//! Every sentence asserted here was read off duckdb v1.5.1 on server2 rather than decided here, and
//! the awkward ones are awkward because the binary is. A collision names the type being created and
//! not the type already there. A drop of the wrong type says which is which even under `IF EXISTS`.
//! A short column list renames a prefix rather than being an error, and a long one is refused. And
//! an insert into a view says `is not an table`, article and all.
//!
//! The last test is the reason the rest of them exist. `CREATE VIEW hits AS SELECT * FROM
//! read_parquet(...)` is what lets a published benchmark query that says `FROM hits` run against
//! rudb with nothing about it changed, which is what makes a number comparable to duckdb's.

use rudb::Database;
use rudb_common::Value;

/// The ClickBench fixture, ten thousand real rows of the real schema.
const HITS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/hits.parquet");

/// A database with the statements already run, panicking on the first that does not.
fn ran(statements: &[&str]) -> Database {
    let database = Database::new();
    for sql in statements {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// The error the last statement gives, after the ones in front of it were run.
fn refused(statements: &[&str], sql: &str) -> String {
    let database = ran(statements);
    let error = database.execute(sql).expect_err(&format!("{sql} should be refused"));
    error.to_string()
}

#[test]
fn a_view_answers_with_the_rows_its_body_produces() {
    let database = ran(&["CREATE VIEW v AS SELECT 1 AS x, 'a' AS y"]);
    let result = database.query("SELECT * FROM v").expect("the view is there");
    assert_eq!(result.names(), ["x", "y"]);
    assert_eq!(result.len(), 1);
    assert_eq!(result.value_at(0, 0), Value::Integer(1));
}

#[test]
fn a_view_is_bound_again_at_every_reference_rather_than_frozen_when_it_was_made() {
    // The measured behaviour, and the whole reason the catalog keeps the text instead of a plan.
    // duckdb v1.5.1 answers a view over `SELECT * FROM t` with a column that was added to `t` after
    // the view was created, so the star is expanded when the view is read and not when it is made.
    let database = ran(&[
        "CREATE TABLE t (i INTEGER)",
        "INSERT INTO t VALUES (1)",
        "CREATE VIEW v AS SELECT * FROM t",
        "DROP TABLE t",
        "CREATE TABLE t (i INTEGER, j INTEGER)",
        "INSERT INTO t VALUES (1, 2)",
    ]);
    let result = database.query("SELECT * FROM v").expect("the view follows the new table");
    assert_eq!(result.names(), ["i", "j"]);
}

#[test]
fn a_view_over_a_table_that_is_gone_complains_when_it_is_read() {
    let error = refused(
        &["CREATE TABLE t (i INTEGER)", "CREATE VIEW v AS SELECT * FROM t", "DROP TABLE t"],
        "SELECT * FROM v",
    );
    assert_eq!(error, "Catalog Error: Table with name t does not exist!");
}

#[test]
fn a_view_over_a_table_that_was_never_there_is_refused_when_it_is_made() {
    let error = refused(&[], "CREATE VIEW v AS SELECT * FROM nope");
    assert_eq!(error, "Catalog Error: Table with name nope does not exist!");
}

#[test]
fn a_view_and_a_table_are_one_namespace_and_the_message_names_what_was_being_made() {
    // Backwards to read and this is what the binary says. Making a table over an existing view says
    // Table, and making a view over an existing table says View.
    assert_eq!(
        refused(&["CREATE VIEW v AS SELECT 1"], "CREATE TABLE v (i INTEGER)"),
        "Catalog Error: Table with name \"v\" already exists!"
    );
    assert_eq!(
        refused(&["CREATE TABLE t (i INTEGER)"], "CREATE VIEW t AS SELECT 1"),
        "Catalog Error: View with name \"t\" already exists!"
    );
    assert_eq!(
        refused(&["CREATE VIEW v AS SELECT 1"], "CREATE VIEW v AS SELECT 2"),
        "Catalog Error: View with name \"v\" already exists!"
    );
}

#[test]
fn dropping_one_as_the_other_says_which_is_which_even_under_if_exists() {
    assert_eq!(
        refused(&["CREATE VIEW v AS SELECT 1"], "DROP TABLE v"),
        "Catalog Error: Existing object v is of type View, trying to drop type Table"
    );
    assert_eq!(
        refused(&["CREATE TABLE t (i INTEGER)"], "DROP VIEW t"),
        "Catalog Error: Existing object t is of type Table, trying to drop type View"
    );
    // `IF EXISTS` is about the name not being there, not about it being something else.
    assert_eq!(
        refused(&["CREATE VIEW v AS SELECT 1"], "DROP TABLE IF EXISTS v"),
        "Catalog Error: Existing object v is of type View, trying to drop type Table"
    );
}

#[test]
fn a_dropped_view_is_gone_and_dropping_one_that_never_was_is_fine_under_if_exists() {
    let database = ran(&["CREATE VIEW v AS SELECT 1", "DROP VIEW v", "DROP VIEW IF EXISTS v"]);
    let error = database.query("SELECT * FROM v").expect_err("it is gone");
    assert_eq!(error.to_string(), "Catalog Error: Table with name v does not exist!");
}

#[test]
fn a_column_list_renames_a_prefix_and_a_list_longer_than_the_body_is_refused() {
    let database = ran(&["CREATE VIEW v (a) AS SELECT 1 AS x, 2 AS y"]);
    let result = database.query("SELECT * FROM v").expect("a short list is not an error");
    // The list runs out after the first column and the second keeps the name the body gave it.
    assert_eq!(result.names(), ["a", "y"]);
    assert_eq!(
        refused(&[], "CREATE VIEW v (a, b, c) AS SELECT 1, 2"),
        "Binder Error: More VIEW aliases than columns in query result"
    );
}

#[test]
fn a_renamed_column_answers_to_the_new_name_and_not_to_the_old_one() {
    let database = ran(&["CREATE VIEW v (a) AS SELECT 1 AS x"]);
    assert_eq!(database.value("SELECT v.a FROM v").expect("the new name"), Value::Integer(1));
    database.query("SELECT v.x FROM v").expect_err("the body's name is not visible through it");
}

#[test]
fn a_view_takes_an_alias_and_a_column_list_where_it_is_referenced() {
    let database = ran(&["CREATE VIEW v AS SELECT 1 AS x"]);
    let result = database.query("SELECT w.y FROM v AS w (y)").expect("aliased twice over");
    assert_eq!(result.names(), ["y"]);
    assert_eq!(result.value_at(0, 0), Value::Integer(1));
}

#[test]
fn or_replace_replaces_the_body_and_the_names_and_if_not_exists_keeps_the_first_one() {
    let replaced =
        ran(&["CREATE VIEW v AS SELECT 1 AS x", "CREATE OR REPLACE VIEW v AS SELECT 2 AS y"]);
    let result = replaced.query("SELECT * FROM v").expect("the second body");
    assert_eq!(result.names(), ["y"]);
    assert_eq!(result.value_at(0, 0), Value::Integer(2));

    let kept =
        ran(&["CREATE VIEW v AS SELECT 1 AS x", "CREATE VIEW IF NOT EXISTS v AS SELECT 2 AS y"]);
    let result = kept.query("SELECT * FROM v").expect("the first body");
    assert_eq!(result.names(), ["x"]);
}

#[test]
fn a_view_that_would_expand_forever_says_so_rather_than_running_out_of_stack() {
    // A cycle cannot be written directly, because the name a view is being created under does not
    // exist while its body binds. It takes `OR REPLACE` to close the loop after the fact.
    let error = refused(
        &[
            "CREATE VIEW a AS SELECT 1 AS x",
            "CREATE VIEW b AS SELECT * FROM a",
            "CREATE OR REPLACE VIEW a AS SELECT * FROM b",
        ],
        "SELECT * FROM a",
    );
    assert_eq!(
        error,
        "Binder Error: infinite recursion detected: attempting to recursively bind view \"a\""
    );
}

#[test]
fn writing_to_a_view_is_refused_in_the_binarys_own_grammar() {
    assert_eq!(
        refused(&["CREATE VIEW v AS SELECT 1 AS x"], "INSERT INTO v VALUES (2)"),
        "Catalog Error: v is not an table"
    );
}

#[test]
fn a_view_over_a_parquet_file_runs_the_published_benchmark_sql_unmodified() {
    // The point of the whole feature. These four are ClickBench q1, q2, q3 and q5 as published,
    // character for character, semicolons and all, and the only thing that makes them run against
    // a Parquet file is the view. The answers are the ones in `testdata/clickbench-answers.txt`,
    // which duckdb wrote over this same fixture.
    let sql = format!("CREATE VIEW hits AS SELECT * FROM read_parquet('{HITS}')");
    let database = ran(&[&sql]);

    assert_eq!(database.value("SELECT COUNT(*) FROM hits;").expect("q1"), Value::BigInt(10_000));
    assert_eq!(
        database.value("SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0;").expect("q2"),
        Value::BigInt(4_844)
    );
    assert_eq!(
        database.value("SELECT COUNT(DISTINCT UserID) FROM hits;").expect("q5"),
        Value::BigInt(141)
    );

    let result = database
        .query("SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits;")
        .expect("q3");
    assert_eq!(result.len(), 1);
    assert_eq!(result.value_at(0, 1), Value::BigInt(10_000));
    assert_eq!(result.value_at(0, 2), Value::Double(699.5));
}

//! `CREATE RECURSIVE VIEW`, which is a view over a recursive definition of the same name. Every
//! expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect()
}

#[test]
fn a_recursive_view_reads_itself_through_its_own_name() {
    let database = Database::new();
    database
        .execute(
            "CREATE RECURSIVE VIEW nums (n) AS VALUES (1) UNION ALL SELECT n + 1 FROM nums WHERE n \
             < 5",
        )
        .expect("the view");
    assert_eq!(answered(&database, "SELECT * FROM nums"), ["1", "2", "3", "4", "5"]);
    assert_eq!(
        answered(&database, "SELECT sql FROM duckdb_views() WHERE view_name = 'nums'"),
        ["CREATE VIEW nums (n) AS WITH RECURSIVE nums (n) AS ((SELECT * FROM (VALUES (1)) AS \
             valueslist) UNION  ALL (SELECT (n + 1) FROM nums WHERE (n < 5)))SELECT n FROM nums;"]
    );

    database
        .execute(
            "CREATE OR REPLACE TEMP RECURSIVE VIEW pairs (n, m) AS VALUES (1, 2) UNION SELECT n + \
             1, m FROM pairs WHERE n < 3",
        )
        .expect("the second view");
    assert_eq!(answered(&database, "SELECT * FROM pairs"), ["1|2", "2|2", "3|2"]);

    database
        .execute("CREATE RECURSIVE VIEW one (n) AS SELECT 1")
        .expect("a view that reads nothing");
    assert_eq!(answered(&database, "SELECT * FROM one"), ["1"]);
}

#[test]
fn a_recursive_view_with_no_column_list_names_the_columns_its_body_has() {
    let database = Database::new();
    let error = database
        .execute(
            "CREATE RECURSIVE VIEW nums AS VALUES (1) UNION ALL SELECT n + 1 FROM nums WHERE n < 5",
        )
        .expect_err("the body reads a column the anchor does not name");
    assert!(
        error.to_string().starts_with(
            "Binder Error: Referenced column \"n\" not found in FROM clause! Candidate bindings: \
             \"col0\""
        ),
        "{error}"
    );
}

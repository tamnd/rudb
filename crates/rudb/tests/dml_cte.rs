//! `WITH` definitions that are an `INSERT`, an `UPDATE` or a `DELETE`. Every expected answer here
//! was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let mut rows: Vec<String> = result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect();
    rows.sort();
    rows
}

fn table(database: &Database) -> Vec<String> {
    answered(database, "SELECT * FROM t")
}

#[test]
fn a_definition_runs_once_and_its_rows_are_what_it_returned() {
    let database = Database::new();
    database.execute("CREATE TABLE t (i INTEGER, j INTEGER)").expect("the table");
    assert_eq!(
        answered(
            &database,
            "WITH ins AS (INSERT INTO t VALUES (1, 10), (2, 20) RETURNING i) SELECT * FROM ins"
        ),
        ["1", "2"]
    );
    assert_eq!(table(&database), ["1|10", "2|20"]);
    assert_eq!(
        answered(
            &database,
            "WITH upd AS (UPDATE t SET j = 99 WHERE i = 2 RETURNING i, j) SELECT * FROM upd"
        ),
        ["2|99"]
    );
    assert_eq!(
        answered(
            &database,
            "WITH del AS (DELETE FROM t WHERE i = 1 RETURNING i) SELECT * FROM del"
        ),
        ["1"]
    );
    assert_eq!(table(&database), ["2|99"]);
    assert_eq!(
        answered(
            &database,
            "WITH ins AS (INSERT INTO t VALUES (3, 3) RETURNING *) SELECT * FROM ins x JOIN ins y USING (i)"
        ),
        ["3|3|3"]
    );
    assert_eq!(
        answered(
            &database,
            "WITH ins(a) AS (INSERT INTO t VALUES (4, 4) RETURNING *) SELECT a, j FROM ins"
        ),
        ["4|4"]
    );
}

#[test]
fn a_definition_nothing_reads_still_runs_and_the_statement_sees_what_it_did() {
    let database = Database::new();
    database.execute("CREATE TABLE t (i INTEGER, j INTEGER)").expect("the table");
    database.execute("INSERT INTO t VALUES (10, 100)").expect("a row");
    assert_eq!(
        answered(
            &database,
            "WITH ins AS (INSERT INTO t VALUES (20, 200) RETURNING i, j) SELECT t.i, t.j FROM t"
        ),
        ["10|100", "20|200"]
    );
    answered(&database, "WITH ins AS (INSERT INTO t VALUES (99, 99)) SELECT 42 WHERE false");
    assert_eq!(answered(&database, "SELECT count(*) FROM t WHERE i = 99"), ["1"]);
    assert_eq!(
        answered(
            &database,
            "WITH ins AS (INSERT INTO t VALUES (55, 55), (56, 56)) SELECT 1 AS x FROM ins"
        ),
        ["1", "1"]
    );
}

#[test]
fn each_definition_sees_what_the_ones_before_it_did() {
    let database = Database::new();
    database.execute("CREATE TABLE t (i INTEGER, j INTEGER)").expect("the table");
    database.execute("CREATE TABLE aux (i INTEGER, j INTEGER)").expect("the other table");
    database.execute("INSERT INTO aux VALUES (1, 10), (2, 20), (3, 30)").expect("rows");
    assert_eq!(
        answered(
            &database,
            "WITH ins AS (INSERT INTO t VALUES (1, 100), (2, 200) RETURNING i), del AS (DELETE \
             FROM aux WHERE i IN (SELECT i FROM ins) RETURNING i) SELECT * FROM del"
        ),
        ["1", "2"]
    );
    assert_eq!(answered(&database, "SELECT * FROM aux"), ["3|30"]);
    database.execute("TRUNCATE t").expect("emptied");
    assert_eq!(
        answered(
            &database,
            "WITH ins AS (INSERT INTO t VALUES (1, 10), (2, 20) RETURNING i, j), upd1 AS (UPDATE t \
             SET j = j + 1 RETURNING i, j), upd2 AS (UPDATE t SET j = j * 2 RETURNING i, j) SELECT \
             'upd1', i, j FROM upd1 UNION ALL SELECT 'upd2', i, j FROM upd2"
        ),
        ["upd1|1|11", "upd1|2|21", "upd2|1|22", "upd2|2|42"]
    );
    assert_eq!(table(&database), ["1|22", "2|42"]);
    assert_eq!(
        answered(
            &database,
            "WITH m AS (INSERT INTO aux VALUES (20, 0) RETURNING i) UPDATE t SET j = t.i * m.i \
             FROM m WHERE t.i <= 1 RETURNING i, j"
        ),
        ["1|20"]
    );
}

#[test]
fn a_failure_in_one_definition_undoes_the_others() {
    let database = Database::new();
    database.execute("CREATE TABLE t (i INTEGER, j INTEGER)").expect("the table");
    database.execute("CREATE TABLE u (i INTEGER UNIQUE)").expect("the unique table");
    database.execute("INSERT INTO u VALUES (1)").expect("a row");
    let error = database
        .execute(
            "WITH a AS (INSERT INTO t VALUES (1, 10)), b AS (INSERT INTO u VALUES (1)) SELECT 1",
        )
        .expect_err("a duplicate key");
    assert!(error.to_string().starts_with("Constraint Error"), "{error}");
    assert_eq!(answered(&database, "SELECT count(*) FROM t"), ["0"]);

    database.execute("BEGIN").expect("a transaction");
    database.execute("WITH a AS (INSERT INTO t VALUES (1, 10)) SELECT 1").expect("the insert");
    database.execute("ROLLBACK").expect("rolled back");
    assert_eq!(answered(&database, "SELECT count(*) FROM t"), ["0"]);
}

#[test]
fn the_pin_refuses_what_it_refuses_in_its_words() {
    let database = Database::new();
    database.execute("CREATE TABLE t (i INTEGER, j INTEGER)").expect("the table");
    for (sql, expected) in [
        (
            "WITH cte AS (CREATE TABLE forbidden (i INT)) SELECT 1",
            "Parser Error: A CTE body must be a SELECT, INSERT, UPDATE, DELETE, or COPY TO statement",
        ),
        (
            "WITH RECURSIVE ins AS (INSERT INTO t VALUES (1, 2) RETURNING i) SELECT * FROM ins",
            "Parser Error: Recursive CTEs with DML statements are not supported",
        ),
        (
            "SELECT (WITH ins AS (INSERT INTO t VALUES (1, 2) RETURNING i) SELECT * FROM ins) AS x",
            "Binder Error: WITH clause containing a data-modifying statement must be at the top level",
        ),
        (
            "CREATE VIEW v AS WITH ins AS (INSERT INTO t VALUES (1, 2) RETURNING i) SELECT * FROM ins",
            "Binder Error: DML statements (INSERT/UPDATE/DELETE) are not allowed as CTE bodies \
             inside a VIEW",
        ),
    ] {
        let error = database.execute(sql).expect_err(sql);
        assert!(error.to_string().starts_with(expected), "{sql}: {error}");
    }
    assert_eq!(answered(&database, "SELECT count(*) FROM t"), ["0"]);
}

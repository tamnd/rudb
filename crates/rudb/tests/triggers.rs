//! `CREATE TRIGGER` and `DROP TRIGGER`, and the statements that fire them. Every expected answer
//! here was taken from the pinned duckdb binary, v2.0.0-dev84237.

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

fn tables(database: &Database) {
    database.execute("CREATE TABLE a (id INTEGER)").expect("the table");
    database.execute("CREATE TABLE b (id INTEGER)").expect("the other table");
}

fn refused(database: &Database, sql: &str, expected: &str) {
    let error = database.execute(sql).expect_err(sql);
    assert!(error.to_string().starts_with(expected), "{sql}: {error}");
}

#[test]
fn a_statement_trigger_fires_once_even_when_nothing_changed() {
    let database = Database::new();
    tables(&database);
    database
        .execute("CREATE TRIGGER x AFTER INSERT ON a FOR EACH STATEMENT INSERT INTO b VALUES (1)")
        .expect("the trigger");
    database
        .execute("CREATE TRIGGER y AFTER DELETE ON a FOR EACH STATEMENT INSERT INTO b VALUES (2)")
        .expect("the delete trigger");
    database.execute("INSERT INTO a VALUES (1), (2), (3)").expect("the insert");
    assert_eq!(answered(&database, "SELECT * FROM b"), ["1"]);
    database.execute("DELETE FROM a WHERE id > 10").expect("a delete of nothing");
    assert_eq!(answered(&database, "SELECT * FROM b"), ["1", "2"]);
}

#[test]
fn before_runs_ahead_of_the_statement_and_after_behind_it() {
    let database = Database::new();
    tables(&database);
    database
        .execute(
            "CREATE TRIGGER x BEFORE INSERT ON a FOR EACH STATEMENT INSERT INTO b SELECT count(*) \
             FROM a",
        )
        .expect("the before trigger");
    database
        .execute(
            "CREATE TRIGGER y AFTER INSERT ON a FOR EACH STATEMENT INSERT INTO b SELECT count(*) + \
             100 FROM a",
        )
        .expect("the after trigger");
    database.execute("INSERT INTO a VALUES (1), (2)").expect("the insert");
    assert_eq!(answered(&database, "SELECT * FROM b"), ["0", "102"]);
}

#[test]
fn update_of_fires_only_when_a_listed_column_is_set() {
    let database = Database::new();
    database.execute("CREATE TABLE a (id INTEGER, v INTEGER)").expect("the table");
    database.execute("CREATE TABLE b (id INTEGER)").expect("the other table");
    database.execute("INSERT INTO a VALUES (1, 1)").expect("a row");
    database
        .execute(
            "CREATE TRIGGER x AFTER UPDATE OF V ON a FOR EACH STATEMENT INSERT INTO b VALUES (1)",
        )
        .expect("the trigger");
    database.execute("UPDATE a SET id = 2").expect("an update of another column");
    assert_eq!(answered(&database, "SELECT count(*) FROM b"), ["0"]);
    database.execute("UPDATE a SET v = 2").expect("an update of the column");
    assert_eq!(answered(&database, "SELECT count(*) FROM b"), ["1"]);
}

#[test]
fn transition_tables_hold_the_rows_the_statement_changed() {
    let database = Database::new();
    tables(&database);
    database.execute("INSERT INTO a VALUES (1), (2)").expect("rows");
    database
        .execute(
            "CREATE TRIGGER x AFTER UPDATE ON a REFERENCING NEW TABLE AS n OLD TABLE AS o FOR EACH \
             STATEMENT INSERT INTO b SELECT n.id * 10 + o.id FROM n, o",
        )
        .expect("the trigger");
    database.execute("UPDATE a SET id = 5 WHERE id = 2").expect("the update");
    assert_eq!(answered(&database, "SELECT * FROM b"), ["52"]);
}

#[test]
fn a_row_trigger_reads_each_row_through_new_and_old() {
    let database = Database::new();
    database.execute("CREATE TABLE a (id INTEGER)").expect("the table");
    database.execute("CREATE TABLE b (id INTEGER, x INTEGER)").expect("the other table");
    database
        .execute(
            "CREATE TRIGGER x AFTER INSERT ON a FOR EACH ROW INSERT INTO b VALUES (NEW.ID, \
             new.\"id\" * 2)",
        )
        .expect("the insert trigger");
    database.execute("INSERT INTO a VALUES (7), (8)").expect("the insert");
    assert_eq!(answered(&database, "SELECT * FROM b"), ["7|14", "8|16"]);

    let database = Database::new();
    tables(&database);
    database.execute("INSERT INTO a VALUES (1), (2), (3)").expect("rows");
    database.execute("INSERT INTO b VALUES (1), (2), (3), (4)").expect("other rows");
    database
        .execute("CREATE TRIGGER x AFTER DELETE ON a FOR EACH ROW DELETE FROM b WHERE id = OLD.id")
        .expect("the delete trigger");
    database.execute("DELETE FROM a WHERE id < 3").expect("the delete");
    assert_eq!(answered(&database, "SELECT * FROM b"), ["3", "4"]);
}

#[test]
fn a_failing_trigger_undoes_the_statement_that_fired_it() {
    let database = Database::new();
    database.execute("CREATE TABLE a (id INTEGER)").expect("the table");
    database.execute("CREATE TABLE b (id INTEGER PRIMARY KEY)").expect("the keyed table");
    database
        .execute("CREATE TRIGGER x AFTER INSERT ON a FOR EACH STATEMENT INSERT INTO b VALUES (1)")
        .expect("the trigger");
    database.execute("INSERT INTO a VALUES (1)").expect("the first insert");
    refused(&database, "INSERT INTO a VALUES (2)", "Constraint Error");
    assert_eq!(answered(&database, "SELECT * FROM a"), ["1"]);
}

#[test]
fn duckdb_triggers_lists_each_trigger() {
    let database = Database::new();
    tables(&database);
    database
        .execute("CREATE TRIGGER x AFTER INSERT ON a FOR EACH STATEMENT INSERT INTO b VALUES (1)")
        .expect("the trigger");
    assert_eq!(
        answered(
            &database,
            "SELECT trigger_name, table_name, action_timing, event_manipulation, columns, \
             for_each, sql FROM duckdb_triggers()"
        ),
        ["x|a|AFTER|INSERT|[]|STATEMENT|CREATE TRIGGER x AFTER INSERT ON a FOR EACH STATEMENT \
          INSERT INTO b (VALUES (1));"]
    );
    database.execute("DROP TRIGGER x ON a").expect("dropped");
    assert_eq!(answered(&database, "SELECT count(*) FROM duckdb_triggers()"), ["0"]);
}

#[test]
fn the_pin_refuses_what_it_refuses_in_its_words() {
    let database = Database::new();
    tables(&database);
    database.execute("CREATE VIEW v AS SELECT 1").expect("the view");
    for (sql, expected) in [
        (
            "CREATE TRIGGER x AFTER INSERT ON a REFERENCING NEW TABLE AS n FOR EACH ROW INSERT \
             INTO b VALUES (NEW.id)",
            "Binder Error: REFERENCING is not valid for FOR EACH ROW triggers",
        ),
        (
            "CREATE TRIGGER x BEFORE INSERT ON a FOR EACH ROW INSERT INTO b VALUES (NEW.id)",
            "Not implemented Error: BEFORE FOR EACH ROW triggers are not yet supported",
        ),
        (
            "CREATE TRIGGER x AFTER INSERT ON v FOR EACH STATEMENT INSERT INTO a VALUES (1)",
            "Binder Error: CREATE TRIGGER requires a base table, not a view or subquery",
        ),
        (
            "CREATE TRIGGER x AFTER INSERT ON a REFERENCING OLD TABLE AS o FOR EACH STATEMENT \
             INSERT INTO b VALUES (1)",
            "Binder Error: REFERENCING OLD TABLE AS is not valid for AFTER INSERT triggers",
        ),
        (
            "CREATE TRIGGER x AFTER INSERT ON a FOR EACH ROW INSERT INTO b VALUES (1)",
            "Binder Error: FOR EACH ROW trigger \"x\" on table \"a\" must reference at least one \
             NEW or OLD column in the trigger body",
        ),
        (
            "CREATE TRIGGER x AFTER INSERT ON a FOR EACH STATEMENT INSERT INTO a VALUES (1)",
            "Not implemented Error: Recursive trigger chains are not yet supported",
        ),
        (
            "DROP TRIGGER nope ON a",
            "Catalog Error: Trigger with name \"nope\" does not exist on table \"a\"",
        ),
    ] {
        refused(&database, sql, expected);
    }
    database
        .execute("CREATE TRIGGER x AFTER INSERT ON a FOR EACH STATEMENT INSERT INTO b VALUES (1)")
        .expect("the trigger");
    refused(
        &database,
        "CREATE TRIGGER x AFTER DELETE ON a FOR EACH STATEMENT INSERT INTO b VALUES (1)",
        "Catalog Error: Trigger with name \"x\" already exists!",
    );
    refused(
        &database,
        "CREATE TRIGGER y AFTER INSERT ON a FOR EACH ROW INSERT INTO b VALUES (NEW.id)",
        "Not implemented Error: Mixing FOR EACH STATEMENT and FOR EACH ROW triggers on the same \
         table is not yet supported",
    );
    refused(
        &database,
        "DROP TABLE b",
        "Dependency Error: Cannot drop entry \"b\" because there are entries that depend on it.",
    );
}

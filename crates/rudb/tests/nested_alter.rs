//! `ALTER TABLE ... ALTER s.a ...`, a change to one field of a struct column. The pin binary,
//! v2.0.0-dev84237, makes the change to the whole of `s` instead, which is tamnd/duckdb#36, so the
//! words here are the ones the test file it ships expects.

use rudb::Database;

fn refused(database: &Database, sql: &str, expected: &str) {
    let error = database.execute(sql).expect_err(sql);
    assert!(error.to_string().contains(expected), "{sql}: {error}");
}

#[test]
fn a_change_to_a_field_of_a_struct_column_is_refused_and_the_column_is_left_alone() {
    let database = Database::new();
    database.execute("CREATE TABLE test (s STRUCT(a INTEGER, b INTEGER))").expect("the table");
    database.execute("INSERT INTO test VALUES ({'a': NULL, 'b': 1})").expect("a row");
    refused(
        &database,
        "ALTER TABLE test ALTER s.a SET NOT NULL",
        "Setting a NOT NULL constraint on a nested field is not yet supported",
    );
    refused(
        &database,
        "ALTER TABLE test ALTER s.a DROP NOT NULL",
        "Dropping a NOT NULL constraint on a nested field is not yet supported",
    );
    refused(
        &database,
        "ALTER TABLE test ALTER s.a SET DEFAULT 5",
        "Setting a default value on a nested field is not yet supported",
    );
    refused(
        &database,
        "ALTER TABLE test ALTER COLUMN s.a TYPE BIGINT",
        "Changing the type of a nested field is not yet supported",
    );
    database.execute("INSERT INTO test VALUES (NULL)").expect("s still takes a null");
    let result = database.query("SELECT count(*) FROM test").expect("the count");
    assert_eq!(result.value_at(0, 0).to_string(), "2");
}

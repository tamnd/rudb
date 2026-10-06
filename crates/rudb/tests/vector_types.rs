//! `test_vector_types()`, a column for each argument's type with the rows of every vector layout.
//! Every expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

/// Every row of `sql` as the shell writes it.
fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let width = result.rows().next().map_or(0, |row| row.len());
    (0..result.len())
        .map(|row| {
            (0..width).map(|column| result.text_at(row, column)).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn refused(database: &Database, sql: &str, expected: &str) {
    let error = database.execute(sql).expect_err(sql);
    assert!(error.to_string().starts_with(expected), "{sql}: {error}");
}

#[test]
fn an_integer_is_flat_constant_dictionary_and_a_sequence() {
    let database = Database::new();
    let sql = "SELECT * FROM test_vector_types(NULL::INT)";
    assert_eq!(
        answered(&database, sql),
        [
            "-2147483648",
            "2147483647",
            "NULL",
            "-2147483648",
            "-2147483648",
            "-2147483648",
            "2147483647",
            "NULL",
            "3",
            "5",
            "7",
        ],
        "{sql}"
    );
}

#[test]
fn nested_types_are_built_from_what_is_inside_them() {
    let database = Database::new();
    let sql = "SELECT * FROM test_vector_types(NULL::ROW(a INT[], b ROW(c TINYINT)))";
    assert_eq!(
        answered(&database, sql),
        [
            "{'a': [-2147483648, 2147483647], 'b': {'c': -128}}",
            "{'a': [], 'b': {'c': 127}}",
            "{'a': [NULL], 'b': {'c': NULL}}",
            "{'a': [-2147483648, 2147483647], 'b': {'c': -128}}",
            "{'a': [-2147483648, 2147483647], 'b': {'c': -128}}",
            "{'a': [-2147483648, 2147483647], 'b': {'c': -128}}",
            "{'a': [], 'b': {'c': 127}}",
            "{'a': [NULL], 'b': {'c': NULL}}",
            "{'a': [3, 5], 'b': {'c': 3}}",
            "{'a': [], 'b': {'c': 5}}",
            "{'a': [7], 'b': {'c': 7}}",
        ],
        "{sql}"
    );
    let sql = "SELECT * FROM test_vector_types(NULL::ROW(m MAP(INT, INT))) OFFSET 8";
    assert_eq!(
        answered(&database, sql),
        ["{'m': {3=3, 5=5}}", "{'m': {}}", "{'m': {7=7}}"],
        "{sql}"
    );
}

#[test]
fn a_map_column_leaves_the_sequence_out() {
    let database = Database::new();
    let sql = "SELECT * FROM test_vector_types(NULL::INT, NULL::MAP(INT, INT))";
    assert_eq!(
        answered(&database, sql),
        [
            "-2147483648|{-2147483648=-2147483648}",
            "2147483647|NULL",
            "NULL|{2147483647=2147483647}",
            "-2147483648|{-2147483648=-2147483648}",
            "-2147483648|{-2147483648=-2147483648}",
            "-2147483648|{-2147483648=-2147483648}",
            "2147483647|NULL",
            "NULL|{2147483647=2147483647}",
        ],
        "{sql}"
    );
    let sql = "SELECT string_agg(column_name || ' ' || column_type, ', ') \
               FROM (DESCRIBE SELECT * FROM test_vector_types(NULL::INT, 'a', NULL::INT[]))";
    assert_eq!(
        answered(&database, sql),
        ["test_vector INTEGER, test_vector2 VARCHAR, test_vector3 INTEGER[]"],
        "{sql}"
    );
}

#[test]
fn a_type_with_no_value_in_test_all_types_is_refused() {
    let database = Database::new();
    for (sql, expected) in [
        (
            "SELECT * FROM test_vector_types(NULL::DECIMAL(18,3))",
            "Not implemented Error: Unimplemented type for test_vector_types DECIMAL(18,3)",
        ),
        (
            "SELECT * FROM test_vector_types(NULL::ENUM('a', 'b'))",
            "Not implemented Error: Unimplemented type for test_vector_types ENUM('a', 'b')",
        ),
        (
            "SELECT * FROM test_vector_types(NULL::INT[2])",
            "Not implemented Error: Unimplemented type for test_vector_types INTEGER[2]",
        ),
        (
            "SELECT * FROM test_vector_types(NULL)",
            "Not implemented Error: Unimplemented type for test_vector_types \"NULL\"",
        ),
        (
            "SELECT * FROM test_vector_types(NULL::STRUCT(v VARIANT))",
            "Not implemented Error: Unimplemented type for test_vector_types",
        ),
        (
            "SELECT * FROM test_vector_types()",
            "Binder Error: No function matches the given name and argument types \
             'test_vector_types()'",
        ),
        (
            "SELECT * FROM test_vector_types(NULL::INT, foo := 1)",
            "Binder Error: Invalid named parameter \"foo\" for function test_vector_types",
        ),
        (
            "SELECT * FROM test_vector_types(NULL::INT, all_flat := NULL)",
            "Invalid Input Error: Cannot use NULL as argument for all_flat",
        ),
        (
            "SELECT * FROM test_vector_types((SELECT 1))",
            "Binder Error: Table function cannot contain subqueries",
        ),
    ] {
        refused(&database, sql, expected);
    }
}

//! `array_distance`, `array_inner_product`, `array_negative_inner_product`,
//! `array_cosine_similarity`, `array_cosine_distance`, the names the pin keeps for them, and
//! `array_cross_product`.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn rows(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect()
}

fn answered(sql: &str) -> String {
    rows(&Database::new(), sql).join("\n")
}

fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

#[test]
fn two_arrays_fold_into_the_number_the_pin_answers() {
    let cases = [
        (
            "SELECT array_distance([1,2]::FLOAT[2], [2,3]::FLOAT[2]), typeof(array_distance([1,2]::FLOAT[2], [2,3]::FLOAT[2]))",
            "1.4142135|FLOAT",
        ),
        (
            "SELECT array_distance([1,2]::FLOAT[2], [2,3]::DOUBLE[2]), typeof(array_distance([1,2]::FLOAT[2], [2,3]::DOUBLE[2]))",
            "1.4142135623730951|DOUBLE",
        ),
        ("SELECT array_distance([1,2]::INTEGER[2], [2,3]::DOUBLE[2])", "1.4142135623730951"),
        (
            "SELECT array_distance([1,2]::BIGINT[2], [2,3]::FLOAT[2]), typeof(array_distance([1,2]::BIGINT[2], [2,3]::FLOAT[2]))",
            "1.4142135|FLOAT",
        ),
        (
            "SELECT array_negative_inner_product([1,2]::DOUBLE[2], [2,3]::DOUBLE[2]), array_cosine_distance([1,2,3]::DOUBLE[3], [4,5,6]::DOUBLE[3])",
            "-8.0|0.025368153802923787",
        ),
        (
            "SELECT array_distance(NULL, [2,3]::DOUBLE[2]), typeof(array_distance(NULL, [2,3]::DOUBLE[2]))",
            "NULL|DOUBLE",
        ),
        (
            "SELECT array_inner_product([1,2]::FLOAT[2], [2,3]::FLOAT[2]), array_dot_product([1,2]::FLOAT[2], [2,3]::FLOAT[2]), array_negative_dot_product([1,2]::FLOAT[2], [2,3]::FLOAT[2])",
            "8.0|8.0|-8.0",
        ),
        (
            "SELECT array_cosine_similarity([0,0]::DOUBLE[2], [2,3]::DOUBLE[2]), array_cosine_distance([0,0]::DOUBLE[2], [2,3]::DOUBLE[2])",
            "-1.0|2.0",
        ),
        (
            "SELECT array_cosine_similarity([1,2]::FLOAT[2], [2,3]::FLOAT[2]), array_cosine_distance([1,2]::FLOAT[2], [2,3]::FLOAT[2])",
            "0.99227786|0.0077221394",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
}

#[test]
fn two_arrays_of_three_cross_into_an_array_of_three() {
    let cases = [
        (
            "SELECT array_cross_product([1,2,3]::DOUBLE[3], [3,2,1]::DOUBLE[3]), typeof(array_cross_product([1,2,3]::DOUBLE[3], [3,2,1]::DOUBLE[3]))",
            "[-4.0, 8.0, -4.0]|DOUBLE[3]",
        ),
        ("SELECT array_cross_product([1,2,3], [3,2,1])", "[-4.0, 8.0, -4.0]"),
        (
            "SELECT array_cross_product([1,2,3]::INTEGER[3], [3,2,1]::INTEGER[3]), typeof(array_cross_product([1,2,3]::INTEGER[3], [3,2,1]::INTEGER[3]))",
            "[-4.0, 8.0, -4.0]|DOUBLE[3]",
        ),
        (
            "SELECT array_cross_product([1,2,3]::FLOAT[3], [3,2,1]::DOUBLE[3]), typeof(array_cross_product([1,2,3]::FLOAT[3], [3,2,1]::DOUBLE[3]))",
            "[-4.0, 8.0, -4.0]|DOUBLE[3]",
        ),
        ("SELECT array_cross_product(NULL, [3,2,1]::DOUBLE[3])", "NULL"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
}

#[test]
fn a_column_of_arrays_is_checked_for_nulls_a_row_at_a_time() {
    let database = Database::new();
    assert_eq!(
        rows(
            &database,
            "SELECT array_distance(a, [1,1]::DOUBLE[2]) FROM (VALUES ([1,2]::DOUBLE[2]), (NULL), ([3,4]::DOUBLE[2])) t(a)"
        ),
        ["1.0", "NULL", "3.605551275463989"]
    );
    // A null element in a row whose other side is null is never looked at.
    assert_eq!(
        rows(
            &database,
            "SELECT array_distance(a, b) FROM (VALUES ([1,NULL]::DOUBLE[2], NULL), ([1,2]::DOUBLE[2], [1,2]::DOUBLE[2])) t(a, b)"
        ),
        ["NULL", "0.0"]
    );
    assert_eq!(
        rows(
            &database,
            "SELECT array_cross_product(a, [1,0,0]::FLOAT[3]), typeof(array_cross_product(a, [1,0,0]::FLOAT[3])) FROM (VALUES ([0,1,0]::FLOAT[3]), (NULL), ([0,0,1]::FLOAT[3])) t(a)"
        ),
        ["[0.0, 0.0, -1.0]|FLOAT[3]", "NULL|FLOAT[3]", "[0.0, 1.0, 0.0]|FLOAT[3]"]
    );
    assert_eq!(
        rows(
            &database,
            "SELECT array_inner_product(a, a) FROM (VALUES ([1.5,2.5]::FLOAT[2]), ([3,4]::FLOAT[2])) t(a)"
        ),
        ["8.5", "25.0"]
    );
    let error = database
        .query(
            "SELECT array_distance(a, [1,1]::DOUBLE[2]) FROM (VALUES (NULL), ([1,NULL]::DOUBLE[2])) t(a)",
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Invalid Input Error: array_distance: left argument can not contain NULL values"
    );
}

#[test]
fn what_the_pin_refuses_about_array_folds_is_refused_in_its_words() {
    let folds = "\n\tCandidate functions:\n\tarray_distance(col0 FLOAT[ANY], col1 FLOAT[ANY]) -> FLOAT\n\tarray_distance(col0 DOUBLE[ANY], col1 DOUBLE[ANY]) -> DOUBLE\n";
    let cases = [
        (
            "SELECT array_distance([1,2]::INTEGER[2], [2,3]::INTEGER[2])".to_owned(),
            "Binder Error: array_distance: Arguments must be arrays of FLOAT or DOUBLE".to_owned(),
        ),
        (
            "SELECT array_distance([1,2]::DECIMAL(4,1)[2], [2,3]::DECIMAL(4,1)[2])".to_owned(),
            "Binder Error: array_distance: Arguments must be arrays of FLOAT or DOUBLE".to_owned(),
        ),
        (
            "SELECT array_distance([1,2]::DOUBLE[2], [1,2,3]::DOUBLE[3])".to_owned(),
            "Binder Error: array_distance: Array arguments must be of the same size".to_owned(),
        ),
        (
            "SELECT array_distance([1,2], [2,3])".to_owned(),
            format!(
                "Binder Error: No function matches the given name and argument types 'array_distance(INTEGER[], INTEGER[])'. You might need to add explicit type casts.{folds}"
            ),
        ),
        (
            "SELECT array_distance([1,2]::DOUBLE[2])".to_owned(),
            format!(
                "Binder Error: No function matches the given name and argument types 'array_distance(DOUBLE[2])'. You might need to add explicit type casts.{folds}"
            ),
        ),
        (
            "SELECT array_distance(NULL, NULL)".to_owned(),
            "Binder Error: Could not choose a best candidate function for the function call \"array_distance(\"NULL\", \"NULL\")\". In order to select one, please add explicit type casts.\n\tCandidate functions:\n\tarray_distance(col0 DOUBLE[ANY], col1 DOUBLE[ANY]) -> DOUBLE\n\tarray_distance(col0 FLOAT[ANY], col1 FLOAT[ANY]) -> FLOAT\n".to_owned(),
        ),
        (
            "SELECT array_distance([1,NULL]::DOUBLE[2], [2,3]::DOUBLE[2])".to_owned(),
            "Invalid Input Error: array_distance: left argument can not contain NULL values"
                .to_owned(),
        ),
        (
            "SELECT array_distance([1,2]::DOUBLE[2], [NULL,3]::DOUBLE[2])".to_owned(),
            "Invalid Input Error: array_distance: right argument can not contain NULL values"
                .to_owned(),
        ),
        (
            "SELECT array_cross_product([1,NULL,3]::DOUBLE[3], [3,2,1]::DOUBLE[3])".to_owned(),
            "Invalid Input Error: array_cross_product: left argument can not contain NULL values"
                .to_owned(),
        ),
        (
            "SELECT array_cross_product([1,2], [3,4,5])".to_owned(),
            "Conversion Error: Cannot cast list with length 2 to array with length 3".to_owned(),
        ),
        (
            "SELECT array_cross_product(NULL, NULL)".to_owned(),
            "Binder Error: Could not choose a best candidate function for the function call \"array_cross_product(\"NULL\", \"NULL\")\". In order to select one, please add explicit type casts.\n\tCandidate functions:\n\tarray_cross_product(col0 DOUBLE[3], col1 DOUBLE[3]) -> DOUBLE[3]\n\tarray_cross_product(col0 FLOAT[3], col1 FLOAT[3]) -> FLOAT[3]\n".to_owned(),
        ),
        (
            "SELECT array_cross_product([1,2]::DOUBLE[2], [3,2]::DOUBLE[2])".to_owned(),
            "Binder Error: No function matches the given name and argument types 'array_cross_product(DOUBLE[2], DOUBLE[2])'. You might need to add explicit type casts.\n\tCandidate functions:\n\tarray_cross_product(col0 FLOAT[3], col1 FLOAT[3]) -> FLOAT[3]\n\tarray_cross_product(col0 DOUBLE[3], col1 DOUBLE[3]) -> DOUBLE[3]\n".to_owned(),
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(refused(&sql), expected, "{sql}");
    }
}

//! List comprehensions, `[x * 2 FOR x IN l IF x > 1]`, which the pin writes out as the lambda calls
//! they stand for and names after those calls.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

/// The name and the text of the one value a query answers.
fn answer(sql: &str) -> (String, String) {
    let database = Database::new();
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let name = result.names()[0].clone();
    let rows: Vec<String> = result.rows().map(|row| format!("{}", row[0])).collect();
    (name, rows.join(";"))
}

#[test]
fn a_comprehension_is_a_list_apply_over_a_lambda() {
    assert_eq!(
        answer("SELECT [x * 2 FOR x IN [1, 2, 3]]"),
        (
            "list_apply(list_value(1, 2, 3), (lambda x: (x * 2)))".to_string(),
            "[2, 4, 6]".to_string()
        )
    );
    assert_eq!(answer("SELECT [upper(s) FOR s IN ['a', NULL, 'c']]").1, "[A, NULL, C]");
    assert_eq!(answer("SELECT [[y FOR y IN x] FOR x IN [[1], [2, 3]]]").1, "[[1], [2, 3]]");
    assert_eq!(answer("SELECT [x FOR x IN NULL]").1, "NULL");
}

#[test]
fn a_comprehension_with_a_filter_keeps_the_elements_the_filter_holds_for() {
    assert_eq!(
        answer("SELECT [x FOR x IN [1, 2, 3] IF x > 1]"),
        (
            "list_apply(list_filter(list_apply(list_value(1, 2, 3), (lambda x: \
             struct_pack(\"filter\" := (x > 1), result := x))), (lambda elem: \
             struct_extract(elem, 'filter'))), (lambda elem: struct_extract(elem, 'result')))"
                .to_string(),
            "[2, 3]".to_string()
        )
    );
    assert_eq!(answer("SELECT [x FOR x, i IN [10, 20, 30] IF i > 1]").1, "[20, 30]");
    assert_eq!(answer("SELECT [elem FOR elem IN [1, 2, 3] IF elem > 1]").1, "[2, 3]");
    assert_eq!(answer("SELECT [x FOR x IN range(5) IF x % 2 = 0]").1, "[0, 2, 4]");
    assert_eq!(answer("SELECT [x FOR x IN [1, 2] IF NULL]").1, "[]");
}

#[test]
fn a_comprehension_sees_the_columns_of_its_row() {
    let database = Database::new();
    database
        .execute("CREATE TABLE integers AS SELECT 42 i UNION ALL SELECT 13")
        .expect("the table");
    let result = database
        .query("SELECT [x + i FOR x IN [1, 2, 3] IF x <> 2] FROM integers ORDER BY i")
        .expect("the comprehension");
    let rows: Vec<String> = result.rows().map(|row| format!("{}", row[0])).collect();
    assert_eq!(rows, ["[14, 16]", "[43, 45]"]);
    let refused = database.query("SELECT [x FOR x IN 1]").unwrap_err().to_string();
    assert!(refused.contains("Invalid LIST argument during lambda function binding!"), "{refused}");
}

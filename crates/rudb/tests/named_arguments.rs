//! Calls written with `name := value` arguments, which go to the places the function gives those
//! names.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn answered(sql: &str) -> (Vec<String>, Vec<String>) {
    let database = Database::new();
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let names = result.names().to_vec();
    let rows = result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join(","))
        .collect();
    (names, rows)
}

fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

#[test]
fn a_named_argument_goes_to_the_place_its_name_has() {
    let cases = [
        (
            "SELECT round(x := 1.234, precision := 2)",
            "round(x := 1.234, \"precision\" := 2)",
            "1.23",
        ),
        ("SELECT round(1.234, precision := 2)", "round(1.234, \"precision\" := 2)", "1.23"),
        (
            "SELECT round(precision := 2, x := 1.234)",
            "round(\"precision\" := 2, x := 1.234)",
            "1.23",
        ),
        ("SELECT ROUND(X := 1.5)", "round(X := 1.5)", "2"),
        (
            "SELECT list_sort(sort_order := 'DESC', list := [3,1,2])",
            "list_sort(sort_order := 'DESC', list := list_value(3, 1, 2))",
            "[3, 2, 1]",
        ),
        (
            "SELECT list_extract(list := [1,2], \"index\" := 2)",
            "list_extract(list := list_value(1, 2), \"index\" := 2)",
            "2",
        ),
        (
            "SELECT regexp_extract('abc', 'b', \"group\" := 0)",
            "regexp_extract('abc', 'b', \"group\" := 0)",
            "b",
        ),
        ("SELECT median(x := 3)", "median(x := 3)", "3.0"),
        (
            "SELECT string_agg(input := 'a', separator := '-')",
            "string_agg(\"input\" := 'a', separator := '-')",
            "a",
        ),
        (
            "SELECT quantile_cont(x := i, quantile := 0.5) FROM range(10) t(i)",
            "quantile_cont(x := i, quantile := 0.5)",
            "4.5",
        ),
        (
            "SELECT percentile_cont(quantile := 0.5) WITHIN GROUP (ORDER BY i) FROM range(10) t(i)",
            "quantile_cont(quantile := 0.5 ORDER BY i)",
            "4.5",
        ),
    ];
    for (sql, name, value) in cases {
        let (names, rows) = answered(sql);
        assert_eq!((names[0].as_str(), rows[0].as_str()), (name, value), "{sql}");
    }
}

#[test]
fn a_window_call_fills_a_skipped_parameter_with_its_default() {
    let (names, rows) =
        answered("SELECT lead(\"default\" := 1337, col := i) OVER (ORDER BY i) FROM range(3) t(i)");
    assert_eq!(names[0], "lead(\"default\" := 1337, col := i) OVER (ORDER BY i)");
    assert_eq!(rows, ["1", "2", "1337"]);
    let (_, rows) =
        answered("SELECT lead(col := i, \"offset\" := 2) OVER (ORDER BY i) FROM range(3) t(i)");
    assert_eq!(rows, ["2", "NULL", "NULL"]);
}

#[test]
fn names_that_fit_no_parameter_are_refused_in_the_pins_words() {
    let cases = [
        (
            "SELECT round(1.234, x := 2)",
            "Binder Error: Named argument '2' cannot be used for parameter '\"x\"' because it has already been provided as a positional argument in function call to '\"round\"'",
        ),
        (
            "SELECT lead(i, col := 2) OVER (ORDER BY i) FROM range(3) t(i)",
            "Named argument '2' cannot be used for parameter '\"col\"' because it has already been provided as a positional argument in function call to '\"lead\"'",
        ),
        (
            "SELECT round(x := 1.234, x := 2)",
            "Binder Error: Duplicate named argument \"x\" in function call to '\"round\"'",
        ),
        (
            "SELECT round(y := 1.234)",
            "No function matches the given name and argument types 'round(\"y\" := DECIMAL(4,3))'",
        ),
        (
            "SELECT lower(x := 'A')",
            "No function matches the given name and argument types 'lower(\"x\" := STRING_LITERAL)'",
        ),
        (
            "SELECT lower(x := i) FROM range(1) t(i)",
            "No function matches the given name and argument types 'lower(\"x\" := BIGINT)'",
        ),
        (
            "SELECT sum(x := i) FROM range(3) t(i)",
            "No function matches the given name and argument types 'sum(\"x\" := BIGINT)'",
        ),
        (
            "SELECT lead(foo := 2) OVER (ORDER BY i) FROM range(3) t(i)",
            "No function matches the given name and argument types 'lead(\"foo\" := INTEGER_LITERAL)'",
        ),
        (
            "SELECT percentile_cont(frac := 0.5) WITHIN GROUP (ORDER BY i) FROM range(10) t(i)",
            "No function matches the given name and argument types 'quantile_cont(BIGINT, \"frac\" := DECIMAL(2,1))'",
        ),
        (
            "SELECT strftime(data := DATE '2020-01-01', format := '%Y')",
            "Could not choose a best candidate function for the function call \"strftime(\"data\" := DATE, \"format\" := STRING_LITERAL)\"",
        ),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

#[test]
fn a_name_on_a_function_that_takes_any_number_of_arguments_is_one_more_of_them() {
    let cases = [
        ("SELECT concat('a', x := 'b')", "concat('a', x := 'b')", "ab"),
        ("SELECT greatest(1, x := 2)", "greatest(1, x := 2)", "2"),
        ("SELECT list_value(1, x := 2, y := 3)", "list_value(1, x := 2, y := 3)", "[1, 2, 3]"),
        (
            "SELECT list_value(true, recursive := true)",
            "list_value(true, \"recursive\" := true)",
            "[true, true]",
        ),
    ];
    for (sql, name, value) in cases {
        let (names, rows) = answered(sql);
        assert_eq!((names[0].as_str(), rows[0].as_str()), (name, value), "{sql}");
    }
    let cases = [
        (
            "SELECT list_value(x := 1, y := 2)",
            "Binder Error: Missing value for parameter \"col0\" in function call to \"list_value\"",
        ),
        (
            "SELECT concat_ws('-', x := 'a')",
            "Missing value for parameter \"col1\" in function call to \"concat_ws\"",
        ),
        (
            "SELECT hash(x := 1)",
            "Missing value for parameter \"col0\" in function call to \"hash\"",
        ),
        (
            "SELECT md5(x := 'a')",
            "No function matches the given name and argument types 'md5(\"x\" := STRING_LITERAL)'",
        ),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

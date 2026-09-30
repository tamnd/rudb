//! `list_zip` and `array_zip`, the struct searches with their spellings `struct_has` and
//! `struct_indexof`, and `struct_extract_at`.
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
fn lists_are_zipped_into_unnamed_structs() {
    let cases = [
        (
            "SELECT list_zip([1,2,3], ['a','b']), typeof(list_zip([1,2,3], ['a','b']))",
            "[(1, a), (2, b), (3, NULL)]|TUPLE(INTEGER, VARCHAR)[]",
        ),
        (
            "SELECT list_zip([1,2,3], ['a','b'], true), list_zip([1,2], [], true), list_zip([1,2], [])",
            "[(1, a), (2, b)]|[]|[(1, NULL), (2, NULL)]",
        ),
        ("SELECT list_zip([1,2]), list_zip([true, false])", "[(1,), (2,)]|[(true,), (false,)]"),
        (
            "SELECT list_zip(NULL, [1]), list_zip([1], NULL), list_zip(NULL, NULL), list_zip(NULL, true)",
            "[(NULL, 1)]|[(1, NULL)]|[]|[]",
        ),
        (
            "SELECT typeof(list_zip(NULL, [1])), typeof(list_zip([1,2], NULL::INT[], [[1]]))",
            "TUPLE(\"NULL\", INTEGER)[]|TUPLE(INTEGER, INTEGER, INTEGER[])[]",
        ),
        (
            "SELECT list_zip([1,2], [3], NULL), list_zip([1], NULL::BOOLEAN), list_zip([1,NULL], [NULL, 2])",
            "[(1, 3, NULL), (2, NULL, NULL)]|[(1,)]|[(1, NULL), (NULL, 2)]",
        ),
        (
            "SELECT list_zip([1],[2],[3],[4],[5],[6],[7],[8],[9],[10],[11])",
            "[(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11)]",
        ),
        (
            "SELECT array_zip([1,2], [3,4]), list_zip([1], b := [2]), list_zip(a := [1], b := ['x'], c := true)",
            "[(1, 3), (2, 4)]|[(1, 2)]|[(1, x)]",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
    let database = Database::new();
    assert_eq!(
        rows(&database, "SELECT list_zip(l, [1,2]) FROM (VALUES ([10,20,30]), (NULL), ([])) t(l)"),
        ["[(10, 1), (20, 2), (30, NULL)]", "[(NULL, 1), (NULL, 2)]", "[(NULL, 1), (NULL, 2)]"]
    );
    // A null flag cuts nothing, the same as false.
    assert_eq!(
        rows(
            &database,
            "SELECT list_zip(l, [1,2], b) FROM (VALUES ([10,20,30], true), ([5], false), ([5], NULL)) t(l, b)"
        ),
        ["[(10, 1), (20, 2)]", "[(5, 1), (NULL, 2)]", "[(5, 1), (NULL, 2)]"]
    );
}

#[test]
fn a_zip_without_lists_is_refused_in_the_pins_words() {
    let cases = [
        ("SELECT list_zip()", "Binder Error: Provide at least one argument to list_zip"),
        ("SELECT array_zip()", "Binder Error: Provide at least one argument to array_zip"),
        ("SELECT list_zip(true)", "Binder Error: Provide at least one list argument to list_zip"),
        ("SELECT list_zip([1], 1)", "Binder Error: Parameter type needs to be List"),
        ("SELECT list_zip([1], [2], 'x')", "Binder Error: Parameter type needs to be List"),
        ("SELECT list_zip([1], [2], true, false)", "Binder Error: Parameter type needs to be List"),
    ];
    for (sql, expected) in cases {
        assert_eq!(refused(sql), expected, "{sql}");
    }
}

#[test]
fn a_struct_is_searched_the_way_the_pin_searches_it() {
    let cases = [
        (
            "SELECT struct_has(ROW(1,2,3), 2), struct_indexof(ROW(1,2,3), 3), struct_has(ROW(1,2,3), 9), struct_indexof(ROW(1,2,3), 9)",
            "true|3|false|NULL",
        ),
        // The position is the last match, and a null is found when it is what is looked for.
        (
            "SELECT struct_has(ROW(1,NULL), NULL), struct_indexof(ROW(1,NULL), NULL), struct_contains(ROW(1,NULL), NULL), struct_position(ROW(1,NULL), NULL)",
            "NULL|2|NULL|2",
        ),
        (
            "SELECT struct_indexof(ROW(1,'a',1), 1), struct_indexof(ROW(NULL, NULL), NULL), struct_position(ROW(2,2), 2)",
            "3|2|2",
        ),
        (
            "SELECT struct_has(NULL, 1), typeof(struct_has(NULL, 1)), struct_indexof(NULL, 1), typeof(struct_indexof(NULL, 1))",
            "NULL|BOOLEAN|NULL|\"NULL\"",
        ),
        (
            "SELECT struct_contains({}, 1), struct_position({}, 1), struct_position({}, NULL), struct_contains({}, NULL)",
            "false|NULL|NULL|NULL",
        ),
        (
            "SELECT struct_has(ROW(1,'a'), 'a'), struct_indexof(ROW(1,'a'), 'a'), struct_has(ROW(1,2), 2.0), struct_indexof(ROW(1, 2.5), 2.5)",
            "true|2|true|2",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
    assert_eq!(
        rows(
            &Database::new(),
            "SELECT struct_has(s, 2), struct_indexof(s, 2), struct_position(s, NULL) FROM (VALUES (ROW(1,2)), (NULL), (ROW(NULL,2))) t(s)"
        ),
        ["true|2|NULL", "NULL|NULL|NULL", "true|2|1"]
    );
}

#[test]
fn a_field_is_picked_by_its_place() {
    let cases = [
        (
            "SELECT struct_extract_at({'a':1,'b':'x'}, 2), struct_extract_at(ROW(1,'x'), 1), struct_extract_at({'a': {'b': 5}}, 1)",
            "x|1|{'b': 5}",
        ),
        (
            "SELECT struct_extract_at({'a':1}, NULL), struct_extract_at(NULL, 1), typeof(struct_extract_at({'a':1}, NULL)), typeof(struct_extract_at(NULL, 1))",
            "NULL|NULL|\"NULL\"|\"NULL\"",
        ),
        ("SELECT struct_extract_at(ROW(1,2), 1::TINYINT), struct_extract_at(ROW(1,2), '2')", "1|2"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
    assert_eq!(
        rows(
            &Database::new(),
            "SELECT struct_extract_at(s, 2) FROM (VALUES ({'a': 1, 'b': 2}), (NULL)) t(s)"
        ),
        ["2", "NULL"]
    );
}

#[test]
fn what_the_pin_refuses_about_structs_is_refused_in_its_words() {
    let cases = [
        (
            "SELECT struct_has({'a':1}, 1)",
            "Binder Error: \"struct_has\" can only be used on unnamed structs",
        ),
        (
            "SELECT struct_indexof({'a':1}, 1)",
            "Binder Error: \"struct_indexof\" can only be used on unnamed structs",
        ),
        (
            "SELECT struct_has()",
            "Binder Error: No function matches the given name and argument types 'struct_has()'. You might need to add explicit type casts.\n\tCandidate functions:\n\tstruct_has(col0 TUPLE, col1 ANY) -> BOOLEAN\n",
        ),
        (
            "SELECT struct_extract_at({'a':1}, 0)",
            "Binder Error: Key index 0 for struct_extract out of range - expected an index between 1 and 1",
        ),
        (
            "SELECT struct_extract_at({'a':1}, -1)",
            "Binder Error: Key index -1 for struct_extract out of range - expected an index between 1 and 1",
        ),
        (
            "SELECT struct_extract_at({'a':1}, i) FROM range(1,2) t(i)",
            "Binder Error: The \"index\" argument in function \"struct_extract_at\" must be a constant expression",
        ),
        (
            "SELECT struct_extract_at(ROW(1,2), 'x')",
            "Invalid Input Error: Could not convert string 'x' to INT64",
        ),
        (
            "SELECT struct_extract_at(ROW(1,2), 1.7)",
            "Binder Error: No function matches the given name and argument types 'struct_extract_at(TUPLE(INTEGER, INTEGER), DECIMAL(2,1))'. You might need to add explicit type casts.\n\tCandidate functions:\n\tstruct_extract_at(\"struct\" STRUCT, \"index\" BIGINT) -> ANY\n",
        ),
        (
            "SELECT struct_extract_at(ROW(1,2), 2::HUGEINT)",
            "Binder Error: No function matches the given name and argument types 'struct_extract_at(TUPLE(INTEGER, INTEGER), HUGEINT)'. You might need to add explicit type casts.\n\tCandidate functions:\n\tstruct_extract_at(\"struct\" STRUCT, \"index\" BIGINT) -> ANY\n",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(refused(sql), expected, "{sql}");
    }
}

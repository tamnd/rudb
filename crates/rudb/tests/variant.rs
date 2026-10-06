//! `VARIANT`, the type that holds a value of any type along with the type it has. Every expected
//! answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

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

fn refused(database: &Database, sql: &str, expected: &str) {
    let error = database.execute(sql).expect_err(sql);
    assert!(error.to_string().starts_with(expected), "{sql}: {error}");
}

#[test]
fn a_variant_names_the_type_it_holds() {
    let database = Database::new();
    for (sql, expected) in [
        ("SELECT 1::VARIANT, typeof(1::VARIANT), variant_typeof(1::VARIANT)", "1|VARIANT|INT32"),
        ("SELECT variant_typeof(1.50::VARIANT)", "DECIMAL(3, 2)"),
        ("SELECT variant_typeof([1, 2]::VARIANT)", "ARRAY(2)"),
        ("SELECT variant_typeof({'a': 1, 'b': 'x'}::VARIANT)", "OBJECT(a, b)"),
        ("SELECT variant_typeof(NULL)", "VARIANT_NULL"),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
}

#[test]
fn casts_out_are_strict() {
    let database = Database::new();
    for (sql, expected) in [
        ("SELECT 'abc'::VARIANT::VARCHAR, '12'::VARIANT::INTEGER", "abc|12"),
        ("SELECT 't'::VARIANT::BOOLEAN, [1, 2]::VARIANT::VARCHAR", "true|[1, 2]"),
        ("SELECT [1, 2, 3]::VARIANT::INTEGER[]", "[1, 2, 3]"),
        ("SELECT {'a': 1, 'b': 2}::VARIANT::STRUCT(a BIGINT, b VARCHAR)", "{'a': 1, 'b': 2}"),
        ("SELECT TRY_CAST('abc'::VARIANT AS INTEGER)", "NULL"),
        ("SELECT TRY_CAST(1::VARIANT AS ENUM('a', 'b'))", "NULL"),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
    for (sql, expected) in [
        (
            "SELECT 'abc'::VARIANT::INTEGER",
            "Conversion Error: Can't convert VARIANT(VARCHAR) value 'abc' to 'INTEGER'",
        ),
        (
            "SELECT '1.5'::VARIANT::INTEGER",
            "Conversion Error: Can't convert VARIANT(VARCHAR) value '1.5' to 'INTEGER'",
        ),
        (
            "SELECT '2000-01-01 12:00:00'::VARIANT::DATE",
            "Conversion Error: Can't convert VARIANT(VARCHAR) value '2000-01-01 12:00:00' to 'DATE'",
        ),
        (
            "SELECT ['a', 'b']::VARIANT::INTEGER[]",
            "Conversion Error: Can't convert VARIANT(VARCHAR) value 'b' to 'INTEGER[]'",
        ),
        (
            "SELECT 1::VARIANT::INTEGER[]",
            "Conversion Error: Expected to find VARIANT(ARRAY), found VARIANT(INT32) instead, \
             can't convert to 'INTEGER[]'",
        ),
        (
            "SELECT [1, 2]::VARIANT::INTEGER[3]",
            "Conversion Error: Array size '3' was expected, found '2', can't convert VARIANT to \
             'INTEGER[3]'",
        ),
        (
            "SELECT {'a': 1}::VARIANT::STRUCT(b INTEGER)",
            "Conversion Error: VARIANT(OBJECT(a)) is missing key 'b' to 'STRUCT(b INTEGER)'",
        ),
        (
            "SELECT 1::VARIANT::ENUM('a', 'b')",
            "Conversion Error: Unimplemented type for cast (VARIANT -> ENUM('a', 'b'))",
        ),
    ] {
        refused(&database, sql, expected);
    }
}

#[test]
fn extraction_reads_keys_and_positions() {
    let database = Database::new();
    for (sql, expected) in [
        ("SELECT variant_extract({'a': 1, 'b': [5, 6]}::VARIANT, 'b')", "[5, 6]"),
        (
            "SELECT variant_extract([5, 6]::VARIANT, 2), variant_extract([5, 6]::VARIANT, 3)",
            "6|NULL",
        ),
        ("SELECT ({'a': {'b': 7}}::VARIANT).a.b, ([10, 20]::VARIANT)[1]", "7|10"),
        ("SELECT ({'a': 1}::VARIANT)['a']", "1"),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
    refused(
        &database,
        "SELECT ([1, 2]::VARIANT)[0]",
        "Binder Error: Extracting index 0 from VARIANT(ARRAY) is invalid, indexes are 1-based",
    );
    refused(
        &database,
        "SELECT ([1, 2]::VARIANT)[-1]",
        "Invalid Input Error: Failed to cast value: Type INT32 with value -1 can't be cast because \
         the value is out of range for the destination type UINT32",
    );
}

#[test]
fn json_goes_in_and_out() {
    let database = Database::new();
    for (sql, expected) in [
        ("SELECT '{\"a\": [1, 2.5, \"x\", null]}'::JSON::VARIANT", "{'a': [1, 2.5, x, NULL]}"),
        ("SELECT variant_typeof('{\"a\": 1}'::JSON::VARIANT)", "OBJECT(a)"),
        ("SELECT 'null'::JSON::VARIANT IS NULL", "true"),
        ("SELECT 1.50::VARIANT::JSON, {'a': [1.50, 2]}::VARIANT::JSON", "1.50|{\"a\":[1.50,2.00]}"),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
}

#[test]
fn functions_read_a_variant_as_the_cheapest_type() {
    let database = Database::new();
    for (sql, expected) in [
        (
            "SELECT typeof(coalesce(NULL::VARIANT, 1)), typeof(greatest(1::VARIANT, 2))",
            "VARIANT|VARIANT",
        ),
        ("SELECT least(1::VARIANT, 'a')", "1"),
        ("SELECT 1::VARIANT + 2, typeof(1::VARIANT + 2), upper('a'::VARIANT)", "3|INTEGER|A"),
        (
            "SELECT sum(v), typeof(sum(v)), avg(v) FROM (VALUES (1::VARIANT), (2::VARIANT)) t(v)",
            "3|HUGEINT|1.5",
        ),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
    refused(
        &database,
        "SELECT length('abc'::VARIANT)",
        "Conversion Error: Can't convert VARIANT(VARCHAR) value 'abc' to 'BIT'",
    );
}

#[test]
fn values_compare_and_group_by_what_they_hold() {
    let database = Database::new();
    assert_eq!(
        answered(
            &database,
            "SELECT v, count(*) FROM (VALUES (1::VARIANT), (1.0::VARIANT), ('x'::VARIANT)) t(v) \
             GROUP BY v",
        ),
        ["1|2", "x|1"]
    );
    for (sql, expected) in [
        (
            "SELECT count(*) FROM (VALUES (1::VARIANT)) a(v) JOIN (VALUES (1.0::VARIANT)) b(w) \
             ON v = w",
            "1",
        ),
        ("SELECT 1::VARIANT = 1.0::VARIANT, 1::VARIANT < 'a'::VARIANT", "true|true"),
        ("SELECT hash(1::VARIANT) = hash(1.0::VARIANT)", "false"),
        (
            "SELECT list(v ORDER BY v) FROM (VALUES (2::VARIANT), (1.0::VARIANT), ('a'::VARIANT), \
             (true::VARIANT)) t(v)",
            "[true, 1.0, 2, a]",
        ),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
}

#[test]
fn a_path_is_one_key() {
    let database = Database::new();
    for (sql, expected) in [
        ("SELECT variant_keys({'a': {'c': 1}}::VARIANT, ['a'])", "[[c]]"),
        ("SELECT variant_keys('a'), variant_keys(NULL)", "[]|NULL"),
        (
            "SELECT variant_type({'a': 1.5::DOUBLE}::VARIANT, ['a', 'b', ''])",
            "[DOUBLE, NULL, OBJECT]",
        ),
        ("SELECT variant_type(1.5::DECIMAL(10, 2)::VARIANT), variant_type('a')", "DECIMAL|VARCHAR"),
        ("SELECT variant_type({'a': NULL}::VARIANT, 'a')", "VARIANT_NULL"),
        (
            "SELECT variant_exists({'a': NULL}::VARIANT, 'a'), variant_exists({'a': 1}::VARIANT, \
             ['a', 'b', ''])",
            "true|[true, false, true]",
        ),
        (
            "SELECT variant_array_length({'a': [1, 2, 3]}::VARIANT, 'a'), \
             variant_array_length({'a': 1}::VARIANT), variant_array_length({'a': 1}::VARIANT, 'b')",
            "3|0|NULL",
        ),
        ("SELECT variant_array_length([1, 2]::VARIANT, ['', 'x'])", "[2, NULL]"),
        (
            "SELECT variant_extract_string({'a': 1.50}::VARIANT, 'a'), \
             variant_extract_string({'a': {'b': 'x'}}::VARIANT, 'a')",
            "1.50|{\"b\":\"x\"}",
        ),
        ("SELECT variant_extract_string({'a': NULL}::VARIANT, '')", "{\"a\":null}"),
        ("SELECT variant_extract_string({'a': 1, 'b': 2}::VARIANT, ['a', 'b'])", "[1, 2]"),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
    refused(
        &database,
        "SELECT variant_keys(1::VARIANT, ['a', NULL])",
        "Binder Error: 'variant_keys' does not accept NULL paths",
    );
    refused(
        &database,
        "SELECT variant_type(1::VARIANT, p) FROM (VALUES (NULL::VARCHAR)) t(p)",
        "Binder Error: The \"path\" argument in function \"variant_type\" must be a constant \
         expression",
    );
    refused(
        &database,
        "SELECT variant_keys(x) FROM (VALUES ('abc')) t(x)",
        "Binder Error: No function matches the given name and argument types \
         'variant_keys(VARCHAR)'",
    );
}

#[test]
fn contains_looks_through_values() {
    let database = Database::new();
    for (sql, expected) in [
        ("SELECT variant_contains([1, 2]::VARIANT, [2, 1, 1]::VARIANT)", "true"),
        (
            "SELECT variant_contains('2000-01-01'::DATE::VARIANT, \
             '2000-01-01 00:00:00'::TIMESTAMP::VARIANT)",
            "true",
        ),
        ("SELECT variant_contains('a', 'a'), variant_contains(1::VARIANT, 'a')", "true|false"),
        ("SELECT variant_contains(1::VARIANT, NULL)", "NULL"),
        ("SELECT variant_normalize({'b': 1, 'a': 2}::VARIANT)", "{'a': 2, 'b': 1}"),
        ("SELECT variant_group_array(x) FROM (VALUES (1), (NULL), (3)) t(x)", "[1, NULL, 3]"),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
}

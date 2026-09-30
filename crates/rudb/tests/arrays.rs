//! `ARRAY`, the list whose length is part of its type: the casts into it and out of it,
//! `array_value`, the bounds on its size, and what it meets a list at.
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
fn a_list_and_a_string_cast_into_an_array_of_their_length() {
    let cases = [
        ("SELECT [1,2]::INTEGER[2], typeof([1,2]::INTEGER[2])", "[1, 2]|INTEGER[2]"),
        (
            "SELECT [1,NULL]::INTEGER[2], NULL::INTEGER[2], typeof(NULL::INTEGER[2])",
            "[1, NULL]|NULL|INTEGER[2]",
        ),
        ("SELECT '[1,2]'::INTEGER[2], typeof('[1,2]'::INTEGER[2])", "[1, 2]|INTEGER[2]"),
        ("SELECT TRY_CAST([1,2,3] AS INTEGER[2]), TRY_CAST('[1,2,3]' AS INTEGER[2])", "NULL|NULL"),
        ("SELECT TRY_CAST(1 AS INTEGER[2])", "NULL"),
        (
            "SELECT [[1,2],[3,4]]::INTEGER[2][2], typeof([[1,2],[3,4]]::INTEGER[2][2])",
            "[[1, 2], [3, 4]]|INTEGER[2][2]",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
}

#[test]
fn an_array_casts_out_to_a_list_a_string_and_an_array_of_its_size() {
    let cases = [
        (
            "SELECT ([1,2]::INTEGER[2])::VARCHAR, ([1,2]::INTEGER[2])::INTEGER[], typeof(([1,2]::INTEGER[2])::BIGINT[])",
            "[1, 2]|[1, 2]|BIGINT[]",
        ),
        (
            "SELECT ([1,2]::INTEGER[2])::BIGINT[2], ([1,2]::INTEGER[2])::VARCHAR[2], typeof(([1,2]::INTEGER[2])::VARCHAR[2])",
            "[1, 2]|[1, 2]|VARCHAR[2]",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
}

#[test]
fn array_value_builds_an_array_of_its_arguments() {
    let cases = [
        (
            "SELECT array_value(1,2,3), typeof(array_value(1,2,3)), typeof(array_value(1, 2.5))",
            "[1, 2, 3]|INTEGER[3]|DECIMAL(11,1)[2]",
        ),
        (
            "SELECT array_value('a','b'), array_value([1],[2,3]), typeof(array_value([1],[2,3]))",
            "[a, b]|[[1], [2, 3]]|INTEGER[][2]",
        ),
        (
            "SELECT array_value(NULL), typeof(array_value(NULL)), array_value(1, NULL)",
            "[NULL]|\"NULL\"[1]|[1, NULL]",
        ),
        (
            "SELECT array_value(array_value(1,2), array_value(3,4)), typeof(array_value(array_value(1,2), array_value(3,4)))",
            "[[1, 2], [3, 4]]|INTEGER[2][2]",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
    assert_eq!(
        rows(&Database::new(), "SELECT array_value(i, i*2) FROM range(3) t(i)"),
        ["[0, 0]", "[1, 2]", "[2, 4]"]
    );
}

#[test]
fn an_array_is_read_as_the_list_it_is_held_as() {
    let cases = [
        (
            "SELECT array_length(array_value(1,2,3)), len(array_value(1,2)), length([1,2]::INTEGER[2])",
            "3|2|2",
        ),
        ("SELECT array_value(1,2)[1], array_value(1,2)[2], array_value(1,2)[3]", "1|2|NULL"),
        (
            "SELECT list_sum(array_value(1,2)), list_contains(array_value(1,2), 2), typeof(list_reverse(array_value(1,2)))",
            "3|true|INTEGER[]",
        ),
        ("SELECT array_value(1,2) IS NULL, hash([1,2]::INTEGER[2]) = hash([1,2])", "false|true"),
        (
            "SELECT list_transform(array_value(1,2), lambda x: x + 1), array_value(1,2) || [3]",
            "[2, 3]|[1, 2, 3]",
        ),
        (
            "SELECT array_value(1,2) || array_value(3,4), typeof(array_value(1,2) || array_value(3,4))",
            "[1, 2, 3, 4]|INTEGER[]",
        ),
        (
            "SELECT list_concat(array_value(1,2), [3]), typeof(list_concat(array_value(1,2), array_value(3)))",
            "[1, 2, 3]|INTEGER[]",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
    assert_eq!(rows(&Database::new(), "SELECT unnest(array_value(1,2))"), ["1", "2"]);
}

#[test]
fn an_array_meets_a_list_and_another_array_where_the_pin_meets_them() {
    let cases = [
        (
            "SELECT array_value(1,2) = array_value(1,2), array_value(1,2) < array_value(1,3), array_value(1,2) = [1,2]",
            "true|true|true",
        ),
        (
            "SELECT typeof(array_value(1,2) || [3]), typeof(CASE WHEN true THEN array_value(1,2) ELSE [1] END)",
            "INTEGER[]|INTEGER[2]",
        ),
        (
            "SELECT typeof(CASE WHEN true THEN array_value(1,2) ELSE array_value(1,2,3) END)",
            "INTEGER[3]",
        ),
        ("SELECT typeof(coalesce(array_value(1,2), array_value(1.5, 2)))", "DECIMAL(11,1)[2]"),
        ("SELECT [1,2]::INTEGER[2] IN ([1,2], [3,4])", "true"),
        ("SELECT typeof([array_value(1,2), [3]])", "INTEGER[2][]"),
        ("SELECT CAST([1,2,3] AS INTEGER[3]) = CAST([1,2,3] AS BIGINT[3])", "true"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
}

#[test]
fn an_array_column_is_stored_grouped_and_sorted() {
    let database = Database::new();
    database.execute("CREATE TABLE a AS SELECT array_value(i, i+1) x FROM range(3) t(i)").unwrap();
    assert_eq!(rows(&database, "SELECT x, x[2] FROM a"), ["[0, 1]|1", "[1, 2]|2", "[2, 3]|3"]);
    database.execute("CREATE TABLE t (a INTEGER[3])").unwrap();
    database.execute("INSERT INTO t VALUES ([1,2,3]), (NULL), ([4,5,6])").unwrap();
    assert_eq!(
        rows(&database, "SELECT a, a[1], typeof(a) FROM t"),
        ["[1, 2, 3]|1|INTEGER[3]", "NULL|NULL|INTEGER[3]", "[4, 5, 6]|4|INTEGER[3]"]
    );
    assert_eq!(
        rows(
            &database,
            "SELECT count(*), max(x) FROM (SELECT array_value(i, 1) x FROM range(5) t(i))"
        ),
        ["5|[4, 1]"]
    );
    assert_eq!(
        rows(
            &database,
            "SELECT x FROM (VALUES (array_value(2,1)), (array_value(1,2))) t(x) ORDER BY x"
        ),
        ["[1, 2]", "[2, 1]"]
    );
    assert_eq!(
        rows(
            &database,
            "SELECT DISTINCT x FROM (VALUES (array_value(1,2)), (array_value(1,2))) t(x)"
        ),
        ["[1, 2]"]
    );
}

#[test]
fn what_the_pin_refuses_about_arrays_is_refused_in_its_words() {
    let cases = [
        (
            "SELECT [1,2,3]::INTEGER[2]",
            "Conversion Error: Cannot cast list with length 3 to array with length 2",
        ),
        ("SELECT ['a']::INTEGER[1]", "Conversion Error: Could not convert string 'a' to INT32"),
        (
            "SELECT '[1,2,3]'::INTEGER[2]",
            "Conversion Error: Type VARCHAR with value '[1,2,3]' can't be cast to the destination type INTEGER[2], the size of the array must match the destination type",
        ),
        (
            "SELECT 'x'::INTEGER[2]",
            "Conversion Error: Type VARCHAR with value 'x' can't be cast to the destination type INTEGER[2], the size of the array must match the destination type",
        ),
        (
            "SELECT ([1,2]::INTEGER[2])::INTEGER[3]",
            "Conversion Error: Cannot cast array of size 2 to array of size 3",
        ),
        (
            "SELECT array_value(1,2) = array_value(1,2,3)",
            "Conversion Error: Cannot cast array of size 2 to array of size 3",
        ),
        (
            "SELECT 1::INTEGER[2]",
            "Conversion Error: Unimplemented type for cast (INTEGER -> INTEGER[2])",
        ),
        (
            "SELECT {'a': 1}::INTEGER[1]",
            "Conversion Error: Unimplemented type for cast (STRUCT(a INTEGER) -> INTEGER[1])",
        ),
        ("SELECT array_value()", "Invalid Input Error: array_value requires at least one argument"),
        ("SELECT [1]::INTEGER[0]", "Binder Error: ARRAY type size must be at least 1"),
        ("SELECT [1]::INTEGER[100001]", "Binder Error: ARRAY type size must be at most 100000"),
    ];
    for (sql, expected) in cases {
        assert_eq!(refused(sql), expected, "{sql}");
    }
}

//! `min(x, n)` and `max(x, n)`, which answer the `n` least or greatest values as a list, and the
//! `ORDER BY` an `arg_min` or `arg_max` call is written with, which decides which of two tied rows
//! it keeps. Every expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect()
}

#[test]
fn the_values_come_best_first_with_nulls_left_out() {
    let database = Database::new();
    for (sql, expected) in [
        (
            "SELECT max(x, 3), min(x, 3), typeof(max(x, 3)) FROM range(10) t(x)",
            vec!["[9, 8, 7]|[0, 1, 2]|BIGINT[]"],
        ),
        ("SELECT max(x, 3) FROM (VALUES (1), (NULL), (3), (2), (3)) t(x)", vec!["[3, 3, 2]"]),
        ("SELECT max(x, 3) FROM range(0) t(x)", vec!["NULL"]),
        ("SELECT max(x, 3) FROM (VALUES (NULL::INT)) t(x)", vec!["NULL"]),
        (
            "SELECT min(s, 2), max(s, 5) FROM (VALUES ('b'), ('a'), ('c')) t(s)",
            vec!["[a, b]|[c, b, a]"],
        ),
        ("SELECT max(x, 2) FROM (VALUES ([1, 2]), ([3]), (NULL)) t(x)", vec!["[[3], [1, 2]]"]),
        ("SELECT max({'a': x}, 2) FROM range(3) t(x)", vec!["[{'a': 2}, {'a': 1}]"]),
        (
            "SELECT max(x, 3) FROM (VALUES (1.5), (2.5), ('nan'::DOUBLE), ('-inf'::DOUBLE)) t(x)",
            vec!["[nan, 2.5, 1.5]"],
        ),
        ("SELECT max(x, '2') FROM range(5) t(x)", vec!["[4, 3]"]),
        (
            "SELECT max(DISTINCT x, 2), max(x, 2) FILTER (WHERE x < 3) FROM (VALUES (1), (2), (2), (5)) t(x)",
            vec!["[5, 2]|[2, 2]"],
        ),
        (
            "SELECT g, max(x, 2) FROM (VALUES (1, 1), (1, 3), (2, 7), (1, 2)) t(g, x) GROUP BY g ORDER BY g",
            vec!["1|[3, 2]", "2|[7]"],
        ),
        (
            "SELECT max(x, 2) OVER (ORDER BY x) FROM (VALUES (1), (2), (5)) t(x)",
            vec!["[1]", "[2, 1]", "[5, 2]"],
        ),
    ] {
        assert_eq!(answered(&database, sql), expected, "{sql}");
    }
}

#[test]
fn a_bad_count_is_refused_in_the_pins_words() {
    let database = Database::new();
    for (sql, expected) in [
        (
            "SELECT max(x, x) FROM range(5) t(x)",
            "Invalid Input Error: Invalid input for MIN/MAX: n value must be > 0",
        ),
        (
            "SELECT min(x, 0) FROM range(5) t(x)",
            "Invalid Input Error: Invalid input for MIN/MAX: n value must be > 0",
        ),
        (
            "SELECT min(x, NULL) FROM range(5) t(x)",
            "Invalid Input Error: Invalid input for MIN/MAX: n value cannot be NULL",
        ),
        (
            "SELECT max(x, 1000000) FROM range(5) t(x)",
            "Invalid Input Error: Invalid input for MIN/MAX: n value must be < 1000000",
        ),
        (
            "SELECT max(x, 'a') FROM range(5) t(x)",
            "Conversion Error: Could not convert string 'a' to INT64",
        ),
    ] {
        let error = database.query(sql).expect_err(sql);
        assert_eq!(error.to_string(), expected, "{sql}");
    }
    for (sql, call) in [
        ("SELECT max(x, 2.7) FROM range(5) t(x)", "max(BIGINT, DECIMAL(2,1))"),
        ("SELECT max(x, 2::UBIGINT) FROM range(5) t(x)", "max(BIGINT, UBIGINT)"),
        ("SELECT arg_max(x, x, 2.7) FROM range(5) t(x)", "arg_max(BIGINT, BIGINT, DECIMAL(2,1))"),
    ] {
        let error = database.query(sql).expect_err(sql);
        let wanted =
            format!("Binder Error: No function matches the given name and argument types '{call}'");
        assert!(error.to_string().starts_with(&wanted), "{error}");
    }
    assert_eq!(answered(&database, "SELECT max(x, 2::USMALLINT) FROM range(5) t(x)"), ["[4, 3]"]);
}

#[test]
fn the_order_a_call_is_written_with_decides_between_ties() {
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE t1 AS SELECT * FROM (VALUES ('a', 2), ('a', 1), ('b', 5), ('b', 4), ('a', 3), \
             ('b', 6)) t(val, arg)",
        )
        .expect("the table");
    for (sql, expected) in [
        (
            "SELECT arg_max(arg, val ORDER BY arg DESC), arg_max(arg, val), arg_max(arg, val ORDER BY arg) FROM t1",
            vec!["6|5|4"],
        ),
        ("SELECT arg_min(arg, val ORDER BY arg DESC), arg_min(arg, val) FROM t1", vec!["3|2"]),
        ("SELECT arg_max(arg, val, 3 ORDER BY arg) FROM t1", vec!["[6, 5, 4]"]),
        (
            "SELECT arg_max(arg, val, 2 ORDER BY arg) FROM t1 GROUP BY val ORDER BY 1",
            vec!["[2, 1]", "[5, 4]"],
        ),
        ("SELECT arg_max(arg, val, 2) FROM t1 GROUP BY val ORDER BY 1", vec!["[1, 2]", "[4, 5]"]),
        ("SELECT max(arg, 2 ORDER BY arg DESC) FROM t1", vec!["[6, 5]"]),
    ] {
        assert_eq!(answered(&database, sql), expected, "{sql}");
    }
}

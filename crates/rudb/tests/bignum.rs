//! `BIGNUM`, the integer with no fixed width that the pin also spells `VARINT`. Every expected
//! answer here was taken from the pinned duckdb binary, v2.0.0-dev84237, except a cast of a value
//! other than zero to `HUGEINT` or `UHUGEINT`, which the pin refuses and rudb answers. That is
//! tamnd/duckdb#41.

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

/// The type the one column of `sql` is described as, without running it.
fn described(database: &Database, sql: &str) -> String {
    answered(database, &format!("SELECT column_type FROM (DESCRIBE {sql})")).join(",")
}

#[test]
fn an_integer_literal_too_wide_for_hugeint_is_a_bignum() {
    let database = Database::new();
    let wide = "99999999999999999999999999999999999999999";
    assert_eq!(described(&database, &format!("SELECT {wide}")), "BIGNUM");
    assert_eq!(described(&database, &format!("SELECT -{wide}")), "BIGNUM");
    assert_eq!(answered(&database, &format!("SELECT -{wide}")), [format!("-{wide}")]);
    assert_eq!(
        answered(&database, &format!("SELECT {wide} + 1")),
        ["100000000000000000000000000000000000000000"]
    );
    assert_eq!(answered(&database, "SELECT 5::VARINT, typeof(5::VARINT)"), ["5|BIGNUM"]);
}

#[test]
fn casts_in_and_out_follow_the_pin() {
    let database = Database::new();
    for (sql, expected) in [
        ("SELECT 1.5::FLOAT::BIGNUM", "1"),
        ("SELECT (-1.9)::DOUBLE::BIGNUM", "-1"),
        ("SELECT '1.5'::BIGNUM", "2"),
        ("SELECT 1e300::BIGNUM::DOUBLE", "1e+300"),
        ("SELECT (10::BIGNUM)::UTINYINT", "10"),
        ("SELECT '5'::BIGNUM::VARCHAR", "5"),
        ("SELECT 5::BIGNUM::JSON", "5"),
        ("SELECT -0::BIGNUM", "0"),
        ("SELECT TRY_CAST('abc' AS BIGNUM)", "NULL"),
        ("SELECT (2::BIGNUM)::HUGEINT", "2"),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
    for (sql, expected) in [
        ("SELECT 'abc'::BIGNUM", "Conversion Error: Could not convert string 'abc' to VARCHAR"),
        (
            "SELECT (300::BIGNUM)::UTINYINT",
            "Out of Range Error: Positive bignum too large for type",
        ),
        (
            "SELECT (-300::BIGNUM)::TINYINT",
            "Out of Range Error: Negative bignum too small for type",
        ),
        (
            "SELECT 5::BIGNUM::DATE",
            "Conversion Error: Unimplemented type for cast (BIGNUM -> DATE)",
        ),
        (
            "SELECT 1.5::BIGNUM",
            "Conversion Error: Unimplemented type for cast (DECIMAL(2,1) -> BIGNUM)",
        ),
        (
            "SELECT true::BIGNUM",
            "Conversion Error: Unimplemented type for cast (BOOLEAN -> BIGNUM)",
        ),
        (
            "SELECT '5'::JSON::BIGNUM",
            "Not implemented Error: Cannot read a value of type BIGNUM from a json file",
        ),
    ] {
        refused(&database, sql, expected);
    }
}

#[test]
fn arithmetic_keeps_a_bignum_only_where_the_pin_does() {
    let database = Database::new();
    for (sql, expected) in [
        ("SELECT 1::BIGNUM + 1", "BIGNUM"),
        ("SELECT 1::BIGNUM + 1.5", "DOUBLE"),
        ("SELECT 1::BIGNUM * 2", "DOUBLE"),
        ("SELECT abs(-5::BIGNUM)", "DOUBLE"),
        ("SELECT coalesce(1::BIGNUM, 'x')", "BIGNUM"),
        ("SELECT coalesce(1::BIGNUM, 1.5::DOUBLE)", "DOUBLE"),
    ] {
        assert_eq!(described(&database, sql), expected, "{sql}");
    }
    assert_eq!(answered(&database, "SELECT -(5::BIGNUM)"), ["-5"]);
    assert_eq!(answered(&database, "SELECT 1::BIGNUM < 1.5::FLOAT"), ["false"]);
    refused(
        &database,
        "SELECT 1::BIGNUM < 2.5",
        "Binder Error: Cannot compare values of type BIGNUM and type DECIMAL(2,1)",
    );
}

#[test]
fn sum_min_and_max_keep_a_bignum() {
    let database = Database::new();
    let sql = "SELECT typeof(sum(x)), sum(x), min(x), max(x) \
               FROM (VALUES (1::BIGNUM), (-2::BIGNUM), (99999999999999999999999999999999999999999)) t(x)";
    assert_eq!(
        answered(&database, sql),
        [
            "BIGNUM|99999999999999999999999999999999999999998|-2|99999999999999999999999999999999999999999"
        ]
    );
    assert_eq!(
        answered(&database, "SELECT sum(x) FROM (SELECT 1::BIGNUM x WHERE false)"),
        ["NULL"]
    );
    let sql = "SELECT x FROM (VALUES (3::BIGNUM), (-20::BIGNUM), (100::BIGNUM)) t(x) ORDER BY x";
    let result = database.execute(sql).expect(sql);
    let order: Vec<String> = result.rows().map(|row| row[0].to_string()).collect();
    assert_eq!(order, ["-20", "3", "100"]);
}

#[test]
fn a_bignum_column_is_kept_in_a_table() {
    let database = Database::new();
    database.execute("CREATE TABLE t (x BIGNUM)").expect("the table");
    database
        .execute("INSERT INTO t VALUES (1), (-99999999999999999999999999999999999999999), (NULL)")
        .expect("the rows");
    assert_eq!(
        answered(&database, "SELECT x FROM t"),
        ["-99999999999999999999999999999999999999999", "1", "NULL"]
    );
    assert_eq!(answered(&database, "SELECT count(DISTINCT x) FROM t"), ["2"]);
}

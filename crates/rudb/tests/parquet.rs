//! `read_parquet` from SQL, which is the first query rudb answers out of a file on disk.
//!
//! The fixture is `rudb-parquet`'s, referred to across the workspace rather than copied, because two
//! copies of a binary file are two things to keep in step and a test that reads the stale one passes
//! while meaning nothing. It is 4096 rows in two row groups, written by DuckDB, and what each column
//! holds is the closed form checked against DuckDB in `crates/rudb-parquet/tests/read.rs`.
//!
//! What is asserted here is the SQL, not the decoding. The decoding has its own test one layer down
//! and repeating it here would be the same assertions twice with a parser in front of them.

use rudb::Database;
use rudb_common::Value;

/// The path of the fixture, as a SQL string literal.
fn fixture() -> String {
    format!("'{}/../rudb-parquet/testdata/mixed.parquet'", env!("CARGO_MANIFEST_DIR"))
}

/// `SELECT <what> FROM read_parquet(<fixture>)`, as one value.
fn one(what: &str) -> Value {
    let database = Database::new();
    let sql = format!("SELECT {what} FROM read_parquet({})", fixture());
    database.value(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"))
}

#[test]
fn counting_the_rows_of_a_file_is_the_number_of_rows_in_it() {
    assert_eq!(one("count(*)"), Value::BigInt(4096));
}

#[test]
fn every_column_of_the_file_is_selectable_by_the_name_the_file_gives_it() {
    let database = Database::new();
    let sql = format!("SELECT a, b, s, d, flag, day, t FROM read_parquet({})", fixture());
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.len(), 4096);
    assert_eq!(result.width(), 7);
    assert_eq!(result.names(), ["a", "b", "s", "d", "flag", "day", "t"]);
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    assert_eq!(types, ["INTEGER", "BIGINT", "VARCHAR", "DOUBLE", "BOOLEAN", "DATE", "TIMESTAMP"]);
}

#[test]
fn the_first_row_is_the_one_duckdb_reads_from_the_same_bytes() {
    let database = Database::new();
    let sql = format!("SELECT * FROM read_parquet({}) LIMIT 1", fixture());
    let result = database.query(&sql).expect("runs");
    let row: Vec<Value> = (0..result.width()).map(|at| result.value_at(0, at)).collect();
    assert_eq!(
        row,
        vec![
            Value::Integer(0),
            Value::BigInt(0),
            Value::Null,
            Value::Double(0.0),
            Value::Boolean(true),
            Value::Date(0),
            Value::Timestamp(1_373_882_400_000_000),
        ]
    );
}

#[test]
fn the_nulls_of_a_dictionary_column_are_the_nulls_duckdb_counts() {
    // 586 of the 4096 rows, which is every seventh, and this is the assertion that was wrong
    // before `nulls_of` read a dictionary's own validity as well as its values'. A file is the
    // first thing that builds that vector, so nothing here could have failed until now.
    assert_eq!(one("count(s)"), Value::BigInt(3510));
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM read_parquet({}) WHERE s IS NULL", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(586));
    let sql = format!("SELECT count(*) FROM read_parquet({}) WHERE s IS NOT NULL", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(3510));
}

#[test]
fn a_where_clause_over_a_file_filters_the_rows_it_read() {
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM read_parquet({}) WHERE flag", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(2048));
    let sql = format!("SELECT count(*) FROM read_parquet({}) WHERE a < 10", fixture());
    // 97 divides 4096 into 42 whole turns and a remainder of 22, so the values under ten come round
    // 42 times each and the first ten of the remainder add one more apiece.
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(430));
}

#[test]
fn an_aggregate_over_a_dictionary_column_groups_by_its_values() {
    let database = Database::new();
    let sql = format!(
        "SELECT s, count(*) FROM read_parquet({}) WHERE s IS NOT NULL GROUP BY s ORDER BY s",
        fixture()
    );
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.len(), 5, "tag0 to tag4");
    assert_eq!(result.value_at(0, 0), Value::Varchar("tag0".into()));
    let total: i64 = (0..5)
        .map(|row| match result.value_at(row, 1) {
            Value::BigInt(count) => count,
            other => panic!("count(*) gave {other}"),
        })
        .sum();
    assert_eq!(total, 3510);
}

#[test]
fn the_sums_are_the_sums_duckdb_reports_for_the_same_file() {
    assert_eq!(one("sum(a)"), Value::HugeInt(195_783));
    assert_eq!(one("sum(b)"), Value::HugeInt(2_002_560_000));
    assert_eq!(one("min(day)"), Value::Date(0));
    assert_eq!(one("max(day)"), Value::Date(999));
}

#[test]
fn an_alias_renames_the_call_and_not_its_columns() {
    let database = Database::new();
    let sql = format!("SELECT p.a FROM read_parquet({}) AS p LIMIT 1", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::Integer(0));
}

#[test]
fn parquet_scan_reads_the_same_file_under_duckdbs_other_name_for_the_function() {
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM parquet_scan({})", fixture());
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(4096));
}

#[test]
fn a_file_that_is_not_there_fails_at_bind_time_with_duckdbs_message() {
    let database = Database::new();
    let error =
        database.query("SELECT * FROM read_parquet('/nowhere/at/all.parquet')").unwrap_err();
    assert_eq!(
        error.message(),
        "No files found that match the pattern \"/nowhere/at/all.parquet\""
    );
}

#[test]
fn a_path_that_is_not_a_string_does_not_become_one() {
    let database = Database::new();
    let error = database.query("SELECT * FROM read_parquet(3)").unwrap_err();
    assert!(error.message().contains("read_parquet(INTEGER)"), "{error}");
}

#[test]
fn a_column_the_file_does_not_have_is_the_error_a_missing_column_always_is() {
    let database = Database::new();
    let sql = format!("SELECT nosuch FROM read_parquet({})", fixture());
    let error = database.query(&sql).unwrap_err();
    assert!(error.message().contains("nosuch"), "{error}");
}

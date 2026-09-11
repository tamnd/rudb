//! A read that covers more than one file, which is what a pattern in a path means.
//!
//! `read_parquet('data/*.parquet')` is how a directory of files is read in every engine that reads
//! one, and it is how the partitioned form of ClickBench is distributed, so it is not a convenience.
//! The fixtures are the two reader crates' own, referred to across the workspace rather than copied,
//! and every answer asserted here was read off duckdb v1.4.1 against the same files.
//!
//! The two readers behave differently on purpose and the difference is the interesting part. A
//! Parquet file states its schema, so the first file settles it and a later file that disagrees is
//! cast to it. A CSV file states nothing, so every file is sniffed and the answers are combined,
//! and a column that one file writes as a decimal widens the whole read. Getting that backwards
//! produces a column of the wrong type, which is a wrong answer rather than a slow query.

use rudb::Database;
use rudb_common::Value;

/// A path under one of the two reader crates' fixture directories, as a SQL string literal.
fn fixture(crate_name: &str, tail: &str) -> String {
    format!("'{}/../{crate_name}/testdata/{tail}'", env!("CARGO_MANIFEST_DIR"))
}

/// `SELECT <what> FROM <function>(<fixture>)`, as one value.
fn one(function: &str, path: &str, what: &str) -> Value {
    let database = Database::new();
    let sql = format!("SELECT {what} FROM {function}({path})");
    database.value(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"))
}

/// Whatever `SELECT * FROM <function>(<fixture>)` complains about.
fn refused(function: &str, path: &str) -> String {
    let database = Database::new();
    let sql = format!("SELECT * FROM {function}({path})");
    database.query(&sql).expect_err("the files disagree").message().to_string()
}

#[test]
fn a_parquet_pattern_reads_every_file_it_matches_as_one_stream() {
    let parts = fixture("rudb-parquet", "parts/*.parquet");
    assert_eq!(one("read_parquet", &parts, "count(*)"), Value::BigInt(3));
    assert_eq!(one("read_parquet", &parts, "sum(id)"), Value::HugeInt(6));
}

#[test]
fn a_parquet_pattern_matches_on_the_name_and_not_on_the_directory() {
    // Two of the three files in that directory start with a t, and the pattern is the difference
    // between reading a directory and reading part of one.
    let some = fixture("rudb-parquet", "parts/t*.parquet");
    assert_eq!(one("read_parquet", &some, "count(*)"), Value::BigInt(2));
}

#[test]
fn the_rows_of_all_the_parquet_files_are_there_and_keep_their_own_columns() {
    let database = Database::new();
    let parts = fixture("rudb-parquet", "parts/*.parquet");
    let sql = format!("SELECT id, tag FROM read_parquet({parts}) ORDER BY id");
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.len(), 3);
    let ids: Vec<Value> = (0..3).map(|row| result.value_at(row, 0)).collect();
    assert_eq!(ids, vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)]);
    assert_eq!(result.value_at(2, 1), Value::Varchar("z".into()));
}

#[test]
fn the_first_parquet_file_settles_the_type_and_a_later_one_is_cast_to_it() {
    // The second file stores `id` as a DOUBLE and the first stores it as an INTEGER, and DuckDB
    // answers INTEGER for both rows rather than widening the stream. 2.75 comes back as 3, so the
    // cast rounds rather than truncating, which is the part that would go unnoticed.
    let database = Database::new();
    let widened = fixture("rudb-parquet", "widened/*.parquet");
    let sql = format!("SELECT id FROM read_parquet({widened}) ORDER BY id");
    let result = database.query(&sql).expect("runs");
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    assert_eq!(types, ["INTEGER"]);
    assert_eq!(result.value_at(0, 0), Value::Integer(1));
    assert_eq!(result.value_at(1, 0), Value::Integer(3));
}

#[test]
fn a_parquet_file_missing_a_column_the_first_one_has_is_duckdbs_own_complaint() {
    let odd = fixture("rudb-parquet", "odd/*.parquet");
    let message = refused("read_parquet", &odd);
    assert!(message.contains("schema mismatch in glob: column \"id\" was read"), "{message}");
    assert!(message.contains("could not be found in file"), "{message}");
    assert!(message.contains("Candidate names: other"), "{message}");
    assert!(message.contains("try setting union_by_name=True"), "{message}");
}

#[test]
fn a_csv_pattern_reads_every_file_and_skips_the_header_of_each_one() {
    // Four rows out of three files that each carry the header line, which is the whole reason
    // somebody points a pattern at a directory of daily exports.
    let parts = fixture("rudb-csv", "parts/*.csv");
    assert_eq!(one("read_csv", &parts, "count(*)"), Value::BigInt(4));
}

#[test]
fn every_csv_file_is_sniffed_and_one_decimal_widens_the_whole_read() {
    let database = Database::new();
    let parts = fixture("rudb-csv", "parts/*.csv");
    let sql = format!("SELECT id FROM read_csv({parts}) ORDER BY id");
    let result = database.query(&sql).expect("runs");
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    // The third file holds 4.5 in a column the other two fill with whole numbers. A reader that
    // took the first file's word would answer BIGINT here and then fail to read the third file.
    assert_eq!(types, ["DOUBLE"]);
    assert_eq!(result.value_at(0, 0), Value::Double(1.0));
    assert_eq!(one("read_csv", &parts, "sum(id)"), Value::Double(10.5));
}

#[test]
fn a_csv_pattern_that_leaves_out_the_decimal_file_is_an_integer_read() {
    // The same directory minus the one file that widened it, which is what says the type came from
    // looking at the files rather than from a rung the sniffer always lands on.
    let two = fixture("rudb-csv", "parts/[ab].csv");
    let database = Database::new();
    let sql = format!("SELECT id FROM read_csv({two})");
    let result = database.query(&sql).expect("runs");
    let types: Vec<String> = result.types().iter().map(ToString::to_string).collect();
    assert_eq!(types, ["BIGINT"]);
    assert_eq!(one("read_csv", &two, "count(*)"), Value::BigInt(3));
    assert_eq!(one("read_csv", &two, "sum(id)"), Value::HugeInt(6));
}

#[test]
fn a_single_character_pattern_matches_each_of_the_one_letter_names() {
    let each = fixture("rudb-csv", "parts/?.csv");
    assert_eq!(one("read_csv", &each, "count(*)"), Value::BigInt(4));
}

#[test]
fn a_csv_file_missing_a_column_the_first_one_has_is_the_csv_readers_own_complaint() {
    // Not the Parquet reader's sentence, because the two readers in DuckDB are two pieces of code
    // that each wrote their own and a compatibility test that compares output compares all of it.
    let odd = fixture("rudb-csv", "odd/*.csv");
    let message = refused("read_csv", &odd);
    assert!(message.starts_with("Schema mismatch between globbed files."), "{message}");
    assert!(message.contains("Column with name: \"id\" is missing"), "{message}");
    assert!(message.contains("* Consider setting union_by_name=true."), "{message}");
}

#[test]
fn a_pattern_that_matches_nothing_is_the_same_complaint_as_a_file_that_is_not_there() {
    let database = Database::new();
    let error = database.query("SELECT * FROM read_parquet('/nowhere/at/all/*.parquet')");
    assert_eq!(
        error.unwrap_err().message(),
        "No files found that match the pattern \"/nowhere/at/all/*.parquet\""
    );
}

#[test]
fn a_pattern_works_where_a_table_name_goes_as_well_as_inside_the_function() {
    // The replacement scan takes a pattern in duckdb v1.4.1, which was measured, so a directory of
    // files can be written where a table goes and never mention a function at all.
    let database = Database::new();
    let parts = fixture("rudb-parquet", "parts/*.parquet");
    let sql = format!("SELECT count(*) FROM {parts}");
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(3));
}

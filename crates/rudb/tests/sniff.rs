//! Where a CSV read of several files gets its types from, which is all of them and not the first.
//!
//! This is the one place the two readers behave differently and the difference is not cosmetic. A
//! Parquet file states its schema in its footer, so the first file settles it and a later file that
//! disagrees is cast to what the first one said. A CSV file states nothing, only what a sample of it
//! suggests, so there is no first file's word to take, and DuckDB sniffs every file the pattern
//! matched and combines the answers. Getting that backwards gives a directory of daily exports a
//! column type that depends on which day sorted first, which is a wrong answer rather than a slow
//! query.
//!
//! Every answer here was read off duckdb v1.4.1 against these same files, including the sentence the
//! mismatch comes back with.

use rudb::Database;
use rudb_common::Value;

/// A pattern over the sniffing fixtures, as a SQL string literal.
///
/// `s1.csv` and `s2.csv` hold whole numbers in `id` and `s3.csv` holds 4.5, so the set is DOUBLE and
/// the first two on their own are BIGINT.
fn sniff(pattern: &str) -> String {
    format!("'{}/../rudb-csv/testdata/sniff/{pattern}'", env!("CARGO_MANIFEST_DIR"))
}

/// `SELECT <what> FROM <from>`, as one value.
fn one(what: &str, from: &str) -> Value {
    let database = Database::new();
    let sql = format!("SELECT {what} FROM {from}");
    database.value(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"))
}

/// The type of every column of a read, as DuckDB spells them.
fn types(from: &str) -> Vec<String> {
    let database = Database::new();
    let sql = format!("SELECT * FROM {from}");
    let result = database.query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result.types().iter().map(ToString::to_string).collect()
}

#[test]
fn every_file_is_sniffed_and_one_decimal_widens_the_whole_read() {
    // One of the three files holds 4.5 in a column the other two fill with whole numbers. A reader
    // that took the first file's word would answer BIGINT and then meet 4.5 in the third file.
    assert_eq!(types(&format!("read_csv({})", sniff("*.csv"))), ["DOUBLE", "VARCHAR"]);
    assert_eq!(one("count(*)", &format!("read_csv({})", sniff("*.csv"))), Value::BigInt(4));
    assert_eq!(one("sum(id)", &format!("read_csv({})", sniff("*.csv"))), Value::Double(10.5));
}

#[test]
fn the_files_that_hold_whole_numbers_are_read_as_the_type_the_set_settled_on() {
    // The rows of the two files that are BIGINT on their own, coming out of a read that is DOUBLE.
    // They are converted where they are parsed rather than read as integers and cast afterwards,
    // which is what the reader being told the type before it starts buys.
    let database = Database::new();
    let sql = format!("SELECT id, tag FROM read_csv({}) ORDER BY id", sniff("*.csv"));
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.len(), 4);
    let ids: Vec<Value> = (0..4).map(|row| result.value_at(row, 0)).collect();
    assert_eq!(
        ids,
        [Value::Double(1.0), Value::Double(2.0), Value::Double(3.0), Value::Double(4.5)]
    );
    assert_eq!(result.value_at(3, 1), Value::Varchar("w".into()));
}

#[test]
fn a_pattern_that_leaves_out_the_decimal_file_is_an_integer_read() {
    // The same directory minus the one file that widened it, which is what says the type came from
    // reading the files rather than from a rung the sniffer always lands on.
    let two = format!("read_csv({})", sniff("s[12].csv"));
    assert_eq!(types(&two), ["BIGINT", "VARCHAR"]);
    assert_eq!(one("count(*)", &two), Value::BigInt(3));
    assert_eq!(one("sum(id)", &two), Value::HugeInt(6));
}

#[test]
fn a_file_missing_a_column_the_first_one_has_is_the_csv_readers_own_complaint() {
    // Not the Parquet reader's sentence for the same situation, because the two readers in DuckDB
    // are two pieces of code that each wrote their own and a compatibility test that compares
    // output compares all of it. The trailing space after `Potential Fixes` is the binary's.
    let database = Database::new();
    let sql = format!("SELECT * FROM read_csv({})", sniff("odd/*.csv"));
    let message = database.query(&sql).expect_err("the files disagree").message().to_string();
    assert!(message.starts_with("Schema mismatch between globbed files."), "{message}");
    assert!(message.contains("Column with name: \"id\" is missing"), "{message}");
    assert!(
        message.contains("Potential Fixes \n* Consider setting union_by_name=true."),
        "{message}"
    );
    assert!(message.contains("files_to_sniff = -1"), "{message}");
}

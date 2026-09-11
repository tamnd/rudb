//! A list of paths where a file name goes, which is the other way a query names many files.
//!
//! `read_parquet(['a.parquet', 'b.parquet'])` is DuckDB's second overload of both file readers, and
//! it is not a glob written differently: a glob is sorted and deduplicated and a list is read in the
//! order it was written and reads a file named twice twice. Both of those were measured against
//! duckdb v1.4.1, and so was every message here.
//!
//! The fixtures are the two readers' own, under `testdata/parts`, and what is in each file is
//! recorded in the README next to them.

use rudb::Database;
use rudb_common::Value;

/// A path to one of the Parquet fixtures, as it would be written inside a list.
fn parquet(name: &str) -> String {
    format!("'{}/../rudb-parquet/testdata/parts/{name}'", env!("CARGO_MANIFEST_DIR"))
}

/// A path to one of the CSV fixtures, as it would be written inside a list.
fn csv(name: &str) -> String {
    format!("'{}/../rudb-csv/testdata/parts/{name}'", env!("CARGO_MANIFEST_DIR"))
}

/// `SELECT <what> FROM <from>`, as one value.
fn one(what: &str, from: &str) -> Value {
    let database = Database::new();
    let sql = format!("SELECT {what} FROM {from}");
    database.value(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"))
}

/// The message a query that cannot run comes back with.
fn refusal(sql: &str) -> String {
    let database = Database::new();
    database.query(sql).unwrap_err().message().to_string()
}

#[test]
fn a_list_of_paths_reads_every_one_of_them() {
    let from = format!("read_parquet([{}, {}])", parquet("p1.parquet"), parquet("p2.parquet"));
    assert_eq!(one("count(*)", &from), Value::BigInt(7));
    assert_eq!(one("sum(a)", &from), Value::HugeInt(49));
}

#[test]
fn a_list_is_read_in_the_order_it_was_written_and_not_in_sorted_order() {
    // The opposite of a glob, which sorts. A list is what somebody wrote down, so the order is
    // theirs, and `p2` first means the rows of `p2` first.
    let database = Database::new();
    let sql = format!(
        "SELECT a FROM read_parquet([{}, {}]) LIMIT 5",
        parquet("p2.parquet"),
        parquet("p1.parquet")
    );
    let result = database.query(&sql).expect("runs");
    let rows: Vec<Value> = (0..result.len()).map(|row| result.value_at(row, 0)).collect();
    assert_eq!(
        rows,
        vec![
            Value::Integer(10),
            Value::Integer(11),
            Value::Integer(12),
            Value::Integer(13),
            Value::Integer(0)
        ]
    );
}

#[test]
fn a_file_named_twice_is_read_twice() {
    // Also the opposite of a glob, which deduplicates. The sort and the dedup belong to one pattern
    // rather than to the list around it.
    let from = format!("read_parquet([{}, {}])", parquet("p1.parquet"), parquet("p1.parquet"));
    assert_eq!(one("count(*)", &from), Value::BigInt(6));
}

#[test]
fn an_item_of_a_list_can_itself_be_a_pattern() {
    let from = format!("read_parquet([{}])", parquet("p*.parquet"));
    assert_eq!(one("count(*)", &from), Value::BigInt(7));
}

#[test]
fn every_item_has_to_find_a_file_of_its_own() {
    // Not the total. A list where one name is a typo is a query that reads less than it asked for,
    // and DuckDB refuses it rather than answering over what was left.
    let sql = format!(
        "SELECT count(*) FROM read_parquet([{}, {}])",
        parquet("p1.parquet"),
        parquet("zz*.parquet")
    );
    let message = refusal(&sql);
    assert!(message.starts_with("No files found that match the pattern"), "{message}");
    assert!(message.ends_with("zz*.parquet\""), "{message}");
}

#[test]
fn an_empty_list_is_the_message_about_overloads_because_it_has_no_element_type() {
    // `[]` is an `INTEGER[]` before anything looks at what it is for, so it never reaches the file
    // reader. DuckDB reports it the same way and this is its sentence.
    let message = refusal("SELECT * FROM read_parquet([])");
    assert!(
        message.starts_with(
            "No function matches the given name and argument types 'read_parquet(INTEGER[])'"
        ),
        "{message}"
    );
    assert!(message.contains("read_parquet(VARCHAR[])"), "{message}");
}

#[test]
fn a_null_inside_a_list_and_a_null_instead_of_one_are_two_different_sentences() {
    let sql = format!("SELECT * FROM read_parquet([{}, NULL])", parquet("p1.parquet"));
    assert_eq!(refusal(&sql), "read_parquet reader cannot take NULL input as parameter");
    assert_eq!(
        refusal("SELECT * FROM read_csv(NULL)"),
        "read_csv cannot take NULL list as parameter"
    );
}

#[test]
fn the_csv_reader_takes_a_list_the_same_way() {
    let from = format!("read_csv([{}, {}])", csv("c1.csv"), csv("c2.csv"));
    assert_eq!(one("count(*)", &from), Value::BigInt(3));
    assert_eq!(one("sum(a)", &from), Value::HugeInt(13));
}

#[test]
fn a_list_call_answers_to_the_function_name_when_nothing_aliased_it() {
    let database = Database::new();
    let sql =
        format!("SELECT read_parquet.a FROM read_parquet([{}]) LIMIT 1", parquet("p1.parquet"));
    assert_eq!(database.value(&sql).expect("runs"), Value::Integer(0));
}

#[test]
fn a_list_where_an_expression_goes_is_a_gap_that_says_so() {
    // There is no LIST vector yet, so a list that is not a file argument has nowhere to be
    // computed. The message comes from the vector layer and names the thing that is missing rather
    // than pretending the syntax is unknown.
    let message = refusal("SELECT [1, 2, 3]");
    assert!(message.contains("List"), "{message}");
}

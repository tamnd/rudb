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
fn an_empty_list_is_the_reader_saying_it_has_no_file_to_read() {
    // `[]` is a list of the untyped null, which the file overload takes, so the empty list reaches
    // the reader rather than being turned away for having the wrong type. DuckDB lets it through to
    // the same place and this is the sentence it answers with there.
    assert_eq!(
        refusal("SELECT * FROM read_parquet([])"),
        "\"read_parquet\" needs at least one file to read"
    );
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
fn a_list_where_an_expression_goes_computes_since_there_is_a_vector_for_it() {
    // This used to be the gap that said so, because there was no LIST vector and a list that was not
    // a file argument had nowhere to be computed. #302 gave it one, so the same query that named the
    // missing piece now answers.
    let database = Database::new();
    assert_eq!(
        database.value("SELECT [1, 2, 3]").expect("runs"),
        Value::List {
            element: rudb_common::LogicalType::Integer,
            values: vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        }
    );
}

#[test]
fn a_list_of_columns_computes_now_that_a_list_is_a_call() {
    // This used to be the other half of the gap. A list of constants folded into one value and a
    // list with a column in it had nothing to fold into and no `list_value` function to become a
    // call to, so it was refused. A list literal is that call now, which is what takes the folding
    // out of the question of whether a list can be written at all.
    let database = Database::new();
    assert_eq!(
        database.value("SELECT [a, 2] FROM (SELECT 1 AS a)").expect("runs"),
        Value::List {
            element: rudb_common::LogicalType::Integer,
            values: vec![Value::Integer(1), Value::Integer(2)],
        }
    );
}

#[test]
fn a_file_name_inside_a_list_does_not_have_to_be_written_out() {
    // The reader needs the name before the plan runs, which is not the same as needing it written
    // down. The list is a call and what reaches the reader is whatever the folding makes of it, so a
    // name that is built out of constants is a name.
    let from = format!("read_parquet([{} || ''])", parquet("p1.parquet"));
    assert_eq!(one("count(*)", &from), Value::BigInt(3));
}

//! A pattern where a file name goes, which is how a directory of files is read.
//!
//! `SELECT * FROM 'hits/*.parquet'` is the shape every partitioned dataset is queried in, and the
//! syntax is DuckDB's: `*` inside one path segment, `?` for one character, `[abc]` for one of a set,
//! and `**` for any number of directories including none. All four were measured against duckdb
//! v1.4.1, and so was the absence of brace expansion.
//!
//! The fixtures are the two readers' own, under `testdata/parts`, and what is in each file is
//! recorded in the README next to them.

use rudb::Database;
use rudb_common::Value;

/// A pattern over the Parquet fixtures, as a SQL string literal.
fn parquet(pattern: &str) -> String {
    format!("'{}/../rudb-parquet/testdata/parts/{pattern}'", env!("CARGO_MANIFEST_DIR"))
}

/// A pattern over the CSV fixtures, as a SQL string literal.
fn csv(pattern: &str) -> String {
    format!("'{}/../rudb-csv/testdata/parts/{pattern}'", env!("CARGO_MANIFEST_DIR"))
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
fn a_star_reads_every_file_of_one_directory_that_matches() {
    assert_eq!(one("count(*)", &parquet("p*.parquet")), Value::BigInt(7));
    assert_eq!(one("sum(a)", &parquet("p*.parquet")), Value::HugeInt(49));
}

#[test]
fn a_star_does_not_cross_a_directory_and_a_middle_one_reaches_exactly_one_level() {
    // `deep/p3.parquet` is not in the first answer and is the whole of the second.
    assert_eq!(one("count(*)", &parquet("p*.parquet")), Value::BigInt(7));
    assert_eq!(one("count(*)", &parquet("*/p*.parquet")), Value::BigInt(2));
    assert_eq!(one("sum(a)", &parquet("*/p*.parquet")), Value::HugeInt(201));
}

#[test]
fn a_double_star_reaches_any_depth_including_none() {
    assert_eq!(one("count(*)", &parquet("**/p*.parquet")), Value::BigInt(9));
    assert_eq!(one("sum(a)", &parquet("**/p*.parquet")), Value::HugeInt(250));
}

#[test]
fn a_question_mark_stands_for_one_character_and_insists_on_one() {
    assert_eq!(one("count(*)", &parquet("p?.parquet")), Value::BigInt(7));
    let sql = format!("SELECT count(*) FROM {}", parquet("p??.parquet"));
    assert!(refusal(&sql).starts_with("No files found"), "{sql}");
}

#[test]
fn a_class_stands_for_one_of_what_it_lists() {
    assert_eq!(one("count(*)", &parquet("p[12].parquet")), Value::BigInt(7));
    assert_eq!(one("count(*)", &parquet("p[1].parquet")), Value::BigInt(3));
    assert_eq!(one("count(*)", &parquet("p[!1].parquet")), Value::BigInt(4));
}

#[test]
fn braces_are_not_a_pattern_here_because_they_are_not_one_in_duckdb_either() {
    let sql = format!("SELECT count(*) FROM {}", parquet("{p1,p2}.parquet"));
    assert!(refusal(&sql).starts_with("No files found"), "{sql}");
}

#[test]
fn the_files_are_read_in_the_sorted_order_of_their_whole_paths() {
    // Not in whatever order the directory happens to hold them, which differs between two machines
    // holding the same files and would make this query answer differently on each of them.
    let database = Database::new();
    let sql = format!("SELECT a FROM read_parquet({}) LIMIT 4", parquet("**/p*.parquet"));
    let result = database.query(&sql).expect("runs");
    let rows: Vec<Value> = (0..result.len()).map(|row| result.value_at(row, 0)).collect();
    // deep/p3 sorts before p1 and p2 because the whole path is what is compared.
    assert_eq!(
        rows,
        vec![Value::Integer(100), Value::Integer(101), Value::Integer(0), Value::Integer(1)]
    );
}

#[test]
fn a_pattern_that_matches_nothing_is_the_error_a_missing_file_is() {
    let sql = format!("SELECT count(*) FROM read_parquet({})", parquet("nothing*.parquet"));
    let message = refusal(&sql);
    assert!(message.starts_with("No files found that match the pattern"), "{message}");
    assert!(message.ends_with("nothing*.parquet\""), "{message}");
}

#[test]
fn a_pattern_with_an_extension_nothing_reads_is_still_a_table_that_does_not_exist() {
    // Measured. A pattern is not tried as a file, so there is nothing to say about extensions and
    // the catalog's own answer stands.
    let sql = format!("SELECT count(*) FROM {}", parquet("*"));
    assert!(refusal(&sql).starts_with("Table with name "), "{sql}");
}

#[test]
fn a_directory_where_a_table_name_goes_is_a_table_that_does_not_exist() {
    let sql = format!("SELECT count(*) FROM {}", parquet("deep"));
    assert!(refusal(&sql).starts_with("Table with name "), "{sql}");
}

#[test]
fn the_columns_of_a_pattern_answer_to_the_whole_of_what_was_written() {
    // A single file gives them its stem. A pattern has no stem to give, so DuckDB keeps the text,
    // and this asserts the same thing by showing that the stem is not what answers.
    let database = Database::new();
    let sql = format!("SELECT p1.a FROM {} LIMIT 1", parquet("p*.parquet"));
    assert!(database.query(&sql).is_err(), "the stem should not name a pattern");
    let sql = format!("SELECT q.a FROM {} AS q LIMIT 1", parquet("p*.parquet"));
    assert_eq!(database.value(&sql).expect("runs"), Value::Integer(0));
}

#[test]
fn the_first_file_decides_the_types_and_the_ones_after_it_are_cast_to_them() {
    // `widen/a_int.parquet` holds an INTEGER 1 and `widen/b_text.parquet` holds the string '5'.
    let database = Database::new();
    let sql = format!("SELECT a FROM read_parquet({})", parquet("widen/*.parquet"));
    let result = database.query(&sql).expect("runs");
    assert_eq!(result.types()[0].to_string(), "INTEGER");
    assert_eq!(result.value_at(0, 0), Value::Integer(1));
    assert_eq!(result.value_at(1, 0), Value::Integer(5));
}

#[test]
fn a_column_missing_from_a_later_file_is_duckdbs_own_sentence_about_it() {
    let sql = format!("SELECT * FROM read_parquet({})", parquet("odd/*.parquet"));
    let message = refusal(&sql);
    assert!(message.contains("schema mismatch in glob: column \"s\" was read from the original"));
    assert!(message.contains("Candidate names: a"), "{message}");
    assert!(message.ends_with("try setting union_by_name=True"), "{message}");
}

#[test]
fn a_pattern_reads_csv_files_the_same_way_it_reads_parquet_ones() {
    assert_eq!(one("count(*)", &csv("c*.csv")), Value::BigInt(3));
    assert_eq!(one("sum(a)", &csv("c*.csv")), Value::HugeInt(13));
    let database = Database::new();
    let sql = format!("SELECT count(*) FROM read_csv({})", csv("c?.csv"));
    assert_eq!(database.value(&sql).expect("runs"), Value::BigInt(3));
}

#[test]
fn a_pattern_can_be_loaded_into_a_table_like_any_other_query() {
    let database = Database::new();
    let sql = format!("CREATE TABLE parts AS SELECT * FROM {}", parquet("**/p*.parquet"));
    database.execute(&sql).expect("loads");
    assert_eq!(database.value("SELECT count(*) FROM parts").expect("runs"), Value::BigInt(9));
    assert_eq!(database.value("SELECT sum(a) FROM parts").expect("runs"), Value::HugeInt(250));
}

//! The result set: the schema, the rows, the columns and the batches under them.

use rudb::{Database, LogicalType, Value};

/// A result of `rows` rows of one INTEGER column counting from zero.
fn counted(rows: usize) -> rudb::QueryResult {
    let db = Database::new();
    db.query(&format!("SELECT * FROM range({rows})")).expect("runs")
}

#[test]
fn a_result_says_what_its_columns_are_called_and_what_they_hold() {
    let db = Database::new();
    let result = db.query("SELECT 1 AS a, 'x' AS b").expect("runs");
    assert_eq!(result.width(), 2);
    assert_eq!(result.names(), ["a", "b"]);
    assert_eq!(result.types(), [LogicalType::Integer, LogicalType::Varchar]);
    assert_eq!(result.column_name(0), "a");
    assert_eq!(result.column_type(1), LogicalType::Varchar);
    // Past the end is empty rather than a panic, because a caller reading a result reads it in a
    // loop and one bound check is enough.
    assert_eq!(result.column_name(2), "");
    assert_eq!(result.column_type(2), LogicalType::Null);
}

#[test]
fn a_result_can_be_read_by_row_or_by_column_or_by_batch() {
    let result = counted(5);
    assert_eq!(result.len(), 5);
    assert_eq!(result.row(2), Some(vec![Value::BigInt(2)]));
    assert_eq!(result.value_at(4, 0), Value::BigInt(4));
    let column: Vec<Value> = result.column(0).collect();
    assert_eq!(column, (0..5).map(Value::BigInt).collect::<Vec<_>>());
    let batched: usize = result.chunk_iter().map(rudb::Chunk::len).sum();
    assert_eq!(batched, 5);
    // A column that is not there is nothing rather than an error, the same as a row past the end.
    assert_eq!(result.column(1).count(), 0);
    assert_eq!(result.row(5), None);
    assert_eq!(result.value_at(5, 0), Value::Null);
}

#[test]
fn a_large_result_is_many_batches_and_the_row_numbers_run_across_them() {
    // More than one vector's worth, so the chunk boundaries are real.
    let result = counted(10_000);
    assert!(result.chunk_count() > 1, "{} chunks", result.chunk_count());
    assert_eq!(result.chunk_count(), result.chunk_iter().len());
    assert_eq!(result.len(), 10_000);
    // Read by row number, which is the read that has to find the right chunk each time.
    for row in [0, 1, 2047, 2048, 9_998, 9_999] {
        assert_eq!(result.value_at(row, 0), Value::BigInt(row as i64), "row {row}");
    }
    assert_eq!(result.value_at(10_000, 0), Value::Null);
    let first = result.chunk(0).expect("there is a first chunk");
    assert_eq!(first.value_at(0, 0), Value::BigInt(0));
    assert!(result.chunk(result.chunk_count()).is_none());
}

#[test]
fn the_batches_can_be_taken_rather_than_copied() {
    let result = counted(3);
    let rows = result.len();
    let chunks = result.into_chunks();
    assert_eq!(chunks.iter().map(rudb::Chunk::len).sum::<usize>(), rows);
    assert_eq!(chunks[0].value_at(1, 0), Value::BigInt(1));
}

#[test]
fn a_statement_that_writes_hands_back_nothing_rather_than_a_count() {
    let db = Database::new();
    let result = db.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    assert!(result.is_empty());
    assert_eq!(result.width(), 0);
    assert_eq!(result.chunk_count(), 0);
}

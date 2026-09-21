//! A top N against the sort it stands in for, over a file big enough to have more than one chunk.
//!
//! The operator keeps an ordered prefix and throws the rest away, and since #482 it throws most of
//! the rest away with one vectorized comparison per chunk rather than a comparison per row. That
//! comparison is a filter and not the decision, so the thing worth testing is not which rows it lets
//! through but that the rows which come out are still exactly the rows a sort with a limit over it
//! produces, ties and nulls and all.
//!
//! Every test here runs the same query twice, once as written and once with the `top_n` pass turned
//! off so the plan is a sort with a limit over it, and asserts the two agree. The fixture is
//! `rudb-parquet`'s 4096 row file, which is four chunks, so the prefix is full long before the last
//! chunk arrives and the path this is about is the one that runs.

use rudb::Database;
use rudb_common::Value;

/// The path of the fixture, as a SQL string literal.
fn fixture() -> String {
    format!("'{}/../rudb-parquet/testdata/mixed.parquet'", env!("CARGO_MANIFEST_DIR"))
}

/// `SELECT * FROM <fixture> ORDER BY <keys> LIMIT <count> OFFSET <skipped>`.
fn query(keys: &str, count: usize, skipped: usize) -> String {
    let from = format!("FROM read_parquet({})", fixture());
    format!("SELECT * {from} ORDER BY {keys} LIMIT {count} OFFSET {skipped}")
}

/// The same, over three columns and with the row's ordinal in the file among them, so a key can say
/// where in the file it wants the nulls. Chunks are 1024 rows, so an ordinal is also a chunk number.
fn placed(nulls: &str, keys: &str, count: usize) -> String {
    let from = format!("FROM read_parquet({}, file_row_number=true)", fixture());
    let key = format!("CASE WHEN {nulls} THEN NULL ELSE t END");
    format!("SELECT a, b, file_row_number {from} ORDER BY {key} {keys} LIMIT {count}")
}

/// The rows of a query, as values.
fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result.rows().collect()
}

/// Asserts the top N and the sort with a limit over it produce the same rows in the same order.
fn agree(keys: &str, count: usize, skipped: usize) {
    same(&query(keys, count, skipped));
}

/// The same assertion over a query written out in full.
fn same(sql: &str) {
    let operator = Database::new();
    assert!(operator.plan(sql).expect("binds").contains("TopN"), "{sql} is not a top N");
    let sorted = Database::new();
    sorted.execute("SET disabled_optimizers = 'top_n'").expect("the pass answers to that name");
    assert_eq!(rows(&operator, sql), rows(&sorted, sql), "{sql}");
}

/// The plain shape, and the one the rewrite is for. No nulls in the key and one key.
#[test]
fn one_ascending_key_with_no_nulls_in_it() {
    agree("t ASC NULLS LAST", 10, 0);
    agree("b ASC NULLS LAST", 10, 0);
}

/// Reversed, which is the other constant the comparison is made against.
#[test]
fn one_descending_key_with_no_nulls_in_it() {
    agree("t DESC NULLS LAST", 10, 0);
    agree("b DESC NULLS LAST", 10, 0);
}

/// The key here is 5 values over 4096 rows, so every row past the first few ties the worst
/// candidate, and a filter that dropped ties on one key would still pass this. With two keys it
/// keeps them, and the second key is what the answer turns on.
#[test]
fn a_first_key_that_almost_every_row_ties() {
    agree("a ASC NULLS LAST, b ASC NULLS LAST", 10, 0);
    agree("flag DESC NULLS LAST, t ASC NULLS LAST", 10, 0);
    agree("d ASC NULLS LAST, day DESC NULLS LAST, b ASC NULLS LAST", 20, 0);
}

/// Ties that are not broken by any key, where the answer is the order the rows arrived in.
#[test]
fn ties_on_every_key_come_out_in_the_order_they_arrived() {
    agree("flag ASC NULLS LAST", 32, 0);
    agree("a DESC NULLS LAST", 40, 0);
}

/// `s` is null in one row of every seven. Asked for last they lose to everything and the filter
/// runs; asked for first they win against everything and it steps aside.
#[test]
fn a_key_with_nulls_in_it_either_way_round() {
    agree("s ASC NULLS LAST", 10, 0);
    agree("s DESC NULLS LAST", 10, 0);
    agree("s ASC NULLS FIRST", 10, 0);
    agree("s DESC NULLS FIRST", 10, 0);
    agree("s ASC NULLS FIRST, b DESC NULLS LAST", 10, 0);
}

/// The awkward one. Nulls asked for first, arriving in a chunk that is not the first, so the prefix
/// is already full of values by the time the rows that beat all of them turn up. A filter that asks
/// the comparison kernel which rows are below a value gets null back for those rows rather than
/// true, and they go missing from the answer.
#[test]
fn nulls_asked_for_first_that_arrive_after_the_prefix_is_full() {
    same(&placed("file_row_number > 2000", "ASC NULLS FIRST", 10));
    same(&placed("file_row_number > 2000", "DESC NULLS FIRST", 10));
    same(&placed("file_row_number % 1000 > 990", "ASC NULLS FIRST", 20));
}

/// The other one. Nulls asked for last and enough of them early that the worst candidate is itself
/// null, so the value the filter compares against is null and nothing comes back below it. The rows
/// that arrive later are values and every one of them beats a null.
#[test]
fn values_that_arrive_after_the_prefix_has_filled_with_nulls() {
    same(&placed("file_row_number BETWEEN 6 AND 2000", "ASC NULLS LAST", 10));
    same(&placed("file_row_number BETWEEN 6 AND 2000", "DESC NULLS LAST", 10));
}

/// Nulls asked for first over a column that has none, where there is nothing for them to get wrong.
#[test]
fn nulls_first_over_a_column_that_has_no_nulls() {
    agree("t ASC NULLS FIRST", 10, 0);
    agree("b DESC NULLS FIRST", 10, 0);
}

/// The rows that are skipped have to be found before they can be skipped, so an offset makes the
/// prefix longer and the bound looser without changing anything else.
#[test]
fn an_offset_makes_the_prefix_longer() {
    agree("t ASC NULLS LAST", 10, 50);
    agree("s ASC NULLS FIRST", 5, 20);
}

/// Either side of the width where the operator stops holding a sorted prefix and starts collecting
/// and trimming, which is the other path through the same operator.
#[test]
fn either_side_of_the_sorted_bound() {
    agree("t ASC NULLS LAST", 64, 0);
    agree("t ASC NULLS LAST", 65, 0);
    agree("a ASC NULLS LAST, b DESC NULLS LAST", 200, 0);
}

/// The batched path rejecting a chunk against the worst candidate it had at its last trim, which is
/// the same filter the sorted path runs and is a step behind rather than exact.
///
/// The bounds here are all above 64 and small enough against a 4096 row file that the running is
/// trimmed several times, which is what puts a bound in the hand of the pass at all. The three cases
/// are the three the sorted path has: a key with no nulls, a first key almost every row ties, and a
/// key that is an expression.
#[test]
fn the_batched_path_rejects_a_chunk_against_its_last_trim() {
    agree("t ASC NULLS LAST", 100, 0);
    agree("t DESC NULLS LAST", 100, 0);
    agree("a ASC NULLS LAST, b ASC NULLS LAST", 100, 0);
    agree("b % 7 ASC NULLS LAST, t ASC NULLS LAST", 128, 0);
}

/// The same two awkward cases the sorted path has, at a bound that takes the batched path instead.
///
/// Nulls asked for first arriving after the running has filled with values, and values arriving
/// after it has filled with nulls. Both of them are where a filter that asks which rows are below a
/// value gets null back rather than true, and both have to make the pass step aside.
#[test]
fn the_batched_path_steps_aside_for_the_same_nulls_the_sorted_one_does() {
    agree("s ASC NULLS FIRST", 100, 0);
    agree("s ASC NULLS LAST", 100, 0);
    agree("s DESC NULLS FIRST", 100, 0);
    same(&placed("file_row_number > 2000", "ASC NULLS FIRST", 100));
    same(&placed("file_row_number BETWEEN 6 AND 2000", "ASC NULLS LAST", 100));
}

/// An offset at a bound that takes the batched path, which is the shape four of the ClickBench
/// queries have and the reason the filter was put on this path.
#[test]
fn the_batched_path_with_an_offset_under_the_bound() {
    agree("t ASC NULLS LAST", 10, 1000);
    agree("a DESC NULLS LAST, b ASC NULLS LAST", 10, 200);
    agree("s ASC NULLS FIRST", 10, 100);
}

/// A limit that asks for more than the file has, so the prefix never fills and the filter never has
/// a bound to compare against.
#[test]
fn a_limit_past_the_end_of_the_file() {
    agree("t ASC NULLS LAST", 5000, 0);
}

/// An offset past the end of the file, where every row the operator kept is skipped and the answer
/// is empty. The offset is what decides how many rows are put in order at the close, so an offset
/// with nothing behind it is the one place that ordering has nothing to work on.
#[test]
fn an_offset_past_the_end_of_the_file() {
    agree("t ASC NULLS LAST", 10, 5000);
    agree("s ASC NULLS FIRST", 10, 4096);
    agree("a DESC NULLS LAST, b ASC NULLS LAST", 10, 4090);
}

/// Ties that no key breaks, at a bound the batched path takes and with an offset over them.
///
/// The close puts only the rows the offset asks for in order and leaves the rest where they fell,
/// so a tie that is broken by the order the rows arrived in has to survive a pass that does not
/// keep equal rows where it found them. It does, because arrival is unique per row and no two
/// candidates ever compare equal, but this is the shape that would show it if it did not.
#[test]
fn ties_under_an_offset_on_the_batched_path() {
    agree("flag ASC NULLS LAST", 10, 1000);
    agree("a DESC NULLS LAST", 10, 2000);
    agree("d ASC NULLS LAST", 20, 500);
}

/// A key that is not a column, which the filter reads out of the vector the expression produced the
/// same way it reads one that is.
#[test]
fn a_key_that_is_an_expression() {
    agree("b % 7 ASC NULLS LAST, t ASC NULLS LAST", 10, 0);
    agree("(a + 1) * 2 DESC NULLS LAST, b ASC NULLS LAST", 10, 0);
}

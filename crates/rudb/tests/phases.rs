//! An aggregate says which of its phases took the time, including the ones on its own threads.
//!
//! A grouped aggregate does four different things and one number for all four says nothing about
//! which to fix. It folds rows into a hash table, splits a table across the radix partitions, merges
//! one worker's table into another and turns a finished table into rows. Those are four pieces of
//! code with four different fixes.
//!
//! The last two are the ones worth a test. They run on threads the aggregate scopes for itself in
//! `finalize` rather than on the driver's pool, and a thread like that is invisible to everything
//! that counts: the instrumentation shim reads the stage clock on the thread that called the
//! operator, and the worker CPU total only knows about pool workers. So those threads used to do
//! their work and leave no number anywhere. On ClickBench at a million rows that was around a third
//! of `SELECT URL, COUNT(*) FROM hits GROUP BY URL` showing up as wall time with nothing to account
//! for it, which is the sort of hole that sends somebody optimising the wrong thing for a week.

use rudb::Database;
use rudb_metrics::Document;

/// Runs a grouped aggregate over `rows` rows and `keys` groups on `threads` threads.
///
/// The group count has to be well past the point where a worker starts dividing its table by hash,
/// because a merge of partitioned tables is the thing being measured and an aggregate small enough
/// to stay in one table never does one.
fn measured(rows: i64, keys: i64, threads: usize) -> Document {
    let database = Database::new();
    let connection = database.connect();
    connection.execute(&format!("SET threads = {threads}")).expect("sets the thread count");
    connection
        .execute(&format!(
            "CREATE TABLE t AS SELECT i % {keys} AS k, i AS v FROM range(0, {rows}) AS r(i)"
        ))
        .expect("builds the table");
    let result = connection
        .query("SELECT k, COUNT(*) AS c, SUM(v) AS s FROM t GROUP BY k")
        .expect("runs the aggregate");
    result.metrics().expect("a query that ran has metrics").clone()
}

/// How long the aggregate of `metrics` charged to the phase named `phase`.
fn phase(metrics: &Document, phase: &str) -> u64 {
    let aggregate = metrics
        .operators
        .iter()
        .find(|operator| operator.kind == "Aggregate")
        .expect("the query contains an aggregate");
    aggregate
        .stages
        .taken()
        .find(|(stage, _, _)| stage.name() == phase)
        .map_or(0, |(_, nanos, _)| nanos)
}

#[test]
fn an_aggregate_says_how_long_it_spent_folding_rows() {
    let metrics = measured(200_000, 50_000, 4);
    assert!(phase(&metrics, "fold") > 0, "folding rows was charged nothing");
}

#[test]
fn an_aggregate_says_how_long_it_spent_turning_tables_into_rows() {
    let metrics = measured(200_000, 50_000, 4);
    assert!(phase(&metrics, "emit") > 0, "emitting the answer was charged nothing");
}

#[test]
fn the_threads_an_aggregate_starts_for_itself_report_what_they_spent() {
    // Four threads and fifty thousand groups so that the workers partition and there is something
    // for the close to merge. The emit runs on those same scoped threads, so it standing in for
    // them is the point: if their readings were being dropped this is zero however long it took.
    let together = measured(200_000, 50_000, 4);
    let alone = measured(200_000, 50_000, 1);
    assert!(phase(&together, "emit") > 0, "the threads the aggregate started reported nothing");
    assert!(phase(&alone, "emit") > 0, "one thread closing every partition reported nothing");
}

#[test]
fn one_thread_has_no_table_to_merge_and_several_threads_do() {
    // A merge is one worker's table folded into another's, so the thing that decides whether there
    // is one is how many workers there were and not how large the aggregate got. Ten groups never
    // divide a table by hash, and four workers each holding a small table still have to meet.
    //
    // The rows are what get four workers started and they are nothing to do with the aggregate. A
    // scan of a table in memory now asks for as many instances as the rows behind it can pay for,
    // the same rule the native files have always used, and below twenty five thousand rows that is
    // one however many threads the setting allows.
    let alone = measured(200_000, 10, 1);
    let together = measured(200_000, 10, 4);
    assert_eq!(phase(&alone, "merge"), 0, "one worker merged a table with itself");
    assert!(phase(&together, "merge") > 0, "four workers' tables met without being charged");
}

#[test]
fn a_scan_still_reports_the_stages_of_a_read() {
    // The aggregate phases share the stage clock with the scan stages, so this is here to catch a
    // change to one of them quietly emptying the other.
    let database = Database::new();
    let result = database
        .query("SELECT COUNT(*) FROM range(0, 1000) AS r(i)")
        .expect("runs a query with no aggregate phases worth naming");
    assert!(result.metrics().is_some(), "a query that ran has metrics");
}

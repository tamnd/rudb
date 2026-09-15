//! A grouped aggregate on several threads adds up to what went into it.
//!
//! These are arithmetic tests and not plan tests. A parallel group by is right when every input row
//! is counted once by exactly one group, and that is a property you can check without knowing
//! anything about how the rows got divided up: the counts have to sum to the number of rows, the
//! sums have to sum to the sum of the column, and the number of groups has to be the number of
//! distinct keys. Nothing here looks at which thread did what, because that is the part that is
//! allowed to differ from run to run.
//!
//! # What these are guarding
//!
//! The aggregate hands each worker its own set of radix partitioned tables to fold into with no
//! lock, and merges the matching partitions at the end. There are several ways for a table to go
//! missing on the way through that, and every one of them looks the same from outside: the counts
//! come out low. #614 turned the whole design off after finding two of them on ClickBench, where
//! one query returned 629 distinct users for a phrase that has 667 and another returned 16,102
//! visits for a URL that has 16,109. Those are small losses on large numbers, which is exactly what
//! makes them worth a test rather than an eye.
//!
//! The shapes below are the ones that decide where a worker's table goes. Whether the aggregate
//! ever grew large enough to start partitioning, whether a given worker grew large enough to
//! partition on its own before the input ran out, and whether the key is one a worker can carry
//! cheaply. A worker that finishes holding a table it never partitioned, while the aggregate as a
//! whole is partitioning, is the case that was losing everything that worker had folded.

use rudb::Database;
use rudb_common::Value;

/// How many groups one worker holds before it starts dividing them by hash. Not reachable from
/// here, so it is written down rather than imported, and the test that would notice it moving is
/// the one below that straddles it.
const PARTITION_FROM: i64 = 4_096;

/// A database holding `t(k BIGINT, v BIGINT)` with `rows` rows over `keys` distinct keys.
///
/// Key `i` is `i % keys`, so the groups are even and the number of distinct keys is known exactly
/// without a second query having to be trusted to find it out.
fn built(rows: i64, keys: i64, threads: usize) -> Database {
    let database = Database::new();
    let connection = database.connect();
    connection.execute(&format!("SET threads = {threads}")).expect("sets the thread count");
    connection
        .execute(&format!(
            "CREATE TABLE t AS SELECT i % {keys} AS k, i AS v FROM range(0, {rows}) AS r(i)"
        ))
        .expect("builds the table");
    database
}

/// Asserts the three sums a correct group by has to produce, whatever it did with the threads.
///
/// All three and not just the count, because they fail differently. A lost worker table takes its
/// counts and its sums together. A group counted twice leaves the count right and the number of
/// groups wrong. A key compared wrongly leaves the totals right and splits one group into two.
fn adds_up(database: &Database, rows: i64, keys: i64) {
    let connection = database.connect();
    let counted = connection
        .value("SELECT SUM(c) FROM (SELECT k, COUNT(*) AS c FROM t GROUP BY k)")
        .expect("counts");
    assert_eq!(counted, Value::HugeInt(i128::from(rows)), "the counts do not add up to the rows");

    let grouped = connection
        .value("SELECT COUNT(*) FROM (SELECT k FROM t GROUP BY k)")
        .expect("counts the groups");
    assert_eq!(grouped, Value::BigInt(keys), "the number of groups is not the distinct keys");

    let summed = connection
        .value("SELECT SUM(s) FROM (SELECT k, SUM(v) AS s FROM t GROUP BY k)")
        .expect("sums");
    let whole = i128::from(rows) * i128::from(rows - 1) / 2;
    assert_eq!(summed, Value::HugeInt(whole), "the sums do not add up to the whole column");
}

/// The shape #614 found. Enough keys that some worker crosses the threshold and turns partitioning
/// on for everybody, and few enough rows that the workers which did not cross it finish still
/// holding a table of their own. That second kind of worker is the one whose table was being
/// scattered into tables nobody ever read.
#[test]
fn a_worker_that_never_partitioned_still_has_its_rows_counted() {
    let keys = PARTITION_FROM * 4;
    let rows = keys * 3;
    adds_up(&built(rows, keys, 8), rows, keys);
}

/// The same thing with the threads turned up, because how the rows get divided is what decides
/// which workers cross the threshold and which do not, and more workers means more of the second
/// kind.
#[test]
fn the_same_shape_on_more_threads_than_there_are_radix_partitions() {
    let keys = PARTITION_FROM * 4;
    let rows = keys * 3;
    adds_up(&built(rows, keys, 32), rows, keys);
}

/// Right at the threshold, where a worker can end up on either side of it. This is the one that
/// would notice `PARTITION_FROM` moving out from under the constant above.
#[test]
fn a_group_count_that_straddles_the_partition_threshold_adds_up() {
    // row at a time: three sizes around the threshold, each a whole query.
    for keys in [PARTITION_FROM - 1, PARTITION_FROM, PARTITION_FROM + 1] {
        let rows = keys * 5;
        adds_up(&built(rows, keys, 8), rows, keys);
    }
}

/// Too few groups for anybody to partition at all, which is the path that keeps one table per
/// worker and merges them in a line at the end. Cheap to check and it is half the queries.
#[test]
fn an_aggregate_too_small_to_partition_adds_up() {
    adds_up(&built(50_000, 16, 8), 50_000, 16);
}

/// A string key, because it is stored and compared differently from a `BIGINT` and it is the key
/// ClickBench loses counts on. The keys are distinct strings, so the group count is the key count
/// the same way.
#[test]
fn a_string_key_on_several_threads_adds_up() {
    let keys = PARTITION_FROM * 4;
    let rows = keys * 3;
    let database = Database::new();
    let connection = database.connect();
    connection.execute("SET threads = 8").expect("sets the thread count");
    connection
        .execute(&format!(
            "CREATE TABLE t AS SELECT 'key-' || (i % {keys}) AS k, i AS v \
             FROM range(0, {rows}) AS r(i)"
        ))
        .expect("builds the table");

    let counted = connection
        .value("SELECT SUM(c) FROM (SELECT k, COUNT(*) AS c FROM t GROUP BY k)")
        .expect("counts");
    assert_eq!(counted, Value::HugeInt(i128::from(rows)), "the counts do not add up to the rows");
    let grouped = connection
        .value("SELECT COUNT(*) FROM (SELECT k FROM t GROUP BY k)")
        .expect("counts the groups");
    assert_eq!(grouped, Value::BigInt(keys), "the number of groups is not the distinct keys");
}

/// Two key columns, which is a different stored width and a different comparison again, and is the
/// shape of the ClickBench queries that group by a pair.
#[test]
fn a_two_column_key_on_several_threads_adds_up() {
    let keys = PARTITION_FROM * 4;
    let rows = keys * 3;
    let database = built(rows, keys, 8);
    let connection = database.connect();
    let counted = connection
        .value("SELECT SUM(c) FROM (SELECT k, v % 7 AS b, COUNT(*) AS c FROM t GROUP BY k, b)")
        .expect("counts");
    assert_eq!(counted, Value::HugeInt(i128::from(rows)), "the counts do not add up to the rows");
}

/// The same query asked repeatedly, because which worker ends up in which state is a race and a
/// test that runs it once can miss. Ten is enough to have caught the lost table every time it was
/// tried by hand.
#[test]
fn the_same_aggregate_answers_the_same_way_every_time() {
    let keys = PARTITION_FROM * 4;
    let rows = keys * 3;
    let database = built(rows, keys, 8);
    // row at a time: the point is that the answer repeats, so each run is a case.
    for run in 0..10 {
        let counted = database
            .connect()
            .value("SELECT SUM(c) FROM (SELECT k, COUNT(*) AS c FROM t GROUP BY k)")
            .expect("counts");
        assert_eq!(counted, Value::HugeInt(i128::from(rows)), "run {run} lost rows");
    }
}

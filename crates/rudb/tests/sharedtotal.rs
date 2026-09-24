//! A `sum` that reads its total out of an `avg` of the same column answers what a `sum` of its own
//! answers.
//!
//! A sum and a mean of one column add the same numbers up, and the mean keeps the count it divides
//! by besides, so an aggregate asking for both only has to fold the column once. The sum is the one
//! that stops folding, because a mean's state already holds everything a sum's does. That is a
//! rewrite nothing about the answer is supposed to notice, so what is checked here is only that:
//! the same query with the mean taken away has to give the same sums back.
//!
//! # What these are guarding
//!
//! Three things, all of them ways the sharing could be right about the plan and wrong about the
//! rows.
//!
//! The mean has to be the one that folds and the sum the one that reads, and the two have to be
//! wired to the same accumulator. Getting that backwards or off by a call gives an answer that is
//! some other column's total, which is why every case below puts more than one aggregate in the
//! query.
//!
//! The types have to be the ones where the two additions are the same addition. An integer or a
//! decimal mean adds the unscaled integers up whole and divides once at the end, so its total is a
//! sum. A `DOUBLE` mean adds in floating point in the order the rows arrive, and so does a `DOUBLE`
//! sum, so a total read out of anything but those same additions rounds differently. The doubles
//! below are picked so that it shows: a large value and small ones after it, where adding whole and
//! adding in floating point reach different numbers.
//!
//! A `DISTINCT` or a `FILTER` on either call makes the two fold different rows, so neither can be
//! read out of the other at all.

use rudb::Database;
use rudb_common::Value;

/// Every row of a result, as values, so an ordered answer can be compared whole.
fn rows(database: &Database, query: &str) -> Vec<Vec<Value>> {
    database.query(query).expect("the query ran").rows().collect()
}

/// Asserts that `query` answers what `wanted` answers, and that it answered something.
///
/// The two are run on one database rather than two, because what is being checked is a decision the
/// aggregate makes about its own call list and not anything about the data.
fn same(database: &Database, query: &str, wanted: &str) {
    let got = rows(database, query);
    assert!(!got.is_empty(), "{query} answered nothing, so it proved nothing");
    assert_eq!(got, rows(database, wanted), "{query} disagrees with {wanted}");
}

/// A table of one key column and one column of every type a sum can be shared over, plus a `DOUBLE`
/// where it cannot be.
///
/// The rows are ordered so that the doubles add up differently whole than in floating point. Row 0
/// of each group carries `1e17`, whose next representable neighbour is 16 away, and every row after
/// it carries `1.0`. Each of those adds rounds back to `1e17`, so a floating point sum of the group
/// is `1e17` exactly while a whole sum of it is `1e17` plus the count. That is the difference a
/// shared total would introduce if the rule ever reached a `DOUBLE`, and it is only visible if the
/// large value comes first.
fn built() -> Database {
    let database = Database::new();
    let connection = database.connect();
    connection.execute("SET threads = 1").expect("sets the thread count");
    connection
        .execute(
            "CREATE TABLE t AS
             SELECT i % 4 AS k,
                    CAST(i AS TINYINT) AS small,
                    CAST(i * 3 AS INTEGER) AS whole,
                    CAST(i AS HUGEINT) * 1000000000000000000 AS huge,
                    CAST(i AS DECIMAL(18, 4)) / 7 AS money,
                    CAST(i AS DECIMAL(30, 6)) / 3 AS wide,
                    CASE WHEN i < 4 THEN 1e17 ELSE 1.0 END AS loose,
                    CASE WHEN i % 5 = 0 THEN NULL ELSE CAST(i AS BIGINT) END AS gappy
             FROM range(0, 97) AS r(i)",
        )
        .expect("builds the table");
    database
}

/// Every column the rule is meant to reach, grouped, one column at a time so that a wrong answer
/// names the type it came from.
#[test]
fn a_sum_beside_a_mean_of_the_same_column_is_the_sum_alone() {
    let database = built();
    for column in ["small", "whole", "huge", "money", "wide", "gappy"] {
        same(
            &database,
            &format!("SELECT k, SUM({column}), AVG({column}) FROM t GROUP BY k ORDER BY k"),
            &format!(
                "SELECT k, SUM({column}), SUM({column}) / COUNT({column}) FROM t GROUP BY k ORDER BY k"
            ),
        );
    }
}

/// The same without a `GROUP BY`, which is a different path through the aggregate: one group made
/// up front and every call folded by vector.
#[test]
fn the_same_over_the_whole_table_with_no_grouping() {
    let database = built();
    for column in ["small", "whole", "huge", "money", "wide", "gappy"] {
        same(
            &database,
            &format!("SELECT SUM({column}), AVG({column}) FROM t"),
            &format!("SELECT SUM({column}), SUM({column}) / COUNT({column}) FROM t"),
        );
    }
}

/// The order the two calls are written in, because which one folds is decided by the call list and
/// a rule that only worked one way round would pass every case above.
#[test]
fn the_mean_written_before_the_sum_answers_the_same() {
    let database = built();
    same(
        &database,
        "SELECT k, AVG(whole), SUM(whole) FROM t GROUP BY k ORDER BY k",
        "SELECT k, SUM(whole) / COUNT(whole), SUM(whole) FROM t GROUP BY k ORDER BY k",
    );
}

/// Several columns at once with the pairs interleaved, which is q01's shape and the one where a sum
/// wired to the wrong accumulator gives a number that is some other column's total.
#[test]
fn two_pairs_interleaved_each_read_their_own_column() {
    let database = built();
    same(
        &database,
        "SELECT k, SUM(whole), SUM(money), AVG(whole), AVG(money), COUNT(*)
         FROM t GROUP BY k ORDER BY k",
        "SELECT k, SUM(whole), SUM(money), SUM(whole) / COUNT(whole), SUM(money) / COUNT(money),
                COUNT(*)
         FROM t GROUP BY k ORDER BY k",
    );
}

/// A `DOUBLE`, where the mean's total is not the sum. Only the sum column is compared, because the
/// mean has no column of its own to be compared against: what is being checked is that putting a mean
/// beside the sum left the sum where it was.
#[test]
fn a_double_sum_beside_a_mean_is_still_the_floating_point_sum() {
    let database = built();
    let beside = rows(&database, "SELECT k, SUM(loose), AVG(loose) FROM t GROUP BY k ORDER BY k");
    let alone = rows(&database, "SELECT k, SUM(loose) FROM t GROUP BY k ORDER BY k");
    assert_eq!(beside.len(), alone.len(), "the two queries found different numbers of groups");
    assert!(!alone.is_empty(), "the query answered nothing, so it proved nothing");
    for (beside, alone) in beside.iter().zip(&alone) {
        assert_eq!(beside[..2], alone[..], "a double sum changed when a mean was put beside it");
    }
}

/// The value that shows a `DOUBLE` sum is not a whole one. Group 0 holds `1e17` and then 24 ones,
/// each of which rounds back to `1e17` when it is added, so the floating point sum is `1e17` exactly.
/// A total added up whole and handed back would be `1e17` plus 24.
#[test]
fn the_double_column_is_one_where_the_two_additions_disagree() {
    let database = built();
    let connection = database.connect();
    let summed = connection
        .value("SELECT SUM(loose) FROM t WHERE k = 0")
        .expect("sums the doubles of one group");
    assert_eq!(summed, Value::Double(1e17), "the doubles no longer round away, so pick others");

    let whole = connection
        .value("SELECT SUM(CAST(loose AS HUGEINT)) FROM t WHERE k = 0")
        .expect("sums the same doubles whole");
    assert_ne!(
        whole,
        Value::HugeInt(100_000_000_000_000_000),
        "the whole sum agrees with the floating point one, so this proves nothing"
    );
}

/// A `DISTINCT` or a `FILTER` on either call, where the two fold different rows and neither can be
/// read out of the other.
#[test]
fn a_distinct_or_a_filter_on_either_call_keeps_both_folding() {
    let database = built();
    same(
        &database,
        "SELECT k, SUM(DISTINCT whole), AVG(whole) FROM t GROUP BY k ORDER BY k",
        "SELECT k, SUM(DISTINCT whole), SUM(whole) / COUNT(whole) FROM t GROUP BY k ORDER BY k",
    );
    same(
        &database,
        "SELECT k, SUM(whole), AVG(DISTINCT whole) FROM t GROUP BY k ORDER BY k",
        "SELECT k, SUM(whole), SUM(DISTINCT whole) / COUNT(DISTINCT whole)
         FROM t GROUP BY k ORDER BY k",
    );
    same(
        &database,
        "SELECT k, SUM(whole) FILTER (WHERE whole > 100), AVG(whole)
         FROM t GROUP BY k ORDER BY k",
        "SELECT k, SUM(CASE WHEN whole > 100 THEN whole END), SUM(whole) / COUNT(whole)
         FROM t GROUP BY k ORDER BY k",
    );
    same(
        &database,
        "SELECT k, SUM(whole), AVG(whole) FILTER (WHERE whole > 100)
         FROM t GROUP BY k ORDER BY k",
        "SELECT k, SUM(whole), AVG(CASE WHEN whole > 100 THEN whole END)
         FROM t GROUP BY k ORDER BY k",
    );
}

/// A group whose rows are all null, where the mean has a total of zero and nothing seen. The sum of
/// that is null and not zero, so this is the case where reading the total without reading whether
/// anything landed in it gives the wrong answer.
#[test]
fn a_group_of_nothing_but_nulls_sums_to_null() {
    let database = Database::new();
    let connection = database.connect();
    connection
        .execute(
            "CREATE TABLE u AS SELECT i % 3 AS k,
                                      CASE WHEN i % 3 = 1 THEN NULL ELSE CAST(i AS BIGINT) END AS v
                               FROM range(0, 30) AS r(i)",
        )
        .expect("builds the table");
    same(
        &database,
        "SELECT k, SUM(v), AVG(v) FROM u GROUP BY k ORDER BY k",
        "SELECT k, SUM(v), SUM(v) / COUNT(v) FROM u GROUP BY k ORDER BY k",
    );
    let answered = rows(&database, "SELECT k, SUM(v), AVG(v) FROM u GROUP BY k ORDER BY k");
    assert_eq!(answered[1][1], Value::Null, "a group of nothing but nulls did not sum to null");
}

/// A total too large for the `i128` a mean keeps, which is where the mean goes inexact and has no
/// exact total left to read. A sum of the same column raises on the row that stops fitting, so the
/// query is expected to fail either way and what is checked is that it fails rather than answering.
#[test]
fn a_total_too_large_for_an_exact_mean_still_raises() {
    let database = Database::new();
    let connection = database.connect();
    connection
        .execute(
            "CREATE TABLE big AS SELECT 1 AS k, CAST(170141183460469231731687303715884105727 AS HUGEINT) AS v
             FROM range(0, 4) AS r(i)",
        )
        .expect("builds the table");
    assert!(
        connection.query("SELECT k, SUM(v), AVG(v) FROM big GROUP BY k").is_err(),
        "a total past the end of a HUGEINT answered instead of raising"
    );
    assert!(
        connection.query("SELECT k, SUM(v) FROM big GROUP BY k").is_err(),
        "the same sum without the mean answered instead of raising"
    );
}

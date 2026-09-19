//! The planner's row count for a filter, held under what the file's zone maps allow.
//!
//! A Parquet writer records the smallest and the largest value of every column of every row group.
//! `pruned.rs` is about a scan walking past the groups a filter rules out, which is the reading
//! side. This is the planning side of the same two numbers, and the planner asks them two different
//! questions.
//!
//! The first is how many rows are left in the groups nothing ruled out. That is a ceiling the answer
//! cannot exceed and the estimator holds its guess under it. The second is what fraction of each
//! surviving group a range keeps, interpolated between that group's two ends. That is a guess and
//! not a ceiling, so it replaces the constant rather than capping it, and it can be wrong in either
//! direction.
//!
//! Every test here has two halves. One says what the estimate came out as, and one says what the
//! query actually answers, because an estimate below the truth is the failure mode this has and it
//! does not look like anything until a plan is built on it. `sorted.parquet` is written in ascending
//! key order on purpose, which is what makes a filter on that key rule groups out at all and what
//! makes each group's two ends narrow enough to interpolate between, and its README entry says so.
//! `scaled.parquet` is the same idea over a decimal and two timestamps, which are the columns whose
//! two ends are an integer and a power of ten under it rather than an integer on its own.

use rudb::Database;
use rudb_common::{LogicalType, Value};

/// The fixture, which is eight row groups of 2048 rows with `k` ascending across them.
fn fixture() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rudb-parquet/testdata/sorted.parquet");
    format!("'{path}'")
}

/// The plan text for a query.
fn explained(database: &Database, sql: &str) -> String {
    let result = database.query(sql).expect("the explain ran");
    assert_eq!(result.types(), [LogicalType::Varchar, LogicalType::Varchar]);
    match result.value_at(0, 1) {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    }
}

/// The line of the plan the filter is on, which is where the estimate for it is printed.
fn filter_line(text: &str) -> String {
    text.lines()
        .find(|line| line.trim_start().starts_with("Filter "))
        .unwrap_or_else(|| panic!("no filter in {text}"))
        .to_string()
}

/// The other fixture, eight row groups of 1024 rows with a decimal and two timestamps ascending.
fn scaled() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../rudb-parquet/testdata/scaled.parquet");
    format!("'{path}'")
}

/// How many rows the filter really keeps.
fn answered(database: &Database, predicate: &str) -> i64 {
    counted(database, &fixture(), predicate)
}

/// The estimate the planner prints for that filter.
fn estimated(database: &Database, predicate: &str) -> String {
    planned(database, &fixture(), predicate)
}

/// How many rows the filter really keeps in `file`.
fn counted(database: &Database, file: &str, predicate: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM read_parquet({file}) WHERE {predicate}");
    let result = database.query(&sql).expect("the count ran");
    match result.value_at(0, 0) {
        Value::BigInt(count) => count,
        other => panic!("the count came back as {other:?}"),
    }
}

/// The estimate the planner prints for that filter over `file`.
fn planned(database: &Database, file: &str, predicate: &str) -> String {
    let sql = format!("EXPLAIN SELECT 1 FROM read_parquet({file}) WHERE {predicate}");
    filter_line(&explained(database, &sql))
}

#[test]
fn a_filter_the_bounds_rule_every_group_out_of_is_exactly_no_rows() {
    // Not an estimate. Every group states a maximum below the constant, so no row of the file can
    // pass, and that is a fact about this file rather than a fraction of it. The one answer here
    // allowed below the one row floor the guess has.
    let database = Database::new();
    let line = estimated(&database, "k > 999999");
    assert!(line.contains("[0 rows exact from zone map]"), "{line}");
    assert_eq!(answered(&database, "k > 999999"), 0, "and it is right");
}

#[test]
fn a_ceiling_under_the_guess_is_what_the_planner_gets() {
    // Equality is the case the ceiling is still the whole answer for. One value out of a range is
    // not a fraction a range knows, so nothing interpolates, the guess stays the constant's 3,276,
    // and the bounds leave one group of 2,048 that no answer to this filter can exceed. That is the
    // estimate and it is marked as a bound rather than as a guess. The truth is one, so this is
    // still enormously too many, and a distinct count is what would answer it: the footer does not
    // state one for `k`.
    let database = Database::new();
    let line = estimated(&database, "k = 5000");
    assert!(line.contains("[2048 rows certified at most 100.00% from zone map]"), "{line}");
    assert_eq!(answered(&database, "k = 5000"), 1);
}

#[test]
fn a_range_is_interpolated_inside_the_groups_that_survive() {
    // The ceiling leaves one group of 2,048, which is twenty times the truth. Interpolating inside
    // that group is what closes the rest of the gap: `k` runs from 0 to 2047 there, the constant
    // cuts it at 100, and none of the other seven groups contributes anything. The answer is a
    // hundred and so is the truth.
    //
    // It is reported as a guess and not as a bound, because the values being spread evenly between
    // a group's two ends is an assumption about the data rather than something the footer states.
    // Here the file happens to be exactly that, which is why it lands exactly.
    let database = Database::new();
    let line = estimated(&database, "k < 100");
    assert!(line.contains("[~100 rows estimated from zone map]"), "{line}");
    assert_eq!(answered(&database, "k < 100"), 100);
}

#[test]
fn a_range_the_bounds_rule_no_group_out_of_is_still_interpolated() {
    // Half the groups survive whole and the other four contribute nothing, so the ceiling is 8,192.
    // A ceiling says nothing about how far under it the answer sits and is still never applied
    // upwards, and it is not what gives the number here: the interpolation arrives at the same
    // 8,192 on its own, which is the truth exactly. What this used to answer was the constant's
    // 3,276, two and a half times under.
    let database = Database::new();
    let line = estimated(&database, "k >= 8192");
    assert!(line.contains("[~8192 rows estimated from zone map]"), "{line}");
    assert_eq!(answered(&database, "k >= 8192"), 8192);
}

#[test]
fn a_column_whose_groups_all_look_alike_is_estimated_the_way_it_always_was() {
    // The control for the bounds. `g` is the row number modulo ninety seven, so every group runs
    // from 0 to 96 and no filter on it rules anything out. The ceiling is the whole file and the
    // bounds change nothing, which is what this is here to show.
    //
    // The number is not the constant any more because the footer counts `g` at ninety seven and an
    // equality against a constant takes one value out of the count. Sixteen thousand rows over
    // ninety seven is 168 against a truth of 169, where the constant said 3,276.
    let database = Database::new();
    let line = estimated(&database, "g = 5");
    assert!(line.contains("[~168 rows estimated from dictionary]"), "{line}");
    assert_eq!(answered(&database, "g = 5"), 169);
}

#[test]
fn the_conjuncts_of_one_filter_are_asked_together_rather_than_one_at_a_time() {
    // A group survives only if no condition rules it out, so the ceiling is the two groups from 0
    // to 4095, which is 4,096 rows. What this used to answer was a fifth of a fifth, 655, six times
    // under the truth of 3,900, and that compounding is what the estimator's own comment calls the
    // part most likely to be wrong.
    //
    // Two conditions on one column are one interval and not two independent events, so the pair is
    // intersected into 100 up to 3999 and interpolated once. That is 3,900 and so is the truth.
    // Multiplying them instead would have said 3,975, right here by luck because the interval covers
    // most of both groups, and much too narrow over a `BETWEEN` that names a slice of one.
    let database = Database::new();
    let line = estimated(&database, "k >= 100 AND k < 4000");
    assert!(line.contains("[~3900 rows estimated from zone map]"), "{line}");
    assert_eq!(answered(&database, "k >= 100 AND k < 4000"), 3900);
}

#[test]
fn a_decimal_column_is_interpolated_over_the_hundredths_it_holds() {
    // `d` is stored as an `INT64` of hundredths, so the footer's 1000 is 10.00 and comparing the
    // two integers as they are would be comparing 10.00 against 0.10. All three of these come out
    // exactly, because the column is one row per hundredth and the interpolation's assumption that
    // the values are spread evenly between a group's ends is true here rather than assumed.
    let database = Database::new();
    for (predicate, rows) in
        [("d < 10.00", 1000), ("d >= 50.00", 3192), ("d BETWEEN 20.00 AND 30.00", 1001)]
    {
        let line = planned(&database, &scaled(), predicate);
        assert!(line.contains(&format!("[~{rows} rows estimated from zone map]")), "{line}");
        assert_eq!(counted(&database, &scaled(), predicate), rows, "{predicate}");
    }
}

#[test]
fn a_decimal_past_every_group_is_exactly_no_rows() {
    // The other half of what the bounds answer, on a decimal rather than an integer. `d` stops at
    // 81.91 and the constant is past every group, which is a fact about the file and not a guess.
    let database = Database::new();
    let line = planned(&database, &scaled(), "d > 999.00");
    assert!(line.contains("[0 rows exact from zone map]"), "{line}");
    assert_eq!(counted(&database, &scaled(), "d > 999.00"), 0, "and it is right");
}

#[test]
fn a_timestamp_column_is_interpolated_over_the_unit_the_file_chose() {
    // One row a minute from 2020-01-01, so a day is 1,440 rows and the rest of the file is 2,432.
    // The second is out by one, which is the boundary landing between two groups rather than on a
    // group's edge, and one row in two thousand is not what a q-error notices.
    let database = Database::new();
    let day = "t < TIMESTAMP '2020-01-02 00:00:00'";
    let line = planned(&database, &scaled(), day);
    assert!(line.contains("[~1440 rows estimated from zone map]"), "{line}");
    assert_eq!(counted(&database, &scaled(), day), 1440);

    let rest = "t >= TIMESTAMP '2020-01-05 00:00:00'";
    let line = planned(&database, &scaled(), rest);
    assert!(line.contains("[~2431 rows estimated from zone map]"), "{line}");
    assert_eq!(counted(&database, &scaled(), rest), 2432);
}

#[test]
fn a_millisecond_column_is_reached_through_a_cast_and_the_bounds_are_not_asked_through_one() {
    // `ms` holds the same instants as `t` in a different unit and its bounds carry that unit, so
    // this would interpolate as well as `t` does if it were asked. It is not asked. The column is a
    // `TIMESTAMP_MS` and the constant is a `TIMESTAMP`, so the binder puts a cast around the column,
    // and what the estimator is handed is that cast rather than a column it has two ends for. The
    // estimate falls back to the constant's fifth.
    //
    // The count on the right should be 1440 and is the whole file, which is issue #959. A count of
    // milliseconds out of Parquet is read here as a count of microseconds, so every value in the
    // column sits in 1970 and every one of them is under a constant in 2020. That is a wrong answer
    // rather than an estimate, it predates the bounds and the binary from before them answers the
    // same, and fixing it is cast kernel work. This pins what the engine does today, and the number
    // becomes 1440 when #959 closes.
    let database = Database::new();
    let predicate = "ms < TIMESTAMP '2020-01-02 00:00:00'";
    let line = planned(&database, &scaled(), predicate);
    assert!(line.contains("[~1638 rows estimated from default]"), "{line}");
    assert_eq!(counted(&database, &scaled(), predicate), 8192, "issue #959, it should be 1440");
}

#[test]
fn a_filter_the_bounds_cannot_read_asks_them_nothing() {
    // A comparison of two columns says nothing a minimum and a maximum can answer, because the
    // bounds of one column say nothing about the other's value in the same row. So there is no
    // test to ask with and no ceiling to apply, and the estimate is the constant.
    let database = Database::new();
    let line = estimated(&database, "k = g");
    assert!(line.contains("[~3276 rows estimated from default]"), "{line}");
    assert_eq!(answered(&database, "k = g"), 97, "k is 0 to 16383 and g is k modulo 97");
}

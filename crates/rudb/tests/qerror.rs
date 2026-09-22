//! How far the plan's row counts were from the rows the run produced, split by class, end to end.
//!
//! P0's third exit criterion is the q-error histogram published per class, and the rule that comes
//! with it is that a number reported exact whose q-error is not one is a wrong answer bug rather
//! than a bad estimate. These are that criterion asked of the engine rather than of a spreadsheet:
//! the section exists, it is split by class, it only appears when there was a run to measure
//! against, and nothing in the corpus of shapes below contradicts a number it said it knew.
//!
//! `dictionary.rs` and `zoned.rs` are the other half of the same question. They are about where a
//! number came from and this is about whether it was right.

use rudb::Database;
use rudb_common::{LogicalType, Value};

/// A database with a table of the given size, built without a per row `INSERT`.
fn with_rows(count: usize) -> Database {
    let database = Database::new();
    database.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").expect("creates");
    database
        .execute(&format!("INSERT INTO t SELECT r::INTEGER, 'x' FROM range({count}) AS s(r)"))
        .expect("inserts");
    database
}

/// The explain text for a query.
fn explained(database: &Database, sql: &str) -> String {
    let result = database.query(sql).expect("the explain ran");
    assert_eq!(result.types(), [LogicalType::Varchar, LogicalType::Varchar]);
    match result.value_at(0, 1) {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    }
}

/// The Statistics section, which is where the histogram goes.
fn statistics(database: &Database, sql: &str) -> String {
    let text = explained(database, &format!("EXPLAIN (ANALYZE, STATISTICS) {sql}"));
    let (_, section) = text.split_once("\nStatistics\n").unwrap_or_else(|| panic!("{text}"));
    let (section, _) = section.split_once("\nTotals\n").unwrap_or_else(|| panic!("{text}"));
    section.to_string()
}

/// The Warnings section, which is empty for a query that has nothing wrong with it.
fn warnings(database: &Database, sql: &str) -> String {
    let text = explained(database, &format!("EXPLAIN ANALYZE {sql}"));
    match text.split_once("\nWarnings\n") {
        Some((_, section)) => section.to_string(),
        None => String::new(),
    }
}

#[test]
fn the_statistics_section_measures_every_class_against_the_run() {
    // Two classes in one plan. The scan's count comes out of the catalog and is exact, so its
    // q-error has to be one. The group by over it is read off the distinct count of the column it
    // groups by, which is a guess and stays one however right it turns out, because a sketch that
    // happened to land is still a sketch. Printing the two together and undivided would say the
    // plan was two thirds right, which is true of neither half of it.
    let database = with_rows(1000);
    let section = statistics(&database, "SELECT a, count(*) FROM t GROUP BY a");
    assert!(section.contains("q-error against the rows the run produced, 3 measured"), "{section}");
    assert!(section.contains("\n    exact 1: 1 at 1\n"), "{section}");
    // Every value of `a` is its own group and nothing filtered any of them away, so the estimate is
    // the distinct count itself and the projection above carries it. The constant this replaced said
    // a tenth of a thousand, which was a hundred groups where there are a thousand.
    assert!(section.contains("\n    estimated 2: 2 at 1\n"), "{section}");
}

#[test]
fn a_scan_that_applies_a_filter_is_measured_against_the_filter_and_not_against_the_table() {
    // The optimizer moves a filter over a stored table into the scan, so one operator does the work
    // of two nodes and the rows it reports are the rows that came out of the filter. The estimate
    // that belongs against those rows is the filter's. Comparing them with the table's exact count
    // instead is comparing two different questions, and it reports the catalog as having counted
    // wrong on every query anybody writes with a `WHERE` on a stored table.
    let database = with_rows(1000);
    let section = warnings(&database, "SELECT a FROM t WHERE a > 5");
    assert!(!section.contains("contradicts"), "{section}");

    // And the line the rows are printed on says which of the two counts it is, because a line that
    // reads `1000 rows exact` next to `994 rows` looks like the count was wrong.
    let text = explained(&database, "EXPLAIN ANALYZE SELECT a FROM t WHERE a > 5");
    assert!(text.contains("[applied by the scan below]"), "{text}");
    assert!(text.contains("[994 rows after the filter above,"), "{text}");
}

#[test]
fn a_plan_that_was_not_run_has_nothing_to_be_measured_against() {
    // The temptation is a section of empty buckets, and a section of empty buckets reads as if
    // every estimate landed. There is no truth here at all, so the section says nothing.
    let database = with_rows(1000);
    let text = explained(&database, "EXPLAIN (STATISTICS) SELECT a FROM t WHERE a > 5");
    assert!(text.contains("\nStatistics\n"), "{text}");
    assert!(text.contains("read to decide:"), "{text}");
    assert!(!text.contains("q-error"), "{text}");
}

#[test]
fn a_limit_does_not_make_the_count_under_it_a_contradiction() {
    // The scan knows exactly how many rows the table holds and produces five of them, because the
    // limit above it stopped the pipeline. The exact count was right and the operator stopped early.
    // This is the readable case of the rule the whole file rests on: a row count in a plan is a
    // ceiling on what execution produces rather than a prediction of it, so only going over it is a
    // contradiction. A rule that fired on both directions would report a wrong answer bug on every
    // query anybody writes with a limit in it, and on most of TPC-H besides.
    let database = with_rows(1000);
    let section = warnings(&database, "SELECT a FROM t LIMIT 5");
    assert!(!section.contains("contradicts"), "{section}");
    // And the q-error is still measured and still says what happened, because the estimate really
    // was two hundred times the rows that came out and that is worth seeing.
    let statistics = statistics(&database, "SELECT a FROM t LIMIT 5");
    assert!(statistics.contains("q-error against the rows the run produced"), "{statistics}");
}

#[test]
fn nothing_in_a_spread_of_shapes_contradicts_a_number_it_said_it_knew() {
    // The milestone rule, asked of every shape that has a statistic behind it today. A run that
    // produces more rows than an exact count said exist, or more than a certificate allows, is a bug
    // in whatever produced the number rather than a bad guess, and this is the test that turns that
    // sentence into something that fails.
    let database = with_rows(1000);
    let file = concat!(env!("CARGO_MANIFEST_DIR"), "/../rudb-parquet/testdata/counted.parquet");
    let queries = [
        "SELECT a FROM t".to_string(),
        "SELECT a FROM t WHERE a > 5".to_string(),
        "SELECT a, count(*) FROM t GROUP BY a".to_string(),
        "SELECT DISTINCT a FROM t".to_string(),
        "SELECT a FROM t ORDER BY a LIMIT 10".to_string(),
        "SELECT l.a FROM t l JOIN t r ON l.a = r.a".to_string(),
        "SELECT count(*) FROM t".to_string(),
        format!("SELECT few FROM read_parquet('{file}') WHERE few = 5"),
        format!("SELECT label, count(*) FROM read_parquet('{file}') GROUP BY label"),
    ];
    for query in &queries {
        let section = warnings(&database, query);
        assert!(!section.contains("contradicts"), "{query}\n{section}");
    }
}

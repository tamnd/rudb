//! A scan that applies the filter instead of handing its rows to one.
//!
//! The scan already asked the zone map of every chunk whether the filter could match anything in it,
//! and walked past the ones where it could not. That is one end of what a pair of bounds knows. The
//! other end is the chunk where the filter matches everything, and acting on it needs the zone and
//! the predicate in front of the same operator, which is what the builder now arranges: a filter
//! whose every conjunct reads as a comparison of one of the scan's own columns against a constant is
//! handed to the scan, and no filter operator is built above it.
//!
//! So a chunk now gets one of three answers rather than two. Ruled out and never read. Proved to
//! keep every row, so handed on with no comparison at all. Or neither, so compared the way it always
//! was. The first was already here, the third is what everything did before, and the middle one is
//! what this is for.
//!
//! What can go wrong is a wrong answer rather than a slow query, in two directions. A conjunct the
//! scan cannot express and nobody above applies is rows that should have gone and did not. A chunk
//! waved through that holds a row the filter wanted gone is the same thing one chunk at a time, and
//! the way to get there is a null, because `v >= 10` over a null row is unknown rather than true.
//! Both directions are below, and every answer is checked against the same query with the pushdown
//! out of reach rather than against a number written down here.
//!
//! The way to put it out of reach is to write the predicate so that one conjunct is not a comparison
//! against a constant. `k + 0 >= 10` keeps exactly the rows `k >= 10` keeps and reads as nothing the
//! scan can take, so the pair of them is one answer computed two ways.

use rudb::Database;
use rudb_common::Value;

/// A table of five thousand rows with a null in every column on a different cycle.
///
/// Five thousand is several chunks whatever the chunk size is, which is what makes the three answers
/// reachable at all: a filter on `k`, which runs with the rows, leaves whole chunks on either side of
/// wherever the constant falls.
fn database() -> Database {
    let db = Database::new();
    db.execute(
        "CREATE TABLE t AS SELECT
             i AS k,
             CASE WHEN i % 7 = 0 THEN NULL ELSE i % 100 END AS v,
             CASE WHEN i % 11 = 0 THEN NULL ELSE 'v' || (i % 50) END AS s
         FROM range(5000) r(i)",
    )
    .expect("the table is created");
    db
}

/// The one value a query of one row and one column answered.
fn one(db: &Database, query: &str) -> Value {
    let result = db.query(query).expect("the query ran");
    let rows: Vec<Vec<Value>> = result.rows().collect();
    assert_eq!(rows.len(), 1, "{query} answered {} rows", rows.len());
    rows[0].first().expect("one column").clone()
}

/// Whether a filter operator was built for this query.
fn filtered(db: &Database, query: &str) -> bool {
    let result = db.query(query).expect("the query ran");
    let metrics = result.metrics().expect("the query was measured");
    metrics.operators.iter().any(|operator| operator.kind == "Filter")
}

/// The same count with the filter in the scan and with it out of reach of the scan, which must agree.
///
/// `predicate` is compared against `blocked`, which is the caller's rewriting of it into something
/// that keeps the same rows and that the scan cannot take. Both the answers and which of them built
/// a filter operator are checked, since two queries agreeing because neither was pushed down would
/// be a test that passes and covers nothing.
fn agree(db: &Database, predicate: &str, blocked: &str) {
    let pushed = format!("SELECT count(*), sum(k) FROM t WHERE {predicate}");
    let above = format!("SELECT count(*), sum(k) FROM t WHERE {blocked}");
    let result = db.query(&pushed).expect("the pushed query ran");
    let wanted = db.query(&above).expect("the blocked query ran");
    let (result, wanted): (Vec<_>, Vec<_>) = (result.rows().collect(), wanted.rows().collect());
    assert_eq!(result, wanted, "{predicate} and {blocked} differ");
    assert!(!filtered(db, &pushed), "{predicate} built a filter operator");
    assert!(filtered(db, &above), "{blocked} was pushed down after all");
}

#[test]
fn a_filter_of_one_comparison_is_applied_by_the_scan_and_no_operator_is_built_for_it() {
    let db = database();
    agree(&db, "k >= 2500", "k + 0 >= 2500");
    agree(&db, "k < 2500", "k + 0 < 2500");
    agree(&db, "k = 2500", "k + 0 = 2500");
    agree(&db, "k > 4999", "k + 0 > 4999");
    agree(&db, "k <= 0", "k + 0 <= 0");
}

/// The chunks past the constant are proved whole and never compared, and the answer is the same as
/// if every one of them had been.
#[test]
fn a_filter_most_of_a_clustered_column_passes_answers_the_same_as_one_that_compares_every_row() {
    let db = database();
    agree(&db, "k >= 10", "k + 0 >= 10");
    assert_eq!(one(&db, "SELECT count(*) FROM t WHERE k >= 10"), Value::BigInt(4990));
}

/// Several conjuncts, which is the case where the scan has to keep every row of every one of them.
#[test]
fn every_conjunct_of_an_and_goes_into_the_scan_together() {
    let db = database();
    agree(&db, "k >= 10 AND k < 4000", "k + 0 >= 10 AND k < 4000");
    agree(&db, "k >= 10 AND v < 50", "k + 0 >= 10 AND v < 50");
    agree(&db, "k >= 10 AND v < 50 AND s >= 'v'", "k + 0 >= 10 AND v < 50 AND s >= 'v'");
    agree(&db, "k BETWEEN 100 AND 3000", "k + 0 BETWEEN 100 AND 3000");
}

/// A null is not a row that passes, however the two ends of the chunk fall.
///
/// Every non-null `v` is between 0 and 99, so the bounds of every chunk say the comparison holds
/// everywhere in it. A seventh of the rows are null and the filter keeps none of them, so a scan
/// that read the bounds and not the null count would answer five thousand here.
#[test]
fn a_column_with_nulls_in_it_is_never_waved_through_on_its_bounds_alone() {
    let db = database();
    agree(&db, "v >= 0", "v + 0 >= 0");
    agree(&db, "v < 100", "v + 0 < 100");
    let nulls = 5000_i64.div_euclid(7) + 1;
    assert_eq!(one(&db, "SELECT count(*) FROM t WHERE v >= 0"), Value::BigInt(5000 - nulls));
    assert_eq!(one(&db, "SELECT count(*) FROM t WHERE v IS NULL"), Value::BigInt(nulls));
}

/// A string column, where the bounds are bytes and the ends of a chunk are two of its values.
#[test]
fn a_string_column_is_pushed_down_the_same_way_a_number_is() {
    let db = database();
    agree(&db, "s >= 'v'", "s || '' >= 'v'");
    agree(&db, "s = 'v3'", "s || '' = 'v3'");
}

/// What the scan will not take, each for its own reason, and which therefore keeps its operator.
///
/// An `OR` first, since a disjunct being false says nothing about the row. Then a comparison of two
/// columns, where the bounds of one say nothing about the other's value in the same row. Then an
/// expression over a column rather than a column, and a predicate that is not a comparison at all.
///
/// `BETWEEN` is not in the list, and it is the interesting absence. It reaches the builder already
/// written out as two comparisons of a column against a constant, so it pushes down like any other
/// pair of conjuncts and there is nothing here for it to fail.
#[test]
fn a_predicate_the_scan_cannot_express_keeps_its_filter_operator() {
    let db = database();
    for predicate in ["k < 10 OR k > 4990", "k = v", "k % 3 = 0", "s IS NULL"] {
        let query = format!("SELECT count(*) FROM t WHERE {predicate}");
        assert!(filtered(&db, &query), "{predicate} was pushed into the scan");
    }
}

/// Half a predicate is not half pushed down, it is not pushed down at all.
///
/// One conjunct the scan cannot express is enough to leave the whole filter above it, because a scan
/// applying the other conjunct and nothing applying this one is rows that should have gone and did
/// not. Splitting the two is worth doing and is a rewrite of the plan rather than a branch in the
/// builder, so until it exists the answer here is the conservative one.
#[test]
fn one_conjunct_the_scan_cannot_take_leaves_the_whole_filter_where_it_was() {
    let db = database();
    let query = "SELECT count(*), sum(k) FROM t WHERE k >= 10 AND k % 3 = 0";
    assert!(filtered(&db, query), "the readable conjunct took the rest of the predicate with it");
    assert_eq!(one(&db, "SELECT count(*) FROM t WHERE k >= 10 AND k % 3 = 0"), Value::BigInt(1663));
}

/// A filter that keeps nothing anywhere, which is every chunk ruled out and no rows read.
#[test]
fn a_filter_no_chunk_can_match_answers_nothing_without_reading_a_row() {
    let db = database();
    agree(&db, "k >= 100000", "k + 0 >= 100000");
    assert_eq!(one(&db, "SELECT count(*) FROM t WHERE k >= 100000"), Value::BigInt(0));
}

/// The rows themselves and not only how many there are.
///
/// A count is answered out of a chunk's length and would come out right even if the scan handed up
/// the wrong rows, as long as it handed up the right number of them. This reads the values.
#[test]
fn the_rows_a_pushed_filter_keeps_are_the_rows_the_filter_names() {
    let db = database();
    let result = db.query("SELECT k FROM t WHERE k >= 4997 ORDER BY k").expect("the query ran");
    let kept: Vec<Value> = result.rows().map(|row| row[0].clone()).collect();
    assert_eq!(kept, vec![Value::BigInt(4997), Value::BigInt(4998), Value::BigInt(4999)]);
}

/// The same table written to a file, where the statistics are per stripe rather than per chunk.
///
/// A stripe covers sixty four parts, so its bounds are wider than any one part's and its null count
/// covers all of them. Both of those are the safe direction and both mean the file waves fewer
/// chunks through than memory does, which is allowed to cost a comparison and is not allowed to
/// change an answer.
#[test]
fn a_stored_table_answers_a_pushed_filter_the_way_a_table_in_memory_does() {
    let path = std::env::temp_dir().join(format!("rudb-pushdown-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");
    let create = "CREATE TABLE t AS SELECT i AS k, CASE WHEN i % 7 = 0 THEN NULL ELSE i % 100 END \
                  AS v FROM range(5000) r(i)";
    {
        let writing = Database::open(name).expect("a file name starts a native database");
        writing.execute(create).expect("the file table is created");
        writing.execute("CHECKPOINT").expect("the file table is committed");
    }
    let file = Database::open(name).expect("the written file opens again");
    let memory = Database::new();
    memory.execute(create).expect("the memory table is created");

    for predicate in ["k >= 10", "k >= 2500 AND k < 4000", "v >= 0", "v < 100", "k = 4999"] {
        let query = format!("SELECT count(*), sum(k) FROM t WHERE {predicate}");
        let stored = file.query(&query).expect("the file query ran");
        let held = memory.query(&query).expect("the memory query ran");
        let stored: Vec<Vec<Value>> = stored.rows().collect();
        let held: Vec<Vec<Value>> = held.rows().collect();
        assert_eq!(stored, held, "{predicate} differs between a file and memory");
        assert!(!filtered(&file, &query), "{predicate} built a filter over the file");
    }
    let _ = std::fs::remove_file(&path);
}

//! What a seam is for, checked end to end: the same query, three implementations, one answer.
//!
//! This is the differential test the seam machinery promises, written by hand while the corpus
//! runner learns to do it for every seam. A query is run with the seam left alone and then pinned
//! to each implementation in turn, and every run has to produce the same rows in the same order.
//! An implementation that is faster and wrong is the failure mode the whole design is exposed to,
//! so the first thing a new seam gets is this.

use rudb::Database;
use rudb_common::Value;

/// Every implementation of the chunk compaction seam, reference first.
const COMPACTION: [&str; 3] = ["never", "fixed-threshold", "learned-gain"];

/// A database with a table wide enough that copying it costs something.
fn database(rows: usize) -> Database {
    let database = Database::new();
    database.execute("CREATE TABLE t (a INTEGER, b BIGINT, c VARCHAR)").expect("creates");
    database
        .execute(&format!(
            "INSERT INTO t SELECT r::INTEGER, (r * 7)::BIGINT, 'row ' || r::VARCHAR FROM \
             range({rows}) AS s(r)"
        ))
        .expect("inserts");
    database
}

/// Every row of a query, as values.
fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

/// The answers this query gives under each implementation of the compaction seam.
fn under_each(database: &Database, sql: &str) -> Vec<(&'static str, Vec<Vec<Value>>)> {
    let mut answers = Vec::new();
    for name in COMPACTION {
        database
            .execute(&format!("SET seam_chunk_compaction = '{name}'"))
            .expect("the seam takes a pin");
        answers.push((name, rows(database, sql)));
    }
    database.execute("RESET seam_chunk_compaction").expect("and gives it back");
    answers
}

/// The queries worth running through all three, which are the shapes where a filter is followed by
/// something that reads the rows it kept.
const QUERIES: &[&str] = &[
    "SELECT a, b, c FROM t WHERE a % 97 = 0 ORDER BY a",
    "SELECT count(*), sum(b) FROM t WHERE a > 900",
    "SELECT c FROM t WHERE a % 3 = 0 AND b > 60 ORDER BY c LIMIT 20",
    "SELECT a FROM t WHERE c LIKE 'row 1%' ORDER BY a",
    "SELECT sum(a) FROM t WHERE a < 0",
    "SELECT l.a, r.c FROM t AS l JOIN t AS r ON l.a = r.a WHERE l.a % 251 = 0 ORDER BY l.a",
    "SELECT b % 5 AS k, count(*) FROM t WHERE a % 7 = 1 GROUP BY k ORDER BY k",
    "SELECT DISTINCT a % 11 AS k FROM t WHERE c LIKE '%7' ORDER BY k",
];

#[test]
fn every_compaction_implementation_answers_what_the_reference_answers() {
    let database = database(1000);
    for sql in QUERIES {
        let answers = under_each(&database, sql);
        let (_, reference) = &answers[0];
        for (name, answer) in &answers[1..] {
            assert_eq!(answer, reference, "{name} answered {sql} differently");
        }
    }
}

/// A filter that keeps nothing and a filter that keeps everything are the two ends, and both of
/// them skip the narrowing entirely on one path and not on the other.
#[test]
fn the_two_ends_of_a_filter_come_out_the_same_under_all_three() {
    let database = database(100);
    for sql in ["SELECT count(*) FROM t WHERE a >= 0", "SELECT count(*) FROM t WHERE a < 0"] {
        let answers = under_each(&database, sql);
        let (_, reference) = &answers[0];
        for (name, answer) in &answers[1..] {
            assert_eq!(answer, reference, "{name} answered {sql} differently");
        }
    }
}

/// A hint pins one statement, which is how a sweep runs a query both ways without a session in
/// between. Same answer, and the pin does not stay behind.
#[test]
fn a_hint_pins_the_compaction_seam_for_one_query() {
    let database = database(200);
    let plain = rows(&database, "SELECT sum(b) FROM t WHERE a % 13 = 0");
    let hinted = rows(
        &database,
        "SELECT /*+ chunk.compaction(learned-gain) */ sum(b) FROM t WHERE a % 13 = 0",
    );
    assert_eq!(plain, hinted);
}

/// A pin naming something that is not registered is an error, and it names what there is.
///
/// The alternative is a run that quietly did something else, and a benchmark number from a run
/// that quietly did something else is worse than no number.
#[test]
fn a_pin_to_an_implementation_nobody_wrote_is_an_error_with_the_list_in_it() {
    let database = database(10);
    database.execute("SET seam_chunk_compaction = 'clairvoyant'").expect("the setting is taken");
    let error = database.query("SELECT a FROM t WHERE a > 1").expect_err("and the query is not");
    let message = error.to_string();
    assert!(message.contains("clairvoyant"), "{message}");
    assert!(message.contains("never"), "{message}");
    assert!(message.contains("learned-gain"), "{message}");
}

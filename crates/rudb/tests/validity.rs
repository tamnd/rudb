//! Null questions a file has already answered, taken out of the plan before the query runs.
//!
//! `spec/stats/05-every-query.md` section 5.10 asks for validity free kernels on a column whose null
//! count is exactly zero. The kernels were already validity free wherever the chunk in hand says it
//! holds no nulls, so what was missing was the other half: a question about the whole column, which
//! no chunk can settle and a stored null count can. `WHERE x IS NOT NULL` over such a column is not
//! a cheaper test, it is no test, and the filter is gone by the time anything reads a row.
//!
//! Every case here runs against a file rather than against a table in memory, because a table in
//! memory hands the planner no directory to read. It keeps a zone map per chunk and those are as
//! good as a file's, but a chunk is owned by the table rather than shared behind a reference count,
//! so nothing is handed over at bind time. The file half is where this starts.
//!
//! The control in each case is the same query with `stats_validity_free` turned off. A rewrite that
//! reads a statistic is allowed to make the query faster and is not allowed to make it different,
//! and the only way to say that out loud is to run it both ways and compare.

use rudb::Database;
use rudb_common::Value;

/// A file with one column that has no nulls and one that has three, plus a table to join to.
///
/// Written by one database and read by another, because a database that just wrote the rows is still
/// holding them in memory and would answer out of those rather than out of the file.
struct Written {
    database: Database,
    path: std::path::PathBuf,
}

impl Written {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-validity-{tag}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let name = path.to_str().expect("a UTF-8 temporary path");
        {
            let writing = Database::open(name).expect("a file name starts a native database");
            writing.execute("CREATE TABLE t (a INTEGER, b INTEGER)").expect("the table is created");
            writing
                .execute(
                    "INSERT INTO t SELECT r::INTEGER, \
                     CASE WHEN r % 100 = 0 THEN NULL ELSE r::INTEGER END FROM range(300) AS s(r)",
                )
                .expect("the rows are inserted");
            writing.execute("CREATE TABLE u (k INTEGER)").expect("the second table is created");
            writing
                .execute("INSERT INTO u SELECT r::INTEGER FROM range(100) AS s(r)")
                .expect("a hundred of the three hundred keys");
            writing.execute("CHECKPOINT").expect("the rows are committed");
        }
        let database = Database::open(name).expect("the written file opens again");
        Self { database, path }
    }

    /// The plan text, with the rewrite on or off.
    fn plan(&self, query: &str, allowed: bool) -> String {
        let setting = format!("SET stats_validity_free = {allowed}");
        self.database.execute(&setting).expect("the rule is a setting");
        self.database.plan(query).unwrap_or_else(|error| panic!("{query} did not plan: {error}"))
    }

    /// Every row of a query, with the rewrite on or off.
    fn rows(&self, query: &str, allowed: bool) -> Vec<Vec<Value>> {
        let setting = format!("SET stats_validity_free = {allowed}");
        self.database.execute(&setting).expect("the rule is a setting");
        let result =
            self.database.query(query).unwrap_or_else(|error| panic!("{query} failed: {error}"));
        result.rows().collect()
    }

    /// The same query both ways, asserted to answer the same, and the answer.
    fn both_ways(&self, query: &str) -> Vec<Vec<Value>> {
        let answer = self.rows(query, true);
        assert_eq!(self.rows(query, false), answer, "the rewrite changed the answer to {query}");
        answer
    }
}

impl Drop for Written {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn a_filter_asking_whether_a_column_with_no_nulls_is_null_is_not_in_the_plan_at_all() {
    let written = Written::new("notnull");
    let query = "SELECT count(*) FROM t WHERE a IS NOT NULL";
    let planned = written.plan(query, true);
    assert!(!planned.contains("Filter"), "the filter should be gone:\n{planned}");
    let control = written.plan(query, false);
    assert!(control.contains("Filter"), "with the rule off it is still planned:\n{control}");
    assert_eq!(written.both_ways(query), vec![vec![Value::BigInt(300)]]);
}

#[test]
fn the_same_question_about_a_column_that_does_have_nulls_is_still_asked() {
    let written = Written::new("nullable");
    let query = "SELECT count(*) FROM t WHERE b IS NOT NULL";
    let planned = written.plan(query, true);
    assert!(planned.contains("Filter"), "nothing settles this one:\n{planned}");
    assert_eq!(written.both_ways(query), vec![vec![Value::BigInt(297)]]);
}

#[test]
fn asking_whether_a_column_with_no_nulls_is_null_answers_no_rows_without_reading_one() {
    let written = Written::new("isnull");
    let query = "SELECT count(*) FROM t WHERE a IS NULL";
    let planned = written.plan(query, true);
    // Settled to false, and then the pass that pulls an empty result up took the scan with it, so
    // there is no table left in the plan at all.
    assert!(planned.contains("rows=[]"), "the predicate should be settled:\n{planned}");
    assert!(!planned.contains("Get "), "and the scan should be gone with it:\n{planned}");
    let control = written.plan(query, false);
    assert!(control.contains("Get "), "with the rule off the table is read:\n{control}");
    assert_eq!(written.both_ways(query), vec![vec![Value::BigInt(0)]]);
}

#[test]
fn counting_a_column_with_no_nulls_is_counting_the_rows() {
    let written = Written::new("count");
    let query = "SELECT count(a), count(b) FROM t";
    let planned = written.plan(query, true);
    // Which binding the nullable column ended up as is not asserted, because the pass that drops the
    // columns nobody reads runs after this one and renumbers what is left.
    let aggregates = planned.lines().find(|line| line.contains("Aggregate")).expect("one aggregate");
    assert!(aggregates.contains("count_star()"), "the first count is a row count:\n{planned}");
    assert!(aggregates.contains("count(#"), "the second one is not:\n{planned}");
    assert_eq!(written.both_ways(query), vec![vec![Value::BigInt(300), Value::BigInt(297)]]);
}

#[test]
fn an_anti_join_written_as_a_left_join_and_a_null_test_still_answers_it() {
    let written = Written::new("antijoin");
    // The nulls this predicate is about are the ones the left join writes for a row of `t` that
    // found nothing in `u`. The file counted no nulls in `u.k` and that says nothing about them, so
    // the filter has to survive. Two hundred of the three hundred keys have no match.
    let query = "SELECT count(*) FROM t LEFT JOIN u ON t.a = u.k WHERE u.k IS NULL";
    let planned = written.plan(query, true);
    assert!(planned.contains("Filter"), "the null test is the anti join:\n{planned}");
    assert_eq!(written.both_ways(query), vec![vec![Value::BigInt(200)]]);
}

#[test]
fn a_settled_conjunct_leaves_the_rest_of_the_predicate_where_it_was() {
    let written = Written::new("conjunct");
    let query = "SELECT count(*) FROM t WHERE a IS NOT NULL AND a > 250";
    let planned = written.plan(query, true);
    assert!(planned.contains("Filter"), "the other half is still a question:\n{planned}");
    assert!(!planned.contains("DISTINCT FROM"), "and the null test is gone:\n{planned}");
    assert_eq!(written.both_ways(query), vec![vec![Value::BigInt(49)]]);
}

//! A grouped `MIN` or `MAX` over a string column a file has sorted the values of.
//!
//! The file keeps one dictionary per string column and the sorted order of it, so the aggregate can
//! ask which of two values is smaller by comparing two positions in that order instead of fetching
//! two strings out of a payload the size of the column. Every test here asks the same question of a
//! table held in memory and of the same table written to a file. Memory has no sorted order and
//! compares bytes, so it is the oracle, and the file is allowed to be faster and not allowed to be
//! different.
//!
//! What they are guarding is the two ways a rank can be read as something it is not. A rank means
//! nothing outside the dictionary that issued it, so a group holding one has to hold the dictionary
//! with it, and a merge of two workers' groups has to check they are talking about the same one
//! before it compares. And the order of the values is not the order they were written in, which is
//! the whole reason the file stores the order at all, so the strings below are built to put the two
//! as far apart as they go, by giving each row a value picked out of a scrambled sequence so that
//! the row that goes in first is nowhere near the one that comes out smallest.

use rudb::Database;
use rudb_common::Value;

/// The same rows in memory and in a file, with the file's answers checked against the memory ones.
struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    fn new(tag: &str, select: &str, threads: usize) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-ranked-{tag}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let create = format!("CREATE TABLE t AS {select}");
        let memory = Database::new();
        memory.execute(&create).expect("the memory table is created");
        let name = path.to_str().expect("a UTF-8 temporary path");
        // Written by one database and read by another, because a table is only actually backed by
        // the file once the database that wrote it has let go of the rows it is still holding.
        {
            let writing = Database::open(name).expect("a file name starts a native database");
            writing.execute(&create).expect("the file table is created");
            writing.execute("CHECKPOINT").expect("the file table is committed");
        }
        let file = Database::open(name).expect("the written file opens again");
        for database in [&memory, &file] {
            database.execute(&format!("SET threads = {threads}")).expect("sets the thread count");
        }
        Self { memory, file, path }
    }

    /// Asserts the file gives the whole result memory gives, row for row and in the same order.
    fn agree(&self, query: &str) {
        let wanted = rows(&self.memory, query);
        let got = rows(&self.file, query);
        assert_eq!(got, wanted, "the file and memory disagree about {query}");
        assert!(!wanted.is_empty(), "{query} answered nothing, so it proved nothing");
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Every row of a result, as values, so a grouped answer can be compared whole.
fn rows(database: &Database, query: &str) -> Vec<Vec<Value>> {
    let result = database.query(query).expect("the query ran");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

/// A table of `rows` rows whose string column is written in one order and sorts in another.
///
/// `s` is the row number multiplied into a scrambled sequence and then written out at a fixed
/// width, so every value is seven digits and no two rows share one. Seven thousand nine hundred and
/// nineteen is coprime with the prime it is taken against, so the sequence is a permutation, and
/// fixed width is what makes the order of the strings the order of the numbers. The smallest string
/// in a group is therefore held by some arbitrary row of it rather than by the first, which is what
/// makes a min that quietly kept the value it saw first, or that read a position as if it were a
/// code, come out wrong rather than come out lucky.
fn ordered(rows: i64, keys: i64) -> String {
    format!("SELECT i % {keys} AS k, {SCRAMBLED} AS s FROM range(0, {rows}) AS r(i)")
}

/// A seven digit string per row, in an order that has nothing to do with the order rows arrive in.
const SCRAMBLED: &str = "CAST(1000000 + (i * 7919) % 999983 AS VARCHAR)";

#[test]
fn a_grouped_extreme_over_a_sorted_dictionary_is_the_one_the_bytes_give() {
    let pair = Pair::new("plain", &ordered(20_000, 97), 4);
    pair.agree("SELECT k, MIN(s), MAX(s) FROM t GROUP BY k ORDER BY k");
}

#[test]
fn a_grouped_extreme_agrees_however_many_workers_split_the_rows() {
    // One worker and many, because the many are what make two tables of groups meet in a merge, and
    // that merge is the one place a rank from one dictionary could be compared against another.
    for threads in [1, 2, 8] {
        let pair = Pair::new(&format!("threads{threads}"), &ordered(20_000, 401), threads);
        pair.agree("SELECT k, MIN(s), MAX(s) FROM t GROUP BY k ORDER BY k");
    }
}

#[test]
fn a_grouped_extreme_skips_the_nulls_the_way_memory_does() {
    let select = "SELECT i % 53 AS k, \
                  CASE WHEN i % 7 = 0 THEN NULL \
                  ELSE CAST(1000000 + (i * 7919) % 999983 AS VARCHAR) END AS s \
                  FROM range(0, 20000) AS r(i)";
    let pair = Pair::new("nulls", select, 4);
    pair.agree("SELECT k, MIN(s), MAX(s) FROM t GROUP BY k ORDER BY k");
}

#[test]
fn a_group_whose_rows_are_all_null_answers_null_rather_than_a_string() {
    // Key 0 is exactly the rows divisible by seven when the key is the remainder by seven, so this
    // is a group with nothing in it to be the minimum of, next to groups that have plenty.
    let select = "SELECT i % 7 AS k, \
                  CASE WHEN i % 7 = 0 THEN NULL \
                  ELSE CAST(1000000 + (i * 7919) % 999983 AS VARCHAR) END AS s \
                  FROM range(0, 20000) AS r(i)";
    let pair = Pair::new("allnull", select, 4);
    pair.agree("SELECT k, MIN(s), MAX(s) FROM t GROUP BY k ORDER BY k");
}

#[test]
fn a_grouped_extreme_beside_the_other_aggregates_leaves_them_alone() {
    // The extreme is one call of several, which is the shape ClickBench query 28 has, and the point
    // is that taking a different path through the scatter for one call does not move the others.
    let pair = Pair::new("mixed", &ordered(20_000, 97), 4);
    pair.agree("SELECT k, COUNT(*), MIN(s), MAX(s), MIN(STRLEN(s)) FROM t GROUP BY k ORDER BY k");
}

#[test]
fn an_ungrouped_extreme_over_the_whole_column_is_the_one_the_bytes_give() {
    // The one here that does not go through the ranks. An aggregate with no grouping folds a whole
    // vector into one accumulator and reduces before it ever looks at a state, so it keeps the path
    // it had, and this is the test that says so by still agreeing with memory.
    let pair = Pair::new("whole", &ordered(20_000, 97), 4);
    pair.agree("SELECT MIN(s), MAX(s) FROM t");
}

#[test]
fn a_grouped_extreme_over_a_filtered_column_is_the_one_the_bytes_give() {
    // A filter leaves the dictionary alone and cuts the codes, so the rows that reach the aggregate
    // point into a dictionary holding values none of them have. A rank read off that dictionary is
    // still the right rank; a minimum read off the dictionary rather than off the rows would not be.
    let pair = Pair::new("filtered", &ordered(20_000, 97), 4);
    pair.agree("SELECT k, MIN(s), MAX(s) FROM t WHERE s > '1500000' GROUP BY k ORDER BY k");
}

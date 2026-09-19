//! A scan that walks past the parts the top N above it has already beaten.
//!
//! `ORDER BY k LIMIT 10` over a stored table holds ten candidates almost immediately, and the worst
//! of those ten is a key nothing worse than can ever come out. A part of the file whose own bounds
//! put every row in it below that key holds nothing the query wants, and the scan skips it without
//! reading a byte. See `rudb_exec::cutoff` for why that leaves the answer exactly as it was.
//!
//! The way it would go wrong is rows quietly missing from an answer, or the same rows coming back in
//! a different order, so every test here asks the same question of a table in a file and of the same
//! table held in memory. Memory keeps no statistics and skips nothing, so it is the oracle. The file
//! is allowed to read less and is not allowed to answer differently.
//!
//! The shapes are written to reach the parts of the rule that can be got wrong. A key that climbs
//! and a key that is scrambled, because the first lets the cutoff rule out nearly everything and the
//! second lets it rule out almost nothing. Both directions. Ties on the key, so that the row the
//! answer keeps is decided by where it arrived and not by which part was read. Nulls in the key at
//! both ends of the file. An offset, because the candidates the cutoff comes from are the count plus
//! the offset and not the count. More than one key, since only the first of them says anything about
//! a whole part. A string key, which is stored as cut down bytes rather than as a number. And a
//! limit larger than the table, which never fills the candidates at all.

use rudb::Database;
use rudb_common::Value;

/// The same rows in memory and in a file, so one can be checked against the other.
struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    fn new(tag: &str, select: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-cutoff-{tag}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let create = format!("CREATE TABLE t AS {select}");
        let memory = Database::new();
        memory.execute(&create).expect("the memory table is created");
        let name = path.to_str().expect("a UTF-8 temporary path");
        // Written by one database and read by another, because the table is only actually read out
        // of the file once the database that wrote it has let go of the rows it still holds.
        {
            let writing = Database::open(name).expect("a file name starts a native database");
            writing.execute(&create).expect("the file table is created");
            writing.execute("CHECKPOINT").expect("the file table is committed");
        }
        let file = Database::open(name).expect("the written file opens again");
        Self { memory, file, path }
    }

    /// Asserts the file gives the whole answer memory gives, row for row and in the same order.
    ///
    /// At one thread and at four, because the cutoff is shared between the workers and the one that
    /// fills its candidates first is the one every other worker is then measured against.
    fn agree(&self, query: &str) {
        for threads in [1, 4] {
            let set = format!("SET threads = {threads}");
            self.memory.execute(&set).expect("sets the thread count");
            self.file.execute(&set).expect("sets the thread count");
            let wanted = rows(&self.memory, query);
            let got = rows(&self.file, query);
            assert_eq!(got, wanted, "the file and memory disagree about {query} at {threads}");
            assert!(!wanted.is_empty(), "{query} answered nothing, so it proves nothing");
        }
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Every row of a result, as values.
fn rows(database: &Database, query: &str) -> Vec<Vec<Value>> {
    let result = database.query(query).expect("the query ran");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

/// Enough rows to fill many parts, with a column for each way an ordering can be arranged.
///
/// `i` climbs, which gives every part a range of its own and so lets the cutoff rule out nearly the
/// whole table. `j` is the same rows scrambled, so every part holds most of the range and the cutoff
/// rules almost nothing out, which is the case with the least to do and still has to be right. `p`
/// holds only a hundred distinct values, so the key ties across thousands of rows and across parts.
/// `s` is the scrambled column as text, for an ordering on bytes rather than on a number. `n` is the
/// climbing column with the first and last thousand rows nulled, so an ordering that puts nulls last
/// has them at both ends of the file.
fn spread(rows: i64) -> String {
    let nulled = format!("CASE WHEN i < 1000 OR i >= {} THEN NULL ELSE i END", rows - 1000);
    format!(
        "SELECT i, (i * 7919) % 1000003 AS j, i % 100 AS p, \
         CAST((i * 7919) % 1000003 AS VARCHAR) AS s, {nulled} AS n \
         FROM range(0, {rows}) AS r(i)"
    )
}

const ROWS: i64 = 300_000;

/// The shape the whole thing is for, and the one ClickBench 24 has.
#[test]
fn a_climbing_key_ascending_is_the_case_the_cutoff_rules_out_the_most_of() {
    let pair = Pair::new("climb", &spread(ROWS));
    pair.agree("SELECT i FROM t ORDER BY i LIMIT 10");
    pair.agree("SELECT i, j FROM t WHERE j > 500000 ORDER BY i LIMIT 10");
}

/// The same table read from the other end, which is the other comparison the ordering can give.
#[test]
fn a_climbing_key_descending_reads_the_other_end_of_the_file() {
    let pair = Pair::new("down", &spread(ROWS));
    pair.agree("SELECT i FROM t ORDER BY i DESC LIMIT 10");
    pair.agree("SELECT i, j FROM t WHERE j > 500000 ORDER BY i DESC LIMIT 10");
}

/// A key whose parts all hold nearly the whole range, so the cutoff rules almost nothing out. The
/// case with the least to gain, and the one where a rule that was slightly too eager would show.
#[test]
fn a_scrambled_key_rules_almost_nothing_out_and_still_answers_the_same() {
    let pair = Pair::new("mixed", &spread(ROWS));
    pair.agree("SELECT j FROM t ORDER BY j LIMIT 10");
    pair.agree("SELECT j FROM t ORDER BY j DESC LIMIT 10");
}

/// Thousands of rows tie on the key, so which ten come back is settled by where each one arrived
/// rather than by the key. Skipping a part must not disturb that, which is the whole soundness
/// argument written as a test.
#[test]
fn a_key_that_ties_across_parts_keeps_the_rows_that_arrived_first() {
    let pair = Pair::new("ties", &spread(ROWS));
    pair.agree("SELECT p, i FROM t ORDER BY p LIMIT 10");
    pair.agree("SELECT p, i FROM t ORDER BY p DESC LIMIT 10");
    pair.agree("SELECT p, i FROM t ORDER BY p LIMIT 25 OFFSET 990");
}

/// Only the first key says anything about a whole part, so a second key that breaks the ties on the
/// first has to be left to the top N itself.
#[test]
fn a_second_sort_key_breaks_the_ties_the_first_one_leaves() {
    let pair = Pair::new("second", &spread(ROWS));
    pair.agree("SELECT p, j FROM t ORDER BY p, j LIMIT 10");
    pair.agree("SELECT p, j FROM t ORDER BY p DESC, j DESC LIMIT 10");
    pair.agree("SELECT p, j, i FROM t ORDER BY p, j DESC, i LIMIT 10");
}

/// The candidates the cutoff comes from are the count plus the offset, so an offset that is larger
/// than the count is the case where taking the count alone would cut too much.
#[test]
fn an_offset_moves_the_cutoff_further_out_than_the_count_alone_would() {
    let pair = Pair::new("offset", &spread(ROWS));
    pair.agree("SELECT i FROM t ORDER BY i LIMIT 10 OFFSET 1000");
    pair.agree("SELECT j FROM t ORDER BY j LIMIT 10 OFFSET 1000");
    pair.agree("SELECT i FROM t ORDER BY i DESC LIMIT 1 OFFSET 5000");
}

/// Nulls at both ends of the file under both directions. Ascending with nulls last is the default
/// and is the one the cutoff is armed for. Nulls first is refused, so those two prove the refusal
/// still answers rather than that it prunes.
#[test]
fn nulls_in_the_key_land_where_the_ordering_puts_them() {
    let pair = Pair::new("nulls", &spread(ROWS));
    pair.agree("SELECT n, i FROM t ORDER BY n LIMIT 10");
    pair.agree("SELECT n, i FROM t ORDER BY n DESC LIMIT 10");
    pair.agree("SELECT n, i FROM t ORDER BY n NULLS FIRST LIMIT 10");
    pair.agree("SELECT n, i FROM t ORDER BY n DESC NULLS FIRST LIMIT 10");
    pair.agree("SELECT n, i FROM t WHERE n IS NOT NULL ORDER BY n LIMIT 10");
}

/// A key stored as bytes rather than as a number, whose part bounds are cut down to their first
/// twenty four bytes on the way into the file. A cut down bound is wider than the rows it covers,
/// which is allowed, and a rule that assumed it was exact would lose rows here.
#[test]
fn a_string_key_is_measured_against_the_cut_down_bounds_the_file_holds() {
    let pair = Pair::new("text", &spread(ROWS));
    pair.agree("SELECT s FROM t ORDER BY s LIMIT 10");
    pair.agree("SELECT s FROM t ORDER BY s DESC LIMIT 10");
    pair.agree("SELECT s, i FROM t WHERE s LIKE '9%' ORDER BY s LIMIT 10");
}

/// A limit nothing reaches, so no instance ever fills its candidates and the cutoff stays empty for
/// the whole query. The case that must read everything, and the case a bug here would break by
/// publishing a cutoff from a set of candidates that was never full.
#[test]
fn a_limit_larger_than_the_table_never_rules_anything_out() {
    let pair = Pair::new("whole", &spread(10_000));
    pair.agree("SELECT COUNT(*) FROM (SELECT i FROM t ORDER BY i LIMIT 100000) x");
    pair.agree("SELECT COUNT(*) FROM (SELECT i FROM t ORDER BY i DESC LIMIT 100000) x");
}

/// A projection and a filter between the top N and the scan, which is the only path the cutoff is
/// allowed down and is what every real query looks like. The projection renames the column, so a
/// cutoff that did not walk the binding down would be about the wrong column entirely.
#[test]
fn a_projection_between_the_top_n_and_the_scan_renames_the_column_the_cutoff_is_about() {
    let pair = Pair::new("named", &spread(ROWS));
    pair.agree("SELECT k FROM (SELECT i AS k, j FROM t WHERE j > 100) x ORDER BY k LIMIT 10");
    pair.agree("SELECT k FROM (SELECT i AS k FROM t) x ORDER BY k DESC LIMIT 10");
}

/// A key the scan cannot be measured against at all, because the value the ordering is on is not a
/// value any column holds. Nothing is armed and the answer is the answer.
#[test]
fn an_ordering_on_an_expression_arms_nothing_and_answers_anyway() {
    let pair = Pair::new("expr", &spread(ROWS));
    pair.agree("SELECT i FROM t ORDER BY -i LIMIT 10");
    pair.agree("SELECT i, j FROM t ORDER BY i + j LIMIT 10");
    pair.agree("SELECT p, COUNT(*) FROM t GROUP BY p ORDER BY COUNT(*) DESC, p LIMIT 10");
}

//! A top N whose sort key is a string column a file has sorted the values of.
//!
//! The file keeps one dictionary per string column and the sorted order of it, so a candidate the
//! operator keeps holds the position of its key in that order rather than the string, and a row is
//! rejected against it by comparing two positions. Nothing is decoded. On ClickBench 25, `ORDER BY
//! SearchPhrase LIMIT 10`, reading the row's value to compare it was forty three percent of the
//! query.
//!
//! Every test here asks the same question of a table held in memory and of the same table written
//! to a file. Memory has no sorted order and compares strings, so it is the oracle, and the file is
//! allowed to be faster and not allowed to be different.
//!
//! What they are guarding is the two ways a rank can be read as something it is not. A rank is a
//! position and not a code, so the order the values sort in has nothing to do with the order the
//! rows arrived in, and the strings below are built to put the two as far apart as they go. And a
//! null row has no code and therefore no rank, so where the query puts the nulls has to keep being
//! decided by the null placement of the sort key and not by an integer comparison.
//!
//! The third way is that a rank means nothing outside the dictionary that issued it. That one is
//! checked in `rudb_kernels::compare`, because no query shape today puts two dictionaries under one
//! top N for it to be asked of here.

use rudb::Database;
use rudb_common::Value;

/// The same rows in memory and in a file, with the file's answers checked against the memory ones.
struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    /// A database in memory and one in a file, both holding the tables `creates` builds.
    fn new(tag: &str, creates: &[String], threads: usize) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-topcoded-{tag}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let memory = Database::new();
        for create in creates {
            memory.execute(create).expect("the memory table is created");
        }
        let name = path.to_str().expect("a UTF-8 temporary path");
        // Written by one database and read by another, because a table is only actually backed by
        // the file once the database that wrote it has let go of the rows it is still holding.
        {
            let writing = Database::open(name).expect("a file name starts a native database");
            for create in creates {
                writing.execute(create).expect("the file table is created");
            }
            writing.execute("CHECKPOINT").expect("the file tables are committed");
        }
        let file = Database::open(name).expect("the written file opens again");
        for database in [&memory, &file] {
            database.execute(&format!("SET threads = {threads}")).expect("sets the thread count");
        }
        Self { memory, file, path }
    }

    /// Asserts the file gives the whole result memory gives, row for row and in the same order.
    fn agree(&self, query: &str) {
        assert!(
            self.file.plan(query).expect("binds").contains("TopN"),
            "{query} is not a top N, so it proves nothing"
        );
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

/// Every row of a result, as values, so an ordered answer can be compared whole.
fn rows(database: &Database, query: &str) -> Vec<Vec<Value>> {
    let result = database.query(query).expect("the query ran");
    result.rows().collect()
}

/// A seven digit string per row, in an order that has nothing to do with the order rows arrive in.
///
/// Seven thousand nine hundred and nineteen is coprime with the prime it is taken against, so the
/// sequence is a permutation and no two rows share a value. Fixed width is what makes the order of
/// the strings the order of the numbers. The smallest string is therefore held by some arbitrary row
/// rather than by the first, which is what makes a candidate that quietly compared codes rather than
/// ranks come out wrong rather than come out lucky.
const SCRAMBLED: &str = "CAST(1000000 + (i * 7919) % 999983 AS VARCHAR)";

/// A table of `rows` rows with a string column written in one order and sorting in another, a key
/// column that many rows share, and a plain integer beside them.
fn ordered(name: &str, rows: i64, keys: i64) -> String {
    format!(
        "CREATE TABLE {name} AS SELECT i % {keys} AS k, {SCRAMBLED} AS s, i AS n \
         FROM range(0, {rows}) AS r(i)"
    )
}

/// The same with nulls in the string column, one row in seven.
fn holed(name: &str, rows: i64, keys: i64) -> String {
    format!(
        "CREATE TABLE {name} AS SELECT i % {keys} AS k, \
         CASE WHEN i % 7 = 0 THEN NULL ELSE {SCRAMBLED} END AS s, i AS n \
         FROM range(0, {rows}) AS r(i)"
    )
}

#[test]
fn one_coded_key_gives_the_rows_the_strings_give() {
    let pair = Pair::new("plain", &[ordered("t", 20_000, 97)], 4);
    pair.agree("SELECT k, s, n FROM t ORDER BY s LIMIT 10");
}

#[test]
fn a_coded_key_read_backwards_gives_the_rows_the_strings_give() {
    // Descending is where a comparison on ranks has to be turned around and a comparison on values
    // is turned around for it already, so it is the one arm that could be reversed twice or not at
    // all and still look plausible on data that happens to be symmetric.
    let pair = Pair::new("desc", &[ordered("t", 20_000, 97)], 4);
    pair.agree("SELECT k, s, n FROM t ORDER BY s DESC LIMIT 10");
}

#[test]
fn a_coded_key_agrees_however_many_workers_split_the_rows() {
    // One worker and many, because the many are what make two instances' candidates meet in a
    // combine, and that combine is the one place a rank from one dictionary could be compared
    // against another.
    for threads in [1, 2, 8] {
        let pair = Pair::new(&format!("threads{threads}"), &[ordered("t", 20_000, 401)], threads);
        pair.agree("SELECT k, s, n FROM t ORDER BY s LIMIT 10");
    }
}

#[test]
fn an_offset_past_the_sorted_bound_takes_the_batched_path() {
    // Above the sorted bound the candidates are not held in order, so the key a row is rejected
    // against is a copy of the worst one taken at the last trim rather than the one sitting at the
    // end of the array. That copy is cells and not values, which is the part this is here for.
    let pair = Pair::new("offset", &[ordered("t", 20_000, 97)], 4);
    pair.agree("SELECT k, s, n FROM t ORDER BY s LIMIT 10 OFFSET 1000");
}

#[test]
fn a_coded_key_after_one_that_almost_every_row_ties() {
    // The first key decides almost nothing, so the second one decides almost everything, and a
    // second key is only ever reached because the first came back equal. A rank comparison that
    // called two equal values unequal would be invisible on a first key and wrong here.
    let pair = Pair::new("second", &[ordered("t", 20_000, 3)], 4);
    pair.agree("SELECT k, s, n FROM t ORDER BY k, s LIMIT 10");
}

#[test]
fn a_coded_key_in_front_of_another_one() {
    let pair = Pair::new("first", &[ordered("t", 20_000, 97)], 4);
    pair.agree("SELECT k, s, n FROM t ORDER BY s, k LIMIT 10");
}

#[test]
fn nulls_in_a_coded_key_go_where_the_query_puts_them() {
    // A null row has no code and so no rank, and both placements are asked for because the pass
    // that rejects a whole chunk steps aside for one of them and not the other.
    let pair = Pair::new("nulls", &[holed("t", 20_000, 97)], 4);
    for placement in ["NULLS FIRST", "NULLS LAST"] {
        for direction in ["ASC", "DESC"] {
            pair.agree(&format!(
                "SELECT k, s, n FROM t ORDER BY s {direction} {placement} LIMIT 10"
            ));
        }
    }
}

#[test]
fn nulls_in_a_coded_key_over_the_batched_path_too() {
    let pair = Pair::new("nullsoffset", &[holed("t", 20_000, 97)], 4);
    pair.agree("SELECT k, s, n FROM t ORDER BY s NULLS FIRST LIMIT 10 OFFSET 1000");
}

#[test]
fn rows_from_two_tables_under_one_top_n_come_out_in_one_order() {
    // Each table keeps its own dictionary for its own string column, so a rank taken off one of them
    // is not a rank in the other, and a candidate kept from either side has to say so rather than
    // compare two numbers that mean different things. Today the union hands the operator plain
    // values and the question never gets asked, which is why the guard itself is checked in
    // `rudb_kernels::compare` where it can be asked directly. This is here for the day the union
    // stops copying: it is the query shape that would put the two orders side by side, and the two
    // tables hold different strings so an answer taken from the wrong order is a different answer.
    let tables = [ordered("t", 20_000, 97), ordered("u", 9_000, 31)];
    let pair = Pair::new("two", &tables, 4);
    let union = "SELECT s, n FROM t UNION ALL SELECT s, n FROM u";
    pair.agree(&format!("SELECT s, n FROM ({union}) ORDER BY s LIMIT 10"));
    pair.agree(&format!("SELECT s, n FROM ({union}) ORDER BY s DESC LIMIT 10"));
}

#[test]
fn a_coded_key_under_a_filter_that_the_candidates_tighten() {
    // A scan under a filter under a projection is the shape that arms the cutoff, so the worst
    // candidate is published to the scan as a bound and the scan skips the parts of the file that
    // cannot beat it. Publishing it means reading it, which for a coded key is the one place the
    // value is still fetched, and a bound read off the wrong candidate would prune away rows that
    // belong in the answer.
    let pair = Pair::new("cutoff", &[ordered("t", 20_000, 97)], 4);
    pair.agree("SELECT s FROM t WHERE s > '1500000' ORDER BY s LIMIT 10");
    pair.agree("SELECT s FROM t WHERE s < '1900000' ORDER BY s DESC LIMIT 10");
}

#[test]
fn a_key_that_is_an_expression_over_a_coded_column() {
    // The operator is handed what the expression produced and not the column, and an expression
    // over a dictionary gives back plain strings, so this is the path where nothing is coded at all
    // and everything is read. It is here to say that path still works rather than that it is fast.
    let pair = Pair::new("expr", &[ordered("t", 20_000, 97)], 4);
    pair.agree("SELECT k, s, n FROM t ORDER BY SUBSTRING(s, 3), n LIMIT 10");
}

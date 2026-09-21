//! A `CASE` over a string column whose values a file holds in one dictionary.
//!
//! Every branch of a `CASE` like `CASE WHEN c THEN s ELSE '' END` names a value rather than working
//! one out, so if all of them name values of one dictionary then so does the answer, and the answer
//! can be the codes. The operator above then sees a coded column rather than a run of strings, which
//! is what a `GROUP BY` over the expression wants and is what ClickBench 39 spends a quarter of
//! itself not having.
//!
//! Every test here asks the same question of a table held in memory and of the same table written to
//! a file. Memory has no dictionary and answers by reading the values, so it is the oracle, and the
//! file is allowed to be faster and not allowed to be different.
//!
//! What they are guarding is the four ways a code can be copied into an answer it does not belong
//! in. A literal the dictionary does not hold has no code, so `ELSE 'nothing here'` cannot be a
//! code and has to fall back. Two columns have two dictionaries and a code in one says nothing
//! about the other. A null is kept in the values a dictionary points at rather than beside its
//! codes, so a column carrying its own nulls is one whose codes do not say everything it says. And
//! a `CASE` with no `ELSE` answers null for the rows nothing claims, which is not a code either.

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
    fn new(tag: &str, creates: &[String]) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-casecoded-{tag}-{}.rudb", std::process::id()));
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
            database.execute("SET threads = 4").expect("sets the thread count");
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

/// Every row of a result, as values, so an ordered answer can be compared whole.
fn rows(database: &Database, query: &str) -> Vec<Vec<Value>> {
    let result = database.query(query).expect("the query ran");
    result.rows().collect()
}

/// A table with two string columns of few distinct values, a flag to switch on, and a counter.
///
/// Few distinct values because the point is the dictionary, and two columns because two dictionaries
/// under one expression is one of the ways a code can be read as something it is not. The empty
/// string is in `s` on purpose, since `ELSE ''` is the shape ClickBench 39 has and it only becomes a
/// code because some row of the column is empty.
fn two_columns(name: &str, rows: i64) -> String {
    format!(
        "CREATE TABLE {name} AS SELECT \
         i % 3 AS k, \
         CASE WHEN i % 5 = 0 THEN '' ELSE 'left' || CAST(i % 11 AS VARCHAR) END AS s, \
         'right' || CAST(i % 7 AS VARCHAR) AS t, \
         i AS n \
         FROM range(0, {rows}) AS r(i)"
    )
}

/// The same with nulls in the first string column, one row in six.
fn holed(name: &str, rows: i64) -> String {
    format!(
        "CREATE TABLE {name} AS SELECT \
         i % 3 AS k, \
         CASE WHEN i % 6 = 0 THEN NULL WHEN i % 5 = 0 THEN '' \
         ELSE 'left' || CAST(i % 11 AS VARCHAR) END AS s, \
         'right' || CAST(i % 7 AS VARCHAR) AS t, \
         i AS n \
         FROM range(0, {rows}) AS r(i)"
    )
}

#[test]
fn a_column_and_a_literal_the_dictionary_holds() {
    // The shape this was written for, and the one ClickBench 39 has: one arm naming a column, an
    // `ELSE` naming a literal that some row of that column is equal to, and the whole thing a group
    // key. The answer is the strings whatever the file does with them.
    let pair = Pair::new("held", &[two_columns("t", 6_000)]);
    let case = "CASE WHEN k = 0 THEN s ELSE '' END";
    pair.agree(&format!("SELECT {case} AS c, COUNT(*) FROM t GROUP BY c ORDER BY c"));
    pair.agree(&format!("SELECT {case} AS c, n FROM t ORDER BY n LIMIT 40"));
}

#[test]
fn a_literal_the_dictionary_does_not_hold() {
    // No row of `s` is this string, so it has no code, and an answer built out of codes would have
    // to invent one. What it does instead is read the values, which is what memory does too.
    let pair = Pair::new("absent", &[two_columns("t", 6_000)]);
    let case = "CASE WHEN k = 0 THEN s ELSE 'no row of s says this' END";
    pair.agree(&format!("SELECT {case} AS c, COUNT(*) FROM t GROUP BY c ORDER BY c"));
}

#[test]
fn two_columns_under_one_case_have_two_dictionaries() {
    // A code is a position in one dictionary and means nothing in another, so an answer that took
    // its rows from both columns as codes would be reading half of them out of the wrong table.
    let pair = Pair::new("two", &[two_columns("t", 6_000)]);
    let case = "CASE WHEN k = 0 THEN s ELSE t END";
    pair.agree(&format!("SELECT {case} AS c, COUNT(*) FROM t GROUP BY c ORDER BY c"));
    pair.agree(&format!("SELECT {case} AS c, n FROM t ORDER BY n LIMIT 40"));
}

#[test]
fn one_column_named_by_both_branches() {
    // The same column on both sides is one dictionary, so this one does get to be codes, and every
    // row's code is the one it arrived with whichever branch claimed it.
    let pair = Pair::new("same", &[two_columns("t", 6_000)]);
    let case = "CASE WHEN k = 0 THEN s ELSE s END";
    pair.agree(&format!("SELECT {case} AS c, COUNT(*) FROM t GROUP BY c ORDER BY c"));
}

#[test]
fn nulls_in_the_column_a_case_names() {
    // A dictionary keeps its nulls in the values it points at, so a column that arrives carrying
    // its own validity is one whose codes do not say everything the column says.
    let pair = Pair::new("nulls", &[holed("t", 6_000)]);
    let case = "CASE WHEN k = 0 THEN s ELSE '' END";
    pair.agree(&format!("SELECT {case} AS c, COUNT(*) FROM t GROUP BY c ORDER BY c NULLS LAST"));
    pair.agree(&format!("SELECT {case} AS c, n FROM t ORDER BY n LIMIT 40"));
}

#[test]
fn a_case_with_no_else_answers_null() {
    // The rows no arm claims are null, and a null is not a code, so this one reads values however
    // the column arrived.
    let pair = Pair::new("noelse", &[two_columns("t", 6_000)]);
    let case = "CASE WHEN k = 0 THEN s END";
    pair.agree(&format!("SELECT {case} AS c, COUNT(*) FROM t GROUP BY c ORDER BY c NULLS LAST"));
}

#[test]
fn two_arms_answer_in_the_order_they_are_written() {
    // Both arms are true for a third of the rows, so an answer that let the second arm claim a row
    // the first one had already taken would be a different answer rather than a slower one.
    // Twice, because `neither` is a string no row of the column holds and so has no code, and the
    // second one is the same three arms with every literal in the dictionary. One takes the coded
    // path and the other cannot, and both have to give the strings.
    let pair = Pair::new("arms", &[two_columns("t", 6_000)]);
    for otherwise in ["'neither'", "'left1'"] {
        let case = format!("CASE WHEN k < 2 THEN s WHEN k < 3 THEN '' ELSE {otherwise} END");
        pair.agree(&format!("SELECT {case} AS c, COUNT(*) FROM t GROUP BY c ORDER BY c"));
        pair.agree(&format!("SELECT {case} AS c, n FROM t ORDER BY n LIMIT 40"));
    }
}

#[test]
fn a_branch_that_works_its_value_out() {
    // A branch that computes rather than names produces a string no dictionary holds, so this is
    // the path where nothing is coded and everything is read. It is here to say that path still
    // works rather than that it is fast.
    let pair = Pair::new("computed", &[two_columns("t", 6_000)]);
    let case = "CASE WHEN k = 0 THEN SUBSTRING(s, 2) ELSE '' END";
    pair.agree(&format!("SELECT {case} AS c, COUNT(*) FROM t GROUP BY c ORDER BY c"));
}

#[test]
fn a_condition_that_would_raise_on_the_rows_it_excludes() {
    // The reason a `CASE` runs its arms over the rows no earlier arm claimed rather than over the
    // whole chunk. Answering by codes changed how the results are put together and not when the
    // conditions run, and this is the test that says so.
    let pair = Pair::new("raise", &[two_columns("t", 6_000)]);
    let case = "CASE WHEN k = 0 THEN s WHEN 10 / k > 4 THEN '' ELSE 'other' END";
    pair.agree(&format!("SELECT {case} AS c, COUNT(*) FROM t GROUP BY c ORDER BY c"));
}

#[test]
fn every_row_claimed_by_one_branch() {
    // One branch taking the whole chunk and the other taking none of it, both ways round, because
    // that is the case both the general path and the coded one shortcut.
    let pair = Pair::new("whole", &[two_columns("t", 6_000)]);
    for condition in ["k >= 0", "k < 0"] {
        let case = format!("CASE WHEN {condition} THEN s ELSE '' END");
        pair.agree(&format!("SELECT {case} AS c, COUNT(*) FROM t GROUP BY c ORDER BY c"));
    }
}

#[test]
fn the_clickbench_shape_end_to_end() {
    // Two string columns as group keys, one of them behind a `CASE`, ordered by the count and cut
    // to a few rows. ClickBench 39 without the hundred million rows.
    let pair = Pair::new("shape", &[two_columns("t", 20_000)]);
    let case = "CASE WHEN k = 0 THEN s ELSE '' END";
    pair.agree(&format!(
        "SELECT {case} AS src, t AS dst, COUNT(*) AS views FROM t \
         GROUP BY src, dst ORDER BY views DESC, src, dst LIMIT 10 OFFSET 5"
    ));
}

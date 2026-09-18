//! A scan that walks past the parts its zone maps rule out.
//!
//! The scan reads a stored table a part at a time and asks the statistics of each part whether
//! anything in it can match the filter. A part that cannot is not read. It is also not handed up
//! the pipeline as an empty chunk any more, and the scan moves on to the next part inside one call
//! instead, which is what these tests are guarding: walking is only safe if what it walks past is
//! exactly the parts with nothing in them.
//!
//! The way that goes wrong is not subtle, it is rows quietly missing from an answer, so every test
//! here asks the same question of a table in a file and of the same table held in memory. Memory
//! keeps no statistics and rules nothing out, so it reads every row and is the oracle. The file is
//! allowed to read less and is not allowed to answer differently.
//!
//! The filters are written to leave survivors in different places on purpose: at the front, at the
//! back, in the middle, scattered, and nowhere at all. A walk that ran one part too far or stopped
//! one part too early would come out right on some of those and wrong on the rest.

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
            std::env::temp_dir().join(format!("rudb-pruned-{tag}-{}.rudb", std::process::id()));
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
    fn agree(&self, query: &str) {
        for threads in [1, 4] {
            let set = format!("SET threads = {threads}");
            self.memory.execute(&set).expect("sets the thread count");
            self.file.execute(&set).expect("sets the thread count");
            let wanted = rows(&self.memory, query);
            let got = rows(&self.file, query);
            assert_eq!(got, wanted, "the file and memory disagree about {query} at {threads}");
        }
    }

    /// Asserts the file answers exactly these rows, so a test can say what it expects out loud.
    fn gives(&self, query: &str, wanted: &[i64]) {
        self.agree(query);
        let got: Vec<i64> = rows(&self.file, query)
            .into_iter()
            .map(|row| match row.first() {
                Some(&Value::BigInt(value)) => value,
                other => panic!("{query} answered {other:?} where a BIGINT was wanted"),
            })
            .collect();
        assert_eq!(got, wanted, "{query}");
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

/// Enough rows to fill many parts, with three columns that rule parts out in three different ways.
///
/// `i` climbs, which is what gives each part a range of its own and therefore makes a filter on it
/// rule parts out rather than rule nothing out. `j` is the same rows scrambled, so every part holds
/// nearly the whole range of it and a filter on it rules almost nothing out, which is the case where
/// the walk has the least to do and still has to be right. `k` is one only in the first two rows and
/// the last two, so a filter on it leaves two survivors far apart with a long run of ruled out parts
/// between them, which is the case that catches a walk stopping at the first gap it meets.
fn climbing(rows: i64) -> String {
    let ends = format!("CASE WHEN i < 2 OR i >= {} THEN 1 ELSE 0 END", rows - 2);
    format!("SELECT i, (i * 7919) % 1000003 AS j, {ends} AS k FROM range(0, {rows}) AS r(i)")
}

const ROWS: i64 = 300_000;

#[test]
fn a_filter_that_rules_out_nothing_still_reads_every_row() {
    // The case with no walking in it at all, and the only one here that reaches none of the loop,
    // which was checked rather than assumed by gating a panic on the walk and rerunning the file.
    let pair = Pair::new("all", &climbing(ROWS));
    pair.agree("SELECT COUNT(*), MIN(i), MAX(i), SUM(i) FROM t WHERE i >= 0");
}

#[test]
fn a_filter_that_rules_out_every_part_answers_nothing() {
    // Nothing survives, so the scan walks the whole table without ever handing a row up, and the
    // walk has to end rather than run off the end of the parts.
    let pair = Pair::new("none", &climbing(ROWS));
    pair.gives("SELECT i FROM t WHERE i = -1", &[]);
    pair.agree("SELECT COUNT(*) FROM t WHERE i = -1");
}

#[test]
fn survivors_at_the_very_front_are_not_walked_past() {
    let pair = Pair::new("front", &climbing(ROWS));
    pair.gives("SELECT i FROM t WHERE i < 3 ORDER BY i", &[0, 1, 2]);
}

#[test]
fn survivors_at_the_very_back_are_reached() {
    // The walk has to cross every ruled out part in the table and still stop on the last one.
    let pair = Pair::new("back", &climbing(ROWS));
    let last = ROWS - 1;
    pair.gives(&format!("SELECT i FROM t WHERE i > {} ORDER BY i", last - 2), &[last - 1, last]);
}

#[test]
fn survivors_in_the_middle_are_reached_with_ruled_out_parts_on_both_sides() {
    let middle = ROWS / 2;
    let pair = Pair::new("middle", &climbing(ROWS));
    pair.gives(
        &format!("SELECT i FROM t WHERE i >= {middle} AND i < {} ORDER BY i", middle + 3),
        &[middle, middle + 1, middle + 2],
    );
}

#[test]
fn survivors_scattered_through_nearly_every_part_are_all_found() {
    // A filter on the scrambled column, where almost every part holds a few matches and so almost
    // none of them can be ruled out. Memory is the oracle for which rows those are.
    let pair = Pair::new("scattered", &climbing(ROWS));
    pair.agree("SELECT COUNT(*), MIN(i), MAX(i), SUM(i) FROM t WHERE j < 40");
}

#[test]
fn two_ranges_far_apart_are_both_found() {
    // Survivors, then a long run of ruled out parts, then survivors again, which is the shape that
    // catches a walk that stops at the first gap it meets.
    let pair = Pair::new("two", &climbing(ROWS));
    let far = ROWS - 2;
    pair.gives("SELECT i FROM t WHERE k = 1 ORDER BY i", &[0, 1, far, far + 1]);
}

#[test]
fn an_answer_over_ruled_out_parts_agrees_however_many_workers_split_them() {
    // Each worker walks the parts of its own morsel, so the answer is only right if no worker walks
    // past a part that belongs to it or stops short of one. The band is in the middle, so a worker
    // holding the front or the back of the table has nothing but ruled out parts to walk.
    let pair = Pair::new("workers", &climbing(ROWS));
    let from = ROWS / 2;
    let query = format!(
        "SELECT COUNT(*), MIN(i), MAX(i) FROM t WHERE i >= {from} AND i < {}",
        from + 50_000
    );
    let wanted = rows(&pair.memory, &query);
    for threads in [1, 2, 4, 8] {
        pair.file.execute(&format!("SET threads = {threads}")).expect("sets the thread count");
        assert_eq!(rows(&pair.file, &query), wanted, "at {threads} workers");
    }
}

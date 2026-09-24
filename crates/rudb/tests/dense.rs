//! A group by over a key inside a known range answers what it answered without the range.
//!
//! The `aggregate_dense` pass reads the two ends a store kept for a grouping key column and, where
//! they are close enough together, hands the aggregate an array from value to slot. The aggregate
//! keeps its hash table either way and the array is only ever a shortcut to a slot it would have
//! walked to, so the whole of what this has to promise is that turning it on changes nothing but
//! the time.
//!
//! That is what every test here checks, and it checks it the one way that cannot be argued with:
//! the same query on the same rows twice, once where the pass can see a range and once where it
//! cannot, compared row for row. A pass whose output is supposed to be identical is a pass whose
//! test can be an equality rather than a set of expected rows somebody typed out, and an equality
//! catches the cases nobody thought to type out.
//!
//! The two runs are a file and the same rows in memory, which is the arrangement `pruned.rs` uses
//! and for the same reason. Only a file keeps the ends per part, so only the file side can have the
//! pass fire, and a table in memory is the same query with nothing for it to read. Each test also
//! runs the file against itself with the pass named in `disabled_optimizers`, so that a change which
//! stopped the file from keeping ends at all would fail here rather than quietly pass.
//!
//! # What these are guarding
//!
//! Three things can go wrong and all of them are quiet. A range narrower than the column leaves
//! values with no cell, which has to send them to the buckets rather than to the wrong group. A key
//! arriving in a form the fast read cannot take has to do the same. And the null key is a group of
//! its own and needs a cell like every other key, so a column with nulls in it has to come back with
//! the same number of groups both ways.

use rudb::Database;
use rudb_common::Value;

/// The same rows in a file and in memory, so one can be checked against the other.
struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    fn new(tag: &str, select: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-dense-{tag}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let create = format!("CREATE TABLE t AS {select}");
        let memory = Database::new();
        memory.execute(&create).expect("the memory table is created");
        let name = path.to_str().expect("a UTF-8 temporary path");
        // Written by one database and read by another, because the ends per part are only actually
        // in the file once the database that wrote it has let go of the rows it still holds.
        {
            let writing = Database::open(name).expect("a file name starts a native database");
            writing.execute(&create).expect("the file table is created");
            writing.execute("CHECKPOINT").expect("the file table is committed");
        }
        let file = Database::open(name).expect("the written file opens again");
        Self { memory, file, path }
    }

    /// Asserts the pass changed nothing about the answer, on one thread and on several.
    ///
    /// Several because the aggregate builds a table per instance and then one per radix partition,
    /// and only the first kind gets the array. A partition is split by hash bits and any value can
    /// land in any of them, so a partition's array would have to cover the whole range anyway. The
    /// two kinds of table meeting at the merge is what the second pass here is for.
    fn the_same_either_way(&self, query: &str) {
        for threads in [1, 8] {
            let set = format!("SET threads = {threads}");
            self.memory.execute(&set).expect("sets the thread count");
            self.file.execute(&set).expect("sets the thread count");

            self.file.execute("SET disabled_optimizers = ''").expect("clears the disabled list");
            let with = rows(&self.file, query);
            assert!(!with.is_empty(), "the query has to produce rows to be worth comparing");

            let wanted = rows(&self.memory, query);
            assert_eq!(with, wanted, "the file and memory disagree about {query} at {threads}");

            self.file
                .execute("SET disabled_optimizers = 'aggregate_dense'")
                .expect("the pass answers to its name");
            let without = rows(&self.file, query);
            assert_eq!(with, without, "the pass changed the answer to {query} at {threads}");
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

/// The name the pass answers to is a name the database knows, so a rename that missed this file
/// fails here rather than leaving every test above silently comparing a pass against itself.
#[test]
fn the_pass_is_one_the_database_can_name() {
    assert!(rudb::optimizers().contains(&"aggregate_dense"));
}

/// The ordinary case the pass is written for. A key over a few hundred values used densely, which is
/// what a month number, a status code or a foreign key into a small dimension looks like.
#[test]
fn a_dense_key_groups_the_same_as_it_did_without_a_range() {
    let pair = Pair::new("dense", "SELECT i % 300 AS k, i AS v FROM range(0, 40000) AS r(i)");
    pair.the_same_either_way("SELECT k, COUNT(*), SUM(v) FROM t GROUP BY k ORDER BY k");
}

/// The null key, which is a group of its own and has a cell of its own, so a column with nulls in it
/// has to come back with one more group than it has values.
#[test]
fn a_null_key_is_still_a_group_of_its_own() {
    let pair = Pair::new(
        "nulls",
        "SELECT CASE WHEN i % 37 = 0 THEN NULL ELSE i % 300 END AS k, i AS v
         FROM range(0, 40000) AS r(i)",
    );
    pair.the_same_either_way("SELECT k, COUNT(*), SUM(v) FROM t GROUP BY k ORDER BY k");
    let groups = rows(&pair.file, "SELECT COUNT(*) FROM (SELECT k FROM t GROUP BY k)");
    assert_eq!(groups, vec![vec![Value::BigInt(301)]], "the null key and the three hundred values");
}

/// A key whose values run below zero, which is where an off by one in the base of the array would
/// show up and where nothing else would.
#[test]
fn a_key_that_runs_below_zero_groups_the_same() {
    let pair =
        Pair::new("signed", "SELECT (i % 601) - 300 AS k, i AS v FROM range(0, 40000) AS r(i)");
    pair.the_same_either_way("SELECT k, COUNT(*), SUM(v) FROM t GROUP BY k ORDER BY k");
}

/// A key of one value, which is the narrowest range there is and the one an off by one in the length
/// of the array would show up in.
#[test]
fn a_key_of_one_value_groups_the_same() {
    let pair = Pair::new("single", "SELECT 7 AS k, i AS v FROM range(0, 10000) AS r(i)");
    pair.the_same_either_way("SELECT k, COUNT(*), SUM(v) FROM t GROUP BY k ORDER BY k");
}

/// A filter under the aggregate, which narrows the rows the keys come out of without narrowing the
/// range the pass read off the column. That is the right way round: the range has to be a superset
/// of what arrives, and a filter can only ever make what arrives smaller.
#[test]
fn a_filter_under_the_aggregate_changes_nothing() {
    let pair = Pair::new("filtered", "SELECT i % 300 AS k, i AS v FROM range(0, 40000) AS r(i)");
    pair.the_same_either_way("SELECT k, COUNT(*) FROM t WHERE k > 200 GROUP BY k ORDER BY k");
}

/// Enough groups that the aggregate partitions rather than keeping one table per instance, which is
/// the path where only some of the tables have the array.
#[test]
fn a_key_wide_enough_to_partition_groups_the_same() {
    let pair = Pair::new("wide", "SELECT i % 20000 AS k, i AS v FROM range(0, 200000) AS r(i)");
    pair.the_same_either_way("SELECT k, COUNT(*), SUM(v) FROM t GROUP BY k ORDER BY k");
}

/// A grouping key that is not one column, which the pass leaves alone because there is no one value
/// to address by. Here so that a later change which starts addressing the first column of a wider
/// key has a test to fail.
#[test]
fn a_key_of_two_columns_groups_the_same() {
    let pair = Pair::new("pair", "SELECT i % 300 AS k, i AS v FROM range(0, 40000) AS r(i)");
    pair.the_same_either_way("SELECT k, v % 7, COUNT(*) FROM t GROUP BY k, v % 7 ORDER BY k, 2");
}

/// Counts and nothing else, which the aggregate keeps in arrays the key indexes rather than in its
/// table. A null key and a null argument both have to come out the way the table had them: the
/// null key as a group of its own and a null argument as a row `COUNT(*)` sees and `COUNT(v)` does
/// not, so a group whose every `v` is null is still a group, with a count of zero.
#[test]
fn counts_alone_group_the_same_with_nulls_in_the_key_and_the_argument() {
    let pair = Pair::new(
        "counts",
        "SELECT CASE WHEN i % 37 = 0 THEN NULL ELSE i % 300 END AS k,
         CASE WHEN i % 300 = 5 OR i % 7 = 0 THEN NULL ELSE i END AS v
         FROM range(0, 40000) AS r(i)",
    );
    pair.the_same_either_way(
        "SELECT k, COUNT(*), COUNT(v), COUNT(*) AS again FROM t GROUP BY k ORDER BY k",
    );
    pair.the_same_either_way("SELECT k, COUNT(v) FROM t GROUP BY k ORDER BY k NULLS FIRST");
}

/// The counts of a narrow key below zero and of a key wide enough that eight threads each see most
/// of its values, which is where adding one instance's arrays into another's has to agree with
/// merging their tables.
#[test]
fn counts_alone_group_the_same_below_zero_and_across_threads() {
    let pair = Pair::new(
        "counted",
        "SELECT ((i % 601) - 300)::SMALLINT AS s, (i * 7919 % 50000)::INTEGER AS k, i AS v
         FROM range(0, 300000) AS r(i)",
    );
    pair.the_same_either_way("SELECT s, COUNT(*) FROM t GROUP BY s ORDER BY s");
    pair.the_same_either_way("SELECT k, COUNT(v), COUNT(*) FROM t GROUP BY k ORDER BY k");
    pair.the_same_either_way(
        "SELECT c, COUNT(*) FROM (SELECT k, COUNT(*) AS c FROM t WHERE v % 3 = 0 GROUP BY k) \
         GROUP BY c ORDER BY c",
    );
}

/// The shape of TPC-H q13, a count of the matches of a left join grouped by the key of the side
/// every row is kept from. The keys with no match arrive after the others with a null argument,
/// and each is a group with a count of zero.
#[test]
fn counts_over_a_left_join_group_the_same() {
    let pair = Pair::new("joined", "SELECT i AS k, i * 3 AS v FROM range(1, 20001) AS r(i)");
    let other = "CREATE TABLE u AS SELECT i % 30000 AS f, i AS w FROM range(0, 90000) AS r(i) \
                 WHERE i % 30000 % 3 <> 0";
    pair.memory.execute(other).expect("the memory side is made");
    pair.file.execute(other).expect("the file side is made");
    pair.the_same_either_way(
        "SELECT c, COUNT(*) FROM (SELECT k, COUNT(w) AS c FROM t LEFT JOIN u ON k = f \
         GROUP BY k) GROUP BY c ORDER BY c",
    );
    pair.the_same_either_way(
        "SELECT k, COUNT(w), COUNT(*) FROM t LEFT JOIN u ON k = f GROUP BY k ORDER BY k",
    );
}

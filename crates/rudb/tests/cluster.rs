//! A group by over a key the table is stored in order of answers what it answered without the order.
//!
//! The `aggregate_cluster` pass marks an aggregate whose one key is a column the file says never
//! goes down, and the aggregate then closes every group strictly inside a chunk without putting it
//! in the hash table. Only the first and last run of a chunk can carry on into another chunk, and
//! those go the ordinary way. So the whole promise is that turning it on changes nothing but the
//! time, and the tests check it the way `dense.rs` does: the file against the same rows in memory,
//! and the file against itself with the pass named in `disabled_optimizers`, on one thread and on
//! several.

use rudb::Database;
use rudb_common::Value;

struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    fn new(tag: &str, select: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-cluster-{tag}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let create = format!("CREATE TABLE t AS {select}");
        let memory = Database::new();
        memory.execute(&create).expect("the memory table is created");
        let name = path.to_str().expect("a UTF-8 temporary path");
        {
            let writing = Database::open(name).expect("a file name starts a native database");
            writing.execute(&create).expect("the file table is created");
            writing.execute("CHECKPOINT").expect("the file table is committed");
        }
        let file = Database::open(name).expect("the written file opens again");
        Self { memory, file, path }
    }

    fn explain(&self, query: &str) -> String {
        let result = self.file.query(&format!("EXPLAIN {query}")).expect("the plan prints");
        (0..result.len())
            .flat_map(|row| (0..result.width()).map(move |column| (row, column)))
            .map(|(row, column)| format!("{:?}", result.value_at(row, column)))
            .collect::<Vec<_>>()
            .join("\n")
    }

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
                .execute("SET disabled_optimizers = 'aggregate_cluster'")
                .expect("the pass answers to its name");
            let without = rows(&self.file, query);
            assert_eq!(with, without, "the pass changed the answer to {query} at {threads}");
        }
        self.file.execute("SET disabled_optimizers = ''").expect("clears the disabled list");
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn rows(database: &Database, query: &str) -> Vec<Vec<Value>> {
    let result = database.query(query).expect("the query ran");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

const CLOSED: &str = "groups closed in key order";

#[test]
fn the_pass_is_one_the_database_can_name() {
    assert!(rudb::optimizers().contains(&"aggregate_cluster"));
}

/// The case the pass is for, a few rows to a key the way lineitem has a few lines to an order,
/// over enough rows that every instance sees many chunks and groups cross chunk edges.
#[test]
fn a_sorted_key_groups_the_same_as_it_did_without_the_order() {
    let pair = Pair::new(
        "sorted",
        "SELECT i // 4 AS k, i AS v, i % 7 AS w, 'x' || (i % 13) AS s FROM range(0, 300000) AS r(i)",
    );
    let query = "SELECT k, COUNT(*), SUM(v), MIN(w), MAX(s), AVG(v) FROM t GROUP BY k ORDER BY k";
    assert!(pair.explain(query).contains(CLOSED), "the pass did not fire on a sorted key");
    pair.the_same_either_way(query);
}

/// The shape of TPC-H q18's inner query, a filter over the groups after the fact, and a filter on
/// the rows before, which drops rows out of runs and whole runs out of chunks.
#[test]
fn filters_before_and_after_leave_the_answer_alone() {
    let pair =
        Pair::new("filtered", "SELECT i // 5 AS k, i % 11 AS v FROM range(0, 300000) AS r(i)");
    pair.the_same_either_way(
        "SELECT k FROM t WHERE v <> 3 GROUP BY k HAVING SUM(v) > 30 ORDER BY k",
    );
    pair.the_same_either_way("SELECT k, SUM(v) FROM t WHERE v > 8 GROUP BY k ORDER BY k");
}

/// One row to a key, which is every group closed and nothing in the table but the chunk edges.
#[test]
fn a_unique_key_is_every_group_closed() {
    let pair = Pair::new("unique", "SELECT i AS k, i * 3 AS v FROM range(0, 200000) AS r(i)");
    pair.the_same_either_way("SELECT k, SUM(v) FROM t GROUP BY k ORDER BY k");
}

/// A column the table is not sorted on is not marked, so the pass leaves it alone.
#[test]
fn an_unsorted_key_is_left_alone() {
    let pair = Pair::new("unsorted", "SELECT i % 1000 AS k, i AS v FROM range(0, 100000) AS r(i)");
    let query = "SELECT k, SUM(v) FROM t GROUP BY k ORDER BY k";
    assert!(!pair.explain(query).contains(CLOSED), "the pass fired on an unsorted key");
    pair.the_same_either_way(query);
}

/// Counts and totals over nulls and decimals, which a closed group answers from its run without
/// an accumulator. Every fifth key has no value at all in `m`, so its total is null and its count
/// is zero, and `n` is null on a third of the rows, which cuts through runs.
#[test]
fn totals_answered_from_their_runs_keep_their_nulls_and_scale() {
    let pair = Pair::new(
        "runs",
        "SELECT i // 4 AS k, ((i % 1000) / 7)::DECIMAL(15, 2) AS d, \
         CASE WHEN i % 3 = 0 THEN NULL ELSE i END AS n, \
         CASE WHEN (i // 4) % 5 = 0 THEN NULL ELSE (i % 9)::TINYINT END AS m \
         FROM range(0, 300000) AS r(i)",
    );
    let query = "SELECT k, COUNT(*), COUNT(n), SUM(n), SUM(d), SUM(m), COUNT(m) FROM t \
                 GROUP BY k ORDER BY k";
    assert!(pair.explain(query).contains(CLOSED), "the pass did not fire on a sorted key");
    pair.the_same_either_way(query);
}

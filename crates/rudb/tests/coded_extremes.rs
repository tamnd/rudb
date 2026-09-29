//! A `min` or a `max` over a string column in a native file goes out of the aggregate as codes into
//! the column's dictionary, and whatever is done with it above answers what the same rows in memory
//! answer.
//!
//! In memory a group holds the string it has seen, and in the file it holds the rank of the code,
//! so each query here runs over both and on one and four threads. The queries put the answer
//! through the things that read a coded column differently from a flat one: a filter that keeps a
//! few groups, a function over it, a grouping on it, a sort, and groups that saw only nulls.

use rudb::Database;
use rudb_common::Value;

const QUERIES: [&str; 8] = [
    "SELECT n % 7 AS k, min(s), max(s), count(*) FROM t GROUP BY k",
    "SELECT n % 11 AS k, min(s) FROM t WHERE n % 11 = 3 OR s IS NOT NULL GROUP BY k",
    "SELECT left(r, 2) AS k, min(r), max(r), sum(c) FROM \
     (SELECT s AS r, count(*) AS c FROM t GROUP BY s) GROUP BY k HAVING sum(c) > 40",
    "SELECT k, upper(m), length(m) FROM (SELECT n % 13 AS k, min(s) AS m FROM t GROUP BY k)",
    "SELECT m, count(*) FROM (SELECT n % 97 AS k, max(s) AS m FROM t GROUP BY k) GROUP BY m",
    "SELECT n % 5 AS k, min(s) AS m FROM t GROUP BY k ORDER BY m DESC LIMIT 3",
    "SELECT n % 3 AS k, min(s), min(s) FROM t GROUP BY k",
    "SELECT n % 17 AS k, min(s) FROM t WHERE n % 17 = 4 GROUP BY k",
];

fn sorted(database: &Database, query: &str) -> Vec<Vec<Value>> {
    let mut rows: Vec<Vec<Value>> = database.query(query).expect("the query ran").rows().collect();
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

const TABLE: [&str; 2] = [
    "CREATE TABLE t(n BIGINT, s VARCHAR)",
    // Group 4 of `n % 17` is null on every row, and the rest hold a few hundred strings.
    "INSERT INTO t SELECT i, CASE WHEN i % 17 = 4 THEN NULL \
     ELSE 'v' || ((i * 7919) % 613)::VARCHAR || '-' || (i % 29)::VARCHAR END \
     FROM range(20000) r(i)",
];

#[test]
fn coded_extremes_answer_what_the_rows_in_memory_answer() {
    let memory = Database::new();
    for sql in TABLE {
        memory.execute(sql).expect("the memory table is made");
    }
    let path =
        std::env::temp_dir().join(format!("rudb-coded-extremes-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");
    {
        let writing = Database::open(name).expect("a file name starts a native database");
        for sql in TABLE {
            writing.execute(sql).expect("the file table is made");
        }
        writing.execute("CHECKPOINT").expect("the file table is committed");
    }
    let file = Database::open(name).expect("the written file opens again");
    for query in QUERIES {
        let wanted = sorted(&memory, query);
        assert!(!wanted.is_empty(), "no rows to compare: {query}");
        for threads in [1, 4] {
            file.execute(&format!("SET threads = {threads}")).expect("sets the threads");
            assert_eq!(sorted(&file, query), wanted, "{threads} threads: {query}");
        }
    }
    drop(file);
    let _ = std::fs::remove_file(&path);
}

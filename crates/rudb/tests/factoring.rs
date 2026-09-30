//! A sum of products grouped by the small factors of the products first answers what the one
//! grouping answers.
//!
//! Each query runs with the pass and with it turned off, over a table in memory and the same rows in
//! a native file, on one thread and on four. The file states how many values each decimal column can
//! hold from its two ends, and one of its plans is checked to have taken the two step form. The
//! averages are compared as they come out, since the two step form divides the same total by the
//! same count.

use rudb::Database;
use rudb_common::Value;

const QUERIES: [&str; 6] = [
    "SELECT k, s, sum(q), sum(p), sum(p * (1 - d)), sum(p * (1 - d) * (1 + x)), avg(q), avg(p), \
     avg(d), count(*) FROM t WHERE n % 5 <> 2 GROUP BY k, s",
    "SELECT sum(p * d) FROM t WHERE d BETWEEN 0.02 AND 0.04 AND n % 3 = 1",
    "SELECT k, sum(n * m), avg(m), min(p), max(d), count(d), count(x) FROM t GROUP BY k",
    "SELECT k, sum(d), sum(d * x), sum(p * d * x * 2) FROM t GROUP BY k",
    "SELECT s, sum(p * (1 - d)) AS r FROM t GROUP BY s HAVING sum(q) > 10 ORDER BY r DESC",
    "SELECT k, sum(p * d) FROM t WHERE d IS NULL GROUP BY k",
];

fn sorted(database: &Database, query: &str) -> Vec<Vec<Value>> {
    let mut rows: Vec<Vec<Value>> = database.query(query).expect("the query ran").rows().collect();
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

fn agree(database: &Database) {
    for query in QUERIES {
        database.execute("SET disabled_optimizers = 'factoring'").expect("turns it off");
        let wanted = sorted(database, query);
        database.execute("RESET disabled_optimizers").expect("turns it back on");
        for threads in [1, 4] {
            database.execute(&format!("SET threads = {threads}")).expect("sets the threads");
            assert_eq!(sorted(database, query), wanted, "{threads} threads: {query}");
        }
    }
}

const TABLE: [&str; 2] = [
    "CREATE TABLE t(k VARCHAR, s VARCHAR, n BIGINT, m INTEGER, q DECIMAL(15,2), \
     p DECIMAL(15,2), d DECIMAL(15,2), x DECIMAL(15,2))",
    // Three flags, two statuses, eleven discounts with every nineteenth one null, nine taxes and
    // seven small integers, over sixty thousand rows.
    "INSERT INTO t SELECT chr((65 + i % 3)::INTEGER), chr((70 + i % 2)::INTEGER), i, i % 7, (1 + i % 50)::DECIMAL(15,2), \
     (900 + (i * 7919) % 100000 / 100.0)::DECIMAL(15,2), \
     CASE WHEN i % 19 = 3 THEN NULL ELSE ((i * 31) % 11 / 100.0)::DECIMAL(15,2) END, \
     ((i * 13) % 9 / 100.0)::DECIMAL(15,2) FROM range(60000) r(i)",
];

#[test]
fn factoring_answers_what_one_grouping_answers() {
    let memory = Database::new();
    for sql in TABLE {
        memory.execute(sql).expect("the memory table is made");
    }
    agree(&memory);

    let path = std::env::temp_dir().join(format!("rudb-factoring-{}.rudb", std::process::id()));
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
    let plan = file.plan(QUERIES[0]).expect("the plan");
    assert_eq!(plan.matches("Aggregate #").count(), 2, "not grouped by d and x first:\n{plan}");
    agree(&file);
    drop(file);
    let _ = std::fs::remove_file(&path);
}

//! A group key that reads no column is added back after the grouping and answers what it did in it.
//!
//! These run each query with the pass that moves the constant out and with it turned off, over a
//! table in memory and the same rows in a native file, and over no rows at all, where a grouping by
//! constants alone and a grouping with a constant beside a column both have no groups.

use rudb::Database;
use rudb_common::Value;

const QUERIES: [&str; 6] = [
    "SELECT 1, u, count(*) AS c FROM t GROUP BY 1, u ORDER BY c DESC, u LIMIT 5",
    "SELECT u, 'x' AS tag, sum(n) FROM t GROUP BY u, 'x'",
    "SELECT NULL AS z, u, max(n) FROM t GROUP BY NULL, u",
    "SELECT 7 AS s, u, count(*) FROM t GROUP BY 1, u HAVING count(*) > 40",
    "SELECT 1 + 2 AS s, n % 5 AS m, count(*) FROM t GROUP BY 1 + 2, n % 5",
    "SELECT 1, 2, count(*) FROM t GROUP BY 1, 2",
];

fn sorted(database: &Database, query: &str) -> Vec<Vec<Value>> {
    let mut rows: Vec<Vec<Value>> = database.query(query).expect("the query ran").rows().collect();
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

fn agree(database: &Database) {
    for query in QUERIES {
        database.execute("SET disabled_optimizers = 'dependent_group_keys'").expect("turns it off");
        let wanted = sorted(database, query);
        database.execute("RESET disabled_optimizers").expect("turns it back on");
        for threads in [1, 4] {
            database.execute(&format!("SET threads = {threads}")).expect("sets the threads");
            assert_eq!(sorted(database, query), wanted, "{threads} threads: {query}");
        }
    }
}

const TABLE: [&str; 2] = [
    "CREATE TABLE t(u VARCHAR, n BIGINT)",
    "INSERT INTO t SELECT CASE WHEN i % 17 = 0 THEN NULL ELSE 'u' || (i % 23)::VARCHAR END, i \
     FROM range(3000) r(i)",
];

#[test]
fn constant_keys_answer_the_same_in_memory_and_in_a_file() {
    let memory = Database::new();
    for sql in TABLE {
        memory.execute(sql).expect("the memory table is made");
    }
    agree(&memory);
    let plan = memory.plan(QUERIES[0]).expect("the plan");
    assert!(plan.contains("groups=[#"), "the constant is still a key:\n{plan}");

    let path = std::env::temp_dir().join(format!("rudb-constant-keys-{}.rudb", std::process::id()));
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
    agree(&file);
    drop(file);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn no_rows_make_no_groups() {
    let database = Database::new();
    database.execute("CREATE TABLE t(u VARCHAR, n BIGINT)").expect("the table is made");
    agree(&database);
    assert!(sorted(&database, QUERIES[0]).is_empty());
    assert!(sorted(&database, QUERIES[5]).is_empty());
}

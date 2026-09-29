//! A grouping by functions of one string column, done by grouping by the column first, answers what
//! it answers done in one step.
//!
//! Each query runs with the pass and with it turned off, over a table in memory and the same rows in
//! a native file, on one thread and on four. The file is where the pass has the distinct count it
//! needs, and one of its plans is checked to have taken the two step form. The averages are compared
//! as they come out, since the two step form divides the same total by the same count.

use rudb::Database;
use rudb_common::Value;

const QUERIES: [&str; 7] = [
    "SELECT regexp_replace(s, '^v([0-9])[0-9]*-.*$', '\\1') AS k, avg(strlen(s)) AS l, count(*), \
     min(s) FROM t GROUP BY k",
    "SELECT left(s, 2) AS k, sum(n), count(s), max(s), max(upper(s)) FROM t GROUP BY k",
    "SELECT left(s, 3) AS k, avg(length(s)), count(*) AS c FROM t GROUP BY k HAVING count(*) > 50 \
     ORDER BY c DESC, k LIMIT 5",
    "SELECT right(s, 2) AS k, avg(n), min(s), sum(length(s)) FROM t WHERE n % 3 <> 1 GROUP BY k",
    "SELECT left(s, 2) AS a, length(s) AS b, count(*), min(s), max(s) FROM t GROUP BY a, b",
    "SELECT upper(s) AS k, avg(length(s) - 3), count(DISTINCT n % 4) FROM t GROUP BY k",
    "SELECT lower(s) AS k, count(s), avg(strlen(s)) FROM t WHERE s IS NULL GROUP BY k",
];

fn sorted(database: &Database, query: &str) -> Vec<Vec<Value>> {
    let mut rows: Vec<Vec<Value>> = database.query(query).expect("the query ran").rows().collect();
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

fn agree(database: &Database) {
    // The group of nulls counts none of its values, whether or not the count becomes a sum of one.
    let nulls = sorted(database, "SELECT count(s), count(length(s)), count(*) FROM t GROUP BY s");
    assert!(nulls.contains(&vec![Value::BigInt(0), Value::BigInt(0), Value::BigInt(1177)]));
    for query in QUERIES {
        database.execute("SET disabled_optimizers = 'pre_grouping'").expect("turns it off");
        let wanted = sorted(database, query);
        database.execute("RESET disabled_optimizers").expect("turns it back on");
        for threads in [1, 4] {
            database.execute(&format!("SET threads = {threads}")).expect("sets the threads");
            assert_eq!(sorted(database, query), wanted, "{threads} threads: {query}");
        }
    }
}

const TABLE: [&str; 2] = [
    "CREATE TABLE t(n BIGINT, s VARCHAR)",
    // Every seventeenth row is null, and the rest hold 679 strings of a few lengths.
    "INSERT INTO t SELECT i, CASE WHEN i % 17 = 4 THEN NULL \
     ELSE 'v' || (i % 97)::VARCHAR || '-' || (i % 7)::VARCHAR END \
     FROM range(20000) r(i)",
];

#[test]
fn pre_grouping_answers_what_one_grouping_answers() {
    let memory = Database::new();
    for sql in TABLE {
        memory.execute(sql).expect("the memory table is made");
    }
    agree(&memory);

    let path = std::env::temp_dir().join(format!("rudb-pre-grouping-{}.rudb", std::process::id()));
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
    assert_eq!(plan.matches("Aggregate #").count(), 2, "not grouped by s first:\n{plan}");
    agree(&file);
    drop(file);
    let _ = std::fs::remove_file(&path);
}

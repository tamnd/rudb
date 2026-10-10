//! A file with its graph and statistics sections dropped answers what it answered with them.
//!
//! Section 3.1 of spec/graph/03-the-file-format.md says deleting every graph section changes no
//! answer. The settings that turn the graph layer off keep a plan from using a section and nothing
//! more, so this drops them from the file and asks again.

use rudb::native::{GRAPH_KINDS, LINK_COUNTS, STATISTICS_KINDS, strip_sections};
use rudb::{Config, Database};
use rudb_common::Value;

const TABLES: [&str; 4] = [
    "CREATE TABLE orders(o_orderkey BIGINT PRIMARY KEY, o_orderdate DATE)",
    "CREATE TABLE lineitem(l_orderkey BIGINT REFERENCES orders(o_orderkey), l_linenumber INTEGER, \
     l_quantity INTEGER, PRIMARY KEY (l_orderkey, l_linenumber))",
    "INSERT INTO orders SELECT i, DATE '1992-01-01' + CAST(i % 2400 AS INTEGER) FROM range(40000) \
     r(i)",
    "INSERT INTO lineitem SELECT i // 4, i % 4, CAST(i % 50 AS INTEGER) FROM range(160000) r(i)",
];

const QUERIES: [&str; 2] = [
    "SELECT extract(year FROM o_orderdate) AS y, sum(l_quantity), count(*) FROM lineitem, orders \
     WHERE l_orderkey = o_orderkey GROUP BY y ORDER BY y",
    "SELECT count(*), min(o_orderdate) FROM lineitem, orders WHERE l_orderkey = o_orderkey AND \
     l_quantity = 7 AND o_orderdate < DATE '1993-01-01'",
];

struct File(std::path::PathBuf);

impl Drop for File {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql).expect(sql).rows().collect()
}

fn answers(name: &str) -> (Vec<Vec<Vec<Value>>>, String) {
    // Read only, so that nothing a close might write puts a section back.
    let db = Database::open_with(name, Config::default().with_read_only(true)).expect("it opens");
    let plan = rows(&db, &format!("EXPLAIN {}", QUERIES[0]))
        .into_iter()
        .flatten()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    (QUERIES.iter().map(|sql| rows(&db, sql)).collect(), plan)
}

#[test]
fn dropping_the_sections_changes_no_answer() {
    let file =
        File(std::env::temp_dir().join(format!("rudb-strip-sections-{}.rudb", std::process::id())));
    let _ = std::fs::remove_file(&file.0);
    let name = file.0.to_str().expect("a UTF-8 temporary path");
    {
        let db = Database::open(name).expect("a new file");
        for sql in TABLES {
            db.execute(sql).expect(sql);
        }
        db.execute("CHECKPOINT").expect("the checkpoint builds the sections");
    }
    let (wanted, plan) = answers(name);
    assert!(plan.contains("reads the link") || plan.contains("reads the key map"), "{plan}");

    let picked = |kind: &[u8; 8]| {
        GRAPH_KINDS.contains(&kind) || STATISTICS_KINDS.contains(&kind) || kind == LINK_COUNTS
    };
    let dropped = strip_sections(name, picked).expect("the sections are dropped");
    assert!(dropped > 0, "the checkpoint wrote no section to drop");
    assert_eq!(strip_sections(name, picked).expect("a second pass"), 0);

    let (got, plan) = answers(name);
    assert!(!plan.contains("reads the link") && !plan.contains("reads the key map"), "{plan}");
    assert_eq!(got, wanted);
}

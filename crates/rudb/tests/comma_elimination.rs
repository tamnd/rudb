//! Join elimination over a verified relationship fires whichever way the join was written.
//!
//! `spec/stats/07-graph-statistics.md` section 7.3 licenses deleting an inner join to a parent
//! nothing reads, and the licence is about the relationship rather than about the syntax. A comma
//! join with the equality in `WHERE` and an explicit `JOIN ... ON` are the same plan by the time the
//! pass runs, so a rule that fires on one and not the other is a rule that fires on almost nothing:
//! all twenty two TPC-H queries are written with comma joins.

use rudb::Database;
use rudb_common::Value;

const TABLES: [&str; 4] = [
    "CREATE TABLE orders(o_orderkey BIGINT PRIMARY KEY, o_orderdate DATE)",
    "CREATE TABLE lineitem(l_orderkey BIGINT REFERENCES orders(o_orderkey), l_linenumber INTEGER, \
     l_quantity INTEGER, PRIMARY KEY (l_orderkey, l_linenumber))",
    "INSERT INTO orders SELECT i, DATE '1992-01-01' + CAST(i % 2400 AS INTEGER) FROM range(40000) \
     r(i)",
    "INSERT INTO lineitem SELECT i // 4, i % 4, CAST(i % 50 AS INTEGER) FROM range(160000) r(i)",
];

const ON: &str = "SELECT count(*) FROM lineitem JOIN orders ON l_orderkey = o_orderkey";
const COMMA: &str = "SELECT count(*) FROM lineitem, orders WHERE l_orderkey = o_orderkey";

struct File(std::path::PathBuf);

impl Drop for File {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn plan(db: &Database, sql: &str) -> String {
    db.query(&format!("EXPLAIN {sql}"))
        .expect(sql)
        .rows()
        .flatten()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_comma_join_is_eliminated_the_way_an_on_join_is() {
    let file = File(
        std::env::temp_dir().join(format!("rudb-comma-elimination-{}.rudb", std::process::id())),
    );
    let _ = std::fs::remove_file(&file.0);
    let name = file.0.to_str().expect("a UTF-8 temporary path");
    {
        let db = Database::open(name).expect("a new file");
        for sql in TABLES {
            db.execute(sql).expect(sql);
        }
        db.execute("CHECKPOINT").expect("the checkpoint builds the link");
    }
    let db = Database::open(name).expect("the file opens again");
    db.execute("SET graph_sections = true").expect("on");

    let wanted = vec![vec![Value::BigInt(160_000)]];
    assert_eq!(db.query(ON).expect(ON).rows().collect::<Vec<_>>(), wanted);
    assert_eq!(db.query(COMMA).expect(COMMA).rows().collect::<Vec<_>>(), wanted);

    let on = plan(&db, ON);
    assert!(!on.contains("Join"), "the ON join was not eliminated:\n{on}");
    let comma = plan(&db, COMMA);
    assert!(!comma.contains("Join"), "the comma join was not eliminated:\n{comma}");
}

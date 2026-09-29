//! A foreign key a table was created with is a relationship, so a checkpoint builds its link and a
//! plan reads it, with nothing set but the switch that turns the graph layer on.
//!
//! Section 2.5 of spec/graph/02-the-data-model.md: every foreign key is a declared relationship and
//! no new syntax is invented. Before this only `SET graph_links` declared one, so a database loaded
//! with the TPC-H schema as it is written, keys and all, got no link for any of them.

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

const QUERY: &str = "SELECT extract(year FROM o_orderdate) AS y, sum(l_quantity), count(*) FROM \
                     lineitem, orders WHERE l_orderkey = o_orderkey GROUP BY y ORDER BY y";

struct File(std::path::PathBuf);

impl Drop for File {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql).expect(sql).rows().collect()
}

fn plan(db: &Database) -> String {
    rows(db, &format!("EXPLAIN {QUERY}"))
        .into_iter()
        .flatten()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_foreign_key_is_a_link_the_plan_reads() {
    let file =
        File(std::env::temp_dir().join(format!("rudb-foreign-links-{}.rudb", std::process::id())));
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
    let listed = rows(&db, "SELECT name, link FROM rudb_links()");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0][0], Value::Varchar("lineitem(l_orderkey) -> orders(o_orderkey)".into()));
    assert!(!listed[0][1].is_null(), "the link was not kept: {listed:?}");

    // Off: the same file, a hash join and the answer to hold the link to.
    db.execute("SET graph_sections = false").expect("off");
    let wanted = rows(&db, QUERY);
    assert!(!plan(&db).contains("reads the link"), "{}", plan(&db));

    // On, with a cache small enough that a table of forty thousand orders does not fit in it.
    db.execute("SET graph_sections = true").expect("on");
    db.execute("SET graph_cache_bytes = 1024").expect("a small cache");
    assert!(plan(&db).contains("reads the link"), "{}", plan(&db));
    for threads in [1, 4] {
        db.execute(&format!("SET threads = {threads}")).expect("threads");
        assert_eq!(rows(&db, QUERY), wanted, "{threads} threads");
    }
}

/// A relationship declared with `SET graph_links` in the session that checkpointed is still known
/// after that session is gone, because the file kept its link and the link says what it is for.
///
/// This is how rudb-bench loads TPC-H: the tables arrive from Parquet with no keys, one process
/// checkpoints with the relationships set, and every query runs in a fresh process that sets
/// nothing. Before, the scan read the links in those processes and the planner never heard of them.
#[test]
fn a_link_declared_for_one_session_is_known_to_every_later_one() {
    let file =
        File(std::env::temp_dir().join(format!("rudb-kept-links-{}.rudb", std::process::id())));
    let _ = std::fs::remove_file(&file.0);
    let name = file.0.to_str().expect("a UTF-8 temporary path");
    {
        let db = Database::open(name).expect("a new file");
        for sql in [
            "CREATE TABLE orders(o_orderkey BIGINT, o_orderdate DATE)",
            "CREATE TABLE lineitem(l_orderkey BIGINT, l_linenumber INTEGER, l_quantity INTEGER)",
            TABLES[2],
            TABLES[3],
        ] {
            db.execute(sql).expect(sql);
        }
        db.execute("SET graph_links = 'lineitem(l_orderkey) -> orders(o_orderkey)'").expect("sets");
        db.execute("CHECKPOINT").expect("the checkpoint builds the link");
    }
    let db = Database::open(name).expect("the file opens again");
    assert_eq!(rows(&db, "SELECT current_setting('graph_links')"), [[Value::Varchar("".into())]]);
    let listed = rows(&db, "SELECT name, link FROM rudb_links()");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0][0], Value::Varchar("lineitem(l_orderkey) -> orders(o_orderkey)".into()));
    assert!(!listed[0][1].is_null(), "the link was not kept: {listed:?}");

    db.execute("SET graph_sections = false").expect("off");
    let wanted = rows(&db, QUERY);
    assert!(!plan(&db).contains("reads the link"), "{}", plan(&db));

    db.execute("SET graph_sections = true").expect("on");
    db.execute("SET graph_cache_bytes = 1024").expect("a small cache");
    assert!(plan(&db).contains("reads the link"), "{}", plan(&db));
    for threads in [1, 4] {
        db.execute(&format!("SET threads = {threads}")).expect("threads");
        assert_eq!(rows(&db, QUERY), wanted, "{threads} threads");
    }
}

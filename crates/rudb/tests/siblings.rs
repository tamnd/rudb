//! A semi or an anti join with more than the key in its condition, answered by walking each row to
//! the other children of its parent rather than by a hash join over a second scan.
//!
//! The answers are checked against the same queries with the graph layer off, which is the hash
//! join, on a child stored in its parent's order and on one stored in no order at all, since the
//! two find their siblings through different sections.

use rudb::{Database, Value};

/// Three thousand customers with up to seven orders each, the relationship declared and built and
/// the graph layer on. `shuffled` stores the orders in an order that has nothing to do with the
/// customers', so the link is packed and the siblings come out of the adjacency.
fn database(name: &str, shuffled: bool) -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "rudb-siblings-{name}-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let database = Database::open(path.to_str().expect("a UTF-8 temporary path")).expect("opens");
    database.execute("SET threads = 1").expect("sets the thread count");
    database.execute("CREATE TABLE customer (c_custkey INTEGER, c_name VARCHAR)").expect("creates");
    database
        .execute(
            "CREATE TABLE orders (o_orderkey INTEGER, o_custkey INTEGER, o_clerk INTEGER, \
             o_late BOOLEAN)",
        )
        .expect("creates");
    database
        .execute("INSERT INTO customer SELECT i, 'c' || i FROM range(1, 3001) AS r(i)")
        .expect("loads");
    let order = if shuffled { "(i * 7919) % 21000" } else { "i" };
    database
        .execute(&format!(
            "INSERT INTO orders SELECT i, 1 + i // 7, CASE WHEN i % 13 = 5 THEN NULL ELSE \
             (i * 31) % 5 END, i % 3 = 0 FROM range(0, 21000) AS r(i) WHERE i % 11 <> 4 ORDER \
             BY {order}"
        ))
        .expect("loads");
    database.execute("SET graph_links = 'orders(o_custkey) -> customer(c_custkey)'").expect("sets");
    database.execute("CHECKPOINT").expect("builds the link");
    database.execute("SET graph_sections = 'on'").expect("turns the layer on");
    (database, path)
}

fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).expect("the query ran");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

fn plan(database: &Database, sql: &str) -> String {
    let result = database.query(&format!("EXPLAIN ANALYZE {sql}")).expect("the explain ran");
    match result.value_at(0, 1) {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    }
}

/// The shape of TPC-H q21, one `EXISTS` and one `NOT EXISTS` over the other children of the
/// same parent, each with a condition the key alone does not answer.
const QUERIES: [&str; 6] = [
    "SELECT count(*), sum(o1.o_orderkey) FROM orders o1 WHERE o1.o_late AND EXISTS (SELECT * \
     FROM orders o2 WHERE o2.o_custkey = o1.o_custkey AND o2.o_clerk <> o1.o_clerk) AND NOT \
     EXISTS (SELECT * FROM orders o3 WHERE o3.o_custkey = o1.o_custkey AND o3.o_clerk <> \
     o1.o_clerk AND o3.o_late)",
    "SELECT count(*), sum(o1.o_orderkey) FROM orders o1 WHERE EXISTS (SELECT * FROM orders o2 \
     WHERE o2.o_custkey = o1.o_custkey AND o2.o_orderkey > o1.o_orderkey + 3)",
    "SELECT count(*), sum(o1.o_orderkey) FROM orders o1 WHERE NOT EXISTS (SELECT * FROM orders \
     o2 WHERE o2.o_custkey = o1.o_custkey AND o2.o_orderkey < o1.o_orderkey AND NOT o2.o_late)",
    "SELECT count(*), sum(o1.o_orderkey) FROM orders o1 WHERE EXISTS (SELECT * FROM orders o2 \
     WHERE o2.o_custkey = o1.o_custkey AND o2.o_clerk = o1.o_clerk + 1)",
    // Two conditions that read both sides, which go through the pairs.
    "SELECT count(*), sum(o1.o_orderkey) FROM orders o1 WHERE NOT EXISTS (SELECT * FROM orders \
     o2 WHERE o2.o_custkey = o1.o_custkey AND o2.o_clerk <> o1.o_clerk AND o2.o_orderkey > \
     o1.o_orderkey)",
    // A key that is not a key of any parent, which has no siblings to find.
    "SELECT count(*), sum(c_custkey) FROM (SELECT c_custkey + 2990 AS c_custkey FROM customer) c \
     WHERE NOT EXISTS (SELECT * FROM orders o WHERE o.o_custkey = c.c_custkey AND o.o_clerk > \
     c.c_custkey % 5)",
];

fn answers_the_same(shuffled: bool) {
    let (database, path) = database(if shuffled { "shuffled" } else { "ordered" }, shuffled);
    for sql in QUERIES {
        let on = rows(&database, sql);
        database.execute("SET graph_sections = 'off'").expect("the layer has a switch");
        assert_eq!(rows(&database, sql), on, "{sql}");
        database.execute("SET graph_sections = 'on'").expect("and back");
    }
    // The other side is never scanned, which is the whole of the difference.
    let text = plan(&database, QUERIES[0]);
    let scan = text
        .lines()
        .find(|line| line.contains("Get ") && line.contains(" o2 "))
        .unwrap_or_else(|| panic!("no scan of o2 on the tree:\n{text}"));
    assert!(scan.contains("not measured"), "{text}");
    drop(database);
    std::fs::remove_file(&path).ok();
}

#[test]
fn a_child_stored_in_its_parents_order_walks_to_its_siblings_and_answers_the_same() {
    answers_the_same(false);
}

#[test]
fn a_child_stored_in_no_order_walks_to_its_siblings_and_answers_the_same() {
    answers_the_same(true);
}

//! Date tests carried across a relationship by the span its link measured.
//!
//! The answers are checked against the same queries with `graph_reduction` off, which leaves the
//! filters as the query wrote them, over a child whose dates sit 1 to 121 days after its parent's
//! and a second child date that is sometimes null.

use rudb::{Database, Value};

/// Twelve thousand orders with up to four lines each, some lines with no order and some with no
/// receipt date, the relationship declared and built and the graph layer on.
fn database() -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "rudb-spans-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let database = Database::open(path.to_str().expect("a UTF-8 temporary path")).expect("opens");
    database.execute("SET threads = 1").expect("sets the thread count");
    database
        .execute("CREATE TABLE orders (o_orderkey INTEGER, o_orderdate DATE)")
        .expect("creates");
    database
        .execute("CREATE TABLE lineitem (l_orderkey INTEGER, l_shipdate DATE, l_receiptdate DATE)")
        .expect("creates");
    // Every other key, so the key of an order is not its row and the join reads the link rather
    // than finding the order by its key, which leaves no build side to carry a test onto. Twelve
    // thousand of them, so the link stays under the 64 KiB a table's graph sections always get and
    // is kept, since a link the build measures and drops carries no span.
    database
        .execute(
            "INSERT INTO orders SELECT i * 2, DATE '1992-01-01' + ((i * 7) % 2400)::INTEGER FROM \
             range(0, 12000) AS r(i)",
        )
        .expect("loads");
    database
        .execute(
            "INSERT INTO lineitem SELECT CASE WHEN (o_orderkey + j) % 97 = 0 THEN -1 ELSE \
             o_orderkey END, o_orderdate + (1 + (o_orderkey * 37 + j * 11) % 121)::INTEGER, CASE \
             WHEN (o_orderkey + j) % 13 = 0 THEN NULL ELSE o_orderdate + (3 + (o_orderkey + j) % \
             150)::INTEGER END FROM orders, range(0, 4) AS r(j) WHERE j <= o_orderkey % 4",
        )
        .expect("loads");
    database
        .execute("SET graph_links = 'lineitem(l_orderkey) -> orders(o_orderkey)'")
        .expect("sets");
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
    let result = database.query(&format!("EXPLAIN {sql}")).expect("the explain ran");
    match result.value_at(0, 1) {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    }
}

/// Every filter written with `<` and `>`, so a `<=` or a `>=` in a plan is one the pass wrote.
const QUERIES: [&str; 6] = [
    "SELECT count(*), sum(o_orderkey) FROM lineitem, orders WHERE l_orderkey = o_orderkey AND \
     l_shipdate > DATE '1995-03-15' AND o_orderdate < DATE '1995-03-15'",
    "SELECT count(*), sum(l_orderkey) FROM orders JOIN lineitem ON l_orderkey = o_orderkey WHERE \
     o_orderdate > DATE '1994-01-01' AND o_orderdate < DATE '1994-02-01' AND l_receiptdate < DATE \
     '1994-03-01'",
    "SELECT count(*), sum(o_orderkey) FROM orders WHERE o_orderdate < DATE '1995-01-01' AND \
     EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_shipdate > DATE \
     '1995-01-20')",
    "SELECT count(*), sum(o_orderkey) FROM orders WHERE o_orderdate > DATE '1995-01-01' AND NOT \
     EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_shipdate < DATE \
     '1995-02-20')",
    "SELECT count(*), count(o_orderkey) FROM lineitem LEFT JOIN orders ON l_orderkey = o_orderkey \
     AND o_orderdate < DATE '1993-06-01' WHERE l_shipdate > DATE '1993-05-01'",
    "SELECT count(*), sum(o_orderkey) FROM lineitem, orders WHERE l_orderkey = o_orderkey AND \
     l_receiptdate > DATE '1996-01-01' AND l_receiptdate < DATE '1996-01-10'",
];

#[test]
fn a_date_test_carried_across_a_link_answers_the_same() {
    let (database, path) = database();
    let mut carried = 0;
    for sql in QUERIES {
        let on = rows(&database, sql);
        let text = plan(&database, sql);
        carried += usize::from(text.contains(" >= ") || text.contains(" <= "));
        database.execute("SET graph_reduction = 'off'").expect("the rule has a switch");
        assert_eq!(rows(&database, sql), on, "{sql}");
        let text = plan(&database, sql);
        assert!(!text.contains(" >= ") && !text.contains(" <= "), "{sql}\n{text}");
        database.execute("RESET graph_reduction").expect("and back");
    }
    assert!(carried > 0, "no query had a test carried across the link");
    drop(database);
    std::fs::remove_file(&path).ok();
}

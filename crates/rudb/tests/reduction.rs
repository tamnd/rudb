//! The exact reduction of spec/graph/05-execution.md section 5.4, end to end through SQL.
//!
//! A filter on the parent side of a join over a stored link turns into the exact set of child rows
//! that can match, and the child's scan reads only the parts holding one. These tests check the
//! answer against the same query with the reduction off, and check that the parts really were
//! skipped, since a reduction that changed nothing would pass the first check trivially.

use rudb::{Database, Value};

/// A file with thirty thousand customers and ten orders each, stored in customer order, with the
/// relationship declared and built and the graph layer on. Three hundred thousand orders is several
/// parts, which a skip needs, since a part of an integer column is about sixty five thousand rows.
fn database(name: &str) -> (Database, std::path::PathBuf) {
    let (database, path) = open(name);
    database.execute("CREATE TABLE customer (c_custkey INTEGER, c_name VARCHAR)").expect("creates");
    database
        .execute("CREATE TABLE orders (o_orderkey INTEGER, o_custkey INTEGER)")
        .expect("creates");
    database
        .execute("INSERT INTO customer SELECT i, 'c' || i FROM range(1, 30001) AS r(i)")
        .expect("loads");
    database
        .execute("INSERT INTO orders SELECT i, 1 + (i - 1) // 10 FROM range(1, 300001) AS r(i)")
        .expect("loads");
    database.execute("SET graph_links = 'orders(o_custkey) -> customer(c_custkey)'").expect("sets");
    database.execute("CHECKPOINT").expect("builds the link");
    database.execute("SET graph_sections = 'on'").expect("turns the layer on");
    (database, path)
}

/// An empty file at a fresh temporary path, running on one thread.
fn open(name: &str) -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "rudb-reduction-{name}-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let database = Database::open(path.to_str().expect("a UTF-8 temporary path")).expect("opens");
    database.execute("SET threads = 1").expect("sets the thread count");
    (database, path)
}

/// A filter that keeps the first two customers and the last one, so the range of the keys the join
/// holds covers the whole of `orders` and only an exact set can rule the parts in between out.
const QUERY: &str = "SELECT count(*), sum(o_orderkey), min(c_name), max(c_name) FROM orders JOIN \
                     customer ON o_custkey = c_custkey WHERE c_custkey % 30000 < 3";

/// The scan line of the child table in an `EXPLAIN ANALYZE`.
fn orders_scan(database: &Database, sql: &str) -> String {
    scan_of(database, "orders", sql)
}

/// The scan line of `table` in an `EXPLAIN ANALYZE`.
fn scan_of(database: &Database, table: &str, sql: &str) -> String {
    let result = database.query(&format!("EXPLAIN ANALYZE {sql}")).expect("the explain ran");
    let text = match result.value_at(0, 1) {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    };
    text.lines()
        .take_while(|line| !line.is_empty())
        .find(|line| line.contains("Get ") && line.contains(table))
        .unwrap_or_else(|| panic!("no scan of {table} on the tree:\n{text}"))
        .to_owned()
}

fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).expect("the query ran");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

#[test]
fn a_filtered_parent_skips_the_child_parts_it_has_no_rows_in_and_answers_the_same() {
    let (database, path) = database("skips");
    let reduced = rows(&database, QUERY);
    // Keys 1, 2 and 30000, ten orders each.
    assert_eq!(reduced[0][0], Value::BigInt(30), "the query should match something");
    let line = orders_scan(&database, QUERY);
    assert!(line.contains("parts skipped"), "the reduction should skip parts of orders: {line}");
    assert!(
        line.contains("link kept 30 of 300000 rows"),
        "the reduction says what it kept: {line}"
    );

    database.execute("SET graph_reduction = 'off'").expect("the rule has a switch");
    assert_eq!(rows(&database, QUERY), reduced, "the reduction changed an answer");
    // The keys the join holds still skip the parts between them, so what says the reduction is off
    // is that it reports nothing.
    let line = orders_scan(&database, QUERY);
    assert!(!line.contains("link kept"), "the reduction is under its switch: {line}");

    database.execute("SET graph_sections = 'off'").expect("the layer has a switch");
    database.execute("RESET graph_reduction").expect("resets");
    assert_eq!(rows(&database, QUERY), reduced, "the layer changed an answer");
    assert!(
        !orders_scan(&database, QUERY).contains("link kept"),
        "the reduction is under the layer's switch"
    );

    drop(database);
    std::fs::remove_file(&path).ok();
}

/// The shape of Q3: the parent's keys come up through a join of their own before the child is
/// joined to them, so the build side is not a scan of `customer` and is still `customer`'s keys.
#[test]
fn a_parent_key_that_comes_up_through_a_join_still_reduces_the_child() {
    let (database, path) = database("through");
    database.execute("CREATE TABLE picked (p_key INTEGER, p_note VARCHAR)").expect("creates");
    database.execute("INSERT INTO picked VALUES (1, 'a'), (2, 'b'), (30000, 'c')").expect("loads");
    let sql = "SELECT count(*), sum(o_orderkey), min(p_note) FROM orders JOIN (SELECT c_custkey, \
               p_note FROM customer JOIN picked ON c_name = 'c' || p_key) AS t ON o_custkey = \
               c_custkey";
    let reduced = rows(&database, sql);
    assert_eq!(reduced[0][0], Value::BigInt(30));
    let line = orders_scan(&database, sql);
    assert!(line.contains("link kept 30 of 300000 rows"), "the reduction should fire: {line}");

    database.execute("SET graph_sections = 'off'").expect("the layer has a switch");
    assert_eq!(rows(&database, sql), reduced, "the reduction changed an answer");
    drop(database);
    std::fs::remove_file(&path).ok();
}

/// A hundred segments with three thousand items each, stored in segment order, and a filter that
/// keeps the first forty. A part of `items` is about a thousand rows, which is one segment or two, so
/// from the counts alone a set of forty segments in a hundred leaves most parts without a match. The
/// push is tried, but the forty are the ones at the front, so it has kept every row by the time it
/// is a third of the way through `items` and stops there.
#[test]
fn a_reduction_that_removes_nothing_early_stops_and_says_so() {
    let (database, path) = open("stops");
    database.execute("CREATE TABLE segment (s_key INTEGER, s_name VARCHAR)").expect("creates");
    database.execute("CREATE TABLE items (i_key INTEGER, i_segment INTEGER)").expect("creates");
    database
        .execute("INSERT INTO segment SELECT i, 's' || i FROM range(1, 101) AS r(i)")
        .expect("loads");
    database
        .execute("INSERT INTO items SELECT i, 1 + (i - 1) // 3000 FROM range(1, 300001) AS r(i)")
        .expect("loads");
    database.execute("SET graph_links = 'items(i_segment) -> segment(s_key)'").expect("sets");
    database.execute("CHECKPOINT").expect("builds the link");
    database.execute("SET graph_sections = 'on'").expect("turns the layer on");
    let sql = "SELECT count(*), sum(i_key) FROM items JOIN segment ON i_segment = s_key WHERE \
               s_key <= 40";
    let stopped = rows(&database, sql);
    assert_eq!(stopped[0][0], Value::BigInt(120_000));
    let line = scan_of(&database, "items", sql);
    assert!(line.contains("link reduction stopped"), "the push should have stopped: {line}");
    assert!(!line.contains("link kept"), "{line}");

    database.execute("SET graph_sections = 'off'").expect("the layer has a switch");
    assert_eq!(rows(&database, sql), stopped, "stopping changed an answer");
    assert!(!scan_of(&database, "items", sql).contains("link"), "no reduction with the layer off");
    drop(database);
    std::fs::remove_file(&path).ok();
}

/// A filter that drops only the last customer leaves nearly every parent key in the build side, so
/// no stretch of `orders` could be skipped. Since #1929 the push is tried anyway, because it costs
/// only the parents it holds, and the scan stops it once a third of the rows removed nothing. Either
/// way nothing is kept off the link, and the keys the join holds answer the scan instead.
#[test]
fn a_side_that_holds_nearly_every_parent_does_not_push() {
    let (database, path) = database("dense");
    let sql = "SELECT count(*), sum(o_orderkey) FROM orders JOIN customer ON o_custkey = c_custkey \
               WHERE c_custkey < 30000";
    let reduced = rows(&database, sql);
    assert_eq!(reduced[0][0], Value::BigInt(299_990));
    let line = orders_scan(&database, sql);
    assert!(!line.contains("link kept"), "{line}");

    database.execute("SET graph_sections = 'off'").expect("the layer has a switch");
    assert_eq!(rows(&database, sql), reduced, "the layer changed an answer");
    drop(database);
    std::fs::remove_file(&path).ok();
}

/// A child with no link to `customer`, because no relationship names it, still gets an exact test
/// of its key against the keys the join holds, since `customer` has a key map for being the parent
/// of `orders`. It is exact, so the answer is the same and the scan hands the join only the rows
/// that match.
#[test]
fn a_child_with_no_link_is_reduced_through_the_parents_key_map() {
    let (database, path) = database("keymap");
    database.execute("CREATE TABLE visits (v_custkey INTEGER, v_day INTEGER)").expect("creates");
    database
        .execute("INSERT INTO visits SELECT 1 + i % 30000, i FROM range(0, 200000) AS r(i)")
        .expect("loads");
    database.execute("INSERT INTO visits VALUES (NULL, 0), (40000, 0)").expect("loads");
    database.execute("CHECKPOINT").expect("commits");
    let sql = "SELECT count(*), sum(v_day), max(c_name) FROM visits JOIN customer ON v_custkey = \
               c_custkey WHERE c_custkey % 1000 = 7";
    let reduced = rows(&database, sql);
    assert_eq!(reduced[0][0], Value::BigInt(200));
    let result = database.query(&format!("EXPLAIN ANALYZE {sql}")).expect("the explain ran");
    let Value::Varchar(text) = result.value_at(0, 1) else { panic!("no plan text") };
    let line = text
        .lines()
        .find(|line| line.contains("Get ") && line.contains("visits"))
        .unwrap_or_else(|| panic!("no scan of visits:\n{text}"));
    assert!(line.contains("key map kept 30 of 30000 parent keys"), "{line}");
    assert!(line.contains("[200 rows"), "the scan should hand on only the matches: {line}");

    database.execute("SET graph_sections = 'off'").expect("the layer has a switch");
    assert_eq!(rows(&database, sql), reduced, "the key map test changed an answer");
    drop(database);
    std::fs::remove_file(&path).ok();
}

/// Every kind of join the runtime filter is armed for, and a few it is not, against the same
/// queries with the layer off.
#[test]
fn the_reduction_changes_no_answer() {
    let (database, path) = database("answers");
    let queries = [
        QUERY,
        "SELECT o_orderkey, c_name FROM orders JOIN customer ON o_custkey = c_custkey WHERE \
         c_name LIKE 'c2999%' ORDER BY o_orderkey",
        "SELECT count(*) FROM orders WHERE o_custkey IN (SELECT c_custkey FROM customer WHERE \
         c_custkey % 997 = 1)",
        "SELECT count(*) FROM orders WHERE o_custkey NOT IN (SELECT c_custkey FROM customer WHERE \
         c_custkey % 997 = 1)",
        "SELECT count(*), count(c_name) FROM orders LEFT JOIN customer ON o_custkey = c_custkey \
         AND c_custkey % 7 = 0",
        "SELECT count(*) FROM orders JOIN customer ON o_custkey = c_custkey WHERE c_custkey > 50000",
        "SELECT count(*) FROM orders JOIN customer ON o_custkey = c_custkey",
    ];
    for sql in queries {
        let on = rows(&database, sql);
        database.execute("SET graph_sections = 'off'").expect("the layer has a switch");
        assert_eq!(rows(&database, sql), on, "{sql}");
        database.execute("SET graph_sections = 'on'").expect("and back");
    }
    drop(database);
    std::fs::remove_file(&path).ok();
}

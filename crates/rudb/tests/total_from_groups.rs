//! A total read off the groups of the same query answers what running the query again does.
//!
//! The optimizer holds a grouped aggregate and adds a total up over its groups when the total is
//! over the same rows, which is TPC-H q11's shape. Turning the rewrite off runs the total's query
//! again over the tables, so asking both ways checks the answer, nulls and empty inputs included.

use rudb::Database;
use rudb_common::Value;

const TABLES: [&str; 6] = [
    "CREATE TABLE nation(n_nationkey INTEGER, n_name VARCHAR)",
    "INSERT INTO nation SELECT i, CASE WHEN i = 7 THEN 'GERMANY' ELSE 'N' || i::VARCHAR END FROM \
     range(25) r(i)",
    "CREATE TABLE supplier(s_suppkey BIGINT, s_nationkey INTEGER)",
    "INSERT INTO supplier SELECT i, (i * 7 % 25)::INTEGER FROM range(1000) r(i)",
    "CREATE TABLE partsupp(ps_partkey BIGINT, ps_suppkey BIGINT, ps_availqty BIGINT, \
     ps_supplycost DECIMAL(15,2))",
    "INSERT INTO partsupp SELECT i % 5000, i * 13 % 1000, i % 9999, CASE WHEN i % 37 = 0 THEN NULL \
     ELSE ((i * 31 % 100000) / 100.0)::DECIMAL(15,2) END FROM range(80000) r(i)",
];

/// The rows `query` gives with the rewrite on, after checking they are the rows it gives with the
/// rewrite off, on one thread and on four.
fn agree(db: &Database, query: &str) -> Vec<Vec<Value>> {
    let mut answers = Vec::new();
    for off in ["", "total_from_groups"] {
        db.execute(&format!("SET disabled_optimizers = '{off}'")).expect("sets the passes");
        for threads in [1, 4] {
            db.execute(&format!("SET threads = {threads}")).expect("sets the threads");
            let rows: Vec<Vec<Value>> =
                db.query(query).expect("the query answers").rows().collect();
            answers.push(rows);
        }
    }
    db.execute("SET disabled_optimizers = ''").expect("sets the passes");
    for other in &answers[1..] {
        assert_eq!(other, &answers[0], "{query}");
    }
    answers.swap_remove(0)
}

/// Whether the plan for `query` reads a total off held groups.
fn held(db: &Database, query: &str) -> bool {
    let plan = db.query(&format!("EXPLAIN {query}")).expect("the plan is described");
    format!("{:?}", plan.rows().collect::<Vec<_>>()).contains("CteScan groups")
}

fn database() -> Database {
    let db = Database::new();
    for sql in TABLES {
        db.execute(sql).expect("the tables are made");
    }
    db
}

fn q11(nation: &str, having: &str) -> String {
    format!(
        "SELECT ps_partkey, sum(ps_supplycost * ps_availqty) AS value FROM partsupp, supplier, \
         nation WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = '{nation}' \
         GROUP BY ps_partkey HAVING {having} ORDER BY value DESC, ps_partkey"
    )
}

#[test]
fn the_parts_worth_a_share_of_the_total_are_the_parts_the_query_run_twice_finds() {
    let db = database();
    let query = q11(
        "GERMANY",
        "sum(ps_supplycost * ps_availqty) > (SELECT sum(ps_supplycost * ps_availqty) * 0.001 FROM \
         partsupp, supplier, nation WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND \
         n_name = 'GERMANY')",
    );
    assert!(held(&db, &query));
    let rows = agree(&db, &query);
    assert!(rows.len() > 50, "only {} parts", rows.len());
}

#[test]
fn no_rows_is_a_null_total_and_the_extremes_are_the_extremes_of_the_groups() {
    let db = database();
    let query = q11(
        "NOWHERE",
        "sum(ps_supplycost * ps_availqty) > (SELECT sum(ps_supplycost * ps_availqty) FROM \
         partsupp, supplier, nation WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND \
         n_name = 'NOWHERE')",
    );
    assert!(held(&db, &query));
    assert!(agree(&db, &query).is_empty());
    let query = "SELECT ps_partkey, min(ps_supplycost), max(ps_supplycost) FROM partsupp WHERE \
                 ps_availqty > 100 GROUP BY ps_partkey HAVING min(ps_supplycost) < (SELECT \
                 min(ps_supplycost) + 1 FROM partsupp WHERE ps_availqty > 100) OR \
                 max(ps_supplycost) = (SELECT max(ps_supplycost) FROM partsupp WHERE ps_availqty > \
                 100) ORDER BY ps_partkey";
    assert!(held(&db, query));
    assert!(!agree(&db, query).is_empty());
}

#[test]
fn a_total_over_other_rows_is_run_on_its_own() {
    let db = database();
    let query = q11(
        "GERMANY",
        "sum(ps_supplycost * ps_availqty) > (SELECT sum(ps_supplycost * ps_availqty) * 0.001 FROM \
         partsupp, supplier, nation WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND \
         n_name = 'N3')",
    );
    assert!(!held(&db, &query));
    agree(&db, &query);
}

//! An average read off a sum of the same column gives the bits `avg` gave on its own.
//!
//! The optimizer answers `avg(x)` from `sum(x)` and a count when the same aggregate already sums
//! `x`. These run the same queries with the pass turned off and compare, over decimals and integers
//! with nulls in them, groups where every value is null, and no rows at all.

use rudb::Database;

fn database() -> Database {
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE t AS SELECT i % 3000 AS g, CASE WHEN i % 13 = 0 THEN NULL ELSE ((i * \
             7919) % 100000 * 0.01)::DECIMAL(15,2) END AS price, CASE WHEN i % 3000 = 5 THEN NULL \
             ELSE (i * 31) % 997 END AS n FROM range(300000) r(i)",
        )
        .expect("the table");
    database
}

/// The rows of `sql`, sorted, with the pass on and with it off.
fn both(database: &Database, sql: &str) -> (Vec<String>, Vec<String>) {
    let rows = |database: &Database| {
        let mut rows: Vec<String> = database
            .query(sql)
            .expect("the query ran")
            .rows()
            .map(|row| format!("{row:?}"))
            .collect();
        rows.sort();
        rows
    };
    let on = rows(database);
    database.execute("SET disabled_optimizers = 'common_aggregate'").expect("turns it off");
    let off = rows(database);
    database.execute("RESET disabled_optimizers").expect("turns it back on");
    (on, off)
}

#[test]
fn a_grouped_average_beside_a_sum_is_the_average_it_was() {
    let database = database();
    let sql = "SELECT g, sum(price), avg(price), avg(n), sum(n), count(*), avg(price) FILTER \
               (WHERE n > 10) FROM t GROUP BY g";
    let plan = database.plan(sql).expect("the plan");
    assert!(plan.contains("__rudb_mean"), "the averages were not shared:\n{plan}");
    let (on, off) = both(&database, sql);
    assert_eq!(on.len(), 3000);
    assert!(on.iter().any(|row| row.contains("Null")), "no group whose average is null");
    assert_eq!(on, off);
}

#[test]
fn an_average_over_no_rows_or_no_values_is_null() {
    let database = database();
    for sql in [
        "SELECT sum(price), avg(price), sum(n), avg(n) FROM t",
        "SELECT sum(price), avg(price) FROM t WHERE g < 0",
        "SELECT sum(n), avg(n) FROM t WHERE g = 5",
        "SELECT g % 7 AS k, sum(n) + 1, avg(n) * 2 FROM t WHERE g < 20 GROUP BY k HAVING avg(n) > 0",
    ] {
        let (on, off) = both(&database, sql);
        assert_eq!(on, off, "{sql}");
    }
}

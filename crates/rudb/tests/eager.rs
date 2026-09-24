//! Sums moved below a join give the answer they gave above it.
//!
//! The optimizer moves a `sum`, `min` or `max` below an inner join to a scan when the join only
//! brings in strings to group by. These run the same queries with the pass turned off and compare,
//! over data with nulls in the summed column, keys with no partner, and a key that matches twice.

use rudb::Database;

fn database() -> Database {
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE c AS SELECT i AS k, 'name ' || (i % 97)::VARCHAR AS name, i % 5 AS n FROM \
             range(2000) r(i)",
        )
        .expect("the customers");
    database.execute("INSERT INTO c VALUES (7, 'second seven', 1)").expect("a key twice");
    database
        .execute(
            "CREATE TABLE o AS SELECT i AS id, (i * 7) % 2500 AS c, CASE WHEN i % 11 = 0 THEN NULL \
             ELSE ((i % 1000) * 3)::DECIMAL(15,2) END AS price FROM range(50000) r(i)",
        )
        .expect("the orders");
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
    database.execute("SET disabled_optimizers = 'eager_aggregation'").expect("turns it off");
    let off = rows(database);
    database.execute("RESET disabled_optimizers").expect("turns it back on");
    (on, off)
}

#[test]
fn a_sum_below_the_join_to_customers_is_the_sum_above_it() {
    let database = database();
    let sql = "SELECT c.k, c.name, sum(o.price), min(o.price), max(o.price) FROM c, o WHERE c.k = \
               o.c AND o.id % 3 = 1 GROUP BY c.k, c.name";
    let plan = database.plan(sql).expect("the plan");
    assert!(plan.matches("Aggregate").count() == 2, "the sum did not move:\n{plan}");
    let (on, off) = both(&database, sql);
    assert!(on.len() > 1000, "only {} groups", on.len());
    assert_eq!(on, off);
}

#[test]
fn a_group_read_from_below_the_join_is_kept_below_it() {
    let database = database();
    let sql = "SELECT c.name, o.id % 4 AS part, sum(o.price) FROM c JOIN o ON c.k = o.c GROUP BY \
               c.name, o.id % 4";
    let (on, off) = both(&database, sql);
    assert_eq!(on, off);
}

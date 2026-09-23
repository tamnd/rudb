//! Memory reservation from statistics, `spec/stats/05-every-query.md` sections 5.1 and 5.7, end to
//! end through SQL.
//!
//! The presize pass gives a grouped aggregate room for as many groups as the statistics allow before
//! the first row arrives. The number is a ceiling, and a ceiling can be far above what the rows make.
//! These tests check that room the budget cannot hold is reserved first and declined, so the query
//! runs with a table that grows, where before it was charged for the room on its first chunk and
//! failed.

use rudb::{Database, Value};

/// A file with two million rows whose key holds three hundred and ninety thousand values, each
/// counted.
///
/// The query filters on an expression the planner cannot see into, so it takes the filter to keep a
/// fifth of the rows, which is four hundred thousand and above the key's count. The presize pass
/// takes the count as the ceiling and asks for room for that many groups. The filter keeps eighty
/// thousand rows in fifteen thousand six hundred groups, which is enough for the aggregate to
/// partition on four threads, and each of its sixty four shared partitions takes its share of the
/// ceiling: sixteen thousand buckets, eight megabytes over all of them, for a few hundred groups
/// each. The second key column is one value, so the key is not a single integer the `dense` pass
/// would turn into an array.
fn database(name: &str) -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "rudb-reservation-{name}-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let database = Database::open(path.to_str().expect("a UTF-8 temporary path")).expect("opens");
    database.execute("SET threads = 4").expect("sets the thread count");
    database.execute("CREATE TABLE t (k INTEGER, c INTEGER, v INTEGER)").expect("creates");
    database
        .execute("INSERT INTO t SELECT i % 390000, 7, i FROM range(0, 2000000) AS r(i)")
        .expect("loads");
    database.execute("CHECKPOINT").expect("writes the file");
    (database, path)
}

const QUERY: &str =
    "SELECT k, c, count(*), sum(v) FROM t WHERE v % 25 = 0 GROUP BY k, c ORDER BY k";

fn rows(database: &Database, sql: &str) -> rudb::Result<Vec<Vec<Value>>> {
    let result = database.query(sql)?;
    Ok((0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect())
}

#[test]
fn room_the_budget_cannot_hold_is_declined_and_the_table_grows_instead() {
    let (database, path) = database("declined");
    let roomy = rows(&database, QUERY).expect("runs with no limit");
    assert_eq!(roomy.len(), 15_600);

    let result = database.query(&format!("EXPLAIN {QUERY}")).expect("explains");
    let Value::Varchar(plan) = result.value_at(0, 1) else { panic!("the plan is text") };
    assert!(
        plan.contains("room for 390,000 groups"),
        "the test needs the presize to overshoot, and the plan says it did not:\n{plan}"
    );

    // The partitions' room is eight megabytes and the groups need a fraction of it.
    database.execute("SET memory_limit = '6MB'").expect("sets the limit");
    assert_eq!(rows(&database, QUERY).expect("runs inside the limit"), roomy);

    // The same query with the room charged rather than reserved, which is what makes the case above
    // mean something: without the reservation the charge comes on the first chunk and fails.
    database.execute("SET stats_memory_reservation = 'off'").expect("the rule has a switch");
    let failed = rows(&database, QUERY).expect_err("the room does not fit the limit");
    assert!(failed.to_string().contains("could not allocate"), "{failed}");

    drop(database);
    std::fs::remove_file(&path).ok();
}

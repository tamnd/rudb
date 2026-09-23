//! Runtime filters that pass through the joins between the join that built them and the scan.
//!
//! A star query joins a fact table to one dimension and then the result to another. The outer join
//! drives from the inner one, so before this the filter it built had nowhere to go and only the
//! innermost dimension narrowed the fact scan. An inner join keeps no row its driving side did not
//! have, so the outer filter is just as true of the fact rows under it, and these tests check that
//! the scan now drops rows for both dimensions and that the answer stays the same.

use rudb::{Database, Value};

/// A fact table of two hundred thousand rows with two keys, and two dimensions to join them to.
fn database() -> Database {
    let database = Database::new();
    database.execute("SET threads = 1").expect("sets the thread count");
    database
        .execute(
            "CREATE TABLE fact AS SELECT i AS id, i % 1000 AS a, i % 777 AS b FROM range(200000) r(i)",
        )
        .expect("the fact table");
    database
        .execute("CREATE TABLE da AS SELECT i AS k, i % 10 AS g FROM range(1000) r(i)")
        .expect("the first dimension");
    database
        .execute("CREATE TABLE db AS SELECT i AS k, i % 7 AS h FROM range(777) r(i)")
        .expect("the second dimension");
    database
}

/// A tenth of the first dimension and a seventh of the second, so the fact rows that match both
/// are about one in seventy.
const QUERY: &str = "SELECT count(*), sum(fact.id) FROM fact JOIN da ON fact.a = da.k JOIN db ON \
                     fact.b = db.k WHERE da.g = 3 AND db.h = 2";

/// The rows the scan of `fact` handed up, read off its line in an `EXPLAIN ANALYZE`.
fn fact_rows(database: &Database, sql: &str) -> u64 {
    let result = database.query(&format!("EXPLAIN ANALYZE {sql}")).expect("the explain ran");
    let text = match result.value_at(0, 1) {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    };
    let line = text
        .lines()
        .find(|line| line.contains("Get ") && line.contains("fact"))
        .unwrap_or_else(|| panic!("no scan of fact on the tree:\n{text}"));
    let measured = line.rsplit_once("[").map_or("", |(_, last)| last);
    measured
        .split(' ')
        .next()
        .and_then(|rows| rows.parse().ok())
        .unwrap_or_else(|| panic!("no row count on the scan line: {line}"))
}

/// The scan drops rows for both dimensions, and writing the joins the other way up, which moves the
/// filters, gives the same answer.
#[test]
fn a_filter_from_the_outer_join_reaches_the_scan_under_the_inner_one() {
    let database = database();
    let rows = fact_rows(&database, QUERY);
    // The first dimension alone keeps twenty thousand. Both keep two thousand eight hundred and
    // fifty seven, and the scan is allowed the few chunks it reads before the filters are ready.
    assert!(rows < 10_000, "the fact scan kept {rows} rows, so the outer filter did not reach it");

    let answer = database.query(QUERY).expect("the query ran");
    assert_eq!(answer.value_at(0, 0), Value::BigInt(2857));
    let swapped = "SELECT count(*), sum(fact.id) FROM fact JOIN db ON fact.b = db.k JOIN da ON \
                   fact.a = da.k WHERE da.g = 3 AND db.h = 2";
    let other = database.query(swapped).expect("the query ran");
    assert_eq!(other.value_at(0, 1), answer.value_at(0, 1));
}

/// A left join keeps every row of its driving side whether or not it matches, so a filter from
/// above it says nothing about the rows below and must not be used there.
#[test]
fn a_filter_does_not_pass_through_a_left_join() {
    let database = database();
    let sql = "SELECT count(*), count(da.k) FROM fact LEFT JOIN da ON fact.a = da.k AND da.g = 3 \
               JOIN db ON fact.b = db.k WHERE db.h = 2";
    let answer = database.query(sql).expect("the query ran");
    let expected = database
        .query("SELECT count(*) FROM fact WHERE b % 7 = 2")
        .expect("the query ran")
        .value_at(0, 0);
    assert_eq!(answer.value_at(0, 0), expected, "every fact row the second dimension keeps");
}

/// A join above an inner join on the same fact column only ever sees the keys the inner join
/// kept, so it leaves the other rows out of its own table. Each kind that streams through a
/// lookup answers here what the same question without the lower join answers.
#[test]
fn a_join_above_another_on_the_same_column_answers_the_same_with_a_narrowed_table() {
    let database = database();
    database
        .execute("CREATE TABLE dc AS SELECT i AS k, i * 3 AS v FROM range(0, 1000, 2) r(i)")
        .expect("a third dimension over the even keys");
    let value =
        |sql: &str, column: usize| database.query(sql).expect("the query ran").value_at(0, column);
    let kept = "fact.a % 10 = 3";

    let inner = "SELECT count(*), sum(dc.v) FROM fact JOIN da ON fact.a = da.k JOIN dc ON fact.a = \
                 dc.k WHERE da.g = 4";
    let expected = "SELECT count(*), sum(a * 3) FROM fact WHERE a % 10 = 4";
    assert_eq!(value(inner, 0), value(expected, 0));
    assert_eq!(value(inner, 1), value(expected, 1));

    let left = "SELECT count(*), count(dc.k) FROM fact JOIN da ON fact.a = da.k LEFT JOIN dc ON \
                fact.a = dc.k WHERE da.g = 3";
    let all = format!("SELECT count(*) FROM fact WHERE {kept}");
    assert_eq!(value(left, 0), value(&all, 0));
    assert_eq!(value(left, 1), Value::BigInt(0), "an odd key has no partner among the even ones");

    let semi = "SELECT count(*) FROM fact JOIN da ON fact.a = da.k WHERE da.g = 3 AND fact.a IN \
                (SELECT k + 1 FROM dc)";
    assert_eq!(value(semi, 0), value(&all, 0), "every odd key is one more than an even one");
    let anti = "SELECT count(*) FROM fact JOIN da ON fact.a = da.k WHERE da.g = 3 AND fact.a NOT \
                IN (SELECT k FROM dc)";
    assert_eq!(value(anti, 0), value(&all, 0));
}

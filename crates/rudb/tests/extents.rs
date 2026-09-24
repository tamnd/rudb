//! A semi or an anti join the plan turned around, whose leftover condition compares one column on
//! each side, answered from the smallest and the largest driving value of each key.
//!
//! `crates/rudb-exec/src/extents.rs` is the argument for why two numbers per key are enough. These
//! tests hold it to the answer worked out here in Rust, pair by pair, over keys that match nothing,
//! keys whose values are all the same, and nulls in the key and in the compared column on both
//! sides, for every comparison it takes and with the columns written either way round.

use rudb::Database;
use rudb_common::Value;

/// How many rows the subquery side has, enough that the plan gathers the other one.
const INNER: i64 = 6000;

/// How many rows the outer side has.
const OUTER: i64 = 90;

/// The subquery side's key, null now and then.
fn inner_key(i: i64) -> Option<i64> {
    (i % 101 != 0).then_some(i % 97)
}

/// The subquery side's compared column. Keys below ten hold one value, so `<>` finds nothing there.
fn inner_value(i: i64) -> Option<i64> {
    if i % 11 == 0 {
        return None;
    }
    Some(if i % 97 < 10 { 5 } else { (i * 7) % 13 })
}

/// The outer side's key, some of them past every key the subquery side holds.
fn outer_key(j: i64) -> Option<i64> {
    (j % 17 != 0).then_some(j % 110)
}

/// The outer side's compared column, some of it outside the range the subquery side holds.
fn outer_value(j: i64) -> Option<i64> {
    (j % 9 != 0).then_some(j % 15)
}

fn tables() -> Database {
    let database = Database::new();
    database
        .execute(&format!(
            "CREATE TABLE t AS SELECT \
             CASE WHEN i % 101 = 0 THEN NULL ELSE i % 97 END AS k, \
             CASE WHEN i % 11 = 0 THEN NULL WHEN i % 97 < 10 THEN 5 ELSE (i * 7) % 13 END AS c \
             FROM range({INNER}) r(i)"
        ))
        .expect("the subquery side");
    database
        .execute(&format!(
            "CREATE TABLE o AS SELECT i AS id, \
             CASE WHEN i % 17 = 0 THEN NULL ELSE i % 110 END AS k, \
             CASE WHEN i % 9 = 0 THEN NULL ELSE i % 15 END AS c \
             FROM range({OUTER}) r(i)"
        ))
        .expect("the outer side");
    database
}

/// The ids of the outer rows the query keeps, in order.
fn ids(database: &Database, sql: &str) -> Vec<i64> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| match row.first() {
            Some(Value::BigInt(id)) => *id,
            other => panic!("{sql} gave {other:?}"),
        })
        .collect()
}

/// The ids `EXISTS` keeps, pair by pair, when `holds(o.c, t.c)` is the leftover condition.
fn expected(holds: impl Fn(i64, i64) -> bool, anti: bool) -> Vec<i64> {
    (0..OUTER)
        .filter(|&j| {
            let found = (0..INNER).any(|i| {
                let same = matches!((outer_key(j), inner_key(i)), (Some(a), Some(b)) if a == b);
                let meets =
                    matches!((outer_value(j), inner_value(i)), (Some(a), Some(b)) if holds(a, b));
                same && meets
            });
            found != anti
        })
        .collect()
}

/// A comparison as SQL spells it, and what it says about an outer value and a subquery value.
type Case = (&'static str, fn(i64, i64) -> bool);

/// Every comparison, both ways round, both kinds.
#[test]
fn every_comparison_keeps_the_rows_the_pairs_say() {
    let database = tables();
    let cases: [Case; 5] = [
        ("<>", |o, t| o != t),
        ("<", |o, t| o < t),
        ("<=", |o, t| o <= t),
        (">", |o, t| o > t),
        (">=", |o, t| o >= t),
    ];
    for (op, holds) in cases {
        for anti in [false, true] {
            let not = if anti { "NOT " } else { "" };
            let want = expected(holds, anti);
            let sql = format!(
                "SELECT id FROM o WHERE {not}EXISTS (SELECT * FROM t WHERE t.k = o.k AND o.c {op} t.c) ORDER BY id"
            );
            let plan = database.plan(&sql).expect("plans");
            assert!(plan.contains("build=left"), "the join was not turned around\n{plan}");
            assert_eq!(ids(&database, &sql), want, "{sql}");
            // The same condition written the other way round.
            let mirrored = match op {
                "<" => ">",
                "<=" => ">=",
                ">" => "<",
                ">=" => "<=",
                same => same,
            };
            let sql = format!(
                "SELECT id FROM o WHERE {not}EXISTS (SELECT * FROM t WHERE t.k = o.k AND t.c {mirrored} o.c) ORDER BY id"
            );
            assert_eq!(ids(&database, &sql), want, "{sql}");
        }
    }
}

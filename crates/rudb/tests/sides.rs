//! Which side of a join the executor gathers, from the SQL down to the rows that come back.
//!
//! `rudb_opt`'s `sides` pass writes one field on each join, `crates/rudb-exec/src/build.rs` reads
//! it, and when it says the left input the executor runs the two inputs the other way round under
//! the mirror of the join kind. That is a rewrite that cannot change an answer and can very easily
//! change one, because a swapped join produces its columns in the other order and everything above
//! it was built against the plan's order.
//!
//! So every test here runs the same query twice, once with the pass on and once with it off by the
//! name DuckDB gives it, and insists the two agree. A pass that is fast and wrong is worse than no
//! pass at all, and the queries below are the ones where wrong would show: outer joins, whose
//! padding lands on a side, and joins under a projection that names columns from both.

use rudb::Database;
use rudb_common::Value;

/// A big table and a small one, in that order, so the plan's left input is the larger.
///
/// The sizes are far enough apart that the estimate has an opinion whatever the selectivity
/// constants do to them, and small enough that a nested loop over the pair finishes.
fn tables() -> Database {
    let database = Database::new();
    database
        .execute("CREATE TABLE big AS SELECT i AS k, i * 2 AS v FROM range(2000) r(i)")
        .expect("the big table");
    database
        .execute("CREATE TABLE small AS SELECT i AS k, i + 100 AS w FROM range(5) r(i)")
        .expect("the small table");
    database
}

/// The same two tables with the pass turned off.
fn without_the_pass() -> Database {
    let database = tables();
    database
        .execute("SET disabled_optimizers = 'build_side_probe_side'")
        .expect("the pass answers to that name");
    database
}

/// The rows of a query, as values, sorted so two orderings of the same answer compare equal.
///
/// Sorted because nothing here has an `ORDER BY` and a join is entitled to produce its rows in
/// whatever order its loops run in. Which order that is does change with the build side, and it is
/// the one thing about the swap that is allowed to.
fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let mut rows: Vec<Vec<Value>> = result.rows().collect();
    rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    rows
}

/// Runs the query against both databases and returns the answer they agreed on.
fn agreed(sql: &str) -> Vec<Vec<Value>> {
    let with = rows(&tables(), sql);
    let without = rows(&without_the_pass(), sql);
    assert_eq!(with, without, "the build side changed the answer to {sql}");
    with
}

#[test]
fn the_larger_input_is_the_one_the_plan_says_to_gather() {
    let database = tables();
    let text = database.plan("SELECT * FROM big JOIN small ON big.k = small.k").expect("plans");
    assert!(text.contains("build=left"), "the pass did not fire\n{text}");
}

/// The other way round, which is the side the binder already emitted, so the pass writes nothing
/// and the printer leaves the field out.
#[test]
fn the_smaller_input_on_the_left_leaves_the_plan_as_the_binder_wrote_it() {
    let database = tables();
    let text = database.plan("SELECT * FROM small JOIN big ON big.k = small.k").expect("plans");
    assert!(!text.contains("build="), "the default side was written down\n{text}");
}

#[test]
fn a_swapped_inner_join_gives_the_rows_the_unswapped_one_gives() {
    let answer = agreed("SELECT big.k, big.v, small.w FROM big JOIN small ON big.k = small.k");
    assert_eq!(answer.len(), 5, "one row per row of the small side");
    assert_eq!(answer[0], [Value::BigInt(0), Value::BigInt(0), Value::BigInt(100)]);
}

/// A `LEFT` join run with its inputs swapped is a `RIGHT` join, and the padding has to stay on the
/// plan's right side. This is the query where getting the mirror wrong gives 5 rows instead of
/// 2000.
#[test]
fn a_swapped_left_join_still_pads_the_side_the_query_named() {
    let answer = agreed("SELECT big.k, small.w FROM big LEFT JOIN small ON big.k = small.k");
    assert_eq!(answer.len(), 2000, "every row of the left side, matched or not");
    let padded = answer.iter().filter(|row| row[1] == Value::Null).count();
    assert_eq!(padded, 1995, "every big row with no small row to match");
}

#[test]
fn a_swapped_right_join_still_keeps_the_side_the_query_named() {
    let answer = agreed("SELECT big.k, small.w FROM big RIGHT JOIN small ON big.k = small.k");
    assert_eq!(answer.len(), 5, "every row of the right side, matched or not");
    assert!(answer.iter().all(|row| row[1] != Value::Null), "every small row matched a big one");
}

#[test]
fn a_swapped_full_join_keeps_both_sides() {
    let answer = agreed("SELECT big.k, small.w FROM big FULL JOIN small ON big.k = small.k");
    assert_eq!(answer.len(), 2000, "the five matches plus the rest of the big side");
}

/// The kinds whose left input is the subject rather than a side. The pass refuses to touch them and
/// this is the end to end statement of that, because a semi join with its inputs swapped is not a
/// semi join of anything.
#[test]
fn a_semi_join_and_an_anti_join_are_left_the_way_the_binder_wrote_them() {
    let database = tables();
    for sql in [
        "SELECT k FROM big WHERE k IN (SELECT k FROM small)",
        "SELECT k FROM big WHERE k NOT IN (SELECT k FROM small)",
        "SELECT k FROM big WHERE EXISTS (SELECT 1 FROM small WHERE small.k = big.k)",
    ] {
        let text = database.plan(sql).expect("plans");
        assert!(!text.contains("build="), "{sql} had a side chosen for it\n{text}");
        assert_eq!(rows(&database, sql), rows(&without_the_pass(), sql), "{sql}");
    }
}

/// Three tables, so that one join's output is another join's input and a swap at the bottom has to
/// leave the columns where the join above it expects them.
#[test]
fn a_join_over_a_swapped_join_reads_the_columns_the_plan_gave_it() {
    let database = tables();
    database
        .execute("CREATE TABLE tiny AS SELECT i AS k, i + 1000 AS z FROM range(3) r(i)")
        .expect("the tiny table");
    let sql = "SELECT big.k, small.w, tiny.z FROM big \
               JOIN small ON big.k = small.k JOIN tiny ON big.k = tiny.k";
    let with = rows(&database, sql);

    let plain = without_the_pass();
    plain
        .execute("CREATE TABLE tiny AS SELECT i AS k, i + 1000 AS z FROM range(3) r(i)")
        .expect("the tiny table");
    assert_eq!(with, rows(&plain, sql), "the build side changed the answer to {sql}");
    assert_eq!(with.len(), 3, "one row per row of the smallest side");
    assert_eq!(with[0], [Value::BigInt(0), Value::BigInt(100), Value::BigInt(1000)]);
}

/// The pass reads the plan after filter pushdown, so what it compares is what each side produces
/// rather than what each table holds. A predicate that leaves the big side with nothing in it is
/// the case where those two differ, and the answer has to be the same either way.
#[test]
fn a_filter_that_empties_the_larger_side_does_not_change_the_answer() {
    let answer =
        agreed("SELECT big.k, small.w FROM big JOIN small ON big.k = small.k WHERE big.k > 1000");
    assert!(answer.is_empty(), "no big row over a thousand has a small row to match");
}

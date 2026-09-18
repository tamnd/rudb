//! Correlated subqueries over a join, which is where the domain is built, from the answers.
//!
//! A correlated subquery is answered by evaluating it once per distinct value its correlated columns
//! take rather than once per outer row, and that set of distinct values is the domain. The rules
//! that build one run before anything has turned the `WHERE` into joins, so the outer side they see
//! is the whole `FROM` list as a cross product, and building the domain over all of it is building
//! it over the product of every table in the query to find the distinct values of a column of one of
//! them. The domain is therefore built from the branch the correlated columns come from.
//!
//! That branch holds every value the product does and usually more, which is allowed, and these are
//! the cases where more could turn into a wrong answer rather than a slower one: a subquery
//! correlated on a table the join then filters rows of, a second subquery whose outer side is the
//! first one's join, an outer row whose correlated column is NULL, and an outer column that only the
//! query above the join reads.
//!
//! The last of those is the one with teeth. The domain and the query above both read the same table
//! and read different columns of it, so the plan is a graph rather than a tree, and column pruning
//! narrows a table to what everything above it reads. `a_second_subquery_over_the_join_the_first_one_made`
//! is the shape where the domain's way to the table is the shorter one, which is TPC-H q21's, and
//! pruning to what the domain alone reads takes the other columns away from the query above.
//!
//! Every answer here was read off DuckDB v1.5.5 first.

use rudb::Database;
use rudb_common::Value;

/// Four outer tables and one inner one, with the rows already in them.
///
/// `whole` is what the subqueries correlate on. `part` joins to some of its rows and not others, so
/// a domain built from `whole` alone holds values no outer row has. `lo` and `hi` are read by a
/// predicate and by nothing else, and `tag` by the answer and by the subqueries, which is what makes
/// the columns of `whole` that the domain reads different from the columns above the join read.
fn database() -> Database {
    let database = Database::new();
    for sql in [
        "CREATE TABLE whole (k BIGINT, tag BIGINT, lo BIGINT, hi BIGINT)",
        "INSERT INTO whole VALUES (1, 10, 1, 9), (2, 20, 5, 5), (3, 30, 1, 9), (4, 40, 1, 9), \
         (NULL, 50, 1, 9)",
        "CREATE TABLE part (k BIGINT)",
        "INSERT INTO part VALUES (1), (2), (3)",
        "CREATE TABLE first_side (k BIGINT)",
        "INSERT INTO first_side VALUES (1), (2), (3), (4)",
        "CREATE TABLE more (k BIGINT)",
        "INSERT INTO more VALUES (1), (2), (3), (4)",
        "CREATE TABLE inner_side (k BIGINT, v BIGINT)",
        "INSERT INTO inner_side VALUES (1, 10), (1, 100), (2, 300), (3, 30), (4, 400)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// Every row of a query as its values, in the order the query asked for them.
fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

/// The one column answer of a query, which most of these are.
fn column(database: &Database, sql: &str) -> Vec<Value> {
    rows(database, sql).into_iter().map(|row| row[0].clone()).collect()
}

/// A row of whole numbers, written the short way.
fn row(values: &[i64]) -> Vec<Value> {
    values.iter().map(|&at| Value::BigInt(at)).collect()
}

#[test]
fn an_exists_correlated_on_one_side_of_a_join_answers_the_rows_the_join_keeps() {
    assert_eq!(
        column(
            &database(),
            "SELECT whole.k FROM whole, part WHERE whole.k = part.k \
             AND EXISTS (SELECT 1 FROM inner_side \
             WHERE inner_side.k = whole.k AND inner_side.v <> whole.tag) \
             ORDER BY whole.k"
        ),
        row(&[1, 2])
    );
}

#[test]
fn a_not_exists_correlated_on_one_side_of_a_join_answers_the_rows_the_join_keeps() {
    assert_eq!(
        column(
            &database(),
            "SELECT whole.k FROM whole, part WHERE whole.k = part.k \
             AND NOT EXISTS (SELECT 1 FROM inner_side \
             WHERE inner_side.k = whole.k AND inner_side.v <> whole.tag) \
             ORDER BY whole.k"
        ),
        row(&[3])
    );
}

#[test]
fn a_second_subquery_over_the_join_the_first_one_made() {
    // TPC-H q21's shape, and the one that catches column pruning taking a column away. Four outer
    // tables, the correlated one second, `lo` and `hi` read by a predicate above the join and by
    // nothing below it, and the second subquery's outer side is the join the first one produced.
    assert_eq!(
        rows(
            &database(),
            "SELECT whole.k, whole.tag FROM first_side, whole, part, more \
             WHERE first_side.k = whole.k AND whole.k = part.k AND whole.k = more.k \
             AND whole.hi > whole.lo \
             AND EXISTS (SELECT 1 FROM inner_side \
             WHERE inner_side.k = whole.k AND inner_side.v <> whole.tag) \
             AND NOT EXISTS (SELECT 1 FROM inner_side \
             WHERE inner_side.k = whole.k AND inner_side.v <> whole.tag AND inner_side.v > 150) \
             ORDER BY whole.k"
        ),
        vec![row(&[1, 10])]
    );
}

#[test]
fn an_outer_column_only_the_query_above_the_join_reads_is_still_there() {
    assert_eq!(
        rows(
            &database(),
            "SELECT whole.k, whole.tag FROM whole, part \
             WHERE whole.k = part.k AND whole.hi > whole.lo \
             AND EXISTS (SELECT 1 FROM inner_side \
             WHERE inner_side.k = whole.k AND inner_side.v <> whole.tag) \
             ORDER BY whole.k"
        ),
        vec![row(&[1, 10])]
    );
}

#[test]
fn a_correlated_scalar_subquery_over_a_join_answers_null_where_nothing_matched() {
    assert_eq!(
        rows(
            &database(),
            "SELECT whole.k, (SELECT max(v) FROM inner_side WHERE inner_side.k = whole.k) AS m \
             FROM whole, part WHERE whole.k = part.k ORDER BY whole.k"
        ),
        vec![row(&[1, 100]), row(&[2, 300]), row(&[3, 30])]
    );
}

#[test]
fn a_correlated_count_over_a_join_counts_zero_where_nothing_matched() {
    assert_eq!(
        rows(
            &database(),
            "SELECT whole.k, (SELECT count(*) FROM inner_side \
             WHERE inner_side.k = whole.k AND inner_side.v <> whole.tag) AS c \
             FROM whole, part WHERE whole.k = part.k ORDER BY whole.k"
        ),
        vec![row(&[1, 1]), row(&[2, 1]), row(&[3, 0])]
    );
}

#[test]
fn an_outer_row_whose_correlated_column_is_null_is_answered_the_same_way_over_a_join() {
    // The NULL row of `whole` matches nothing in `part` and so is padded rather than dropped, and it
    // is in the domain, which is where a rule that treated the domain as an answer would put it in
    // the output twice or not at all.
    assert_eq!(
        column(
            &database(),
            "SELECT whole.k FROM whole LEFT JOIN part ON whole.k = part.k \
             WHERE NOT EXISTS (SELECT 1 FROM inner_side \
             WHERE inner_side.k = whole.k AND inner_side.v <> whole.tag) \
             ORDER BY whole.k NULLS LAST"
        ),
        vec![Value::BigInt(3), Value::Null]
    );
}

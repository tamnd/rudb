//! `WITH ... AS MATERIALIZED`, from the answers rather than from the plan.
//!
//! Every sentence asserted here was read off the pinned duckdb on server2 first, which is
//! v2.0.0-dev84237 at cc7e7bac7f, the commit the grammar is vendored from. The plan side of the
//! same feature is tested where it is built: the binder's tests say what the nodes look like, the
//! shape tests in `rudb-plan` say which pipeline fills the rows and which one waits for them, and
//! `rudb-opt` says when the whole thing is dropped. What is here is whether the rows come out.
//!
//! Two of these record the pin going its own way. A column list on the definition with more names
//! in it than the definition has columns is ignored past the end rather than reported, which is
//! filed as tamnd/duckdb#8, and the same list on a table alias is reported. The recursive form is
//! not here at all, because it needs a fixpoint operator and that is D8.

use rudb::Database;
use rudb_common::Value;

/// A database with the statements already run, panicking on the first that does not.
fn ran(statements: &[&str]) -> Database {
    let database = Database::new();
    for sql in statements {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// The one column of integers a query answers with, read out in the order they came.
fn integers(database: &Database, sql: &str) -> Vec<i32> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| match result.value_at(row, 0) {
            Value::Integer(value) => value,
            other => panic!("{sql} answered with {other:?} rather than an integer"),
        })
        .collect()
}

/// The error a query gives.
fn refused(database: &Database, sql: &str) -> String {
    database.query(sql).expect_err(&format!("{sql} should be refused")).to_string()
}

#[test]
fn a_materialized_definition_answers_with_its_rows_where_it_is_named() {
    let database = Database::new();
    assert_eq!(
        integers(
            &database,
            "WITH c AS MATERIALIZED (SELECT 1 AS n UNION ALL SELECT 2) SELECT n FROM c ORDER BY n"
        ),
        [1, 2]
    );
}

#[test]
fn the_held_rows_are_read_in_full_at_every_use() {
    // The interesting one. The rows are held once and every read gets all of them, so the two sides
    // of the product are four rows and not two, which is what a shared cursor over the held chunks
    // would have given.
    let database = Database::new();
    let result = database
        .query(
            "WITH c AS MATERIALIZED (SELECT 1 AS n UNION ALL SELECT 2) \
             SELECT a.n + b.n AS pair FROM c a, c b ORDER BY pair",
        )
        .expect("the product of the two reads");
    let sums: Vec<Value> = (0..result.len()).map(|row| result.value_at(row, 0)).collect();
    assert_eq!(sums, [Value::Integer(2), Value::Integer(3), Value::Integer(3), Value::Integer(4)]);
}

#[test]
fn two_reads_in_different_pipelines_both_see_the_rows() {
    // The two reads here are not in one pipeline the way a product's are. Each side of the union is
    // a pipeline of its own and both of them wait for the one that fills the rows.
    let database = Database::new();
    assert_eq!(
        integers(
            &database,
            "WITH c AS MATERIALIZED (SELECT 1 AS n) SELECT n FROM c UNION ALL SELECT n FROM c"
        ),
        [1, 1]
    );
}

#[test]
fn a_definition_can_read_the_one_written_before_it() {
    let database = Database::new();
    assert_eq!(
        integers(
            &database,
            "WITH a AS MATERIALIZED (SELECT 1 AS n), b AS MATERIALIZED (SELECT n + 1 AS n FROM a) \
             SELECT n FROM b"
        ),
        [2]
    );
}

#[test]
fn a_definition_written_inside_another_one_is_held_separately() {
    let database = Database::new();
    assert_eq!(
        integers(
            &database,
            "WITH outer_ AS MATERIALIZED (\
                 WITH inner_ AS MATERIALIZED (SELECT 1 AS n UNION ALL SELECT 2) \
                 SELECT n + 10 AS n FROM inner_\
             ) SELECT n FROM outer_ ORDER BY n"
        ),
        [11, 12]
    );
}

#[test]
fn the_body_reads_rows_a_table_put_there() {
    let database = ran(&["CREATE TABLE t (i INTEGER)", "INSERT INTO t VALUES (1), (2), (3)"]);
    let result = database
        .query(
            "WITH c AS MATERIALIZED (SELECT i FROM t WHERE i > 1) \
             SELECT count(*) AS rows, sum(i) AS total FROM c",
        )
        .expect("the count of the held rows");
    assert_eq!(result.names(), ["rows", "total"]);
    assert_eq!(result.value_at(0, 0), Value::BigInt(2));
    assert_eq!(result.value_at(0, 1), Value::HugeInt(5));
}

#[test]
fn a_definition_that_produces_nothing_gives_the_body_nothing() {
    let database = Database::new();
    let result = database
        .query("WITH c AS MATERIALIZED (SELECT 1 AS n WHERE false) SELECT count(*) AS rows FROM c")
        .expect("an empty materialisation is still a materialisation");
    assert_eq!(result.value_at(0, 0), Value::BigInt(0));
}

#[test]
fn a_definition_nothing_reads_is_not_run_and_is_not_in_the_plan() {
    let database = Database::new();
    assert_eq!(integers(&database, "WITH c AS MATERIALIZED (SELECT 1 AS n) SELECT 42"), [42]);
    // The pin says the same thing twice over. `WITH c AS MATERIALIZED (SELECT error('boom'))
    // SELECT 42` answers 42 rather than raising, and its `EXPLAIN` has no CTE node in it. rudb has
    // no function that would say so out loud yet, so this reads the plan instead.
    let plan = database.plan("WITH c AS MATERIALIZED (SELECT 1 AS n) SELECT 42").expect("plans");
    assert!(!plan.contains("MaterializedCte"), "{plan}");
}

#[test]
fn the_column_list_on_the_definition_names_what_a_read_sees() {
    let database = Database::new();
    let result = database
        .query("WITH c(a, b) AS MATERIALIZED (SELECT 1, 2) SELECT * FROM c")
        .expect("the declared names");
    assert_eq!(result.names(), ["a", "b"]);
}

#[test]
fn a_column_list_shorter_than_the_definition_renames_a_prefix() {
    let database = Database::new();
    let result = database
        .query("WITH c(a) AS MATERIALIZED (SELECT 1, 2) SELECT * FROM c")
        .expect("one name for the first column");
    assert_eq!(result.names(), ["a", "2"]);
}

#[test]
fn a_column_list_longer_than_the_definition_ignores_the_names_past_the_end() {
    // The pin's own rule, filed as tamnd/duckdb#8. PostgreSQL reports this one and so does the pin
    // when the same list is written on a table alias, which the test below says.
    let database = Database::new();
    let result = database
        .query("WITH c(a, b, d, e) AS MATERIALIZED (SELECT 1, 2) SELECT * FROM c")
        .expect("the extra names are dropped rather than reported");
    assert_eq!(result.names(), ["a", "b"]);
}

#[test]
fn an_alias_at_the_read_renames_the_table_and_a_prefix_of_its_columns() {
    let database = Database::new();
    let result = database
        .query("WITH c(a, b) AS MATERIALIZED (SELECT 1, 2) SELECT x.y, x.b FROM c AS x(y)")
        .expect("the alias renames the first column and the table");
    assert_eq!(result.names(), ["y", "b"]);
    assert_eq!(
        refused(&database, "WITH c AS MATERIALIZED (SELECT 1 AS n) SELECT * FROM c AS x(y, z)"),
        "Binder Error: table \"x\" has 1 columns available but 2 columns specified"
    );
}

#[test]
fn the_name_is_gone_once_the_query_that_wrote_it_is_over() {
    let database = Database::new();
    assert_eq!(
        refused(&database, "WITH c AS MATERIALIZED (SELECT 1 AS n) SELECT * FROM d"),
        "Catalog Error: Table with name d does not exist!"
    );
    // The name is not in a schema either, so qualifying it finds nothing. The pin sends this one
    // off to the file readers and says no extension can read `main.c`, which is a worse sentence
    // for the same fact and is not copied.
    assert_eq!(
        refused(&database, "WITH c AS MATERIALIZED (SELECT 1 AS n) SELECT n FROM main.c"),
        "Catalog Error: Table with name c does not exist!"
    );
}

#[test]
fn a_definition_written_inside_an_inlined_one_is_held_once_per_use() {
    // A plain `WITH` read once goes into the place it is named, so a materialised one written
    // inside it is bound there and holds rows of its own. Read twice the outer one is held instead
    // and the inner one is bound once, which is the second query here, and both answer the same
    // thing as writing the text out by hand.
    let database = Database::new();
    assert_eq!(
        integers(
            &database,
            "WITH plain AS (WITH held AS MATERIALIZED (SELECT 1 AS n) SELECT n FROM held) \
             SELECT n FROM plain"
        ),
        [1]
    );
    assert_eq!(
        integers(
            &database,
            "WITH plain AS (WITH held AS MATERIALIZED (SELECT 1 AS n) SELECT n FROM held) \
             SELECT n FROM plain UNION ALL SELECT n FROM plain"
        ),
        [1, 1]
    );
}

#[test]
fn a_plain_definition_read_twice_answers_what_reading_it_twice_means() {
    // Reading a plain definition twice holds its rows rather than running it twice, which is the
    // pin's rule and is worth having whether or not it is: a definition two places read is a
    // definition that would otherwise be computed two times. What must not change is the answer,
    // so these are the shapes where holding the rows could have gone wrong. The product of a
    // definition with itself is every pair, not a cursor shared between the two sides. A read
    // filtered one way and the same read filtered another way each see all the rows, since the
    // filter is above the read and not inside the definition. And a definition read from two
    // scalar subqueries answers both of them.
    let database = ran(&["CREATE TABLE t (n INTEGER)", "INSERT INTO t VALUES (1), (2), (3)"]);
    assert_eq!(
        integers(
            &database,
            "WITH c AS (SELECT n FROM t) SELECT a.n * 10 + b.n FROM c a, c b ORDER BY 1"
        ),
        [11, 12, 13, 21, 22, 23, 31, 32, 33]
    );
    assert_eq!(
        integers(
            &database,
            "WITH c AS (SELECT n FROM t) \
             SELECT a.n FROM c a WHERE a.n < 3 UNION ALL SELECT b.n FROM c b WHERE b.n > 1 \
             ORDER BY 1"
        ),
        [1, 2, 2, 3]
    );
    assert_eq!(
        integers(
            &database,
            "WITH c AS (SELECT n FROM t) \
             SELECT (SELECT max(n) FROM c) * 10 + (SELECT min(n) FROM c)"
        ),
        [31]
    );
    // An empty definition read twice is still empty both times, which is the case where a pipeline
    // that waits for rows could have waited for rows that never come.
    assert_eq!(
        integers(&database, "WITH c AS (SELECT n FROM t WHERE n > 100) SELECT a.n FROM c a, c b"),
        []
    );
}

#[test]
fn not_materialized_and_the_plain_form_answer_the_same_way() {
    let database = Database::new();
    assert_eq!(
        integers(&database, "WITH c AS NOT MATERIALIZED (SELECT 1 AS n) SELECT n FROM c"),
        [1]
    );
    assert_eq!(integers(&database, "WITH c AS (SELECT 1 AS n) SELECT n FROM c"), [1]);
}

#[test]
fn a_materialisation_can_be_written_inside_a_subquery() {
    let database = Database::new();
    let result = database
        .query(
            "SELECT count(*) AS rows FROM \
             (WITH c AS MATERIALIZED (SELECT 1 AS n UNION ALL SELECT 2) SELECT n FROM c)",
        )
        .expect("a materialisation under a subquery");
    assert_eq!(result.value_at(0, 0), Value::BigInt(2));
}

#[test]
fn values_is_a_definition_like_any_other() {
    let database = Database::new();
    let result = database
        .query("WITH c AS MATERIALIZED (VALUES (1), (2)) SELECT * FROM c ORDER BY 1")
        .expect("values in a materialisation");
    assert_eq!(result.names(), ["col0"]);
    assert_eq!(result.len(), 2);
}

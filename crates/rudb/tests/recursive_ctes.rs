//! `WITH RECURSIVE`, from the answers rather than from the plan.
//!
//! Every answer and every error asserted here was read off the pinned duckdb on server2 first,
//! which is v2.0.0-dev84237 at cc7e7bac7f. The columns of a recursive definition are the anchor's,
//! names and types both, and the recursive side is cast to them rather than met halfway, which is
//! where most of the surprises below come from.

use rudb::Database;

/// A database with the statements already run, panicking on the first that does not.
fn ran(statements: &[&str]) -> Database {
    let database = Database::new();
    for sql in statements {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// Every row a query answers with, each as its cells joined with a bar, in the order they came.
fn rows(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| {
            (0..result.width())
                .map(|column| result.text_at(row, column))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// The error a query gives, up to the end of its first line.
fn refused(database: &Database, sql: &str) -> String {
    let error = database.query(sql).expect_err(&format!("{sql} should be refused")).to_string();
    error.lines().next().unwrap_or_default().to_string()
}

#[test]
fn a_count_runs_until_the_recursive_side_adds_nothing() {
    let database = Database::new();
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM t WHERE x < 5) \
             SELECT * FROM t"
        ),
        ["1", "2", "3", "4", "5"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE fib(a, b) AS (SELECT 0, 1 UNION ALL SELECT b, a + b FROM fib \
             WHERE b < 50) SELECT a FROM fib"
        ),
        ["0", "1", "1", "2", "3", "5", "8", "13", "21", "34"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(a) AS (SELECT 1 UNION ALL SELECT a + 1 FROM t WHERE a < 3) \
             SELECT sum(a), count(*) FROM t"
        ),
        ["6|3"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(a) AS (SELECT 'x' UNION ALL SELECT a || 'x' FROM t \
             WHERE length(a) < 4) SELECT * FROM t"
        ),
        ["x", "xx", "xxx", "xxxx"]
    );
}

#[test]
fn the_anchor_decides_the_types_and_the_recursive_side_is_cast_to_them() {
    let database = Database::new();
    // 1 + 1.5 is 2.5, which goes back in as the integer 3, and 3 stops the walk.
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t AS (SELECT 1 AS a UNION ALL SELECT a + 1.5 FROM t WHERE a < 3) \
             SELECT a, typeof(a) FROM t"
        ),
        ["1|INTEGER", "3|INTEGER"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(a) AS (SELECT 1::TINYINT UNION ALL SELECT a + 100 FROM t \
             WHERE a < 100) SELECT * FROM t"
        ),
        ["1", "101"]
    );
    assert_eq!(
        refused(
            &database,
            "WITH RECURSIVE t(a) AS (SELECT 1 UNION ALL SELECT 'x' FROM t WHERE a < 3) \
             SELECT * FROM t"
        ),
        "Conversion Error: Could not convert string 'x' to INT32"
    );
}

#[test]
fn union_without_all_drops_rows_already_produced_and_so_stops_at_a_cycle() {
    let database = ran(&[
        "CREATE TABLE e(s INT, d INT)",
        "INSERT INTO e VALUES (1, 2), (2, 3), (3, 1), (3, 4)",
    ]);
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(a) AS (SELECT 1 UNION SELECT a % 3 + 1 FROM t) \
             SELECT * FROM t ORDER BY a"
        ),
        ["1", "2", "3"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT d FROM e JOIN r ON e.s = r.n) \
             SELECT n FROM r ORDER BY n"
        ),
        ["1", "2", "3", "4"]
    );
}

#[test]
fn the_recursive_side_may_read_the_name_more_than_once() {
    let database = Database::new();
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(a) AS (SELECT 1 UNION ALL SELECT t1.a + t2.a FROM t t1, t t2 \
             WHERE t1.a < 8) SELECT * FROM t"
        ),
        ["1", "2", "4", "8"]
    );
}

#[test]
fn the_finished_rows_are_read_like_any_other_definition() {
    let database = Database::new();
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(a) AS (SELECT 1 UNION ALL SELECT a + 1 FROM t WHERE a < 3), \
             u AS (SELECT a * 10 AS b FROM t) SELECT b FROM u"
        ),
        ["10", "20", "30"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(a) AS (SELECT 1 UNION ALL SELECT a + 1 FROM t WHERE a < 3) \
             SELECT * FROM t AS x(b) WHERE b > 1"
        ),
        ["2", "3"]
    );
    assert_eq!(
        rows(
            &database,
            "SELECT (WITH RECURSIVE t(a) AS (SELECT 1 UNION ALL SELECT a + 1 FROM t \
             WHERE a < 4) SELECT max(a) FROM t)"
        ),
        ["4"]
    );
    // More declared names than columns is not an error, the same as for a plain definition.
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(a, b) AS (SELECT 1 UNION ALL SELECT a + 1 FROM t WHERE a < 2) \
             SELECT * FROM t"
        ),
        ["1", "2"]
    );
}

#[test]
fn a_view_holding_one_is_written_back_and_read_the_way_the_pin_does() {
    let database =
        ran(&["CREATE VIEW v AS WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t \
         WHERE n < 3) SELECT * FROM t"]);
    assert_eq!(
        rows(&database, "SELECT sql FROM duckdb_views() WHERE view_name = 'v'"),
        ["CREATE VIEW v AS WITH RECURSIVE t (n) AS ((SELECT 1) UNION  ALL (SELECT (n + 1) FROM t \
          WHERE (n < 3)))SELECT * FROM t;"]
    );
    assert_eq!(rows(&database, "SELECT * FROM v"), ["1", "2", "3"]);
}

#[test]
fn what_the_pin_refuses_is_refused_in_its_words() {
    let database = Database::new();
    for (sql, expected) in [
        (
            "WITH RECURSIVE t(a) AS (SELECT 1 UNION ALL SELECT a + 1 FROM t WHERE a < 3 \
             ORDER BY a) SELECT * FROM t",
            "Parser Error: ORDER BY in a recursive query is not allowed",
        ),
        (
            "WITH RECURSIVE t(a) AS (SELECT 1 UNION ALL SELECT a + 1 FROM t WHERE a < 3 \
             LIMIT 1) SELECT * FROM t",
            "Parser Error: LIMIT or OFFSET in a recursive query is not allowed",
        ),
        (
            "WITH RECURSIVE t(a) AS (SELECT 1 UNION ALL SELECT a + 1, 2 FROM t WHERE a < 3) \
             SELECT * FROM t",
            "Binder Error: Set operations can only apply to expressions with the same number of \
             result columns",
        ),
        (
            "WITH RECURSIVE t(a) AS (SELECT 1 INTERSECT SELECT a FROM t) SELECT * FROM t",
            "Binder Error: Circular reference to CTE \"t\", use WITH RECURSIVE to use recursive \
             CTEs.",
        ),
        (
            "WITH RECURSIVE t(a) AS (SELECT a FROM t UNION ALL SELECT 1) SELECT * FROM t",
            "Binder Error: Circular reference to CTE \"t\", use WITH RECURSIVE to use recursive \
             CTEs.",
        ),
    ] {
        assert_eq!(refused(&database, sql), expected, "{sql}");
    }
}

/// A limit over a recursion that never ends on its own ends it, as PostgreSQL and the pin end it by
/// producing rows only as the query reads them. Here the rounds stop once the rows the limit reads
/// are produced, and the rows are the first ones in the order the rounds made them.
#[test]
fn a_limit_ends_a_recursion_that_would_not_end_on_its_own() {
    let database = Database::new();
    let endless = "WITH RECURSIVE t(n) AS (VALUES (1) UNION ALL SELECT n + 1 FROM t)";
    assert_eq!(
        rows(&database, &format!("{endless} SELECT * FROM t LIMIT 10")),
        ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"]
    );
    assert_eq!(
        rows(&database, &format!("{endless} SELECT * FROM t LIMIT 3 OFFSET 2")),
        ["3", "4", "5"]
    );
    assert_eq!(rows(&database, &format!("{endless} SELECT n * 2 FROM t LIMIT 3")), ["2", "4", "6"]);
    assert_eq!(
        rows(&database, &format!("{endless} SELECT * FROM t LIMIT 0")),
        Vec::<String>::new()
    );
    // What reads the limit can be anything.
    assert_eq!(
        rows(&database, &format!("{endless} SELECT sum(n) FROM (SELECT * FROM t LIMIT 100) AS s")),
        ["5050"]
    );
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(n) AS (VALUES (1) UNION SELECT n + 1 FROM t) SELECT * FROM t LIMIT 4"
        ),
        ["1", "2", "3", "4"]
    );
    // A round can make more rows than the limit reads, and only the first ones are kept.
    assert_eq!(
        rows(
            &database,
            "WITH RECURSIVE t(n) AS (VALUES (1) UNION ALL SELECT n * 10 + k FROM t, \
             (VALUES (1), (2), (3)) AS v(k)) SELECT * FROM t LIMIT 5"
        ),
        ["1", "11", "12", "13", "111"]
    );
}

/// The recursion learns how many rows it has to make only when nothing but projections stands
/// between the limit and the read of it, since a filter, a sort or a second read of it needs an
/// unknown number of its rows.
#[test]
fn only_a_limit_that_reads_the_recursion_alone_bounds_it() {
    let database = Database::new();
    let plan = |sql: &str| {
        database.plan(sql).unwrap_or_else(|error| panic!("{sql} did not plan: {error}"))
    };
    let finite = "WITH RECURSIVE t(n) AS (VALUES (1) UNION ALL SELECT n + 1 FROM t WHERE n < 20)";
    assert!(plan(&format!("{finite} SELECT n + 1 FROM t LIMIT 3 OFFSET 4")).contains("WANTED 7 "));
    assert!(
        plan(&format!("{finite} SELECT max(n) FROM (SELECT n FROM t LIMIT 6) AS s"))
            .contains("WANTED 6 ")
    );
    for read in [
        "SELECT * FROM t WHERE n % 2 = 0 LIMIT 3",
        "SELECT * FROM t ORDER BY n DESC LIMIT 3",
        "SELECT a.n FROM t AS a, t AS b LIMIT 3",
        "SELECT count(*) FROM (SELECT n FROM t LIMIT 7) AS s, t AS u",
        "SELECT * FROM t",
    ] {
        let sql = format!("{finite} {read}");
        assert!(!plan(&sql).contains("WANTED"), "{sql}");
    }
    assert_eq!(
        rows(&database, &format!("{finite} SELECT * FROM t WHERE n % 2 = 0 LIMIT 3")),
        ["2", "4", "6"]
    );
    assert_eq!(
        rows(&database, &format!("{finite} SELECT * FROM t ORDER BY n DESC LIMIT 3")),
        ["20", "19", "18"]
    );
}

//! A `WITH` inside a correlated subquery or a `LATERAL`, recursive or materialized, whose definition
//! reads the outer row. Every expected answer here was taken from the pinned duckdb binary,
//! v2.0.0-dev84237.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect()
}

#[test]
fn a_recursive_definition_runs_once_for_every_outer_row() {
    let database = Database::new();
    for (sql, expected) in [
        (
            "SELECT s.lo, q.n FROM (VALUES (1, 3), (2, 4), (5, 5)) s(lo, hi), LATERAL (WITH \
             RECURSIVE t(n) AS (SELECT s.lo UNION ALL SELECT n + 1 FROM t WHERE n < s.hi) SELECT n \
             FROM t) q ORDER BY ALL",
            vec!["1|1", "1|2", "1|3", "2|2", "2|3", "2|4", "5|5"],
        ),
        (
            "SELECT s.lo, (WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < \
             s.lo) SELECT sum(n) FROM t) FROM (VALUES (1), (3), (NULL), (3)) s(lo) ORDER BY ALL",
            vec!["1|1", "3|6", "3|6", "NULL|1"],
        ),
        (
            "SELECT s.lo, (WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT (n + 1) % s.lo FROM t) \
             SELECT count(*) FROM t) FROM (VALUES (2), (3), (5)) s(lo) ORDER BY ALL",
            vec!["2|2", "3|3", "5|5"],
        ),
        (
            "SELECT limit_value, final_value FROM range(4) limits(limit_value), LATERAL (WITH \
             RECURSIVE t(k, v) USING KEY (k) AS (VALUES (1, 0) UNION SELECT k, v + 1 FROM t WHERE \
             v < limit_value) SELECT v AS final_value FROM t) ORDER BY ALL",
            vec!["0|0", "1|1", "2|2", "3|3"],
        ),
        (
            "SELECT s.x, EXISTS (WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t \
             WHERE n < 5) SELECT 1 FROM t WHERE n = s.x) FROM (VALUES (2), (9)) s(x) ORDER BY ALL",
            vec!["2|true", "9|false"],
        ),
        (
            "SELECT a.x, b.x, n FROM (SELECT {x: lo} AS a, {x: hi} AS b FROM (VALUES (1, 3), (2, \
             4), (1, 3), (NULL, 3), (1, NULL)) bounds(lo, hi)) seed, LATERAL (WITH RECURSIVE t(n) \
             USING KEY (n) AS (SELECT a.x UNION ALL SELECT n + 1 FROM t WHERE n < b.x AND NOT \
             EXISTS (SELECT 1 FROM recurring.t r WHERE r.n = b.x)) SELECT * FROM t) q ORDER BY ALL",
            vec![
                "1|3|1",
                "1|3|1",
                "1|3|2",
                "1|3|2",
                "1|3|3",
                "1|3|3",
                "1|NULL|1",
                "2|4|2",
                "2|4|3",
                "2|4|4",
                "NULL|3|NULL",
            ],
        ),
    ] {
        assert_eq!(answered(&database, sql), expected, "{sql}");
    }
}

#[test]
fn a_materialized_definition_is_held_once_for_every_outer_row() {
    let database = Database::new();
    for (sql, expected) in [
        (
            "SELECT s.x, q.* FROM (VALUES (1), (2)) s(x), LATERAL (WITH c AS MATERIALIZED (SELECT \
             s.x * 10 AS y) SELECT y, y + 1 FROM c) q ORDER BY ALL",
            vec!["1|10|11", "2|20|21"],
        ),
        (
            "SELECT s.x, (WITH c AS MATERIALIZED (SELECT range AS r FROM range(5)) SELECT count(*) \
             FROM c WHERE r < s.x) FROM (VALUES (1), (3)) s(x) ORDER BY ALL",
            vec!["1|1", "3|3"],
        ),
    ] {
        assert_eq!(answered(&database, sql), expected, "{sql}");
    }
}

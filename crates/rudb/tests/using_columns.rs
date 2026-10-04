//! The column a `USING` or `NATURAL` join joins on, which `SELECT *` and a bare name see once and
//! which each side's own name still reaches. Every expected answer here was taken from the pinned
//! duckdb binary, v2.0.0-dev84237.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect()
}

#[test]
fn each_side_keeps_its_own_copy_under_its_own_name() {
    let database = Database::new();
    for (sql, expected) in [
        ("SELECT b.k FROM (SELECT 1 k) a JOIN (SELECT 1 k) b USING (k)", vec!["1"]),
        ("SELECT b.k FROM (SELECT 1 k) a NATURAL JOIN (SELECT 1 k) b", vec!["1"]),
        (
            "SELECT c.k, b.k FROM (SELECT 1 k) a JOIN (SELECT 1 k) b USING (k) JOIN (SELECT 1 k) c \
             USING (k)",
            vec!["1|1"],
        ),
        (
            "SELECT b.k + 1 FROM (SELECT 1 k) a JOIN (SELECT 1 k) b USING (k) WHERE b.k = 1",
            vec!["2"],
        ),
        ("SELECT * FROM (SELECT 1 k, 2 x) a JOIN (SELECT 1 k, 3 y) b USING (k)", vec!["1|2|3"]),
        ("SELECT b.* FROM (SELECT 1 k, 2 x) a JOIN (SELECT 1 k, 3 y) b USING (k)", vec!["1|3"]),
        (
            "SELECT b.*, a.* FROM (SELECT 1 k, 2 x) a NATURAL JOIN (SELECT 1 k, 3 y) b",
            vec!["1|3|1|2"],
        ),
        (
            "SELECT * EXCLUDE (b.k) FROM (SELECT 1 k, 2 x) a JOIN (SELECT 1 k, 3 y) b USING (k)",
            vec!["1|2|3"],
        ),
        (
            "SELECT typeof(k), typeof(a.k), typeof(b.k) FROM (SELECT 1::INT k) a JOIN (SELECT \
             1::BIGINT k) b USING (k)",
            vec!["INTEGER|INTEGER|BIGINT"],
        ),
    ] {
        assert_eq!(answered(&database, sql), expected, "{sql}");
    }
}

#[test]
fn a_right_or_full_join_reads_the_key_from_whichever_side_has_it() {
    let database = Database::new();
    for (sql, expected) in [
        (
            "SELECT a.k, b.k, k FROM (SELECT 1 k) a LEFT JOIN (SELECT 2 k) b USING (k)",
            vec!["1|NULL|1"],
        ),
        (
            "SELECT a.k, b.k, k FROM (SELECT 1 k) a RIGHT JOIN (SELECT 2 k) b USING (k)",
            vec!["NULL|2|2"],
        ),
        (
            "SELECT a.k, b.k, k FROM (SELECT 1 k) a FULL JOIN (SELECT 2 k) b USING (k) ORDER BY ALL",
            vec!["1|NULL|1", "NULL|2|2"],
        ),
        (
            "SELECT * FROM (SELECT 1 k, 5 x) a NATURAL FULL JOIN (SELECT 2 k, 6 y) b ORDER BY ALL",
            vec!["1|5|NULL", "2|NULL|6"],
        ),
        (
            "SELECT a.*, '|', b.* FROM (SELECT 1 k, 5 x) a FULL JOIN (SELECT 2 k, 6 y) b USING (k) \
             ORDER BY ALL",
            vec!["1|5|||NULL|NULL", "NULL|NULL|||2|6"],
        ),
        (
            "SELECT k FROM (SELECT 1 k) a FULL JOIN (SELECT 1 k) b USING (k) FULL JOIN (SELECT 3 \
             k) c USING (k) ORDER BY 1",
            vec!["1", "3"],
        ),
        (
            "SELECT typeof(k) FROM (SELECT 1::INT k) a RIGHT JOIN (SELECT 1::BIGINT k) b USING (k)",
            vec!["BIGINT"],
        ),
        (
            "SELECT typeof(k) FROM (SELECT 1::INT k) a FULL JOIN (SELECT 2::BIGINT k) b USING (k) \
             LIMIT 1",
            vec!["BIGINT"],
        ),
    ] {
        assert_eq!(answered(&database, sql), expected, "{sql}");
    }
}

/// The names and the one row of a query, in the form the pin's `-list` mode prints them.
fn named(database: &Database, sql: &str) -> [String; 2] {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    [result.names().join("|"), answered(database, sql).join(" ")]
}

#[test]
fn a_bare_star_puts_the_key_at_the_first_copy_it_keeps() {
    let database = Database::new();
    let two = "FROM (SELECT 1 k, 2 x) a JOIN (SELECT 1 k, 3 y) b USING (k)";
    let three = "FROM (SELECT 1 k, 2 x) a JOIN (SELECT 1 k, 3 y) b USING (k) JOIN (SELECT 1 k, 4 z) \
                 c USING (k)";
    let full = "FROM (SELECT 1 k, 2 x) a FULL JOIN (SELECT 5 k, 3 y) b USING (k)";
    for (sql, expected) in [
        (format!("SELECT * EXCLUDE (a.k) {two}"), ["x|k|y", "2|1|3"]),
        (format!("SELECT * EXCLUDE (k) {two}"), ["x|y", "2|3"]),
        (format!("SELECT COLUMNS(* EXCLUDE (a.k)) {two}"), ["x|k|y", "2|1|3"]),
        (format!("SELECT * EXCLUDE (a.k) {three}"), ["x|k|y|z", "2|1|3|4"]),
        (format!("SELECT * EXCLUDE (a.k, b.k) {three}"), ["x|y|k|z", "2|3|1|4"]),
        (format!("SELECT * EXCLUDE (a.k) {full} ORDER BY ALL"), ["x|k|y", "2|1|NULL NULL|5|3"]),
        (format!("SELECT * EXCLUDE (a.k, b.k) {full} ORDER BY ALL"), ["x|y", "2|NULL NULL|3"]),
        (format!("SELECT * RENAME (a.k AS n) {two}"), ["n|x|y", "1|2|3"]),
        (format!("SELECT * RENAME (b.k AS n) {two}"), ["k|x|y", "1|2|3"]),
        (format!("SELECT * RENAME (b.k AS n, a.k AS m) {two}"), ["m|x|y", "1|2|3"]),
        (format!("SELECT * EXCLUDE (a.k) RENAME (b.k AS n) {two}"), ["x|n|y", "2|1|3"]),
        (format!("SELECT * RENAME (a.k AS n) {full} ORDER BY ALL"), ["n|x|y", "1|2|NULL 5|NULL|3"]),
        (
            format!("SELECT * REPLACE (k + 10 AS k) {full} ORDER BY ALL"),
            ["k|x|y", "11|2|NULL 15|NULL|3"],
        ),
    ] {
        assert_eq!(named(&database, &sql), expected, "{sql}");
    }
}

#[test]
fn a_recursive_side_can_name_the_key_of_the_state_it_joined_to() {
    let database = Database::new();
    let sql = "WITH RECURSIVE t(k, v) USING KEY (k) AS (SELECT i, 0 FROM range(4) r(i) UNION ALL \
               SELECT n.k, r.v + 1 FROM recurring.t r JOIN t n USING (k) WHERE n.v < 3) SELECT \
               count(*), min(v), max(v), sum(v) FROM t";
    assert_eq!(answered(&database, sql), ["4|3|3|12"]);
}

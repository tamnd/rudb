//! `QUALIFY`, which filters the rows of a select block after its windows have run.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

/// Every row of `sql`, one row per line and the cells joined with a bar.
fn rows(sql: &str) -> String {
    let result = Database::new().query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let width = result.rows().next().map_or(0, |row| row.len());
    (0..result.len())
        .map(|row| {
            (0..width).map(|column| result.text_at(row, column)).collect::<Vec<_>>().join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

#[test]
fn qualify_keeps_the_rows_a_window_picks() {
    assert_eq!(
        rows(
            "SELECT x FROM range(5) t(x) QUALIFY row_number() OVER (ORDER BY x DESC) <= 2 ORDER BY x"
        ),
        "3\n4"
    );
    assert_eq!(
        rows("SELECT x, row_number() OVER (ORDER BY x) FROM range(5) t(x) QUALIFY x > 3"),
        "4|5"
    );
    assert_eq!(
        rows(
            "SELECT x, row_number() OVER (ORDER BY x) AS r FROM range(5) t(x) QUALIFY r > 2 ORDER BY x"
        ),
        "2|3\n3|4\n4|5"
    );
    assert_eq!(
        rows("SELECT x FROM range(4) t(x) WINDOW w AS (ORDER BY x) QUALIFY lag(x) OVER w = 1"),
        "2"
    );
    assert_eq!(rows("SELECT x FROM range(3) t(x) QUALIFY sum(x) OVER () ORDER BY x"), "0\n1\n2");
    assert_eq!(rows("SELECT x FROM range(3) t(x) QUALIFY [1] = [row_number() OVER ()]"), "0");
    assert_eq!(
        rows(
            "SELECT x FROM range(5) t(x) QUALIFY row_number() OVER (ORDER BY x) IN (SELECT 2 UNION SELECT 4) ORDER BY x"
        ),
        "1\n3"
    );
}

#[test]
fn qualify_runs_after_the_grouping_and_before_distinct() {
    assert_eq!(
        rows(
            "SELECT x % 2 AS g, sum(x) FROM range(6) t(x) GROUP BY g QUALIFY rank() OVER (ORDER BY sum(x) DESC) = 1"
        ),
        "1|9"
    );
    assert_eq!(
        rows("SELECT count(*) FROM range(5) t(x) QUALIFY count(*) OVER () = 1"),
        "5"
    );
    assert_eq!(
        rows(
            "SELECT DISTINCT x % 2 AS y FROM range(6) t(x) QUALIFY count(*) OVER (PARTITION BY x % 2) = 3 ORDER BY y"
        ),
        "0\n1"
    );
}

#[test]
fn qualify_needs_a_window_somewhere_in_the_block() {
    let missing =
        "Binder Error: at least one window function must appear in the SELECT column or QUALIFY clause";
    assert_eq!(refused("SELECT x FROM range(3) t(x) QUALIFY x > 1"), missing);
    assert_eq!(refused("SELECT x FROM range(3) t(x) QUALIFY 'a'"), missing);
    assert_eq!(
        refused("SELECT x FROM range(3) t(x) QUALIFY DATE '2020-01-01' > row_number() OVER ()"),
        "Binder Error: Cannot compare values of type DATE and type BIGINT - an explicit cast is required"
    );
    assert_eq!(
        refused("SELECT x FROM range(3) t(x) QUALIFY z > 1 AND row_number() OVER () > 0"),
        "Binder Error: Referenced column z not found in FROM clause and can't find in alias map."
    );
}

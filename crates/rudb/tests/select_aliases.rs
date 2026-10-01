//! The aliases of a select list read from the block's other clauses and from later targets.
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

/// The error `sql` gives, of which the pin's first line is the part compared when it has a second
/// line of candidates, since rudb gives those on the same line.
fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

#[test]
fn where_reads_an_alias_after_the_columns_of_the_from() {
    assert_eq!(rows("SELECT x + 1 AS y FROM range(3) t(x) WHERE y > 1"), "2\n3");
    assert_eq!(rows("SELECT x + 1 AS x FROM range(3) t(x) WHERE x > 1"), "3");
    assert_eq!(rows("SELECT x AS Y FROM range(3) t(x) WHERE y > 1"), "2");
    assert_eq!(rows("SELECT x AS y, x + 1 AS y FROM range(3) t(x) WHERE y > 1"), "1|2\n2|3");
    assert_eq!(rows("SELECT (SELECT 1) AS y FROM range(1) t(x) WHERE y = 1"), "1");
    assert_eq!(rows("SELECT 5 AS current_date FROM range(1) t(x) WHERE current_date = 5"), "5");
}

#[test]
fn where_refuses_what_an_alias_stands_for_in_its_own_words() {
    assert!(
        refused("SELECT y + 1 AS y FROM range(3) t(x) WHERE y > 1")
            .starts_with("Binder Error: Referenced column \"y\" not found in FROM clause!")
    );
    assert_eq!(
        refused("SELECT sum(x) AS s FROM range(3) t(x) WHERE s > 1"),
        "Binder Error: WHERE clause cannot contain aggregates!"
    );
    assert_eq!(
        refused("SELECT row_number() OVER () AS r FROM range(3) t(x) WHERE r > 1"),
        "Binder Error: WHERE clause cannot contain window functions!"
    );
}

#[test]
fn a_target_reads_the_aliases_before_it() {
    assert_eq!(rows("SELECT x AS y, y + 1 AS z FROM range(3) t(x)"), "0|1\n1|2\n2|3");
    assert_eq!(
        rows("SELECT x AS y, sum(y) FROM range(3) t(x) GROUP BY x ORDER BY y"),
        "0|0\n1|1\n2|2"
    );
    assert_eq!(
        rows("SELECT x AS y, row_number() OVER (ORDER BY y DESC) FROM range(3) t(x) ORDER BY 1"),
        "0|3\n1|2\n2|1"
    );
    assert_eq!(rows("SELECT [1,2] AS l, l[1] FROM range(1)"), "[1, 2]|1");
    assert_eq!(
        refused("SELECT y + 1 AS z, x AS y FROM range(3) t(x)"),
        "Binder Error: Column \"y\" referenced that exists in the SELECT clause - but this column cannot be referenced before it is defined"
    );
    assert_eq!(
        refused("SELECT (SELECT 1) AS y, y + 1 FROM range(1) t(x)"),
        "Binder Error: Alias \"y\" referenced in a SELECT clause - but the expression has a subquery. This is not yet supported."
    );
}

#[test]
fn having_reads_an_alias_over_a_column_it_does_not_group_by() {
    assert_eq!(
        rows("SELECT x % 2 AS g, sum(x) AS s FROM range(6) t(x) GROUP BY g HAVING s > 6"),
        "1|9"
    );
    assert_eq!(rows("SELECT sum(x) AS x FROM range(6) t(x) HAVING x > 6"), "15");
    assert_eq!(rows("SELECT x % 2 AS g FROM range(6) t(x) GROUP BY x % 2 HAVING g = 1"), "1");
    assert_eq!(
        refused("SELECT 1 AS y FROM range(3) t(x) HAVING z > 1"),
        "Binder Error: column \"z\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
    assert!(
        refused("SELECT x AS y FROM range(3) t(x) GROUP BY x HAVING sum(y) > 0")
            .starts_with("Binder Error: Referenced column \"y\" not found in FROM clause!")
    );
}

#[test]
fn having_is_bound_before_the_select_list() {
    assert_eq!(
        refused("SELECT x FROM range(3) t(x) HAVING x > 1"),
        "Binder Error: column \"x\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
    assert_eq!(
        refused("SELECT x FROM range(3) t(x) GROUP BY x % 2 HAVING x > 1"),
        "Binder Error: column \"x\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
}

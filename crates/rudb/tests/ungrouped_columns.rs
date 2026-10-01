//! The error for a column that is neither grouped nor inside an aggregate, in the pin's words.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

#[test]
fn a_column_left_out_of_the_grouping_is_refused_with_a_hint() {
    for sql in [
        "SELECT count(*), x FROM (VALUES (1)) v(x)",
        "SELECT count(*), v.x + 1 FROM (VALUES (1)) v(x)",
        "SELECT x FROM (VALUES (1, 2)) v(x, y) GROUP BY y",
        "SELECT count(*) FROM (VALUES (1)) v(x) ORDER BY x",
    ] {
        assert_eq!(
            refused(sql),
            "Binder Error: column \"x\" must appear in the GROUP BY clause or must be part of an aggregate function.\nEither add it to the GROUP BY list, or use ANY_VALUE(\"x\") if the exact value of \"x\" is not important.",
            "{sql}"
        );
    }
}

#[test]
fn a_having_names_the_column_without_the_hint() {
    for sql in [
        "SELECT y FROM (VALUES (1, 2)) v(x, y) GROUP BY y HAVING x > 1",
        "SELECT count(*) FROM (VALUES (1)) v(x) HAVING x > 0",
    ] {
        assert_eq!(
            refused(sql),
            "Binder Error: column \"x\" must appear in the GROUP BY clause or be used in an aggregate function",
            "{sql}"
        );
    }
}

//! The name a column comes back with when nobody gave it one.
//!
//! A client keys a row by the name of a column, so a name that does not match DuckDB's is a
//! compatibility gap even when every value under it is right. None of these names is written down
//! anywhere, so every pair in the table below was read off the pinned duckdb binary on server2 by
//! running `SELECT <expression> FROM t` over a table of `i INTEGER, s VARCHAR, b BOOLEAN` and
//! taking the header it printed.
//!
//! The shape of the answers is that a name is the expression after DuckDB has rewritten it rather
//! than as somebody wrote it. An operator is named after the function it resolves to, which is why
//! `LIKE` comes back as `~~` and a prefix minus comes back with brackets. A `CASE` is named as the
//! searched form every `CASE` becomes. `IS TRUE` is named as the comparison it means. What is
//! missing from the table is the expressions rudb cannot answer yet, since a name for an answer
//! nobody can get is a name nobody can read: the bitwise operators, `GLOB`, `SIMILAR TO`, the
//! regular expression operators, a subquery, a struct, a subscript and `COALESCE` are all in the
//! sweep that produced this and all refused with a not implemented error.

use rudb::Database;

/// The expression as it is written, and the name DuckDB gives the column it produces.
const NAMES: &[(&str, &str)] = &[
    ("i", "i"),
    ("t.i", "i"),
    ("-i", "-(i)"),
    ("+i", "+(i)"),
    ("- -i", "-(-(i))"),
    ("-(i)", "-(i)"),
    ("-1", "-1"),
    ("-(1)", "-1"),
    ("-(-1)", "1"),
    ("- -1", "1"),
    ("-(1.5)", "-(1.5)"),
    ("+1", "+(1)"),
    ("+ -1", "+(-1)"),
    ("- +1", "-(+(1))"),
    ("-(1 + 1)", "-((1 + 1))"),
    ("-NULL", "-(NULL)"),
    ("abs(-i)", "abs(-(i))"),
    ("-abs(i)", "-(abs(i))"),
    ("-i + 1", "(-(i) + 1)"),
    ("-i * -i", "(-(i) * -(i))"),
    ("(i + 1) * 2", "((i + 1) * 2)"),
    ("NOT b", "(NOT b)"),
    ("NOT NOT b", "(NOT (NOT b))"),
    ("i IS NULL", "(i IS NULL)"),
    ("i IS NOT NULL", "(i IS NOT NULL)"),
    ("b IS TRUE", "(CAST(b AS BOOLEAN) IS NOT DISTINCT FROM true)"),
    ("b IS NOT TRUE", "(CAST(b AS BOOLEAN) IS DISTINCT FROM true)"),
    ("b IS FALSE", "(CAST(b AS BOOLEAN) IS NOT DISTINCT FROM false)"),
    ("b IS NOT FALSE", "(CAST(b AS BOOLEAN) IS DISTINCT FROM false)"),
    ("b IS UNKNOWN", "(b IS NULL)"),
    ("b IS NOT UNKNOWN", "(b IS NOT NULL)"),
    ("i IS NULL IS TRUE", "(CAST((i IS NULL) AS BOOLEAN) IS NOT DISTINCT FROM true)"),
    (
        "i BETWEEN 1 AND 2 IS TRUE",
        "(CAST((i BETWEEN 1 AND 2) AS BOOLEAN) IS NOT DISTINCT FROM true)",
    ),
    ("i + 1", "(i + 1)"),
    ("i - 1", "(i - 1)"),
    ("i * 2", "(i * 2)"),
    ("i / 2", "(i / 2)"),
    ("i // 2", "(i // 2)"),
    ("i % 2", "(i % 2)"),
    ("i = 1", "(i = 1)"),
    ("i <> 1", "(i != 1)"),
    ("i != 1", "(i != 1)"),
    ("i < 1", "(i < 1)"),
    ("i <= 1", "(i <= 1)"),
    ("i > 1", "(i > 1)"),
    ("i >= 1", "(i >= 1)"),
    ("i IS DISTINCT FROM 1", "(i IS DISTINCT FROM 1)"),
    ("i IS NOT DISTINCT FROM 1", "(i IS NOT DISTINCT FROM 1)"),
    ("b AND b", "(b AND b)"),
    ("b OR b", "(b OR b)"),
    ("i IS NULL AND b", "((i IS NULL) AND b)"),
    ("s || 'x'", "(s || 'x')"),
    ("s LIKE 'a%'", "(s ~~ 'a%')"),
    ("s NOT LIKE 'a%'", "(s !~~ 'a%')"),
    ("s ILIKE 'a%'", "(s ~~* 'a%')"),
    ("s NOT ILIKE 'a%'", "(s !~~* 'a%')"),
    ("CASE WHEN b THEN 1 ELSE 2 END", "CASE  WHEN (b) THEN (1) ELSE 2 END"),
    ("CASE WHEN b THEN 1 END", "CASE  WHEN (b) THEN (1) ELSE NULL END"),
    (
        "CASE WHEN b THEN 1 WHEN NOT b THEN 2 ELSE 3 END",
        "CASE  WHEN (b) THEN (1) WHEN ((NOT b)) THEN (2) ELSE 3 END",
    ),
    (
        "CASE i WHEN 1 THEN 'a' WHEN 2 THEN 'b' ELSE 'c' END",
        "CASE  WHEN ((i = 1)) THEN ('a') WHEN ((i = 2)) THEN ('b') ELSE 'c' END",
    ),
    ("CASE i WHEN 1 THEN 1 END", "CASE  WHEN ((i = 1)) THEN (1) ELSE NULL END"),
    ("i BETWEEN 1 AND 2", "(i BETWEEN 1 AND 2)"),
    ("i NOT BETWEEN 1 AND 2", "(NOT (i BETWEEN 1 AND 2))"),
    ("NOT (i BETWEEN 1 AND 2)", "(NOT (i BETWEEN 1 AND 2))"),
    ("i NOT BETWEEN 1 AND 2 AND b", "((NOT (i BETWEEN 1 AND 2)) AND b)"),
    ("i IN (1, 2)", "(i IN (1, 2))"),
    ("i NOT IN (1, 2)", "(NOT (i IN (1, 2)))"),
    ("NOT i IN (1, 2)", "(NOT (i IN (1, 2)))"),
    ("[1, 2]", "list_value(1, 2)"),
    ("NULL", "NULL"),
    ("TRUE", "true"),
    ("FALSE", "false"),
    ("'abc'", "'abc'"),
    ("1", "1"),
    ("1.5", "1.5"),
    ("count(*)", "count_star()"),
    ("count(DISTINCT i)", "count(DISTINCT i)"),
    ("sum(i) + 1", "(sum(i) + 1)"),
    ("CAST(i AS VARCHAR)", "CAST(i AS VARCHAR)"),
    ("TRY_CAST(i AS VARCHAR)", "TRY_CAST(i AS VARCHAR)"),
    ("i::BIGINT", "CAST(i AS BIGINT)"),
    ("length(s)", "length(s)"),
    ("LENGTH(s)", "length(s)"),
];

#[test]
fn an_unaliased_expression_is_named_what_duckdb_names_it() {
    let database = Database::new();
    database.execute("CREATE TABLE t(i INTEGER, s VARCHAR, b BOOLEAN)").expect("the table is made");
    let mut wrong = Vec::new();
    for (expression, name) in NAMES {
        let sql = format!("SELECT {expression} FROM t");
        let result = database.query(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
        if result.column_name(0) != *name {
            wrong.push(format!("{expression}: duckdb {name}, rudb {}", result.column_name(0)));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

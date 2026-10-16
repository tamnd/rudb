//! Calls written on a value, `s.upper()`, which the pin reads as `upper(s)` once the qualifier is
//! not a schema or a database the function is in.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

/// The text of every value the last query answers, a row to a line and the columns comma
/// separated.
fn answers(sqls: &[&str]) -> String {
    let database = Database::new();
    let (last, setup) = sqls.split_last().expect("a query");
    for sql in setup {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    let result = database.query(last).unwrap_or_else(|error| panic!("{last} failed: {error}"));
    let rows: Vec<String> = result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join(","))
        .collect();
    rows.join("\n")
}

const TABLE: &str = "CREATE TABLE t AS SELECT 'axb' AS s, [1, 2, 3] AS l, {'a': 'qq'} AS st";

#[test]
fn a_call_on_a_column_takes_the_column_as_its_first_argument() {
    assert_eq!(
        answers(&[
            TABLE,
            "SELECT s.replace('x', 'y'), l.list_sum(), t.s.upper(), st.a.upper() FROM t"
        ]),
        "ayb,6,AXB,QQ"
    );
    assert_eq!(answers(&[TABLE, "SELECT s.upper() FROM t WHERE s.length() = 3"]), "AXB");
    assert_eq!(
        answers(&[TABLE, "SELECT list_transform(l, lambda x: x.add(1)) FROM t"]),
        "[2, 3, 4]"
    );
    assert_eq!(answers(&[TABLE, "SELECT (SELECT s.upper()) FROM t"]), "AXB");
    assert_eq!(answers(&["SELECT 'abc'.length(), main.upper('a')"]), "3,A");
}

#[test]
fn a_qualifier_that_is_not_a_column_is_not_found_as_one() {
    let database = Database::new();
    let error = database.query("SELECT nope.upper('a')").unwrap_err();
    assert!(
        error.to_string().contains(
            "Referenced column \"nope\" was not found because the FROM clause is missing"
        ),
        "{error}"
    );
}

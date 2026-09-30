//! Stars with lists on them and `COLUMNS`, from the SQL down to the names and rows that come back.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;
use rudb_common::Value;

fn database() -> Database {
    let database = Database::new();
    database
        .execute("CREATE TABLE integers AS SELECT 42 i, 84 j UNION ALL SELECT 13, 14")
        .expect("the table");
    database
}

/// The names and the rows of a query, with the rows as text and sorted.
fn answer(database: &Database, sql: &str) -> (Vec<String>, Vec<String>) {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let names = result.names().to_vec();
    let mut rows: Vec<String> = result
        .rows()
        .map(|row: Vec<Value>| {
            row.iter().map(|value| format!("{value}")).collect::<Vec<_>>().join("|")
        })
        .collect();
    rows.sort();
    (names, rows)
}

fn error(database: &Database, sql: &str) -> String {
    match database.query(sql) {
        Ok(_) => panic!("{sql} was expected to fail"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn a_star_takes_exclude_replace_and_rename_lists() {
    let database = database();
    let (names, rows) = answer(&database, "SELECT * EXCLUDE (i) FROM integers");
    assert_eq!((names, rows), (vec!["j".to_string()], vec!["14".to_string(), "84".to_string()]));
    let (names, rows) =
        answer(&database, "SELECT * REPLACE (i + 1 AS i) RENAME (j AS k) FROM integers");
    assert_eq!(names, ["i", "k"]);
    assert_eq!(rows, ["14|14", "43|84"]);
    assert!(
        error(&database, "SELECT * EXCLUDE (x) FROM integers")
            .contains("Column \"x\" in EXCLUDE list not found in FROM clause")
    );
    assert!(
        error(&database, "SELECT * EXCLUDE (i, j) FROM integers")
            .contains("SELECT list is empty after resolving * expressions!")
    );
}

#[test]
fn columns_binds_the_expression_once_per_column_it_picks() {
    let database = database();
    let (names, rows) =
        answer(&database, "SELECT MIN(COLUMNS(*)), MAX(COLUMNS('j') + 1) FROM integers");
    assert_eq!(names, ["i", "j", "j"]);
    assert_eq!(rows, ["13|14|85"]);
    let (names, _) = answer(&database, "SELECT COLUMNS(['j', 'i']) AS \"x_\\0\" FROM integers");
    assert_eq!(names, ["x_i", "x_j"]);
    let (names, rows) = answer(&database, "SELECT COLUMNS(lambda c: c = 'j') FROM integers");
    assert_eq!((names, rows), (vec!["j".to_string()], vec!["14".to_string(), "84".to_string()]));
    let (_, rows) = answer(&database, "SELECT i FROM integers WHERE COLUMNS(*) > 20");
    assert_eq!(rows, ["42"]);
}

#[test]
fn columns_refuses_what_the_pin_refuses() {
    let database = database();
    assert!(
        error(&database, "SELECT COLUMNS('x') FROM integers")
            .contains("No matching columns found that match regex \"x\"")
    );
    assert!(
        error(&database, "SELECT COLUMNS(COLUMNS(*)) FROM integers")
            .contains("COLUMNS expression is not allowed inside another COLUMNS expression")
    );
    assert!(
        error(&database, "SELECT COLUMNS(*) + COLUMNS('i') FROM integers")
            .contains("Multiple different STAR/COLUMNS in the same expression are not supported")
    );
    assert!(error(&database, "SELECT COLUMNS(lambda c: c = 'k') FROM integers").contains(
        "Star expression \"COLUMNS(list_filter(['i', 'j'], (lambda c: (c = 'k'))))\" resulted in \
         an empty set of columns"
    ));
    assert!(
        error(&database, "SELECT COUNT(DISTINCT *) FROM integers")
            .contains("STAR expression is only allowed as the root element of an expression")
    );
}

#[test]
fn a_star_with_a_pattern_on_it_picks_the_names_the_pattern_holds_for() {
    let database = database();
    let (names, _) = answer(&database, "SELECT * LIKE 'i' FROM integers");
    assert_eq!(names, ["i"]);
    let (names, _) = answer(&database, "SELECT * NOT LIKE 'i' FROM integers");
    assert_eq!(names, ["j"]);
    let (names, _) = answer(&database, "SELECT * SIMILAR TO '.' AS \"\\0_x\" FROM integers");
    assert_eq!(names, ["i_x", "j_x"]);
    let (_, rows) = answer(&database, "SELECT i FROM integers ORDER BY * LIKE 'i'");
    assert_eq!(rows, ["13", "42"]);
    assert!(
        error(&database, "SELECT * + 42 FROM integers")
            .contains("Function \"\"+\"\" cannot be applied to a star expression")
    );
    assert!(
        error(&database, "SELECT * LIKE i FROM integers")
            .contains("Pattern applied to a star expression must be a constant")
    );
}

#[test]
fn an_unpacked_columns_hands_every_column_it_picks_to_the_call_around_it() {
    let database = database();
    let (names, rows) = answer(&database, "SELECT coalesce(*COLUMNS(*)) FROM integers");
    assert_eq!(names, ["COALESCE(memory.main.integers.i, memory.main.integers.j)"]);
    assert_eq!(rows, ["13", "42"]);
    let (names, rows) = answer(&database, "SELECT greatest(0, *COLUMNS('j')) + 1 FROM integers");
    assert_eq!(names, ["(greatest(0, memory.main.integers.j) + 1)"]);
    assert_eq!(rows, ["15", "85"]);
    let (_, rows) = answer(&database, "SELECT [*COLUMNS(*), *COLUMNS(*)] FROM integers");
    assert_eq!(rows, ["[13, 14, 13, 14]", "[42, 84, 42, 84]"]);
    let (_, rows) = answer(&database, "SELECT struct_pack(*COLUMNS(*)) FROM integers");
    assert_eq!(rows, ["{'i': 13, 'j': 14}", "{'i': 42, 'j': 84}"]);
    let (names, _) = answer(&database, "SELECT coalesce(*COLUMNS(*)) FROM integers a, integers b");
    assert_eq!(names, ["COALESCE(a.i, a.j, b.i, b.j)"]);
    let (_, rows) = answer(&database, "SELECT i FROM integers WHERE 42 IN (*COLUMNS(*))");
    assert_eq!(rows, ["42"]);
    let (_, rows) = answer(&database, "SELECT i FROM integers WHERE greatest(*COLUMNS(*)) > 50");
    assert_eq!(rows, ["42"]);
}

#[test]
fn an_unpacked_columns_is_refused_where_the_pin_refuses_it() {
    let database = database();
    assert!(
        error(&database, "SELECT (*COLUMNS(*)) FROM integers")
            .contains("*COLUMNS not allowed at the root level, use COLUMNS instead")
    );
    assert!(
        error(&database, "SELECT i FROM integers WHERE *COLUMNS(*) > 1")
            .contains("*COLUMNS() can not be used in this place")
    );
    assert!(
        error(&database, "SELECT sum(*COLUMNS(*)) FROM integers").contains(
            "No function matches the given name and argument types 'sum(INTEGER, INTEGER)'"
        )
    );
    assert!(
        error(&database, "SELECT concat(*COLUMNS(*), *COLUMNS('i')) FROM integers")
            .contains("Multiple different STAR/COLUMNS in the same expression are not supported")
    );
    assert!(
        error(&database, "SELECT i FROM integers GROUP BY greatest(*COLUMNS(*))")
            .contains("STAR expression is not supported here")
    );
}

#[test]
fn a_column_inside_an_expression_is_named_the_way_it_was_written() {
    let database = database();
    let (names, _) = answer(&database, "SELECT a.i + 1, sum(a.j) FROM integers a GROUP BY a.i");
    assert_eq!(names, ["(a.i + 1)", "sum(a.j)"]);
    let (names, _) = answer(&database, "SELECT \"select\".i + 1 FROM integers \"select\"");
    assert_eq!(names, ["(\"select\".i + 1)"]);
    let (names, _) = answer(&database, "SELECT integers.i FROM integers");
    assert_eq!(names, ["i"]);
}

#[test]
fn struct_pack_names_a_field_after_the_column_it_was_given() {
    let database = database();
    let (_, rows) = answer(&database, "SELECT struct_pack(a.i, J) FROM integers a");
    assert_eq!(rows, ["{'i': 13, 'j': 14}", "{'i': 42, 'j': 84}"]);
    assert!(
        error(&database, "SELECT struct_pack(i, i) FROM integers")
            .contains("Duplicate struct entry name \"i\"")
    );
    assert!(
        error(&database, "SELECT struct_pack(i + 1) FROM integers")
            .contains("Need named argument for struct pack, e.g. STRUCT_PACK(a := b)")
    );
}

#[test]
fn group_by_a_star_on_its_own_groups_by_everything() {
    let database = database();
    let (_, rows) = answer(&database, "SELECT i, sum(j) FROM integers GROUP BY *");
    assert_eq!(rows, ["13|14", "42|84"]);
    let (_, rows) = answer(&database, "SELECT i FROM integers GROUP BY integers.*");
    assert_eq!(rows, ["13", "42"]);
    for sql in [
        "SELECT i FROM integers GROUP BY * EXCLUDE (j)",
        "SELECT i FROM integers GROUP BY *, i",
        "SELECT i FROM integers GROUP BY COLUMNS(*)",
    ] {
        assert!(error(&database, sql).contains("STAR expression is not supported here"), "{sql}");
    }
}

#[test]
fn greatest_and_least_skip_nulls_and_promote_their_arguments() {
    let database = database();
    let (names, rows) = answer(
        &database,
        "SELECT greatest(i, j), least(1, NULL, 3), greatest(1, 2.5), greatest(NULL, NULL) FROM \
         integers WHERE i = 42",
    );
    assert_eq!(
        names,
        ["greatest(i, j)", "least(1, NULL, 3)", "greatest(1, 2.5)", "greatest(NULL, NULL)"]
    );
    assert_eq!(rows, ["84|1|2.5|NULL"]);
    let (_, rows) = answer(&database, "SELECT greatest('abc', 'b'), least([1, 2], [1])");
    assert_eq!(rows, ["b|[1]"]);
}

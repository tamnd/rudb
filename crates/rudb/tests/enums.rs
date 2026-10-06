//! `ENUM` values ordered by where their labels were declared, and enums made from a query. Every
//! expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

/// Every row of `sql` as the shell writes it.
fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let width = result.rows().next().map_or(0, |row| row.len());
    (0..result.len())
        .map(|row| {
            (0..width).map(|column| result.text_at(row, column)).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn refused(database: &Database, sql: &str, expected: &str) {
    let error = database.execute(sql).expect_err(sql);
    assert!(error.to_string().starts_with(expected), "{sql}: {error}");
}

/// A database with an enum whose labels are declared out of their alphabetical order, so that an
/// answer ordered by spelling and one ordered by place come out different.
fn declared() -> Database {
    let database = Database::new();
    database.execute("CREATE TYPE t AS ENUM ('z', 'x', 'y')").expect("creates");
    database
}

#[test]
fn an_enum_is_ordered_by_where_its_labels_were_declared() {
    let database = declared();
    for (sql, expected) in [
        (
            "SELECT greatest('x'::t, 'z'::t, 'y'::t), least('x'::t, 'z'::t, 'y'::t), \
             greatest(NULL::t, 'y'::t)",
            "y|z|y",
        ),
        (
            "SELECT list_sort(['x'::t, 'z'::t, 'y'::t]), \
             list_reverse_sort(['x'::t, 'z'::t, NULL, 'y'::t]), list_grade_up(['x'::t, 'z'::t, 'y'::t])",
            "[z, x, y]|[y, x, z, NULL]|[2, 1, 3]",
        ),
        (
            "SELECT histogram(v), max(v), min(v), arg_max(v, v), arg_min(v, v) \
             FROM (VALUES ('x'::t), ('y'::t), ('z'::t), ('x'::t)) s(v)",
            "{z=1, x=2, y=1}|y|z|y|z",
        ),
        (
            "SELECT arg_max(n, v), arg_min(n, v) FROM (VALUES (1, 'x'::t), (2, 'y'::t), (3, 'z'::t)) \
             s(n, v)",
            "2|3",
        ),
    ] {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
}

#[test]
fn a_grouped_or_ordered_aggregate_weighs_an_enum_by_place() {
    let database = declared();
    database.execute("CREATE TABLE u(g INT, v t)").expect("creates");
    database
        .execute("INSERT INTO u VALUES (1, 'x'), (1, 'y'), (1, 'z'), (2, NULL), (2, 'y')")
        .expect("inserts");
    assert_eq!(
        answered(
            &database,
            "SELECT g, min(v), max(v), list(v ORDER BY v DESC), string_agg(v, ',' ORDER BY v) \
             FROM u GROUP BY g ORDER BY g"
        ),
        ["1|z|y|[y, x, z]|z,x,y", "2|y|y|[y, NULL]|y"]
    );
    assert_eq!(
        answered(&database, "SELECT list(DISTINCT v ORDER BY v) FROM u WHERE g = 1"),
        ["[z, x, y]"]
    );
    assert_eq!(
        answered(&database, "SELECT v, hash(v) FROM u WHERE g = 1 ORDER BY v"),
        ["z|0", "x|4717996019076358352", "y|2060787363917578834"]
    );
}

#[test]
fn an_enum_hashes_as_the_integer_it_is_stored_in() {
    let database = declared();
    assert_eq!(
        answered(&database, "SELECT hash('z'::t), hash('x'::t), hash('x'::t) = hash(1::UTINYINT)"),
        ["0|4717996019076358352|true"]
    );
}

#[test]
fn the_ends_of_a_range_boundary_are_enums_or_nulls() {
    let database = declared();
    assert_eq!(
        answered(
            &database,
            "SELECT enum_range_boundary(NULL, 'x'::t), enum_range_boundary('x'::t, NULL)"
        ),
        ["[z, x]|[x, y]"]
    );
    for sql in [
        "SELECT enum_range_boundary('x'::t, 1)",
        "SELECT enum_range_boundary(1, 'x'::t)",
        "SELECT enum_range_boundary('x'::t, 'y')",
    ] {
        refused(&database, sql, "Binder Error: This function needs an ENUM as an argument");
    }
}

#[test]
fn an_enum_can_take_its_labels_from_a_query() {
    for (setup, sql, expected) in [
        (
            "CREATE TYPE t AS ENUM (SELECT 'b' UNION ALL SELECT 'a' UNION ALL SELECT 'b' UNION ALL \
             SELECT NULL)",
            "SELECT enum_range(NULL::t)",
            "[b, a]",
        ),
        (
            "CREATE TYPE t AS ENUM (SELECT * FROM range(3))",
            "SELECT enum_range(NULL::t)",
            "[0, 1, 2]",
        ),
        ("CREATE TYPE t AS ENUM (SELECT 'a' WHERE false)", "SELECT enum_range(NULL::t)", "[]"),
    ] {
        let database = Database::new();
        database.execute(setup).unwrap_or_else(|error| panic!("{setup} failed: {error}"));
        assert_eq!(answered(&database, sql), [expected], "{setup}");
    }
    let database = Database::new();
    database.execute("CREATE TABLE s(v VARCHAR)").expect("creates");
    database.execute("INSERT INTO s VALUES ('q'), ('p')").expect("inserts");
    database.execute("CREATE TYPE t AS ENUM (SELECT v FROM s ORDER BY v)").expect("creates");
    assert_eq!(
        answered(&database, "SELECT enum_range(NULL::t), 'q'::t < 'p'::t"),
        ["[p, q]|false"]
    );
    refused(
        &database,
        "CREATE TYPE u AS ENUM (SELECT 1, 2)",
        "Binder Error: The query must return a single column",
    );
}

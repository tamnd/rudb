//! The rows of an `INSERT ... VALUES`, which are cast to the columns they land in one by one
//! rather than first agreeing on a type among themselves. Every answer here is the pinned duckdb
//! binary's for the same statements.

use rudb::Database;

/// The rows `sql` reads after `setup` runs, one line per row with the columns between bars, or
/// the error the setup ended in.
fn after(setup: &[&str], sql: &str) -> String {
    let database = Database::new();
    for statement in setup {
        if let Err(error) = database.execute(statement) {
            return error.to_string();
        }
    }
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn each_value_is_cast_to_the_column_it_lands_in() {
    for (table, insert, expected) in [
        ("t (c VARCHAR)", "INSERT INTO t VALUES ('a'), (2)", "a\n2"),
        ("t (i INT)", "INSERT INTO t VALUES ('1'), (2.5)", "1\n3"),
        ("t (i INT[])", "INSERT INTO t VALUES ([1]), ('[2,3]')", "[1]\n[2, 3]"),
        ("t (i INT, j VARCHAR)", "INSERT INTO t (j, i) VALUES (1, '2'), ('x', 3)", "2|1\n3|x"),
        (
            "t (a INT, c VARCHAR, d INT[])",
            "INSERT INTO t VALUES (0, 'short', [0, 1]), (-42, 2, [])",
            "0|short|[0, 1]\n-42|2|[]",
        ),
    ] {
        let create = format!("CREATE TABLE {table}");
        assert_eq!(after(&[&create, insert], "SELECT * FROM t"), expected, "{insert}");
    }
}

#[test]
fn a_value_that_does_not_fit_its_column_is_refused_in_the_pins_words() {
    for (table, insert, expected) in [
        (
            "t (i INT)",
            "INSERT INTO t VALUES (1), ('a')",
            "Conversion Error: Could not convert string 'a' to INT32",
        ),
        (
            "t (i INT, j VARCHAR)",
            "INSERT INTO t VALUES (1, 'a', 3)",
            "Binder Error: table \"t\" has 2 columns but 3 values were supplied",
        ),
    ] {
        let create = format!("CREATE TABLE {table}");
        let error = after(&[&create, insert], "SELECT * FROM t");
        assert!(error.starts_with(expected), "{insert}: {error}");
    }
}

//! A bare name in the arguments of a table function, which the pin reads as the string it spells.
//!
//! `read_csv(data)` reads the file `data` there, with a warning that this will stop one day, and
//! so does every table function but the five that can take a column from a lateral neighbour. In
//! those a name that is no column is the missing column it always was. Every answer here was
//! measured against the pinned duckdb binary.

use rudb::Database;

fn answered(sql: &str) -> String {
    let result = Database::new().query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn refused(sql: &str) -> String {
    Database::new().query(sql).expect_err(sql).to_string()
}

#[test]
fn a_name_that_is_no_column_is_the_string_it_spells() {
    for (sql, pattern) in [
        ("SELECT * FROM read_csv(nofile)", "nofile"),
        ("SELECT * FROM read_csv(a.b)", "a.b"),
        ("SELECT * FROM read_csv(x || 'y')", "xy"),
        ("SELECT * FROM read_csv([x, y])", "x"),
        ("SELECT * FROM read_parquet(nofile)", "nofile"),
        ("SELECT * FROM read_json(nofile, columns={a: VARCHAR})", "nofile"),
    ] {
        assert_eq!(
            refused(sql),
            format!("IO Error: No files found that match the pattern \"{pattern}\""),
            "{sql}"
        );
    }
    let csv = format!("{}/../rudb-csv/testdata/parts", env!("CARGO_MANIFEST_DIR"));
    let sql = format!("SELECT count(*) FROM read_csv(\"{csv}/c1.csv\")");
    assert_eq!(answered(&sql), answered(&sql.replace('"', "'")));
}

#[test]
fn a_function_that_can_read_a_lateral_column_still_says_it_is_missing() {
    for sql in [
        "SELECT * FROM range(x)",
        "SELECT * FROM range(1, x)",
        "SELECT * FROM generate_series(x)",
        "SELECT * FROM unnest(x)",
        "SELECT * FROM json_each(x)",
        "SELECT * FROM json_tree(x)",
    ] {
        let error = refused(sql);
        assert!(error.starts_with("Binder Error: Referenced column \"x\""), "{sql}: {error}");
    }
}

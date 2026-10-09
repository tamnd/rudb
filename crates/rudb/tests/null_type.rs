//! The type of an untyped null, written `"null"`, which a column can be declared as.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn refusal(database: &Database, sql: &str) -> String {
    match database.execute(sql) {
        Ok(_) => panic!("{sql} worked and the pin refuses it"),
        Err(error) => error.to_string(),
    }
}

fn answer(database: &Database, sql: &str) -> String {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let width = result.rows().next().map_or(0, |row| row.len());
    (0..result.len())
        .map(|row| {
            (0..width).map(|column| result.text_at(row, column)).collect::<Vec<_>>().join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_value_does_not_cast_to_the_null_type_and_a_null_does() {
    let database = Database::new();
    for (sql, from) in [
        (r#"SELECT 42::BIGINT::"null""#, "BIGINT"),
        (r#"SELECT 'a'::"null""#, "VARCHAR"),
        (r#"SELECT [1, NULL]::"null"[]"#, "INTEGER"),
    ] {
        let message = refusal(&database, sql);
        let wanted = format!("Conversion Error: Unimplemented type for cast ({from} -> \"NULL\")");
        assert!(message.contains(&wanted), "{sql}: {message}");
    }
    assert_eq!(
        answer(&database, r#"SELECT TRY_CAST(42 AS "null"), typeof(TRY_CAST(42 AS "null"))"#),
        "NULL|\"NULL\""
    );
    assert_eq!(answer(&database, r#"SELECT CAST(NULL::INTEGER AS "null")"#), "NULL");
    assert_eq!(
        answer(&database, r#"SELECT TRY_CAST([1, NULL] AS "null"[]), [NULL, NULL]::"null"[]"#),
        "[NULL, NULL]|[NULL, NULL]"
    );
}

#[test]
fn a_column_of_the_null_type_takes_only_nulls() {
    let database = Database::new();
    for sql in [
        r#"CREATE TABLE t (i "null")"#,
        r#"CREATE TABLE l (i "null"[])"#,
        r#"CREATE TABLE s (i STRUCT(n "null"))"#,
        "INSERT INTO t VALUES (NULL)",
        "INSERT INTO l VALUES (NULL), ([NULL])",
        "INSERT INTO s VALUES (NULL), ({n: NULL})",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    for (sql, from) in [
        ("INSERT INTO t VALUES (42::BIGINT)", "BIGINT"),
        ("INSERT INTO l VALUES ([42::VARCHAR])", "VARCHAR"),
        ("INSERT INTO s VALUES ({n: 1.5::DOUBLE})", "DOUBLE"),
    ] {
        let message = refusal(&database, sql);
        let wanted = format!("Conversion Error: Unimplemented type for cast ({from} -> \"NULL\")");
        assert!(message.contains(&wanted), "{sql}: {message}");
    }
    assert_eq!(
        answer(
            &database,
            "SELECT (SELECT count(*) FROM t), (SELECT count(*) FROM l), (SELECT count(*) FROM s)"
        ),
        "1|2|2"
    );
}

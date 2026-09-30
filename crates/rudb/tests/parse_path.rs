//! `parse_path`, `parse_dirname`, `parse_dirpath` and `parse_filename`.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn answered(sql: &str) -> Vec<String> {
    let database = Database::new();
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join(","))
        .collect()
}

fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

#[test]
fn a_path_is_cut_and_trimmed_the_way_the_pin_does_it() {
    let cases = [
        ("SELECT parse_path('/a/b/c')", "[/, a, b, c]"),
        ("SELECT parse_path('//a//b/')", "[/, a, b]"),
        ("SELECT parse_path('')", "[]"),
        ("SELECT parse_path('/')", "[/]"),
        ("SELECT parse_path(NULL)", "NULL"),
        ("SELECT parse_path('a/b\\c', 'system')", "[a, b\\c]"),
        ("SELECT parse_path('a/b\\c', 'backslash')", "[a/b, c]"),
        ("SELECT parse_path('a/b\\c', 'BACKSLASH')", "[a, b, c]"),
        ("SELECT parse_path('a/b\\c', NULL)", "[a, b, c]"),
        ("SELECT parse_dirname('/a/b/c')", "/"),
        ("SELECT parse_dirname('a/b')", "a"),
        ("SELECT parse_dirname('/a', 'backslash')", ""),
        ("SELECT parse_dirpath('/a/b/c')", "/a/b"),
        ("SELECT parse_dirpath('/')", "/"),
        ("SELECT parse_dirpath('/abc')", ""),
        ("SELECT parse_filename('/a/b/c.txt')", "c.txt"),
        ("SELECT parse_filename('/a/b/c.txt', true)", "c"),
        ("SELECT parse_filename('c.tar.gz', true)", "c.tar"),
        ("SELECT parse_filename('.bashrc', true)", ""),
        ("SELECT parse_filename('a.b/c', true)", "c"),
        ("SELECT parse_filename('a\\b.c', 'forward_slash')", "a\\b.c"),
        ("SELECT parse_filename('a\\b.c', true, 'backslash')", "b"),
        ("SELECT parse_filename('a/b.c', 'true')", "b.c"),
        ("SELECT parse_filename('a/b.c', 'true', 'system')", "b"),
        ("SELECT parse_filename('a/b.c', NULL, NULL)", "b.c"),
        ("SELECT parse_filename(NULL, NULL, NULL)", "NULL"),
        ("SELECT parse_filename('é/ö.ü', true)", "ö"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), [expected], "{sql}");
    }
}

#[test]
fn a_column_of_options_is_read_from_its_first_row() {
    let rows = answered(
        "SELECT parse_path(s, t) FROM (VALUES ('a/b\\c', 'backslash'), ('a/b\\c', NULL), \
         ('x/y\\z', 'system')) v(s, t)",
    );
    assert_eq!(rows, ["[a/b, c]", "[a/b, c]", "[x/y, z]"]);
    let rows = answered(
        "SELECT parse_filename(s, t) FROM (VALUES ('a/b.c', true), ('a/b.c', NULL), \
         ('x/y.z', false)) v(s, t)",
    );
    assert_eq!(rows, ["b", "NULL", "y.z"]);
}

#[test]
fn what_the_pin_refuses_is_refused_in_its_words() {
    let cases = [
        ("SELECT parse_path('a/b', 1)", "parse_path(col0 VARCHAR, col1 VARCHAR) -> VARCHAR[]"),
        (
            "SELECT parse_filename('a/b', true, 1)",
            "parse_filename(col0 VARCHAR, col1 BOOLEAN, col2 VARCHAR) -> VARCHAR",
        ),
        (
            "SELECT parse_filename('p', 'system', true)",
            "No function matches the given name and argument types 'parse_filename(STRING_LITERAL, STRING_LITERAL, BOOLEAN)'",
        ),
        (
            "SELECT parse_filename('p', 'system', 'system')",
            "Could not convert string 'system' to BOOL",
        ),
        (
            "SELECT parse_filename(s, t, 'system') FROM (VALUES ('a/b.c', 'true')) v(s, t)",
            "No function matches the given name and argument types 'parse_filename(VARCHAR, VARCHAR, STRING_LITERAL)'",
        ),
        (
            "SELECT parse_dirname('a', true, 'b')",
            "No function matches the given name and argument types 'parse_dirname(STRING_LITERAL, BOOLEAN, STRING_LITERAL)'",
        ),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

#[test]
fn paths_are_joined_and_an_incompatible_one_is_refused() {
    let cases = [
        ("SELECT path_join('a/./b/../c', '..', 'd/')", "a/d/"),
        ("SELECT path_join('S3://Bucket/x', 'y')", "s3://Bucket/x/y"),
        ("SELECT path_join('/a/b', '/a/b/c')", "/a/b/c"),
        ("SELECT path_join('')", "."),
        ("SELECT path_join('a', NULL, 'c')", "NULL"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), [expected], "{sql}");
    }
    let rows = answered(
        "SELECT path_join(s, t) FROM (VALUES ('a', 'b'), (NULL, 'c'), ('/x', 'y/')) v(s, t)",
    );
    assert_eq!(rows, ["a/b", "NULL", "/x/y/"]);
    let cases = [
        (
            "SELECT path_join('/a', '/b')",
            "Path: cannot join incompatible paths: \"/b\" onto \"/a\"",
        ),
        (
            "SELECT path_join('file://host/a', 'b')",
            "Path: file:// scheme only supports localhost authority, got: host",
        ),
        ("SELECT path_join('a', 1)", "path_join(col0 VARCHAR, [VARCHAR...]) -> VARCHAR"),
        ("SELECT path_join(x := 'a', y := 'b')", "Missing value for parameter \"col0\""),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

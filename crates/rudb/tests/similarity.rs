//! The string distances and similarities, and the prefix and suffix tests.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn answered(sql: &str) -> (Vec<String>, String) {
    let database = Database::new();
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let names = result.names().to_vec();
    let row = result.rows().next().expect("one row");
    (names, row.iter().map(ToString::to_string).collect::<Vec<_>>().join(","))
}

fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

#[test]
fn the_distances_count_bytes() {
    let cases = [
        (
            "SELECT levenshtein('kitten', 'sitting'), levenshtein('héllo', 'hello'), editdist3('kitten', 'sitting'), levenshtein('', ''), levenshtein('A', 'a')",
            "3,2,3,0,1",
        ),
        (
            "SELECT damerau_levenshtein('ca', 'abc'), damerau_levenshtein('abcdef', 'badcfe'), damerau_levenshtein('a cat', 'an abct'), damerau_levenshtein('héllo', 'hlélo')",
            "2,3,3,2",
        ),
        (
            "SELECT mismatches('hé', 'hé'), hamming('abc', 'abd'), levenshtein(NULL, 'hi')",
            "0,1,NULL",
        ),
        (
            "SELECT typeof(levenshtein('a', 'b')), typeof(jaccard('a', 'b')), typeof(jaro_similarity('a', 'b')), typeof(mismatches('a', 'b'))",
            "BIGINT,DOUBLE,DOUBLE,BIGINT",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql).1, expected, "{sql}");
    }
}

#[test]
fn the_similarities_are_the_pins_to_the_last_digit() {
    let cases = [
        (
            "SELECT jaro_similarity('', ''), jaro_similarity('a', ''), jaro_winkler_similarity('', 'a'), jaro_similarity('CRATE', 'TRACE', 0.9), jaro_similarity('CRATE', 'TRACE', 0.5)",
            "0.0,0.0,0.0,0.0,0.7333333333333334",
        ),
        (
            "SELECT jaro_winkler_similarity('DIXON', 'DICKSONX'), jaro_similarity('DIXON', 'DICKSONX'), jaro_winkler_similarity('MARTHA', 'MARHTA'), jaro_similarity('MARTHA', 'MARHTA')",
            "0.8133333333333332,0.7666666666666666,0.9611111111111111,0.9444444444444445",
        ),
        (
            "SELECT jaro_similarity('héllo', 'hello'), jaro_winkler_similarity('héllo', 'hello')",
            "0.8222222222222223,0.8400000000000001",
        ),
        (
            "SELECT jaro_winkler_similarity('abcdefgh', 'abcdefgx', 0.95), jaro_winkler_similarity('abcdefgh', 'abcdefgx', 0.99), jaro_similarity('a', 'b', 2), jaro_similarity('a', 'a', -1), jaro_similarity('a', 'b', '0.5'), jaro_similarity('a', 'b', NULL)",
            "0.95,0.0,0.0,1.0,0.0,NULL",
        ),
        (
            "SELECT jaccard('héllo', 'hello'), jaccard('ab', 'ba'), jaccard('aab', 'ab'), jaccard('abc', 'xyz'), jaccard('A', 'a')",
            "0.5,1.0,1.0,0.0,0.0",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql).1, expected, "{sql}");
    }
}

#[test]
fn a_prefix_or_suffix_is_tested_under_every_spelling() {
    let (names, row) = answered(
        "SELECT 'abc' ^@ 'a', starts_with('abc', 'b'), ends_with('abc', 'c'), prefix('', ''), prefix('a', NULL), suffix('abc', 'bc'), prefix('héllo', 'hé')",
    );
    assert_eq!(row, "true,false,true,true,NULL,true,true");
    assert_eq!(names[0], "('abc' ^@ 'a')");
}

#[test]
fn a_call_the_functions_cannot_answer_is_refused_in_the_pins_words() {
    let cases = [
        (
            "SELECT jaccard('hello', '')",
            "Invalid Input Error: Jaccard Function: An argument too short!",
        ),
        (
            "SELECT mismatches('hoi', 'hallo')",
            "Invalid Input Error: Mismatch Function: Strings must be of equal length!",
        ),
        ("SELECT mismatches('', '')", "Mismatch Function: Strings must be of length > 0!"),
        (
            "SELECT damerau_levenshtein('one')",
            "No function matches the given name and argument types 'damerau_levenshtein(STRING_LITERAL)'. You might need to add explicit type casts.\n\tCandidate functions:\n\tdamerau_levenshtein(col0 VARCHAR, col1 VARCHAR) -> BIGINT",
        ),
        ("SELECT levenshtein(1, 2)", "'levenshtein(INTEGER_LITERAL, INTEGER_LITERAL)'"),
        (
            "SELECT jaro_similarity(1, 'b')",
            "\tjaro_similarity(col0 VARCHAR, col1 VARCHAR) -> DOUBLE\n\tjaro_similarity(col0 VARCHAR, col1 VARCHAR, col2 DOUBLE) -> DOUBLE",
        ),
        (
            "SELECT jaro_similarity('a', 'b', 'x')",
            "Conversion Error: Could not convert string 'x' to DOUBLE",
        ),
        (
            "SELECT 1 ^@ 2",
            "'^@(INTEGER_LITERAL, INTEGER_LITERAL)'. You might need to add explicit type casts.\n\tCandidate functions:\n\t\"^@\"(col0 VARCHAR, col1 VARCHAR) -> BOOLEAN",
        ),
        ("SELECT starts_with(1, 'a')", "'starts_with(INTEGER_LITERAL, STRING_LITERAL)'"),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

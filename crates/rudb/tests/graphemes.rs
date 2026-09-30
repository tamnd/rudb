//! `length_grapheme`, `left_grapheme`, `right_grapheme`, `substring_grapheme`, `reverse` and
//! `regexp_escape`.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn answered(sql: &str) -> Vec<String> {
    let database = Database::new();
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect()
}

fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

#[test]
fn a_cluster_counts_as_one_character() {
    let cases = [
        (
            "SELECT length_grapheme('🤦🏼‍♂️'), length('🤦🏼‍♂️'), length_grapheme('abc'), length_grapheme('')",
            "1|5|3|0",
        ),
        // The ASCII shortcut counts a carriage return and a line feed as two, and an accent
        // anywhere in the string turns the shortcut off.
        ("SELECT length_grapheme(E'a\\r\\nb'), length_grapheme(E'é\\r\\nb')", "4|3"),
        (
            "SELECT length_grapheme('🇺🇸🇫🇷🇩'), length_grapheme('क्षि'), length_grapheme('각'), length_grapheme('e' || chr(769))",
            "3|1|1|1",
        ),
        (
            "SELECT left_grapheme('🤦🏼‍♂️🤦🏽‍♀️x', 1), left_grapheme('🤦🏼‍♂️🤦🏽‍♀️x', -1), right_grapheme('🤦🏼‍♂️🤦🏽‍♀️x', 2), right_grapheme('🤦🏼‍♂️🤦🏽‍♀️x', -2)",
            "🤦🏼‍♂️|🤦🏼‍♂️🤦🏽‍♀️|🤦🏽‍♀️x|x",
        ),
        (
            "SELECT '[' || left_grapheme('abc', 0) || ']', right_grapheme('abc', 5), left_grapheme('abc', -5) = '', right_grapheme('abc', -9223372036854775808) = ''",
            "[]|abc|true|true",
        ),
        (
            "SELECT substring_grapheme('🦆🦆x🦆', 2, 2), substring_grapheme('🦆🦆x🦆', -2), substring_grapheme('🦆🦆x🦆', 0, 2), substring_grapheme('🦆🦆x🦆', 3, -2)",
            "🦆x|x🦆|🦆|🦆🦆",
        ),
        (
            "SELECT '[' || substring_grapheme(E'a\\r\\nb', 2, 1) || ']' = E'[\\r]', substring_grapheme(E'é\\r\\nb', 2, 1) = E'\\r\\n', substring_grapheme('ae' || chr(769) || 'b', 2, 1)",
            "true|true|e\u{301}",
        ),
        (
            "SELECT '[' || substring_grapheme('abc', 2, 0) || ']', substring_grapheme('abc', -10, 12), substring_grapheme('xé', 1, 1)",
            "[]|abc|x",
        ),
        // tamnd/duckdb#26: a window that counting back empties on clusters but not on bytes runs
        // to the end of the string, where `substring` answers nothing.
        (
            "SELECT '[' || substring_grapheme('🦆🦆', -5, 2) || ']', '[' || substring('🦆🦆', -5, 2) || ']', substring_grapheme('🦆🦆🦆', -9, 5), '[' || substring_grapheme('abé', -9, 7) || ']'",
            "[🦆🦆]|[]|🦆🦆🦆|[a]",
        ),
        (
            "SELECT substring_grapheme('abc', '2'), left_grapheme('abc', '2'), right_grapheme('abc', 2::UTINYINT), length_grapheme(NULL)",
            "bc|ab|bc|NULL",
        ),
        ("SELECT right_grapheme('abc', 4294967296)", "abc"),
        (
            "SELECT typeof(length_grapheme('a')), typeof(substring_grapheme(NULL, NULL)), typeof(left_grapheme(NULL, NULL))",
            "BIGINT|VARCHAR|VARCHAR",
        ),
        (
            "SELECT reverse('abc'), '[' || reverse('') || ']', reverse(NULL), typeof(reverse(NULL))",
            "cba|[]|NULL|VARCHAR",
        ),
        (
            "SELECT reverse('héllo'), reverse('e' || chr(769) || 'x'), reverse('🇺🇸🇫🇷')",
            "olléh|xe\u{301}|🇫🇷🇺🇸",
        ),
        (
            "SELECT reverse('🤦🏼‍♂️x🇺🇸'), reverse(E'a\\r\\nb') = E'b\\n\\ra', reverse(E'é\\r\\nb') = E'b\\r\\né'",
            "🇺🇸x🤦🏼‍♂️|true|true",
        ),
        ("SELECT reverse('ab' || chr(0) || 'c') = 'c' || chr(0) || 'ba'", "true"),
        (
            "SELECT regexp_escape('a.b*c?d+e(f)[g]{h}|i^j$k\\l'), '[' || regexp_escape('') || ']', regexp_escape(NULL)",
            "a\\.b\\*c\\?d\\+e\\(f\\)\\[g\\]\\{h\\}\\|i\\^j\\$k\\\\l|[]|NULL",
        ),
        (
            "SELECT regexp_escape('a-b c/d#e&f~g'), regexp_escape('é\\n'), regexp_escape(E'a\\tb') = E'a\\\\\\tb'",
            "a\\-b\\ c\\/d\\#e\\&f\\~g|é\\\\n|true",
        ),
        ("SELECT regexp_escape('a' || chr(0) || 'b')", "a\\x00b"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), [expected], "{sql}");
    }
}

#[test]
fn a_column_is_cut_row_by_row() {
    let rows = answered(
        "SELECT length_grapheme(s), reverse(s), left_grapheme(s, 1) FROM (VALUES ('abc'), (NULL), ('🦆x')) v(s)",
    );
    assert_eq!(rows, ["3|cba|a", "NULL|NULL|NULL", "2|x🦆|🦆"]);
}

#[test]
fn what_the_pin_refuses_is_refused_in_its_words() {
    let cases = [
        (
            "SELECT substring_grapheme('abc', 4294967296)",
            "Out of Range Error: Substring offset outside of supported range (> 4294967295)",
        ),
        (
            "SELECT substring_grapheme('abc', 1, -4294967297)",
            "Substring length outside of supported range (< -4294967296)",
        ),
        (
            "SELECT substring_grapheme('abc', -4294967297)",
            "Substring offset outside of supported range (< -4294967296)",
        ),
        (
            "SELECT substring_grapheme('abc', 1, 4294967296)",
            "Substring length outside of supported range (> 4294967295)",
        ),
        (
            "SELECT left_grapheme('abc', 4294967296)",
            "Substring length outside of supported range (> 4294967295)",
        ),
        (
            "SELECT substring_grapheme(1, 2)",
            "'substring_grapheme(INTEGER_LITERAL, INTEGER_LITERAL)'. You might need to add explicit type casts.\n\tCandidate functions:\n\tsubstring_grapheme(col0 VARCHAR, col1 BIGINT, col2 BIGINT) -> VARCHAR\n\tsubstring_grapheme(col0 VARCHAR, col1 BIGINT) -> VARCHAR",
        ),
        (
            "SELECT substring_grapheme('abc', 1.5)",
            "'substring_grapheme(STRING_LITERAL, DECIMAL(2,1))'",
        ),
        (
            "SELECT substring_grapheme('abc', 2::UBIGINT)",
            "'substring_grapheme(STRING_LITERAL, UBIGINT)'",
        ),
        (
            "SELECT substring_grapheme('abc', 2::HUGEINT)",
            "'substring_grapheme(STRING_LITERAL, HUGEINT)'",
        ),
        ("SELECT left_grapheme(1, 1)", "left_grapheme(col0 VARCHAR, col1 BIGINT) -> VARCHAR"),
        ("SELECT right_grapheme('a')", "right_grapheme(col0 VARCHAR, col1 BIGINT) -> VARCHAR"),
        ("SELECT length_grapheme(1)", "length_grapheme(col0 VARCHAR) -> BIGINT"),
        ("SELECT length_grapheme('abc'::BLOB)", "'length_grapheme(BLOB)'"),
        ("SELECT reverse(123)", "'reverse(INTEGER_LITERAL)'"),
        ("SELECT reverse('ab'::BLOB)", "reverse(col0 VARCHAR) -> VARCHAR"),
        ("SELECT reverse([1,2,3])", "'reverse(INTEGER[])'"),
        ("SELECT reverse()", "'reverse()'"),
        ("SELECT regexp_escape(1)", "regexp_escape(col0 VARCHAR) -> VARCHAR"),
        ("SELECT regexp_escape('a', 'b')", "'regexp_escape(STRING_LITERAL, STRING_LITERAL)'"),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

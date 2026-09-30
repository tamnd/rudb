//! `regexp_extract_all`, every match of a pattern as a list of strings, or as a list of structs
//! when it is given a list of names for the groups.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> Vec<String> {
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
fn every_match_is_walked_the_way_the_pin_walks_them() {
    let database = Database::new();
    let cases = [
        (r"SELECT regexp_extract_all('1a 2b 14m', '(\d+)', 1)", "[1, 2, 14]"),
        (r"SELECT regexp_extract_all('1a 2b 14m', '(\d+)([a-z]+)', 2)", "[a, b, m]"),
        (
            r"SELECT regexp_extract_all('1a 2b 14m', '(\\d+)?', 1)",
            "[NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL]",
        ),
        ("SELECT regexp_extract_all('aabca', 'a*')", "[aa, '', '', a, '']"),
        ("SELECT regexp_extract_all('baac', 'a*')", "['', aa, '', '']"),
        ("SELECT regexp_extract_all('щццф', 'ц*')", "['', цц, '', '']"),
        ("SELECT regexp_extract_all('щцф', '.{2}')", "[щц]"),
        (
            "SELECT regexp_extract_all('this_is__a___Test', '(.*?)(?:_|$)')",
            "[this_, is_, _, a_, _, _, Test, '']",
        ),
        ("SELECT regexp_extract_all('aaa', '^a')", "[a]"),
        ("SELECT regexp_extract_all('', '')", "['']"),
        ("SELECT regexp_extract_all('', 'abc')", "[]"),
        ("SELECT regexp_extract_all('abc', '.', -1)", "[]"),
        ("SELECT regexp_extract_all('foobarbaz', '(BA[R|Z])', 1, 'i')", "[bar, baz]"),
        ("SELECT regexp_extract_all('abc', '(.)', 1::TINYINT)", "[a, b, c]"),
        ("SELECT regexp_extract_all('abc', '.', NULL)", "NULL"),
        ("SELECT regexp_extract_all(NULL, '.', 0)", "NULL"),
        // The pin finds an empty match past where it searched from twice, which is tamnd/duckdb#23.
        ("SELECT regexp_extract_all('ab', '$')", "['', '']"),
        (r"SELECT regexp_extract_all('ab cd', '\b')", "['', '', '', '', '', '']"),
        ("SELECT regexp_extract_all('ab', 'b*$')", "[b, '']"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
    let rows = answered(
        &database,
        "SELECT regexp_extract_all('aaaaaaaa', pattern) FROM (VALUES (NULL), ('(a)(a)(a)'), ('()'), ('(a)(b)?(a)')) t(pattern)",
    );
    assert_eq!(
        rows,
        ["NULL", "[aaa, aaa]", "['', '', '', '', '', '', '', '', '']", "[aa, aa, aa, aa]"]
    );
    let rows = answered(
        &database,
        r"SELECT regexp_extract_all(s, '([a-z])\d', g) FROM (VALUES ('a1b2', 1), ('c3', 0), (NULL, 1), ('d4', NULL)) t(s, g)",
    );
    assert_eq!(rows, ["[a, b]", "[c3]", "NULL", "NULL"]);
}

#[test]
fn a_list_of_names_answers_a_struct_for_each_match() {
    let database = Database::new();
    let cases = [
        (
            r"SELECT regexp_extract_all('Peter:33 Paul:14', '(\w+):(\d+)', ['name','num'])",
            "[{'name': Peter, 'num': 33}, {'name': Paul, 'num': 14}]",
        ),
        (
            r"SELECT regexp_extract_all('a1 b2 c', '(\w)(\d)?', ['c','d'])",
            "[{'c': a, 'd': 1}, {'c': b, 'd': 2}, {'c': c, 'd': NULL}]",
        ),
        (
            "SELECT regexp_extract_all('Aa aA', '(a)', ['lower'], 'i')",
            "[{'lower': A}, {'lower': a}, {'lower': a}, {'lower': A}]",
        ),
        (
            "SELECT regexp_extract_all('hi', '(h|i)?', ['ch'])",
            "[{'ch': h}, {'ch': i}, {'ch': NULL}]",
        ),
        ("SELECT regexp_extract_all('bb', '(a*)b', ['a'])", "[{'a': ''}, {'a': ''}]"),
        ("SELECT regexp_extract_all('abc', '(x)(y)', ['x','y'])", "[]"),
        ("SELECT regexp_extract_all(NULL, '(a)', ['g'])", "NULL"),
        ("SELECT regexp_extract_all('abc', '(b)', ['x'])[1].x", "b"),
        ("SELECT typeof(regexp_extract_all('abc', '(a)', ['g1']))", "STRUCT(g1 VARCHAR)[]"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(&database, sql), [expected], "{sql}");
    }
    let rows = answered(
        &database,
        "SELECT regexp_extract_all(s, '(h|i)?', ['ch']) FROM (VALUES ('hi'), ('h'), ('')) t(s)",
    );
    assert_eq!(
        rows,
        ["[{'ch': h}, {'ch': i}, {'ch': NULL}]", "[{'ch': h}, {'ch': NULL}]", "[{'ch': NULL}]"]
    );
}

#[test]
fn what_the_pin_refuses_is_refused_in_its_words() {
    let cases = [
        (
            "SELECT regexp_extract_all('hello', '.', 2)",
            "Invalid Input Error: Pattern has 0 groups. Cannot access group 2",
        ),
        (
            "SELECT regexp_extract_all('hello', '(.)', 10)",
            "Pattern has 1 groups. Cannot access group 10",
        ),
        ("SELECT regexp_extract_all('abc', '(')", "Invalid Input Error: missing ): ("),
        ("SELECT regexp_extract_all('abc', '.', 0, 'q')", "Unrecognized Regex option q"),
        (
            "SELECT regexp_extract_all('abc', '.', 0, 'g')",
            "Option 'g' (global replace) is only valid for regexp_replace",
        ),
        (
            "SELECT regexp_extract_all('abc', '.', 0, NULL)",
            "Invalid Input Error: Regex options field must not be NULL",
        ),
        (
            "SELECT regexp_extract_all('abc', '.', 0, o) FROM (VALUES ('i')) t(o)",
            "Binder Error: The \"options\" argument in function \"regexp_extract_all\" must be a constant expression",
        ),
        (
            "SELECT regexp_extract_all('abc', '.', 1.5)",
            "No function matches the given name and argument types 'regexp_extract_all(STRING_LITERAL, STRING_LITERAL, DECIMAL(2,1))'",
        ),
        (
            "SELECT regexp_extract_all('abc', '.', 1::BIGINT)",
            "No function matches the given name and argument types 'regexp_extract_all(STRING_LITERAL, STRING_LITERAL, BIGINT)'",
        ),
        (
            "SELECT regexp_extract_all(1234, '\\d')",
            "No function matches the given name and argument types 'regexp_extract_all(INTEGER_LITERAL, STRING_LITERAL)'",
        ),
        (
            "SELECT regexp_extract_all('abc', '(a)', ['g1', NULL::VARCHAR])",
            "Binder Error: NULL group name in regexp_extract_all",
        ),
        (
            "SELECT regexp_extract_all('abc', '(a)', ['dup','dup'])",
            "Binder Error: Duplicate group name 'dup' in regexp_extract_all",
        ),
        (
            "SELECT regexp_extract_all('abc', '(', ['g1'])",
            "Binder Error: Pattern failed to parse: missing ): (",
        ),
        (
            "SELECT regexp_extract_all('abc', '(a)', ['g1','g2'])",
            "Binder Error: Not enough capturing groups (1) for provided names (2)",
        ),
        (
            "SELECT regexp_extract_all('abc', 'b', ['x'])",
            "Not enough capturing groups (0) for provided names (1)",
        ),
        (
            "SELECT regexp_extract_all('abc', '(a)', [])",
            "Binder Error: Group name list must be non-empty",
        ),
        (
            "SELECT regexp_extract_all('abc', '(a)(b)', []::VARCHAR[])",
            "Binder Error: Group name list must be non-empty",
        ),
        (
            "SELECT regexp_extract_all('abc', '(a)', NULL::VARCHAR[])",
            "Binder Error: Group specification must be a non-NULL LIST",
        ),
        (
            "SELECT regexp_extract_all('abc', NULL, ['g'])",
            "Binder Error: \"regexp_extract_all\" with LIST requires a constant pattern",
        ),
        (
            "SELECT regexp_extract_all(s, p, ['g']) FROM (VALUES ('abc', '(.)')) t(s, p)",
            "\"regexp_extract_all\" with LIST requires a constant pattern",
        ),
        (
            "WITH params(name_list) AS (SELECT ['g1','g2']) SELECT regexp_extract_all('abc', '(a)(b)', name_list) FROM params",
            "Binder Error: The \"name_list\" argument in function \"regexp_extract_all\" must be a constant expression",
        ),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

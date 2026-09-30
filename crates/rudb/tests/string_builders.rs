//! `concat_ws`, `repeat`, `lpad`, `rpad`, `ascii`, `unicode`, `translate`, `url_encode`,
//! `url_decode`, `bar` and `to_base`.
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
fn a_string_is_built_the_way_the_pin_builds_it() {
    let cases = [
        ("SELECT concat_ws(',', 'a', NULL, 'b', '', 'c')", "a,b,,c"),
        ("SELECT concat_ws(NULL, 'a')", "NULL"),
        ("SELECT concat_ws(',', NULL, NULL)", ""),
        ("SELECT concat_ws(',', 1, 2.5, DATE '2024-01-01', true)", "1,2.5,2024-01-01,true"),
        ("SELECT concat_ws('-', [1,2,NULL,3], 'x', NULL::INT[], ['y'])", "1-2-3-x-y"),
        ("SELECT concat_ws(',', [1.5, 2])", "1.5,2.0"),
        ("SELECT concat_ws(',', {'a': [1]})", "{'a': [1]}"),
        ("SELECT typeof(concat_ws(',', 1))", "VARCHAR"),
        ("SELECT repeat('ab', 3), repeat('ab', 0), repeat('ab', -1)", "ababab||"),
        (
            "SELECT repeat([1,2], 2), repeat([]::INT[], 3), repeat(NULL::INT[], 2)",
            "[1, 2, 1, 2]|[]|NULL",
        ),
        ("SELECT repeat([[1],[2]], 2)", "[[1], [2], [1], [2]]"),
        ("SELECT typeof(repeat('a', 2)), typeof(repeat('a'::BLOB, 2))", "VARCHAR|BLOB"),
        ("SELECT typeof(repeat(NULL, 2))", "BLOB"),
        ("SELECT repeat('a', 3::UINTEGER), repeat('a', '3'), repeat(['a'], '2')", "aaa|aaa|[a, a]"),
        (
            "SELECT lpad('abc', 5, 'xy'), lpad('abc', 2, 'x'), lpad('abc', -1, 'x'), lpad('héllo', 7, 'ö')",
            "xyabc|ab||ööhéllo",
        ),
        ("SELECT rpad('abc', 8, 'xyz'), rpad('', 3, 'ab'), lpad('abc', 2, '')", "abcxyzxy|aba|ab"),
        ("SELECT lpad('a', 3::UTINYINT, 'x'), lpad('a', '3', 'x')", "xxa|xxa"),
        ("SELECT typeof(lpad(NULL, NULL, NULL))", "VARCHAR"),
        (
            "SELECT ascii('A'), ascii('é'), ascii(''), ascii('ab'), unicode('é'), unicode(''), ord('€')",
            "65|233|0|97|233|-1|8364",
        ),
        (
            "SELECT translate('hello', 'el', 'ip'), translate('hello', 'elo', 'x'), translate('aaa', 'aa', 'bc')",
            "hippo|hx|bbb",
        ),
        ("SELECT translate('abcabc', 'abc', 'ab'), translate('ööx', 'öx', 'oy')", "abab|ooy"),
        ("SELECT url_encode('a b/c?d=é&~_.-')", "a%20b%2Fc%3Fd%3D%C3%A9%26~_.-"),
        (
            "SELECT url_decode('a%20b%2Fc+d%C3%A9'), url_decode('%4'), url_decode('%zz')",
            "a b/c+dé|%4|%zz",
        ),
        ("SELECT url_decode('%e2%82%ac'), url_decode('%%41'), url_decode('abc%2')", "€|%A|abc%2"),
        ("SELECT bar(3.3, 0, 10, 7), bar(5, 0, 10, 2.5)", "██▎    |█▎"),
        ("SELECT bar(5, 0, 0, 10), bar(0.5, 0, 1, 3.99)", "██████████|█▉ "),
        ("SELECT bar(0.999, 0, 1, 1), bar(0.125, 0, 1, 1), bar('inf'::DOUBLE, 0, 1, 2)", "▉|▏|██"),
        ("SELECT bar(5, 0, 10, '3'), length(bar(5, 0, 10))", "█▌ |80"),
        ("SELECT bar(5::BIGINT, 0::DECIMAL(3,1), 10::HUGEINT, 4)", "██  "),
        (
            "SELECT to_base(255, 16), to_base(255, 2, 12), to_base(0, 10), to_base(9223372036854775807, 36)",
            "FF|000011111111|0|1Y2P0IJ32E8E7",
        ),
        ("SELECT to_base('10', 2)", "1010"),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), [expected], "{sql}");
    }
}

#[test]
fn a_column_is_built_row_by_row() {
    let rows = answered("SELECT concat_ws(s, 'a', 'b') FROM (VALUES (','), (NULL), ('-')) v(s)");
    assert_eq!(rows, ["a,b", "NULL", "a-b"]);
    let rows = answered("SELECT repeat(x, 2) FROM (VALUES ([1, NULL]), (NULL), ([])) v(x)");
    assert_eq!(rows, ["[1, NULL, 1, NULL]", "NULL", "[]"]);
    let rows = answered("SELECT lpad(x::VARCHAR, 3, '0') FROM range(9, 12) t(x)");
    assert_eq!(rows, ["009", "010", "011"]);
}

#[test]
fn what_the_pin_refuses_is_refused_in_its_words() {
    let cases = [
        ("SELECT concat_ws('-', [[1]])", "Binder Error: concat_ws() does not support nested lists"),
        ("SELECT concat_ws(',', NULL::INT[][])", "concat_ws() does not support nested lists"),
        ("SELECT concat_ws(',')", "concat_ws(col0 VARCHAR, col1 ANY, [ANY...]) -> VARCHAR"),
        ("SELECT concat_ws(1, 2)", "'concat_ws(INTEGER_LITERAL, INTEGER_LITERAL)'"),
        ("SELECT repeat('a', 2.7)", "repeat(col0 T[], col1 BIGINT) -> T[]"),
        ("SELECT repeat('a', 3::UBIGINT)", "'repeat(STRING_LITERAL, UBIGINT)'"),
        ("SELECT repeat(1, 2)", "'repeat(INTEGER_LITERAL, INTEGER_LITERAL)'"),
        (
            "SELECT repeat('ab', 9223372036854775807)",
            "Out of Range Error: Cannot create a string of size: '18446744073709551614', the maximum supported string size is: '4294967295'",
        ),
        (
            "SELECT repeat('ab', 2147483648)",
            "Cannot create a string of size: '4294967296', the maximum supported string size is: '4294967295'",
        ),
        ("SELECT lpad('abc', 5, '')", "Invalid Input Error: Insufficient padding in LPAD."),
        ("SELECT rpad('abc', 5, '')", "Invalid Input Error: Insufficient padding in RPAD."),
        ("SELECT lpad('abc', 5)", "lpad(col0 VARCHAR, col1 INTEGER, col2 VARCHAR) -> VARCHAR"),
        ("SELECT lpad(123, 5, '0')", "'lpad(INTEGER_LITERAL, INTEGER_LITERAL, STRING_LITERAL)'"),
        ("SELECT lpad('a', 3::BIGINT, 'x')", "'lpad(STRING_LITERAL, BIGINT, STRING_LITERAL)'"),
        ("SELECT lpad('a', 3::UINTEGER, 'x')", "'lpad(STRING_LITERAL, UINTEGER, STRING_LITERAL)'"),
        ("SELECT lpad('a', 'z', 'x')", "Could not convert string 'z' to INT32"),
        ("SELECT ascii(65)", "ascii(col0 VARCHAR) -> INTEGER"),
        ("SELECT ascii('a'::BLOB)", "'ascii(BLOB)'"),
        ("SELECT ord(1)", "ord(col0 VARCHAR) -> INTEGER"),
        (
            "SELECT translate(1, 2, 3)",
            "translate(col0 VARCHAR, col1 VARCHAR, col2 VARCHAR) -> VARCHAR",
        ),
        ("SELECT url_encode(1)", "url_encode(col0 VARCHAR) -> VARCHAR"),
        (
            "SELECT url_decode('%FF')",
            "Invalid Input Error: Failed to decode string \"%FF\" using URL decoding - decoded value is invalid UTF8",
        ),
        ("SELECT bar(1)", "bar(col0 DOUBLE, col1 DOUBLE, col2 DOUBLE) -> VARCHAR"),
        ("SELECT bar(5, 0, 10, 0.5)", "Out of Range Error: Max bar width must be >= 1"),
        ("SELECT bar(5, 0, 10, 1001)", "Max bar width must be <= 1000"),
        ("SELECT bar(5, 0, 10, 'inf'::DOUBLE)", "Max bar width must not be NaN or infinity"),
        ("SELECT bar(1e308, -1e308, 'inf'::DOUBLE, 3)", "Bar width must not be NaN or infinity"),
        ("SELECT to_base(-1, 10)", "'to_base' number must be greater than or equal to 0"),
        ("SELECT to_base(10, 1)", "'to_base' radix must be between 2 and 36"),
        ("SELECT to_base(10, 10, 65)", "'to_base' min_length must be between 0 and 64"),
        ("SELECT to_base(10, 10, -1)", "'to_base' min_length must be between 0 and 64"),
        ("SELECT to_base(10.5, 10)", "to_base(col0 BIGINT, col1 INTEGER, col2 INTEGER) -> VARCHAR"),
        ("SELECT to_base(10::HUGEINT, 2)", "'to_base(HUGEINT, INTEGER_LITERAL)'"),
        ("SELECT to_base(10, 2::BIGINT)", "'to_base(INTEGER_LITERAL, BIGINT)'"),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

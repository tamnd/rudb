//! `md5`, `md5_number`, `sha1`, `sha256`, `hex`, `bin`, `unhex`, `unbin`, `encode`, `decode`,
//! `base64` and `from_base64`, and the other names each goes by.
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
fn bytes_are_hashed_and_written_the_way_the_pin_does() {
    let cases = [
        (
            "SELECT md5('abc'), md5(''), md5('abc'::BLOB)",
            "900150983cd24fb0d6963f7d28e17f72|d41d8cd98f00b204e9800998ecf8427e|900150983cd24fb0d6963f7d28e17f72",
        ),
        (
            "SELECT sha1('abc'), sha256('abc'::BLOB)",
            "a9993e364706816aba3e25717850c26c9cd0d89d|ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            "SELECT md5_number('abc'), md5_number(''), typeof(md5_number('a')), typeof(md5(NULL))",
            "152195979970564155685860391459828531600|167830467844043968176572005485231480276|UHUGEINT|VARCHAR",
        ),
        (
            "SELECT hex('abc'), hex('abc'::BLOB), hex(255), hex(-1::INTEGER), hex(0), hex(1::TINYINT)",
            "616263|616263|FF|FFFFFFFFFFFFFFFF|0|1",
        ),
        (
            "SELECT hex(18446744073709551615::UBIGINT), hex(-2::HUGEINT), hex(255::UHUGEINT), to_hex(16)",
            "FFFFFFFFFFFFFFFF|FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFE|FF|10",
        ),
        (
            "SELECT hex(1.5::FLOAT), hex(-2.5::DOUBLE), hex(-0.7::DOUBLE), hex(1e30::DOUBLE), hex(16777217::FLOAT)",
            "80000101|7FFFFEFD|7FFFFEFF|80000D0C9F2C9CD04675000000000000|80000401000000",
        ),
        (
            "SELECT bin('a'), bin(5), bin(0), bin(-1::TINYINT) = repeat('1', 64), to_binary(3.0::DOUBLE)",
            "01100001|101|0|true|10000000000000000000000100000011",
        ),
        (
            "SELECT bin(255::UTINYINT), bin(-1::HUGEINT) = repeat('1', 128), bin(2::UHUGEINT)",
            "11111111|true|10",
        ),
        (
            "SELECT unhex('616263'), unhex('abc'), from_hex('41'), unhex(''), typeof(unhex('41'))",
            "abc|\\x0A\\xBC|A||BLOB",
        ),
        (
            "SELECT unbin('01000001'), unbin('111111111'), from_binary('10'), unbin('')",
            "A|\\x01\\xFF|\\x02|",
        ),
        (
            "SELECT encode('héllo'), decode(encode('héllo')), decode('abc'), typeof(encode('a'))",
            "h\\xC3\\xA9llo|héllo|abc|BLOB",
        ),
        (
            "SELECT decode('a\\xFFb'::BLOB, 'replace'), decode('a\\xC3b'::BLOB, 'replace'), decode('\\xF0\\x9F\\x98'::BLOB, 'Replace')",
            "a?b|a??|???",
        ),
        (
            "SELECT decode('a\\xFFb'::BLOB, 'ignore'), decode('a\\xC3b'::BLOB, 'IGNORE'), decode('\\xE2\\x82x\\xE2\\x82\\xAC'::BLOB, 'ignore')",
            "ab|ab|x€",
        ),
        (
            "SELECT decode('\\xE2\\x82x\\xE2\\x82\\xAC'::BLOB, 'replace'), decode('\\xC0\\x80'::BLOB, 'replace'), decode('ok'::BLOB, 'bad')",
            "???€|??|ok",
        ),
        (
            "SELECT decode('ab'::BLOB, NULL), decode(NULL::BLOB, 'x'), typeof(decode(NULL))",
            "NULL|NULL|VARCHAR",
        ),
        (
            "SELECT base64('abc'), base64(''::BLOB), to_base64('a'::BLOB), base64('\\x00\\xFF'::BLOB)",
            "YWJj||YQ==|AP8=",
        ),
        (
            "SELECT from_base64('YWJj'), from_base64('YWI='), from_base64('YQ=='), from_base64(''), from_base64('YW=j')",
            "abc|ab|a||a",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), [expected], "{sql}");
    }
}

#[test]
fn a_column_is_written_row_by_row() {
    let rows = answered("SELECT hex(x), md5(x) FROM (VALUES ('a'), (NULL), ('')) v(x)");
    assert_eq!(
        rows,
        ["61|0cc175b9c0f1b6a831c399e269772661", "NULL|NULL", "|d41d8cd98f00b204e9800998ecf8427e"]
    );
    assert_eq!(answered("SELECT hex(x) FROM range(14, 18) t(x)"), ["E", "F", "10", "11"]);
}

#[test]
fn what_the_pin_refuses_is_refused_in_its_words() {
    let not_utf8 = "Conversion Error: Failure in decode: could not convert blob to UTF8 string, the blob \
                    contained invalid UTF8 characters. \nUse try(decode(BLOB)) to return NULL and \
                    continue instead of returning an error. Specify decode(BLOB, 'replace') to \
                    replace invalid characters with '?'. Specify decode(BLOB, 'ignore') to remove \
                    invalid characters when encountered.";
    let cases = [
        ("SELECT md5(1)", "'md5(INTEGER_LITERAL)'"),
        ("SELECT md5(1)", "md5(col0 VARCHAR) -> VARCHAR\n\tmd5(col0 BLOB) -> VARCHAR"),
        ("SELECT md5_number(1)", "md5_number(col0 BLOB) -> UHUGEINT"),
        ("SELECT sha256(1)", "sha256(col0 VARCHAR) -> VARCHAR"),
        ("SELECT hex(1.5)", "'hex(DECIMAL(2,1))'"),
        (
            "SELECT hex(1.5)",
            "hex(col0 VARCHAR) -> VARCHAR\n\thex(col0 BIGNUM) -> VARCHAR\n\thex(col0 BLOB) -> VARCHAR\n\thex(col0 BIGINT) -> VARCHAR\n\thex(col0 UBIGINT) -> VARCHAR\n\thex(col0 HUGEINT) -> VARCHAR\n\thex(col0 UHUGEINT) -> VARCHAR",
        ),
        ("SELECT to_hex(true)", "'to_hex(BOOLEAN)'"),
        ("SELECT to_hex(true)", "to_hex(col0 BIGNUM) -> VARCHAR"),
        ("SELECT bin('a'::BLOB)", "'bin(BLOB)'"),
        (
            "SELECT bin('a'::BLOB)",
            "bin(col0 VARCHAR) -> VARCHAR\n\tbin(col0 BIGNUM) -> VARCHAR\n\tbin(col0 UBIGINT) -> VARCHAR\n\tbin(col0 BIGINT) -> VARCHAR\n\tbin(col0 HUGEINT) -> VARCHAR\n\tbin(col0 UHUGEINT) -> VARCHAR",
        ),
        ("SELECT to_binary(DATE '2024-01-01')", "'to_binary(DATE)'"),
        ("SELECT unhex(1)", "unhex(col0 VARCHAR) -> BLOB"),
        ("SELECT from_binary(1)", "from_binary(col0 VARCHAR) -> BLOB"),
        ("SELECT unhex('0g')", "Invalid Input Error: Invalid input for hex digit: g"),
        ("SELECT unbin('12')", "Invalid Input Error: Invalid input for binary digit: 2"),
        ("SELECT encode('a'::BLOB)", "encode(col0 VARCHAR) -> BLOB"),
        (
            "SELECT decode('abc'::VARCHAR)",
            "decode(col0 BLOB) -> VARCHAR\n\tdecode(col0 BLOB, col1 VARCHAR) -> VARCHAR",
        ),
        ("SELECT decode('\\xFF'::BLOB)", not_utf8),
        ("SELECT decode('a\\xFFb'::BLOB, 'strict')", not_utf8),
        (
            "SELECT decode('\\xFF'::BLOB, 'bad')",
            "Conversion Error: decode error behavior specifier \"bad\" not recognized",
        ),
        ("SELECT base64('abc'::VARCHAR)", "base64(col0 BLOB) -> VARCHAR"),
        ("SELECT to_base64(1)", "to_base64(col0 BLOB) -> VARCHAR"),
        ("SELECT from_base64(1)", "from_base64(col0 VARCHAR) -> BLOB"),
        (
            "SELECT from_base64('YWJ')",
            "Conversion Error: Could not decode string \"YWJ\" as base64: length must be a multiple of 4",
        ),
        (
            "SELECT from_base64('YW J')",
            "Could not decode string \"YW J\" as base64: invalid byte value '32' at position 2",
        ),
        ("SELECT from_base64('=WJj')", "invalid byte value '61' at position 0"),
        ("SELECT from_base64('YW=jYWJj')", "invalid byte value '61' at position 2"),
        (
            "SELECT hex('inf'::DOUBLE)",
            "Conversion Error: Type DOUBLE with value inf can't be cast to the destination type VARCHAR",
        ),
        ("SELECT bin('-inf'::DOUBLE)", "Type DOUBLE with value -inf can't be cast"),
        ("SELECT hex('nan'::DOUBLE)", "Type DOUBLE with value nan can't be cast"),
    ];
    for (sql, expected) in cases {
        let said = refused(sql);
        assert!(said.contains(expected), "{sql}: {said}");
    }
}

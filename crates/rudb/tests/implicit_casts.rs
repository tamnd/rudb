//! `can_cast_implicitly`, which answers from the two types whether the pin casts one to the other
//! without being asked to.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn answer(sql: &str) -> String {
    let database = Database::new();
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let width = result.rows().next().map_or(0, |row| row.len());
    (0..result.len())
        .map(|row| {
            (0..width).map(|column| result.text_at(row, column)).collect::<Vec<_>>().join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The types the matrix is over, in the order its rows and columns are.
const TYPES: &[&str] = &[
    "BOOLEAN",
    "TINYINT",
    "SMALLINT",
    "INTEGER",
    "BIGINT",
    "HUGEINT",
    "UTINYINT",
    "USMALLINT",
    "UINTEGER",
    "UBIGINT",
    "UHUGEINT",
    "FLOAT",
    "DOUBLE",
    "DECIMAL(4,1)",
    "DECIMAL(18,3)",
    "DECIMAL(38,0)",
    "VARCHAR",
    "BLOB",
    "BIT",
    "UUID",
    "DATE",
    "TIME",
    "TIMETZ",
    "TIMESTAMP",
    "TIMESTAMP_S",
    "TIMESTAMP_MS",
    "TIMESTAMP_NS",
    "TIMESTAMPTZ",
    "INTERVAL",
    "INTEGER[]",
    "BIGINT[]",
    "VARCHAR[]",
    "INTEGER[2]",
    "BIGINT[2]",
    "BIGINT[3]",
    "STRUCT(a INTEGER)",
    "STRUCT(a BIGINT)",
    "STRUCT(b INTEGER)",
    "STRUCT(a INTEGER, b INTEGER)",
    "MAP(INTEGER, INTEGER)",
    "MAP(VARCHAR, VARCHAR)",
    "UNION(a INTEGER, b VARCHAR)",
    "UNION(a BIGINT)",
];

/// The pin's answer for each type cast to each type, one row per source type and a 1 where it casts.
const MATRIX: &[&str] = &[
    "1000000000000000000000000000000000000000000",
    "0111110000011111000000000000000000000000011",
    "0011110000011111000000000000000000000000011",
    "0001110000011111000000000000000000000000011",
    "0000110000011111000000000000000000000000001",
    "0000010000011111000000000000000000000000000",
    "0011111111111111000000000000000000000000011",
    "0001110111111111000000000000000000000000011",
    "0000110011111111000000000000000000000000001",
    "0000010001111111000000000000000000000000000",
    "0000000000111111000000000000000000000000000",
    "0000000000011000000000000000000000000000000",
    "0000000000001000000000000000000000000000000",
    "0000000000011111000000000000000000000000000",
    "0000000000011111000000000000000000000000000",
    "0000000000011111000000000000000000000000000",
    "0000000000000000100000000000000000000000010",
    "0000000000000000010000000000000000000000000",
    "0000000000000000001000000000000000000000000",
    "0000000000000000000100000000000000000000000",
    "0000000000000000000010011111000000000000000",
    "0000000000000000000001000000000000000000000",
    "0000000000000000000000100000000000000000000",
    "0000000000000000000000010011000000000000000",
    "0000000000000000000000011110000000000000000",
    "0000000000000000000000010110000000000000000",
    "0000000000000000000000010010000000000000000",
    "0000000000000000000000000001000000000000000",
    "0000000000000000000000000000100000000000000",
    "0000000000000000000000000000011011100000000",
    "0000000000000000000000000000001001100000000",
    "0000000000000000000000000000000100000000000",
    "0000000000000000000000000000011011000000000",
    "0000000000000000000000000000001001000000000",
    "0000000000000000000000000000001000100000000",
    "0000000000000000000000000000000000011000000",
    "0000000000000000000000000000000000001000000",
    "0000000000000000000000000000000000000100000",
    "0000000000000000000000000000000000000010000",
    "0000000000000000000000000000000000000001100",
    "0000000000000000000000000000000000000001100",
    "0000000000000000000000000000000000000000010",
    "0000000000000000000000000000000000000000011",
];

#[test]
fn every_type_casts_to_the_types_the_pin_casts_it_to() {
    for (source, expected) in TYPES.iter().zip(MATRIX) {
        let cells: Vec<String> = TYPES
            .iter()
            .map(|target| format!("can_cast_implicitly(NULL::{source}, NULL::{target})::INT"))
            .collect();
        let sql = format!("SELECT concat_ws('', {})", cells.join(", "));
        assert_eq!(answer(&sql), *expected, "casting from {source}");
    }
}

#[test]
fn a_literal_is_its_default_type_and_a_null_casts_to_anything() {
    assert_eq!(
        answer(
            "SELECT can_cast_implicitly(1, 2::BIGINT), can_cast_implicitly(1::BIGINT, 2), can_cast_implicitly('a', 1), can_cast_implicitly(1, 'a'), can_cast_implicitly(NULL, 1), can_cast_implicitly(1, NULL)"
        ),
        "true|false|false|false|true|false"
    );
    assert_eq!(
        answer(
            "SELECT typeof(can_cast_implicitly(1, 2)), can_cast_implicitly(x, 1::BIGINT) FROM (VALUES (1), (NULL)) v(x)"
        ),
        "BOOLEAN|true\nBOOLEAN|true"
    );
}

#[test]
fn an_enum_becomes_a_string_and_unions_and_structs_go_member_by_member() {
    assert_eq!(
        answer(
            "SELECT can_cast_implicitly(NULL::ENUM('x','y'), NULL::VARCHAR), can_cast_implicitly(NULL::VARCHAR, NULL::ENUM('x','y')), can_cast_implicitly(NULL::ENUM('x','y'), NULL::ENUM('a')), can_cast_implicitly(NULL::ENUM('x'), NULL::INTEGER), can_cast_implicitly(NULL::INTEGER, NULL::UNION(a ENUM('x'), b BIGINT))"
        ),
        "true|false|true|false|true"
    );
    assert_eq!(
        answer(
            "SELECT can_cast_implicitly(NULL::UNION(a INTEGER), NULL::UNION(a VARCHAR)), can_cast_implicitly(NULL::UNION(a INTEGER), NULL::UNION(b INTEGER, a INTEGER)), can_cast_implicitly(ROW(1, 2), NULL::STRUCT(a BIGINT, b BIGINT)), can_cast_implicitly(NULL::STRUCT(A INTEGER), NULL::STRUCT(a BIGINT))"
        ),
        "true|true|true|true"
    );
}

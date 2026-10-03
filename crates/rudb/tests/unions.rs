//! The `UNION` type: a value of one of several named members, cast into one by the member its type
//! goes into, built with `union_value`, read with `union_tag`, `union_extract` and a dot, stored in
//! a table, ordered tag first, and written to and read from JSON as an object of one key.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn answered(database: &Database, sql: &str) -> String {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

fn check(cases: &[(&str, &str)]) {
    let database = Database::new();
    for (sql, expected) in cases {
        assert_eq!(answered(&database, sql), *expected, "{sql}");
    }
}

#[test]
fn a_value_goes_into_the_member_of_its_type_or_the_cheapest_one() {
    check(&[
        ("SELECT 1::UNION(a BIGINT, b DOUBLE), union_tag(1::UNION(a BIGINT, b DOUBLE))", "1|a"),
        ("SELECT 1::UNION(a DOUBLE, b BIGINT), union_tag(1::UNION(a DOUBLE, b BIGINT))", "1|b"),
        ("SELECT 1::UNION(a VARCHAR, b DOUBLE)", "1.0"),
        ("SELECT union_tag(1::UNION(a HUGEINT, b DECIMAL(10,2)))", "a"),
        ("SELECT 1.5::UNION(a INT, b FLOAT)", "1.5"),
        ("SELECT [1,2]::UNION(a INT[], b VARCHAR)", "[1, 2]"),
        ("SELECT 'not set'::UNION(num INTEGER, str VARCHAR)", "not set"),
        ("SELECT 13.37::FLOAT::UNION(numeric UNION(i INTEGER, f FLOAT), str VARCHAR)", "13.37"),
        ("SELECT union_value(b := 4)::UNION(b BIGINT, c INT)", "4"),
        ("SELECT TRY_CAST(union_value(b := 'x') AS UNION(b INT, c INT))", "NULL"),
        ("SELECT union_value(k := 1)::VARCHAR, union_value(a := [1,2])::VARCHAR", "1|[1, 2]"),
    ]);
}

#[test]
fn a_cast_with_no_member_or_two_is_refused_even_when_tried() {
    let ambiguous = "Conversion Error: Type INTEGER can't be cast as UNION(a INTEGER, b INTEGER). \
                     The cast is ambiguous, multiple possible members in target: 'a (INTEGER)', \
                     'b (INTEGER)'. Disambiguate the target type by using the \
                     'union_value(<tag> := <arg>)' function to promote the source value to a \
                     single member union before casting.";
    assert_eq!(refused("SELECT 1::UNION(a INT, b INT)"), ambiguous);
    let none = "Conversion Error: Type VARCHAR can't be cast as UNION(a INTEGER, b DATE). VARCHAR \
                can't be implicitly cast to any of the union member types: INTEGER, DATE";
    assert_eq!(refused("SELECT 'x'::UNION(a INT, b DATE)"), none);
    assert_eq!(refused("SELECT TRY_CAST('x' AS UNION(a INT, b DATE))"), none);
    assert_eq!(
        refused("SELECT union_value(a := 4)::UNION(b INT, c INT)"),
        "Conversion Error: Type UNION(a INTEGER) can't be cast as UNION(b INTEGER, c INTEGER). \
         The member '\"a\"' is not present in target union"
    );
    assert_eq!(
        refused("SELECT union_value(k := 1)::INT"),
        "Conversion Error: Unimplemented type for cast (UNION(k INTEGER) -> INTEGER)"
    );
}

#[test]
fn union_value_builds_a_union_of_the_one_member_it_names() {
    check(&[
        (
            "SELECT typeof(union_value(k := 1)), union_tag(union_value(k := 1)), \
             typeof(union_tag(union_value(k := 1)))",
            "UNION(k INTEGER)|k|ENUM('k')",
        ),
        (
            "SELECT union_extract(union_value(k := 1), 'k'), union_extract(union_value(k := 1), 'K')",
            "1|1",
        ),
        (
            "SELECT union_extract(1::UNION(a INT, b VARCHAR), 'b'), (1::UNION(a INT, b VARCHAR)).A",
            "NULL|1",
        ),
        (
            "SELECT NULL::UNION(a INT), (NULL::UNION(a INT)) IS NULL, union_value(a := NULL) IS NULL",
            "NULL|true|false",
        ),
        (
            "SELECT union_tag(NULL::UNION(a INT, b INT)), union_extract(NULL::UNION(a INT), 'a')",
            "NULL|NULL",
        ),
        ("SELECT union_tag(1::UNION(a INT, b VARCHAR)) = 'a'", "true"),
    ]);
    assert_eq!(
        refused("SELECT union_value(1)"),
        "Binder Error: Need named argument for union tag, e.g. UNION_VALUE(a := b)"
    );
    assert_eq!(
        refused("SELECT union_value()"),
        "Binder Error: union_value takes exactly one argument"
    );
    assert_eq!(
        refused("SELECT union_value(k := 1, j := 2)"),
        "Binder Error: union_value takes exactly one argument"
    );
    assert_eq!(
        refused("SELECT union_extract(union_value(k := 1), 'z')"),
        "Binder Error: Could not find key \"z\" in union\nCandidate Entries: \"k\""
    );
}

#[test]
fn a_typed_null_keeps_its_member_and_only_an_untyped_one_is_a_null_union() {
    check(&[(
        "SELECT union_tag(x::UNION(a INT, b VARCHAR)), x::UNION(a INT, b VARCHAR) IS NULL \
         FROM (VALUES (1), (NULL)) t(x)",
        "a|false\na|false",
    )]);
}

#[test]
fn a_union_column_holds_each_row_in_its_own_member() {
    let database = Database::new();
    for sql in [
        "CREATE TABLE tbl(u UNION(i INTEGER, f FLOAT))",
        "INSERT INTO tbl VALUES (1::INTEGER)",
        "INSERT INTO tbl VALUES (2.0::FLOAT)",
    ] {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    assert_eq!(answered(&database, "SELECT * FROM tbl"), "1\n2.0");
    assert_eq!(answered(&database, "SELECT u.i FROM tbl"), "1\nNULL");
    assert_eq!(answered(&database, "SELECT union_tag(u) FROM tbl"), "i\nf");
    assert_eq!(
        Database::new().execute("CREATE TABLE t(a UNION(b INT, B INT))").unwrap_err().to_string(),
        "Binder Error: Duplicate UNION type member name \"B\""
    );
}

#[test]
fn two_unions_meet_at_the_one_whose_members_cover_the_other() {
    check(&[
        (
            "SELECT typeof(x) FROM (SELECT 1::UNION(i32 INT, str VARCHAR) UNION ALL \
             SELECT 'a'::UNION(str VARCHAR, i32 INT, f32 FLOAT)) t(x) LIMIT 1",
            "UNION(str VARCHAR, i32 INTEGER, f32 FLOAT)",
        ),
        (
            "SELECT typeof(x) FROM (SELECT 1::UNION(a INT) UNION ALL SELECT 2) t(x) LIMIT 1",
            "UNION(a INTEGER)",
        ),
        ("SELECT typeof([union_value(a := 1), 2])", "UNION(a INTEGER)[]"),
    ]);
}

#[test]
fn a_union_is_an_object_of_one_key_in_json() {
    check(&[
        (
            "SELECT union_value(a := 1)::JSON, to_json(union_value(a := NULL)), \
             [union_value(a := 1)]::JSON",
            r#"{"a":1}|{"a":null}|[{"a":1}]"#,
        ),
        (
            r#"SELECT '{"a":1}'::JSON::UNION(a INT, b VARCHAR), '{"b":"x"}'::JSON::UNION(a INT, b VARCHAR)"#,
            "1|x",
        ),
        (
            r#"SELECT '{"a":null}'::JSON::UNION(a INT, b VARCHAR) IS NULL, 'null'::JSON::UNION(a INT, b VARCHAR) IS NULL"#,
            "false|true",
        ),
        (
            r#"SELECT TRY_CAST('{"c":1}'::JSON AS UNION(a INT, b VARCHAR)), TRY_CAST('"x"'::JSON AS UNION(a INT, b VARCHAR))"#,
            "NULL|NULL",
        ),
    ]);
    for (sql, expected) in [
        (
            r#"SELECT '{"c":1}'::JSON::UNION(a INT, b VARCHAR)"#,
            "Conversion Error: Found object containing unknown key, instead of union: c",
        ),
        (
            r#"SELECT '{"a":1,"b":"x"}'::JSON::UNION(a INT, b VARCHAR)"#,
            "Conversion Error: Found object containing more than one key, instead of union",
        ),
        (
            "SELECT '{}'::JSON::UNION(a INT, b VARCHAR)",
            "Conversion Error: Found empty object, instead of union",
        ),
        (
            r#"SELECT '"x"'::JSON::UNION(a INT, b VARCHAR)"#,
            "Conversion Error: Expected an object representing a union, got string",
        ),
    ] {
        assert_eq!(refused(sql), expected, "{sql}");
    }
}

#[test]
fn a_struct_written_out_as_the_union_casts_to_the_member_its_tag_names() {
    check(&[
        ("SELECT {tag: 0::UTINYINT, A: true, B: NULL::INT}::UNION(a BOOL, b INT)", "true"),
        ("SELECT {t: 0::UTINYINT, a: true, b: NULL::INT}::UNION(a BOOL, b INT)", "true"),
        (
            "SELECT union_tag({tag: 1::UTINYINT, a: NULL::BOOL, b: 5}::UNION(a BOOL, b INT)), \
             {tag: 1::UTINYINT, a: NULL::BOOL, b: '5'}::UNION(a BOOL, b INT)",
            "b|5",
        ),
        (
            "SELECT {tag: NULL::UTINYINT, a: NULL::BOOL, b: NULL::INT}::UNION(a BOOL, b INT) IS NULL",
            "true",
        ),
        (
            "SELECT union_tag({tag: 0::UTINYINT, a: NULL::BOOL, b: NULL::INT}::UNION(a BOOL, b INT))",
            "a",
        ),
    ]);
    assert_eq!(
        refused("SELECT {tag: 0::UTINYINT, a: 1, b: NULL::INT}::UNION(a BOOL, b INT)"),
        "Conversion Error: Type STRUCT(tag UTINYINT, a INTEGER, b INTEGER) can't be cast as \
         UNION(a BOOLEAN, b INTEGER). STRUCT(tag UTINYINT, a INTEGER, b INTEGER) can't be \
         implicitly cast to any of the union member types: BOOLEAN, INTEGER"
    );
    let database = Database::new();
    database.execute("CREATE TABLE u(col UNION(a BOOL, b INTEGER, c TINYINT))").unwrap();
    for (row, expected) in [
        (
            "{tag: 4::UINT8, a: true, b: NULL::INTEGER, c: NULL::TINYINT}",
            "Conversion Error: One or more of the tags do not point to a valid union member",
        ),
        (
            "{tag: 1::UINT8, a: NULL::BOOLEAN, b: 32412, c: 123::TINYINT}",
            "Conversion Error: One or more rows in the produced UNION have validity set for more \
             than 1 member",
        ),
        (
            "{tag: 0::UINT8, a: NULL::BOOLEAN, b: 1, c: NULL::TINYINT}",
            "Conversion Error: One or more rows in the produced UNION have tags that don't point \
             to the valid member",
        ),
    ] {
        let sql = format!("INSERT INTO u VALUES ({row})");
        assert_eq!(database.execute(&sql).unwrap_err().to_string(), expected, "{sql}");
    }
}

#[test]
fn a_typed_null_written_into_a_union_column_keeps_its_member() {
    let database = Database::new();
    database.execute("CREATE TABLE tbl (u UNION(a INT, b VARCHAR))").unwrap();
    database.execute("INSERT INTO tbl VALUES (1), (NULL), (NULL::VARCHAR), (NULL::INT)").unwrap();
    assert_eq!(
        answered(&database, "SELECT union_tag(u), u FROM tbl"),
        "a|1\nNULL|NULL\nb|NULL\na|NULL"
    );
}

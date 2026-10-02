//! The `JSON` type, the casts into and out of it, and the functions that read a document: checking
//! it, naming the type of a value in it, picking values out of it by a path, and its operators, and
//! the functions that build, merge, print and match whole documents.
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
fn a_cast_to_json_checks_a_string_and_writes_anything_else() {
    check(&[
        (r#"SELECT '{"a" : 1}'::JSON, typeof('{"a":1}'::JSON)"#, r#"{"a" : 1}|JSON"#),
        (r#"SELECT TRY_CAST('x"y' AS JSON)"#, "NULL"),
        (
            "SELECT 1::JSON, 1.5::JSON, (-0.0)::JSON, 'NaN'::DOUBLE::JSON, 1e300::JSON, 0.1::FLOAT::JSON, 123.456::JSON",
            "1|1.5|0.0|NaN|1e300|0.10000000149011612|123.456",
        ),
        (
            r#"SELECT TIMESTAMP '2020-01-01 10:00:00'::JSON, TIME '10:00:00'::JSON, {'a': 'x"y'}::JSON, [1,2,NULL]::JSON, MAP {'k': 1}::JSON"#,
            r#""2020-01-01 10:00:00"|"10:00:00"|{"a":"x\"y"}|[1,2,null]|{"k":1}"#,
        ),
        (r#"SELECT 42::JSON = '42', '{"a":1}'::JSON = '{"a":1}'::JSON"#, "true|true"),
        (
            r#"SELECT upper('{"a":1}'::JSON), typeof(upper('{"a":1}'::JSON)), length('[1]'::JSON)"#,
            r#"{"A":1}|VARCHAR|3"#,
        ),
    ]);
    assert_eq!(
        refused(r#"SELECT '{"a":'::JSON"#),
        r#"Conversion Error: Malformed JSON at byte 5 of input: unexpected end of data.  Input: "{"a":""#
    );
    assert_eq!(
        refused(r#"SELECT 'x"y'::VARCHAR::JSON"#),
        r#"Conversion Error: Malformed JSON at byte 0 of input: unexpected character.  Input: "x"y""#
    );
}

#[test]
fn a_cast_from_json_reads_the_document_into_the_type() {
    check(&[
        (
            r#"SELECT '1.5'::JSON::INTEGER, '1.5'::JSON::DECIMAL(4,2), 'true'::JSON::INTEGER, '1'::JSON::BOOLEAN, '"true"'::JSON::BOOLEAN, 'null'::JSON::INTEGER"#,
            "2|1.50|1|true|true|NULL",
        ),
        (
            r#"SELECT '[1,null]'::JSON::INTEGER[], '{"a":null}'::JSON::STRUCT(a INTEGER), '{"k":2}'::JSON::MAP(VARCHAR, INTEGER)"#,
            "[1, NULL]|{'a': NULL}|{k=2}",
        ),
        (
            r#"SELECT TRY_CAST('"x"'::JSON AS INTEGER), TRY_CAST('[1]'::JSON AS STRUCT(a INTEGER))"#,
            "NULL|{'a': NULL}",
        ),
        (
            r#"SELECT '{"a" : 1}'::JSON::VARCHAR, ['{"a" : 1}'::JSON]::VARCHAR, ['{"a" : 1}'::JSON]::VARCHAR[]"#,
            r#"{"a" : 1}|[{"a" : 1}]|['{"a" : 1}']"#,
        ),
    ]);
    let cases = [
        (r#"SELECT '"x"'::JSON::INTEGER"#, r#"Failed to cast value to numerical: "x""#),
        ("SELECT '[1]'::JSON::INTEGER", "Failed to cast value to numerical: [1]"),
        ("SELECT '300'::JSON::TINYINT", "Failed to cast value to numerical: 300"),
        ("SELECT '1'::JSON::INTEGER[]", "Expected ARRAY, but got UBIGINT: 1"),
        ("SELECT '[1]'::JSON::STRUCT(a INTEGER)", "Expected OBJECT, but got ARRAY: [1]"),
        (r#"SELECT '{"a":1}'::JSON::STRUCT(b INTEGER)"#, r#"Object {"a":1} has unknown key "a""#),
        (
            r#"SELECT '{"a":1,"c":2}'::JSON::STRUCT(a INTEGER)"#,
            r#"Object {"a":1,"c":2} has unknown key "c""#,
        ),
        (
            "SELECT '[1,2]'::JSON::INTEGER[3]",
            "Expected array of size 3, but got '[1,2]' with size 2",
        ),
        (
            r#"SELECT '"abc"'::JSON::DATE"#,
            "invalid date field format: \"abc\", expected format is (YYYY-MM-DD)\n If this error occurred during read_json, line/object number information is approximate",
        ),
    ];
    for (sql, message) in cases {
        assert_eq!(refused(sql), format!("Conversion Error: {message}"), "{sql}");
    }
}

#[test]
fn the_functions_answer_what_the_pin_answers() {
    check(&[
        (r#"SELECT json_valid('{"a":1}'), json_valid('{a}'), json_valid(NULL)"#, "true|false|NULL"),
        (
            r#"SELECT json_type('null'), json_type('[1]'), json_type('{"a":1}', '$.a'), json_type('18446744073709551616'), json_type('-0'), json_type('"x"')"#,
            "NULL|ARRAY|UBIGINT|DOUBLE|BIGINT|VARCHAR",
        ),
        (
            r#"SELECT json_extract('{"a":{"b":[1,2,3]}}', '$.a.b[1]'), json_extract('{"a":{"b":[1,2,3]}}', '$.a.b[-1]'), json_extract('{"a":{"b":[1,2,3]}}', '$.a.b[#-1]')"#,
            "2|3|3",
        ),
        (
            r#"SELECT json_extract('{"a":{"b":[1,2,3]}}', '$.a.b[*]'), json_extract('{"a":{"b":[1,2,3]}}', ['$.a', '$.x'])"#,
            r#"[1, 2, 3]|[{"b":[1,2,3]}, NULL]"#,
        ),
        (
            r#"SELECT json_extract('[1,2,3]', 1), json_extract('{"a b":4}', 'a b'), json_extract('{"m~n":5}', '/m~0n')"#,
            "2|4|5",
        ),
        (
            r#"SELECT json_extract_string('{"a":"x"}', '$.a'), json_extract_string('{"a":[1]}', '$.a'), json_extract_string('{"a":null}', '$.a')"#,
            "x|[1]|NULL",
        ),
        (
            r#"SELECT json_value('{"a":"x"}', '$.a'), json_value('{"a":[1]}', '$.a'), json_value('{"a":2}', '$.a')"#,
            r#""x"|NULL|2"#,
        ),
        (
            r#"SELECT json_keys('{"a":1,"b":2}'), json_keys('[1]'), json_keys('{"a":{"c":1}}', '$.a')"#,
            "[a, b]|[]|[c]",
        ),
        (
            r#"SELECT json_array_length('[1,2,3]'), json_array_length('{}'), json_array_length('{"a":[1]}', '$.a')"#,
            "3|0|1",
        ),
        (
            r#"SELECT json_exists('{"a":1}', '$.a'), json_exists('{"a":1}', '$.b'), json_exists('{"a":1}', ['$.a', '$.b'])"#,
            "true|false|[true, false]",
        ),
        (
            r#"SELECT '{"a":1}' -> '$.a', '{"a":1}' ->> 'a', typeof('{"a":1}' -> '$.a'), typeof('{"a":1}'::JSON ->> 'a')"#,
            "1|1|JSON|VARCHAR",
        ),
        (r#"SELECT '{"a":[1]}' -> 'a' ->> 0, '{"a":[1]}' -> ('a' ->> 0)"#, "1|1"),
        (r#"SELECT '{"a":{"b":2}}' -> 'a' ->> 'b' -> 'c'"#, "NULL"),
        (
            r#"SELECT p, json_extract('{"a":1,"b":[2]}', p) FROM (VALUES ('$.a'), ('$.b[0]'), ('b'), ('$.z')) t(p)"#,
            "$.a|1\n$.b[0]|2\nb|[2]\n$.z|NULL",
        ),
        (
            r#"SELECT json('{ "a" : [1, 2] }'), json_extract('[1e-7, 1E+2, 0.5, 10.0, -0, 1.0e400, 18446744073709551616]', '$')"#,
            r#"{"a":[1,2]}|[1e-7,100.0,0.5,10.0,0,1.0e400,18446744073709551616]"#,
        ),
        (
            r#"SELECT json_extract('{"a":{"b":1},"c":{"a":3}}', '$..a'), json_extract('[[1,[2]],3]', '$..*')"#,
            r#"[{"b":1}, 3]|[[1,[2]], 3, 1, [2], 2]"#,
        ),
        (
            r#"SELECT json_extract(NULL, '$.a'), json_extract('{"a":1}', NULL), json_exists(NULL, '$'), json_type(NULL)"#,
            "NULL|NULL|NULL|NULL",
        ),
    ]);
}

#[test]
fn a_path_the_pin_cannot_read_is_refused_in_its_words() {
    let cases = [
        (r#"SELECT json_extract('{"a":1}', '$.')"#, "Binder Error: JSON path error near '.'"),
        (r#"SELECT json_extract('{"a":1}', '$a')"#, "Binder Error: JSON path error near 'a'"),
        (r#"SELECT json_extract('{"a":1}', '$.a[1')"#, "Binder Error: JSON path error near '[1'"),
        (
            r#"SELECT json_extract('{"a":1}', '$.""')"#,
            r#"Binder Error: JSON path error near '.""'"#,
        ),
        (
            r#"SELECT json_extract('{"a":1}', ['$.*'])"#,
            "Binder Error: Cannot have wildcards in JSON path when supplying multiple paths",
        ),
        (r#"SELECT json_extract('{"a":1}', [NULL])"#, "Binder Error: JSON path cannot be NULL"),
        (
            "SELECT json_extract('{x', '$')",
            r#"Invalid Input Error: Malformed JSON at byte 1 of input: unexpected character.  Input: "{x""#,
        ),
        (
            r#"SELECT json_extract('{"a":1}', p) FROM (VALUES ('$.*')) t(p)"#,
            "Invalid Input Error: JSON path cannot contain wildcards if the path is not a constant parameter",
        ),
        (
            r#"SELECT json_extract('{"a":1}', p) FROM (VALUES (['$.a'])) t(p)"#,
            r#"Binder Error: The "col1" argument in function "json_extract" must be a constant expression"#,
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(refused(sql), expected, "{sql}");
    }
    let candidates = "\n\tCandidate functions:\n\tjson_extract(col0 VARCHAR, col1 BIGINT) -> JSON\n\tjson_extract(col0 VARCHAR, col1 VARCHAR) -> JSON\n\tjson_extract(col0 VARCHAR, col1 VARCHAR[]) -> JSON[]\n\tjson_extract(col0 JSON, col1 BIGINT) -> JSON\n\tjson_extract(col0 JSON, col1 VARCHAR) -> JSON\n\tjson_extract(col0 JSON, col1 VARCHAR[]) -> JSON[]\n";
    for sql in [r#"SELECT json_extract('{"a":1}', 1.5)"#, r#"SELECT '{"a":1}' -> 1.5"#] {
        assert_eq!(
            refused(sql),
            format!(
                "Binder Error: No function matches the given name and argument types 'json_extract(STRING_LITERAL, DECIMAL(2,1))'. You might need to add explicit type casts.{candidates}"
            ),
            "{sql}"
        );
    }
}

#[test]
fn the_builders_write_a_value_as_a_document() {
    check(&[
        (
            r#"SELECT to_json('{"a":1}'), to_json('{"a":1}'::JSON), to_json(1.5::DECIMAL(20,2)), to_json(NULL::INTEGER) IS NULL, typeof(to_json(1))"#,
            r#""{\"a\":1}"|{"a":1}|1.50|true|JSON"#,
        ),
        (
            r#"SELECT to_json(DATE '2020-01-02'), to_json(INTERVAL 1 DAY), to_json('ab'::BLOB), to_json({'a': [1, NULL], 'b': 'x'}), json_quote('x"y')"#,
            r#""2020-01-02"|"1 day"|"ab"|{"a":[1,null],"b":"x"}|"x\"y""#,
        ),
        (
            "SELECT array_to_json([1, 2]), array_to_json(NULL), row_to_json({'a': 1}), row_to_json(NULL)",
            r#"[1,2]|NULL|{"a":1}|NULL"#,
        ),
        (
            "SELECT json_array(), json_array(1, 'a', NULL, [true]), json_array(NULL), json_object(), json_object('a', 1, 'a', 2, 'b', NULL)",
            r#"[]|[1,"a",null,[true]]|[null]|{}|{"a":1,"a":2,"b":null}"#,
        ),
    ]);
    let cases = [
        ("SELECT to_json()", "Binder Error: to_json() takes exactly one argument"),
        (
            "SELECT array_to_json(1)",
            "Binder Error: array_to_json() argument type must be LIST or ARRAY",
        ),
        ("SELECT row_to_json([1])", "Binder Error: row_to_json() argument type must be STRUCT"),
        (
            "SELECT json_object('a')",
            "Binder Error: json_object() requires an even number of arguments",
        ),
        (
            "SELECT json_object(1, 2)",
            r#"Binder Error: json_object() keys must be VARCHAR, add an explicit cast to argument ""1"""#,
        ),
        (
            "SELECT json_object(k, 1) FROM (VALUES (NULL::VARCHAR)) t(k)",
            "Invalid Input Error: JSON key cannot be NULL",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(refused(sql), expected, "{sql}");
    }
}

#[test]
fn the_whole_document_functions_merge_print_and_match() {
    check(&[
        (
            r#"SELECT json_merge_patch('{"a":1,"b":2}', '{"b":null,"c":3}'), json_merge_patch('[1]', '{"a":1}'), json_merge_patch('{"a":{"x":1}}', '{"a":{"y":2}}', '{"z":0}'), json_merge_patch(NULL, '{"a":1}'), json_merge_patch('{"a":1}', NULL)"#,
            r#"{"a":1,"c":3}|{"a":1}|{"a":{"x":1,"y":2},"z":0}|{"a":1}|NULL"#,
        ),
        (
            r#"SELECT json_deep_merge('{"a":{"x":1},"b":1}', '{"a":{"y":2},"b":null}'), json_deep_merge('1', 'null'), json_deep_merge('{"a":[1]}', '{"a":[2]}')"#,
            r#"{"b":1,"a":{"x":1,"y":2}}|1|{"a":[2]}"#,
        ),
        (
            r#"SELECT json_merge_patch_diff('{"a":1,"b":2,"c":{"x":1}}', '{"a":1,"c":{"x":2},"d":4}'), json_merge_patch_diff('{"a":1}', '{"a":1}'), json_merge_patch_diff('[1]', '[2]'), json_merge_patch_diff(NULL, '{"a":1}')"#,
            r#"{"b":null,"c":{"x":2},"d":4}|{}|[2]|{"a":1}"#,
        ),
        (
            r#"SELECT json_pretty('{"a":[1,{"b":null}],"c":{},"d":[]}')"#,
            "{\n    \"a\": [\n        1,\n        {\n            \"b\": null\n        }\n    ],\n    \"c\": {},\n    \"d\": []\n}",
        ),
        (
            r#"SELECT json_strip_nulls('{"a":null,"b":[null,{"c":null,"d":1}]}'), typeof(json_pretty('1')), typeof(json_strip_nulls('1'))"#,
            r#"{"b":[null,{"d":1}]}|VARCHAR|JSON"#,
        ),
        (
            r#"SELECT json_contains('{"a":1,"b":[1,2,{"c":3}]}', '{"c":3}'), json_contains('[1,2,3]', '[3,1]'), json_contains('{"a":1}', '1.0'), json_contains('{"a":{"b":1,"c":2}}', '{"b":1}'), json_contains('[1]', '"1"')"#,
            "true|true|false|true|false",
        ),
        (
            r#"SELECT json_structure('{"a":1,"b":[1,2.5],"c":null,"d":"x","e":true}'), json_structure('[1,"a"]'), json_structure('[]'), json_structure('{}'), json_structure('[{"a":1},{"b":-1}]')"#,
            r#"{"a":"UBIGINT","b":["DOUBLE"],"c":"NULL","d":"VARCHAR","e":"BOOLEAN"}|["JSON"]|["NULL"]|"JSON"|[{"a":"UBIGINT","b":"BIGINT"}]"#,
        ),
        (
            r#"SELECT json_structure('[1,18446744073709551615]'), json_structure('[-1,18446744073709551615]'), json_structure('[null,1]'), json_structure('{"a":1,"a":"x"}')"#,
            r#"["UBIGINT"]|["HUGEINT"]|["UBIGINT"]|{"a":"JSON"}"#,
        ),
    ]);
    assert_eq!(
        refused("SELECT json_contains('{x', '1')"),
        r#"Invalid Input Error: Malformed JSON at byte 1 of input: unexpected character.  Input: "{x""#
    );
}

#[test]
fn a_deep_document_is_merged_and_matched_without_running_out_of_stack() {
    let deep =
        |value: &str| format!(r#"repeat('{{"a":', 50000) || '{value}' || repeat('}}', 50000)"#);
    let (one, two) = (deep("1"), deep("2"));
    check(&[
        (
            &format!(
                "SELECT length(json_merge_patch({one}, {two})), length(json_deep_merge({one}, {two})), length(json_merge_patch_diff({one}, {two}))"
            ),
            "300001|300001|300001",
        ),
        ("SELECT json_contains(repeat('[', 50000) || '1' || repeat(']', 50000), '1')", "true"),
        // The pin crashes past about 25000 levels here, tamnd/duckdb#31.
        ("SELECT length(json_structure(repeat('[', 20000) || repeat(']', 20000)))", "40006"),
        ("SELECT length(json_pretty(repeat('[', 1000) || repeat(']', 1000)))", "3996002"),
    ]);
}

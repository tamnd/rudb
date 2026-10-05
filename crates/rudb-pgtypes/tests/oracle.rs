//! Checks the text forms against `oracle.tsv`, which `oracle.sql` records from the server at the
//! pin. Each line is the type, `extra_float_digits`, the input, and the output or the error.

use rudb_pgtypes::*;

fn text(f: impl FnOnce(&mut Vec<u8>)) -> String {
    let mut out = Vec::new();
    f(&mut out);
    String::from_utf8(out).unwrap()
}

fn run(type_name: &str, extra_float_digits: i32, input: &str) -> String {
    let result = match type_name {
        "int2" => int2_in(input).map(|v| text(|out| int_out(v.into(), out))),
        "int4" => int4_in(input).map(|v| text(|out| int_out(v.into(), out))),
        "int8" => int8_in(input).map(|v| text(|out| int_out(v, out))),
        "oid" => oid_in(input).map(|v| text(|out| oid_out(v, out))),
        "bool" => bool_in(input).map(|v| text(|out| bool_out(v, out))),
        "\"char\"" => Ok(text(|out| char_out(char_in(input), out))),
        "name" => Ok(name_in(input).to_string()),
        "bytea" => bytea_in(input).map(|v| text(|out| bytea_out(&v, ByteaOutput::Hex, out))),
        "uuid" => uuid_in(input).map(|v| text(|out| uuid_out(&v, out))),
        "float8" => float8_in(input).map(|v| text(|out| float8_out(v, extra_float_digits, out))),
        "float4" => float4_in(input).map(|v| text(|out| float4_out(v, extra_float_digits, out))),
        _ => panic!("the fixture has a type that the test does not know: {type_name}"),
    };
    result.unwrap_or_else(|error| format!("ERROR {} {}", error.sqlstate.as_str(), error.message))
}

#[test]
fn the_text_forms_match_the_server_at_the_pin() {
    let mut failures = Vec::new();
    for line in include_str!("oracle.tsv").lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let [type_name, extra_float_digits, input, expected] = fields[..] else {
            panic!("a line of the fixture does not have four fields: {line:?}");
        };
        let got = run(type_name, extra_float_digits.parse().unwrap(), input);
        if got != expected {
            failures.push(format!(
                "{type_name} {extra_float_digits} {input:?}: {got:?}, not {expected:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{} lines differ:\n{}", failures.len(), failures.join("\n"));
}

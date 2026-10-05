//! Checks the text forms against `oracle.tsv`, which `oracle.sql` records from the server at the
//! pin. Each line is the type, `extra_float_digits`, the input, and the output or the error. A
//! type that starts with `send` has the hex of the binary output.

use rudb_pgtypes::*;

fn text(f: impl FnOnce(&mut Vec<u8>)) -> String {
    let mut out = Vec::new();
    f(&mut out);
    String::from_utf8(out).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The typmod of `numeric` or `numeric(p,s)`.
fn numeric_typmod(type_name: &str) -> Option<i32> {
    let args = type_name.strip_prefix("numeric")?;
    if args.is_empty() {
        return Some(-1);
    }
    let (p, s) = args.strip_prefix('(')?.strip_suffix(')')?.split_once(',')?;
    Some(typmod::numeric_typmod(p.parse().ok()?, s.parse().ok()?))
}

/// The text or the binary output of `numeric`. When the value fits the decimal of the engine at
/// its display scale, the fast path must give the same bytes.
fn numeric(typmod: i32, send: bool, input: &str) -> Result<String, TypeError> {
    let v = numeric_in(input, typmod)?;
    let mut out = Vec::new();
    if send {
        numeric_send(&v, &mut out);
        let back = numeric_recv(&mut Recv::new(&out), typmod)?;
        assert_eq!(back, v, "{input:?} does not read back from its binary form");
    } else {
        numeric_out(&v, &mut out);
    }
    if let Some(value) = v.to_decimal(u32::from(v.dscale())).filter(|_| v.dscale() <= 38) {
        let mut fast = Vec::new();
        match send {
            true => decimal_send(value, u32::from(v.dscale()), &mut fast),
            false => decimal_out(value, u32::from(v.dscale()), &mut fast),
        }
        assert_eq!(fast, out, "{input:?} as a decimal");
    }
    Ok(if send { hex(&out) } else { String::from_utf8(out).unwrap() })
}

fn run(type_name: &str, extra_float_digits: i32, input: &str) -> String {
    let (send, base) = match type_name.strip_prefix("send ") {
        Some(base) => (true, base),
        None => (false, type_name),
    };
    if let Some(typmod) = numeric_typmod(base) {
        return error_text(numeric(typmod, send, input));
    }
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
    error_text(result)
}

fn error_text(result: Result<String, TypeError>) -> String {
    result.unwrap_or_else(|error| {
        let detail = error.detail.map(|d| format!(" DETAIL {d}")).unwrap_or_default();
        format!("ERROR {} {}{detail}", error.sqlstate.as_str(), error.message)
    })
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

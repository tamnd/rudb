//! Checks the text forms against `oracle.tsv`, which `oracle.sql` records from the server at the
//! pin. Each line is the type, a setting, the input, and the output or the error. The setting is
//! `extra_float_digits` for the floats, `DateStyle` for the date and time types with the
//! `TimeZone` after a `|` for `timestamptz`, and `IntervalStyle` for `interval`. A type that starts
//! with `send` has the hex of the binary output. For the output of the date and time types the
//! input is the hex of the binary form. A type that starts with `in` is the text input of a date or
//! time type with a typmod, and the output is the hex of the binary form. Its setting is the
//! `DateStyle` and the `TimeZone` after a `|`, or the `IntervalStyle`. A string type or `json`
//! after `in` has no setting, and `inhex` is the same with the hex of the input, for an input with
//! a control character. A type that starts with `coerce` is the length cast of `varchar` or
//! `bpchar` with the typmod and `true` for an explicit cast.

use rudb_pgtypes::*;

fn text(f: impl FnOnce(&mut Vec<u8>)) -> String {
    let mut out = Vec::new();
    f(&mut out);
    String::from_utf8(out).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

fn date_format(setting: &str) -> DateFormat {
    let (style, order) = setting.split_once(", ").unwrap();
    let style = match style {
        "ISO" => DateStyle::Iso,
        "SQL" => DateStyle::Sql,
        "Postgres" => DateStyle::Postgres,
        "German" => DateStyle::German,
        _ => panic!("unknown DateStyle {setting:?}"),
    };
    let order = match order {
        "MDY" => DateOrder::Mdy,
        "DMY" => DateOrder::Dmy,
        "YMD" => DateOrder::Ymd,
        _ => panic!("unknown DateStyle {setting:?}"),
    };
    DateFormat { style, order }
}

/// The fixed zones of the fixture. A POSIX zone such as `+05:30` is west of UTC and has no
/// abbreviation.
fn zone(name: &str) -> FixedZone {
    match name {
        "UTC" => FixedZone::utc(),
        "<+05:30>-05:30" => FixedZone { offset: 19800, abbrev: "+05:30".to_string() },
        "+05:30" => FixedZone { offset: -19800, abbrev: String::new() },
        _ => panic!("unknown TimeZone {name:?}"),
    }
}

fn interval_style(setting: &str) -> IntervalStyle {
    match setting {
        "postgres" => IntervalStyle::Postgres,
        "postgres_verbose" => IntervalStyle::PostgresVerbose,
        "sql_standard" => IntervalStyle::SqlStandard,
        "iso_8601" => IntervalStyle::Iso8601,
        _ => panic!("unknown IntervalStyle {setting:?}"),
    }
}

/// The text output of a date or time type from its binary form. The value must also go back to
/// the same bytes.
fn datetime(type_name: &str, setting: &str, input: &str) -> Result<String, TypeError> {
    let bytes = unhex(input);
    let recv = &mut Recv::new(&bytes);
    let mut out = Vec::new();
    let mut back = Vec::new();
    match type_name {
        "date" => {
            let v = date_recv(recv)?;
            date_out(v, date_format(setting), &mut out);
            back.extend_from_slice(&v.to_be_bytes());
        }
        "time" => {
            let v = time_recv(recv, -1)?;
            time_out(v, &mut out);
            back.extend_from_slice(&v.to_be_bytes());
        }
        "timetz" => {
            let (time, zone) = timetz_recv(recv, -1)?;
            timetz_out(time, zone, &mut out);
            back.extend_from_slice(&time.to_be_bytes());
            back.extend_from_slice(&zone.to_be_bytes());
        }
        "timestamp" => {
            let v = timestamp_recv(recv, -1)?;
            timestamp_out(v, date_format(setting), &mut out)?;
            back.extend_from_slice(&v.to_be_bytes());
        }
        "timestamptz" => {
            let (style, name) = setting.split_once('|').unwrap();
            let v = timestamp_recv(recv, -1)?;
            timestamptz_out(v, date_format(style), &zone(name), &mut out)?;
            back.extend_from_slice(&v.to_be_bytes());
        }
        "interval" => {
            let v = interval_recv(recv, -1)?;
            interval_out(&v, interval_style(setting), &mut out);
            interval_send(&v, &mut back);
        }
        _ => unreachable!(),
    }
    assert_eq!(back, bytes, "{type_name} {input} does not go back to the same bytes");
    Ok(String::from_utf8(out).unwrap())
}

/// The text input of a date or time type, as the hex of the binary form.
fn datetime_in(
    type_name: &str,
    typmod: i32,
    setting: &str,
    input: &str,
) -> Result<String, TypeError> {
    let mut out = Vec::new();
    if type_name == "interval" {
        let v = interval_in(input, typmod, interval_style(setting))?;
        interval_send(&v, &mut out);
        return Ok(hex(&out));
    }
    let (style, name) = setting.split_once('|').unwrap();
    let zone = zone(name);
    let cx = DateTimeInput {
        order: date_format(style).order,
        zone: &zone,
        zones: &NoZones,
        abbrevs: ZoneAbbrevs::postgres_default(),
        now: 0,
    };
    match type_name {
        "date" => out.extend_from_slice(&date_in(input, &cx)?.to_be_bytes()),
        "time" => out.extend_from_slice(&time_in(input, typmod, &cx)?.to_be_bytes()),
        "timetz" => {
            let (time, zone) = timetz_in(input, typmod, &cx)?;
            out.extend_from_slice(&time.to_be_bytes());
            out.extend_from_slice(&zone.to_be_bytes());
        }
        "timestamp" => out.extend_from_slice(&timestamp_in(input, typmod, &cx)?.to_be_bytes()),
        "timestamptz" => out.extend_from_slice(&timestamptz_in(input, typmod, &cx)?.to_be_bytes()),
        _ => panic!("the fixture has a type that the test does not know: {type_name}"),
    }
    Ok(hex(&out))
}

/// The text input of a string type or `json`, as the hex of the binary form. The binary form of
/// each is the bytes of the string, so the receive function is the same after the encoding check.
fn string_in(type_name: &str, typmod: i32, input: &str) -> Option<Result<String, TypeError>> {
    let result = match type_name {
        "text" => Ok(input.to_string()),
        "varchar" => varchar_in(input, typmod).map(str::to_string),
        "bpchar" => bpchar_in(input, typmod).map(|v| v.into_owned()),
        "json" => json_in(input).map(str::to_string),
        _ => return None,
    };
    Some(result.map(|v| {
        assert_eq!(Recv::new(v.as_bytes()).text(), Ok(&v[..]));
        hex(v.as_bytes())
    }))
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

fn run(type_name: &str, setting: &str, input: &str) -> String {
    let (send, base) = match type_name.strip_prefix("send ") {
        Some(base) => (true, base),
        None => (false, type_name),
    };
    if let Some(rest) = type_name.strip_prefix("inhex ") {
        let input = String::from_utf8(unhex(input)).unwrap();
        return run(&format!("in {rest}"), setting, &input);
    }
    if let Some(rest) = type_name.strip_prefix("in ") {
        let (base, typmod) = rest.split_once(' ').unwrap();
        let typmod = typmod.parse().unwrap();
        if let Some(result) = string_in(base, typmod, input) {
            return error_text(result);
        }
        return error_text(datetime_in(base, typmod, setting, input));
    }
    if let Some(rest) = type_name.strip_prefix("coerce ") {
        let [base, typmod, explicit] = rest.split(' ').collect::<Vec<_>>()[..] else {
            panic!("a coerce line needs a type, a typmod and t or f: {type_name}");
        };
        let (typmod, explicit) = (typmod.parse().unwrap(), explicit == "true");
        let result = match base {
            "varchar" => varchar_coerce(input, typmod, explicit).map(|v| hex(v.as_bytes())),
            "bpchar" => bpchar_coerce(input, typmod, explicit).map(|v| hex(v.as_bytes())),
            _ => panic!("the fixture has a type that the test does not know: {type_name}"),
        };
        return error_text(result);
    }
    if let Some(typmod) = numeric_typmod(base) {
        return error_text(numeric(typmod, send, input));
    }
    if ["date", "time", "timetz", "timestamp", "timestamptz", "interval"].contains(&type_name) {
        return error_text(datetime(type_name, setting, input));
    }
    let extra_float_digits = || setting.parse::<i32>().unwrap();
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
        "float8" => float8_in(input).map(|v| text(|out| float8_out(v, extra_float_digits(), out))),
        "float4" => float4_in(input).map(|v| text(|out| float4_out(v, extra_float_digits(), out))),
        _ => panic!("the fixture has a type that the test does not know: {type_name}"),
    };
    error_text(result)
}

fn error_text(result: Result<String, TypeError>) -> String {
    result.unwrap_or_else(|error| {
        let detail = error.detail.map(|d| format!(" DETAIL {d}")).unwrap_or_default();
        let hint = error.hint.map(|h| format!(" HINT {h}")).unwrap_or_default();
        format!("ERROR {} {}{detail}{hint}", error.sqlstate.as_str(), error.message)
    })
}

#[test]
fn the_text_forms_match_the_server_at_the_pin() {
    let mut failures = Vec::new();
    for line in include_str!("oracle.tsv").lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let [type_name, setting, input, expected] = fields[..] else {
            panic!("a line of the fixture does not have four fields: {line:?}");
        };
        let got = run(type_name, setting, input);
        if got != expected {
            failures.push(format!("{type_name} {setting} {input:?}: {got:?}, not {expected:?}"));
        }
    }
    assert!(failures.is_empty(), "{} lines differ:\n{}", failures.len(), failures.join("\n"));
}

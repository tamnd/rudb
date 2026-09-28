//! `COPY ... TO` a CSV or a JSON file.
//!
//! In CSV, every value is written as its cast to VARCHAR, under the session's time zone, which is
//! what the pin writes: a list is `[1, 2]`, a double is `1e+20`, a blob is `\x00\xFF`. A value
//! is quoted when it holds the delimiter, the quote or a line break, or when it reads the same as a
//! null would, so an empty string beside an empty null is `""` and a null is nothing at all. An
//! escape character alone does not quote a value. Spaces at either end are left bare, the way the
//! pin leaves them.
//!
//! In JSON, a row is one object keyed by the column names, with no spaces, which is what the pin's
//! `to_json` of the row writes. A number or a bool is bare, a decimal is written as the double it
//! casts to, a list is an array, a struct and a map are objects, and anything else is the string its
//! cast to VARCHAR reads. The rows go one to a line, or with `ARRAY` into one array with a row a
//! line after a tab, where no rows at all is an array holding one empty line.

use std::fs::File;
use std::io::{BufWriter, Write};

use rudb_bind::CopyTo;
use rudb_common::{Error, LogicalType, Result, SessionTimeZone, Value};
use rudb_kernels::cast::{cast_in_time_zone, cast_value};
use rudb_vector::Vector;

use crate::QueryResult;

/// Writes the rows of `result` to the file `copy` names and answers how many there were.
pub(crate) fn write_csv(
    copy: &CopyTo,
    result: &QueryResult,
    zone: SessionTimeZone,
) -> Result<usize> {
    let file = File::create(&copy.path)
        .map_err(|error| Error::io(format!("Cannot open file \"{}\": {error}", copy.path)))?;
    let mut out = BufWriter::new(file);
    let names = result.names();
    let forced = names
        .iter()
        .map(|name| {
            copy.force_quote_all
                || copy.force_quote.iter().any(|column| column.eq_ignore_ascii_case(name))
        })
        .collect::<Vec<_>>();
    for column in &copy.force_quote {
        if !names.iter().any(|name| name.eq_ignore_ascii_case(column)) {
            return Err(Error::binder(format!(
                "\"force_quote\" expected to find {column}, but it was not found in the table"
            )));
        }
    }
    let written = |error: std::io::Error| {
        Error::io(format!("Could not write file \"{}\": {error}", copy.path))
    };
    let mut line = String::new();
    if copy.header {
        for (column, name) in names.iter().enumerate() {
            if column > 0 {
                line.push_str(&copy.delimiter);
            }
            field(&mut line, name, false, copy);
        }
        line.push('\n');
        out.write_all(line.as_bytes()).map_err(written)?;
    }
    let mut rows = 0;
    for chunk in result.chunks() {
        let mut columns = Vec::with_capacity(chunk.width());
        for column in 0..chunk.width() {
            columns.push(cast_in_time_zone(
                chunk.column(column)?,
                &LogicalType::Varchar,
                false,
                Some(zone),
            )?);
        }
        for row in 0..chunk.len() {
            line.clear();
            for (column, vector) in columns.iter().enumerate() {
                if column > 0 {
                    line.push_str(&copy.delimiter);
                }
                match vector.try_value_at(row)? {
                    Value::Null => line.push_str(&copy.null),
                    Value::Varchar(text) => field(&mut line, &text, forced[column], copy),
                    other => field(&mut line, &other.to_string(), forced[column], copy),
                }
            }
            line.push('\n');
            out.write_all(line.as_bytes()).map_err(written)?;
        }
        rows += chunk.len();
    }
    out.flush().map_err(written)?;
    Ok(rows)
}

/// Appends one value, quoted if it has to be or was asked to be.
fn field(line: &mut String, text: &str, forced: bool, copy: &CopyTo) {
    let quoted = forced
        || text == copy.null
        || text.contains(copy.delimiter.as_str())
        || (!copy.quote.is_empty() && text.contains(copy.quote.as_str()))
        || text.contains(['\n', '\r']);
    if !quoted {
        line.push_str(text);
        return;
    }
    line.push_str(&copy.quote);
    let mut rest = text;
    while !rest.is_empty() {
        let special = [copy.quote.as_str(), copy.escape.as_str()]
            .into_iter()
            .filter(|mark| !mark.is_empty())
            .find(|mark| rest.starts_with(mark));
        if let Some(mark) = special {
            line.push_str(&copy.escape);
            line.push_str(mark);
            rest = &rest[mark.len()..];
        } else {
            let next = rest.chars().next().map_or(1, char::len_utf8);
            line.push_str(&rest[..next]);
            rest = &rest[next..];
        }
    }
    line.push_str(&copy.quote);
}

/// Writes the rows of `result` to the JSON file `copy` names and answers how many there were.
pub(crate) fn write_json(
    copy: &CopyTo,
    result: &QueryResult,
    zone: SessionTimeZone,
) -> Result<usize> {
    let file = File::create(&copy.path)
        .map_err(|error| Error::io(format!("Cannot open file \"{}\": {error}", copy.path)))?;
    let mut out = BufWriter::new(file);
    let written = |error: std::io::Error| {
        Error::io(format!("Could not write file \"{}\": {error}", copy.path))
    };
    let names = result.names();
    let types = result.types();
    if copy.array {
        out.write_all(b"[\n").map_err(written)?;
    }
    let mut line = String::new();
    let mut rows = 0;
    for chunk in result.chunks() {
        let mut columns = Vec::with_capacity(chunk.width());
        for (column, ty) in types.iter().enumerate() {
            let vector = chunk.column(column)?;
            columns.push(if quoted(ty) {
                cast_in_time_zone(vector, &LogicalType::Varchar, false, Some(zone))?
            } else {
                vector.clone()
            });
        }
        for row in 0..chunk.len() {
            line.clear();
            if copy.array {
                line.push_str(if rows == 0 { "\t" } else { ",\n\t" });
            }
            line.push('{');
            for (column, vector) in columns.iter().enumerate() {
                if column > 0 {
                    line.push(',');
                }
                string(&mut line, &names[column]);
                line.push(':');
                json(&mut line, &vector.try_value_at(row)?, &types[column], zone)?;
            }
            line.push('}');
            if !copy.array {
                line.push('\n');
            }
            out.write_all(line.as_bytes()).map_err(written)?;
            rows += 1;
        }
    }
    if copy.array {
        out.write_all(if rows == 0 { b"\t\n]\n" } else { b"\n]\n" }).map_err(written)?;
    }
    out.flush().map_err(written)?;
    Ok(rows)
}

/// Whether a value of `ty` is written as a JSON string, the text of its cast to VARCHAR.
fn quoted(ty: &LogicalType) -> bool {
    !matches!(
        ty,
        LogicalType::Null
            | LogicalType::Boolean
            | LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::HugeInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt
            | LogicalType::UHugeInt
            | LogicalType::Float
            | LogicalType::Double
            | LogicalType::Decimal { .. }
            | LogicalType::List(_)
            | LogicalType::Array(..)
            | LogicalType::Struct(_)
            | LogicalType::Map(..)
            | LogicalType::Union(_)
    )
}

/// Appends one value of type `ty`. A whole column of a quoted type arrives already cast to VARCHAR,
/// and what is nested in a list, a struct or a map is cast here, one value at a time.
fn json(line: &mut String, value: &Value, ty: &LogicalType, zone: SessionTimeZone) -> Result<()> {
    match value {
        Value::Null => line.push_str("null"),
        Value::Boolean(value) => line.push_str(if *value { "true" } else { "false" }),
        Value::TinyInt(value) => line.push_str(&value.to_string()),
        Value::SmallInt(value) => line.push_str(&value.to_string()),
        Value::Integer(value) => line.push_str(&value.to_string()),
        Value::BigInt(value) if !quoted(ty) => line.push_str(&value.to_string()),
        Value::HugeInt(value) if !quoted(ty) => line.push_str(&value.to_string()),
        Value::UTinyInt(value) => line.push_str(&value.to_string()),
        Value::USmallInt(value) => line.push_str(&value.to_string()),
        Value::UInteger(value) => line.push_str(&value.to_string()),
        Value::UBigInt(value) => line.push_str(&value.to_string()),
        Value::UHugeInt(value) => line.push_str(&value.to_string()),
        Value::Float(value) => double(line, f64::from(*value)),
        Value::Double(value) => double(line, *value),
        Value::Decimal { .. } => match cast_value(value, &LogicalType::Double, false)? {
            Value::Double(value) => double(line, value),
            other => line.push_str(&other.to_string()),
        },
        Value::Varchar(text) => string(line, text),
        Value::List { values, .. } => {
            let element = match ty {
                LogicalType::List(element) | LogicalType::Array(element, _) => element.as_ref(),
                _ => &LogicalType::Null,
            };
            line.push('[');
            for (at, value) in values.iter().enumerate() {
                if at > 0 {
                    line.push(',');
                }
                json(line, value, element, zone)?;
            }
            line.push(']');
        }
        Value::Struct(fields) if !matches!(ty, LogicalType::Union(_)) => {
            let types = match ty {
                LogicalType::Struct(types) => types.as_slice(),
                _ => &[],
            };
            line.push('{');
            for (at, (name, value)) in fields.iter().enumerate() {
                if at > 0 {
                    line.push(',');
                }
                string(line, name);
                line.push(':');
                let ty = types.get(at).map_or(&LogicalType::Null, |field| &field.ty);
                json(line, value, ty, zone)?;
            }
            line.push('}');
        }
        Value::Map { key, value: value_type, entries } => {
            line.push('{');
            for (at, (name, value)) in entries.iter().enumerate() {
                if at > 0 {
                    line.push(',');
                }
                match text(name, key, zone)? {
                    Some(name) => string(line, &name),
                    None => line.push_str("null"),
                }
                line.push(':');
                json(line, value, value_type, zone)?;
            }
            line.push('}');
        }
        Value::Struct(fields) => {
            // A union is written as whichever member it holds.
            let members = match ty {
                LogicalType::Union(members) => members.as_slice(),
                _ => &[],
            };
            let held = fields.iter().enumerate().skip(1).find(|(_, (_, value))| !value.is_null());
            match held {
                Some((at, (_, value))) => {
                    let ty = members.get(at - 1).map_or(&LogicalType::Null, |field| &field.ty);
                    json(line, value, ty, zone)?;
                }
                None => line.push_str("null"),
            }
        }
        other => match text(other, ty, zone)? {
            Some(text) => string(line, &text),
            None => line.push_str("null"),
        },
    }
    Ok(())
}

/// A value's cast to VARCHAR under the session's time zone, or `None` for a null.
fn text(value: &Value, ty: &LogicalType, zone: SessionTimeZone) -> Result<Option<String>> {
    let ty = if matches!(ty, LogicalType::Null) { value.logical_type() } else { ty.clone() };
    let vector = Vector::from_values(ty, std::slice::from_ref(value))?;
    let cast = cast_in_time_zone(&vector, &LogicalType::Varchar, false, Some(zone))?;
    Ok(match cast.try_value_at(0)? {
        Value::Null => None,
        Value::Varchar(text) => Some(text),
        other => Some(other.to_string()),
    })
}

/// Appends `text` as a JSON string, escaped the way the pin's writer escapes it.
fn string(line: &mut String, text: &str) {
    line.push('"');
    for character in text.chars() {
        match character {
            '"' => line.push_str("\\\""),
            '\\' => line.push_str("\\\\"),
            '\n' => line.push_str("\\n"),
            '\t' => line.push_str("\\t"),
            '\r' => line.push_str("\\r"),
            '\u{8}' => line.push_str("\\b"),
            '\u{c}' => line.push_str("\\f"),
            control if u32::from(control) < 0x20 => {
                line.push_str(&format!("\\u{:04X}", u32::from(control)));
            }
            other => line.push(other),
        }
    }
    line.push('"');
}

/// Appends a double the way the pin's JSON writer does: the shortest digits that read back the
/// same, laid out plainly with a `.0` on a whole number when the exponent is from -6 to 20, and as
/// `1.5e-7` or `1e21` outside that, with `Infinity`, `-Infinity` and `NaN` bare.
fn double(line: &mut String, value: f64) {
    if value.is_nan() {
        line.push_str("NaN");
        return;
    }
    if value.is_infinite() {
        line.push_str(if value > 0.0 { "Infinity" } else { "-Infinity" });
        return;
    }
    if value == 0.0 {
        line.push_str(if value.is_sign_negative() { "-0.0" } else { "0.0" });
        return;
    }
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((scientific.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits = mantissa.replace('.', "");
    if value < 0.0 {
        line.push('-');
    }
    if !(-6..=20).contains(&exponent) {
        line.push_str(mantissa);
        line.push('e');
        line.push_str(&exponent.to_string());
        return;
    }
    if exponent < 0 {
        line.push_str("0.");
        for _ in 0..(-exponent - 1) {
            line.push('0');
        }
        line.push_str(&digits);
        return;
    }
    let whole = usize::try_from(exponent).unwrap_or(0) + 1;
    if digits.len() <= whole {
        line.push_str(&digits);
        for _ in digits.len()..whole {
            line.push('0');
        }
        line.push_str(".0");
    } else {
        line.push_str(&digits[..whole]);
        line.push('.');
        line.push_str(&digits[whole..]);
    }
}

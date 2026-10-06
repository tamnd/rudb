//! Casts into and out of `VARIANT`.
//!
//! A cast in is [`rudb_common::variant::encode`], which never fails. A cast out follows the pin's
//! `from_variant.cpp`: a scalar of the kind the target names is taken as it is, any other scalar is
//! cast the strict way, a nested target reads the array or object it needs, and a nested target
//! that cannot be read that way gets one more try as the whole value cast the ordinary way. A
//! failure names the target at the top, whatever level it happened at, so `[1, 'x']` going to
//! `INTEGER[]` says it could not convert `'x'` to `'INTEGER[]'`.

use rudb_common::variant::{self, kind};
use rudb_common::{Error, Field, LogicalType, Result, SessionTimeZone, Value};

use crate::cast::{cast_value, cast_value_in_time_zone};

/// A value of a type cast to `VARIANT`, a null staying a null.
///
/// A `JSON` anywhere in the type is read as the document it is, the way a cast of `JSON` itself
/// is, so `{'a': '[1]'::JSON}` holds an array and not the text of one.
pub(crate) fn to_variant(value: &Value, from: &LogicalType) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    if !mentions_json(from) {
        return variant::encode(value).map_or(Value::Null, Value::Variant);
    }
    let mut out = Vec::new();
    write_typed(value, from, &mut out);
    Value::Variant(out)
}

/// Whether a type has a `JSON` anywhere in it.
pub(crate) fn mentions_json(ty: &LogicalType) -> bool {
    match ty {
        LogicalType::Json => true,
        LogicalType::List(element) | LogicalType::Array(element, _) => mentions_json(element),
        LogicalType::Map(key, value) => mentions_json(key) || mentions_json(value),
        LogicalType::Struct(fields) => fields.iter().any(|field| mentions_json(&field.ty)),
        _ => false,
    }
}

fn write_typed(value: &Value, ty: &LogicalType, out: &mut Vec<u8>) {
    let typed = |value: &Value, ty: &LogicalType| {
        let mut held = Vec::new();
        write_typed(value, ty, &mut held);
        held
    };
    match (value, ty) {
        (Value::Varchar(text), LogicalType::Json) => match crate::json::read(text) {
            Ok(document) => out.extend(document.variant(0)),
            Err(_) => variant::write(value, out),
        },
        (
            Value::List { values, .. },
            LogicalType::List(element) | LogicalType::Array(element, _),
        ) => {
            let children: Vec<Vec<u8>> = values.iter().map(|child| typed(child, element)).collect();
            out.extend(variant::array(&children));
        }
        (Value::Struct(values), LogicalType::Struct(fields)) if fields.len() == values.len() => {
            if Field::unnamed(fields) {
                let children: Vec<Vec<u8>> = values
                    .iter()
                    .zip(fields)
                    .map(|((_, child), field)| typed(child, &field.ty))
                    .collect();
                out.extend(variant::array(&children));
            } else {
                let entries = values
                    .iter()
                    .zip(fields)
                    .map(|((name, child), field)| (name.clone(), typed(child, &field.ty)))
                    .collect();
                out.extend(variant::object(entries));
            }
        }
        (Value::Map { entries, .. }, LogicalType::Map(key, held)) => {
            let children: Vec<Vec<u8>> = entries
                .iter()
                .map(|(name, item)| {
                    variant::object(vec![
                        ("key".to_string(), typed(name, key)),
                        ("value".to_string(), typed(item, held)),
                    ])
                })
                .collect();
            out.extend(variant::array(&children));
        }
        _ => variant::write(value, out),
    }
}

/// A variant cast to a type that is not `VARIANT`.
///
/// # Errors
///
/// The pin's sentence for the first thing that did not convert, which `TRY_CAST` turns into a
/// null, and the same for a target the pin has no cast to at all.
pub(crate) fn from_variant(
    held: &[u8],
    target: &LogicalType,
    try_cast: bool,
    zone: Option<SessionTimeZone>,
) -> Result<Value> {
    if matches!(target, LogicalType::Enum(_)) {
        return if try_cast {
            Ok(Value::Null)
        } else {
            Err(Error::conversion(format!("Unimplemented type for cast (VARIANT -> {target})")))
        };
    }
    match convert(held, target, zone) {
        Ok(value) => Ok(value),
        Err(_) if try_cast => Ok(Value::Null),
        Err(message) => Err(Error::conversion(format!("{message} to '{target}'"))),
    }
}

fn convert(
    held: &[u8],
    target: &LogicalType,
    zone: Option<SessionTimeZone>,
) -> std::result::Result<Value, String> {
    if variant::kind_of(held) == kind::NULL {
        return Ok(Value::Null);
    }
    match target {
        LogicalType::Variant => Ok(Value::Variant(held.to_vec())),
        LogicalType::Union(_) => Err("Can't convert VARIANT".to_string()),
        LogicalType::List(_)
        | LogicalType::Array(..)
        | LogicalType::Map(..)
        | LogicalType::Struct(_) => nested(held, target, zone).or_else(|message| {
            let whole = variant::unwrapped(held);
            // The pin's struct cast refuses two structs with no member in common, where ours
            // would fill the target with nulls.
            if let (Value::Struct(have), LogicalType::Struct(want)) = (&whole, target)
                && !Field::unnamed(want)
                && !want.iter().any(|field| {
                    have.iter().any(|(name, _)| name.eq_ignore_ascii_case(&field.name))
                })
            {
                return Err(message);
            }
            cast_value_in_time_zone(&whole, target, false, zone).map_err(|_| message)
        }),
        _ => scalar(held, target, zone),
    }
}

/// The children of an array, or the pin's sentence for a variant that is not one.
fn array_of(held: &[u8]) -> std::result::Result<Vec<&[u8]>, String> {
    let found = variant::kind_of(held);
    if found == kind::ARRAY {
        Ok(variant::children(held))
    } else {
        Err(format!(
            "Expected to find VARIANT(ARRAY), found VARIANT({}) instead, can't convert",
            variant::kind_name(found)
        ))
    }
}

fn nested(
    held: &[u8],
    target: &LogicalType,
    zone: Option<SessionTimeZone>,
) -> std::result::Result<Value, String> {
    match target {
        LogicalType::List(element) => {
            let values = each(array_of(held)?, element, zone)?;
            Ok(Value::List { element: element.as_ref().clone(), values })
        }
        LogicalType::Array(element, size) => {
            let children = array_of(held)?;
            if children.len() != *size as usize {
                return Err(format!(
                    "Array size '{size}' was expected, found '{}', can't convert VARIANT",
                    children.len()
                ));
            }
            let values = each(children, element, zone)?;
            Ok(Value::List { element: element.as_ref().clone(), values })
        }
        LogicalType::Map(key, value) => {
            let pair = LogicalType::Struct(vec![
                Field::new("key", key.as_ref().clone()),
                Field::new("value", value.as_ref().clone()),
            ]);
            let mut entries = Vec::new();
            for child in array_of(held)? {
                match convert(child, &pair, zone)? {
                    Value::Struct(mut fields) if fields.len() == 2 => {
                        let value = fields.pop().map(|(_, value)| value).unwrap_or(Value::Null);
                        let key = fields.pop().map(|(_, key)| key).unwrap_or(Value::Null);
                        entries.push((key, value));
                    }
                    _ => entries.push((Value::Null, Value::Null)),
                }
            }
            Ok(Value::Map { key: key.clone(), value: value.clone(), entries })
        }
        // An unnamed struct is the pin's tuple, read from an array by position.
        LogicalType::Struct(fields) if Field::unnamed(fields) => {
            let children = array_of(held)?;
            let mut values = Vec::with_capacity(fields.len());
            for (index, field) in fields.iter().enumerate() {
                let child = children.get(index).ok_or_else(|| {
                    format!(
                        "VARIANT(ARRAY) is missing element at index {index}, can't convert to TUPLE"
                    )
                })?;
                values.push((String::new(), convert(child, &field.ty, zone)?));
            }
            Ok(Value::Struct(values))
        }
        LogicalType::Struct(fields) => {
            let found = variant::kind_of(held);
            if found != kind::OBJECT {
                return Err(format!(
                    "Expected to find VARIANT(OBJECT), found VARIANT({}) instead, can't convert",
                    variant::kind_name(found)
                ));
            }
            let mut values = Vec::with_capacity(fields.len());
            for field in fields {
                let child = variant::field(held, &field.name).ok_or_else(|| {
                    let keys: Vec<&str> =
                        variant::entries(held).into_iter().map(|(key, _)| key).collect();
                    format!("VARIANT(OBJECT({})) is missing key '{}'", keys.join(","), field.name)
                })?;
                values.push((field.name.clone(), convert(child, &field.ty, zone)?));
            }
            Ok(Value::Struct(values))
        }
        _ => Err(format!("Nested type: '{target}' not handled, can't convert VARIANT")),
    }
}

/// Every element converted, or the last failure, since the pin converts them all and the message
/// it keeps is the one written last.
fn each(
    children: Vec<&[u8]>,
    element: &LogicalType,
    zone: Option<SessionTimeZone>,
) -> std::result::Result<Vec<Value>, String> {
    let mut values = Vec::with_capacity(children.len());
    let mut failed = None;
    for child in children {
        match convert(child, element, zone) {
            Ok(value) => values.push(value),
            Err(message) => failed = Some(message),
        }
    }
    failed.map_or(Ok(values), Err)
}

fn scalar(
    held: &[u8],
    target: &LogicalType,
    zone: Option<SessionTimeZone>,
) -> std::result::Result<Value, String> {
    let found = variant::kind_of(held);
    let refused = || {
        format!(
            "Can't convert VARIANT({}) value '{}'",
            variant::kind_name(found),
            variant::unwrapped(held)
        )
    };
    if found == kind::ARRAY || found == kind::OBJECT {
        // Text of a nested value is the nested value cast to text, which is the one way out.
        if *target == LogicalType::Varchar {
            return cast_value_in_time_zone(&variant::unwrapped(held), target, false, zone)
                .map_err(|_| refused());
        }
        return Err(refused());
    }
    let value = variant::decode(held);
    if value.logical_type() == *target {
        return Ok(value);
    }
    // The cast out is the strict one, which will not round text with a fraction into an integer
    // or drop the time of day from text going to a date.
    if let Value::Varchar(text) = &value
        && ((target.is_integer() && fractional(text))
            || (*target == LogicalType::Date && text.contains(':')))
    {
        return Err(refused());
    }
    cast_value_in_time_zone(&value, target, false, zone).map_err(|_| refused())
}

/// Whether text that would be read as an integer has a fraction or an exponent in it.
fn fractional(text: &str) -> bool {
    let trimmed = text.trim();
    let hex = trimmed
        .trim_start_matches(['+', '-'])
        .get(..2)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("0x"));
    !hex && trimmed.contains(['.', 'e', 'E'])
}

/// Whether a cast between the two types is one of the casts here.
///
/// A cast between a variant and `JSON` is not, since it is the document that is read or written.
#[must_use]
pub(crate) fn involved(from: &LogicalType, target: &LogicalType) -> bool {
    (*from == LogicalType::Variant) != (*target == LogicalType::Variant)
        && *from != LogicalType::Json
        && *target != LogicalType::Json
}

/// One value cast into or out of `VARIANT`.
///
/// # Errors
///
/// What [`from_variant`] reports.
pub(crate) fn cast(
    value: &Value,
    from: &LogicalType,
    target: &LogicalType,
    try_cast: bool,
    zone: Option<SessionTimeZone>,
) -> Result<Value> {
    match (value, target) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Variant(held), LogicalType::Variant) => Ok(Value::Variant(held.clone())),
        (Value::Variant(held), _) => from_variant(held, target, try_cast, zone),
        (_, LogicalType::Variant) => Ok(to_variant(value, from)),
        _ => cast_value(value, target, try_cast),
    }
}

/// `variant_typeof`, `variant_extract` and `variant_comparator`, and nothing for any other name.
///
/// A key or a position the variant does not have is a null, and so is a key asked of an array or
/// a position asked of an object. The pin types a top level null as its null kind, so
/// `variant_typeof(NULL)` is `VARIANT_NULL` and not a null.
///
/// # Errors
///
/// Position zero, since positions count from one.
pub(crate) fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let answer = match (name, args) {
        ("variant_typeof", [Value::Null]) => {
            Value::Varchar(variant::kind_name(kind::NULL).to_string())
        }
        ("variant_typeof", [Value::Variant(held)]) => Value::Varchar(variant::type_name(held)),
        ("variant_comparator", [Value::Variant(held)]) => Value::Blob(variant::sort_key_of(held)),
        ("variant_extract", [Value::Variant(held), Value::Varchar(key)]) => {
            variant::field(held, key).map_or(Value::Null, |child| Value::Variant(child.to_vec()))
        }
        ("variant_extract", [Value::Variant(_), Value::UInteger(0)]) => {
            return Err(Error::binder(
                "Extracting index 0 from VARIANT(ARRAY) is invalid, indexes are 1-based",
            ));
        }
        ("variant_extract", [Value::Variant(held), Value::UInteger(index)]) => {
            variant::element(held, *index as usize - 1)
                .map_or(Value::Null, |child| Value::Variant(child.to_vec()))
        }
        (
            "variant_keys"
            | "variant_type"
            | "variant_exists"
            | "variant_array_length"
            | "variant_extract_string",
            [Value::Variant(held), rest @ ..],
        ) => match rest {
            [] => read(name, held, "")?,
            [Value::Varchar(path)] => read(name, held, path)?,
            [Value::List { values, .. }] => {
                let mut answers = Vec::with_capacity(values.len());
                for path in values {
                    let Value::Varchar(path) = path else { return Ok(Some(Value::Null)) };
                    answers.push(read(name, held, path)?);
                }
                let element = match name {
                    "variant_keys" => LogicalType::List(Box::new(LogicalType::Varchar)),
                    "variant_exists" => LogicalType::Boolean,
                    "variant_array_length" => LogicalType::UBigInt,
                    _ => LogicalType::Varchar,
                };
                Value::List { element, values: answers }
            }
            _ => Value::Null,
        },
        ("variant_contains", [Value::Variant(haystack), Value::Variant(needle)]) => {
            Value::Boolean(contains(haystack, needle))
        }
        // Every object already holds its keys in byte order, which is all this puts right.
        ("variant_normalize", [Value::Variant(held)]) => Value::Variant(held.clone()),
        (
            "variant_typeof"
            | "variant_comparator"
            | "variant_extract"
            | "variant_keys"
            | "variant_type"
            | "variant_exists"
            | "variant_array_length"
            | "variant_extract_string"
            | "variant_contains"
            | "variant_normalize",
            _,
        ) => Value::Null,
        _ => return Ok(None),
    };
    Ok(Some(answer))
}

/// What one of the functions that take a path answers for one path.
///
/// A path is a single key matched exactly, and the empty path is the variant itself, so `'a.b'` is
/// the key `a.b` and not `b` inside `a`. A key that is not there is a null, while a key holding
/// the null kind is there.
fn read(name: &str, held: &[u8], path: &str) -> Result<Value> {
    let found = if path.is_empty() { Some(held) } else { variant::field(held, path) };
    if name == "variant_exists" {
        return Ok(Value::Boolean(found.is_some()));
    }
    let Some(found) = found else { return Ok(Value::Null) };
    Ok(match name {
        "variant_keys" => Value::List {
            element: LogicalType::Varchar,
            values: variant::entries(found)
                .into_iter()
                .map(|(key, _)| Value::Varchar(key.to_string()))
                .collect(),
        },
        "variant_type" => Value::Varchar(variant::kind_name(variant::kind_of(found)).to_string()),
        "variant_array_length" => Value::UBigInt(variant::children(found).len() as u64),
        _ => match variant::kind_of(found) {
            kind::NULL => Value::Null,
            kind::ARRAY | kind::OBJECT => Value::Varchar(crate::json::variant_document(found)?),
            _ => Value::Varchar(variant::unwrapped(found).to_string()),
        },
    })
}

/// Whether the needle is somewhere in the haystack: the haystack itself, or an element of an array
/// or the value of a key in an object anywhere inside it. Keys are never searched.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    matches(haystack, needle)
        || match variant::kind_of(haystack) {
            kind::ARRAY => variant::children(haystack).iter().any(|child| contains(child, needle)),
            kind::OBJECT => {
                variant::entries(haystack).iter().any(|(_, child)| contains(child, needle))
            }
            _ => false,
        }
}

/// Whether the needle matches the haystack where it stands.
///
/// An object matches when each of the needle's keys is in the haystack and matches there, and an
/// array matches when each of the needle's elements matches some element of the haystack, the
/// same one any number of times, so `[1]` holds `[1, 1]`. Two scalars match when they sort as one
/// value, which makes `1` and `1.0` the same and `1` and `1::DOUBLE` not, the way the pin
/// compares them. A scalar never matches an array or an object.
fn matches(haystack: &[u8], needle: &[u8]) -> bool {
    match (variant::kind_of(haystack), variant::kind_of(needle)) {
        (kind::OBJECT, kind::OBJECT) => {
            let held = variant::entries(haystack);
            variant::entries(needle).iter().all(|(key, wanted)| {
                held.iter().any(|(name, child)| name == key && matches(child, wanted))
            })
        }
        (kind::ARRAY, kind::ARRAY) => {
            let held = variant::children(haystack);
            variant::children(needle)
                .iter()
                .all(|wanted| held.iter().any(|child| matches(child, wanted)))
        }
        (kind::OBJECT | kind::ARRAY, _) | (_, kind::OBJECT | kind::ARRAY) => false,
        _ => variant::sort_key_of(haystack) == variant::sort_key_of(needle),
    }
}

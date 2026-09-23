//! Building a map and reading one, one value at a time.
//!
//! The binder in `rudb_bind::maps` has worked out every type here and cast every key to the map's
//! key type, so a key is compared with the keys already in the map as two values of one type.

use std::cmp::Ordering;

use rudb_common::{Error, LogicalType, Result, Value};

use crate::compare;

/// The calls that answer something other than null for a null argument: a lookup with a null key
/// finds nothing, and a null map joined to others is a map with nothing in it.
pub(crate) fn before_nulls(
    name: &str,
    args: &[Value],
    returns: &LogicalType,
) -> Option<Result<Value>> {
    match (name, args) {
        ("map_extract", [map, Value::Null]) if !map.is_null() => {
            Some(Ok(Value::List { element: element(returns), values: Vec::new() }))
        }
        ("map_concat", _) => Some(concat(args, returns)),
        _ => None,
    }
}

/// The rest of the map calls, once no argument is null, or `None` for any other name.
pub(crate) fn value(name: &str, args: &[Value], returns: &LogicalType) -> Option<Result<Value>> {
    let answer = match (name, args) {
        ("map", [Value::List { values: keys, .. }, Value::List { values, .. }]) => {
            if keys.len() != values.len() {
                return Some(Err(Error::invalid_input(
                    "The map key list does not align with the map value list.",
                )));
            }
            build(keys.iter().cloned().zip(values.iter().cloned()).collect(), returns)
        }
        ("map_from_entries", [Value::List { values: entries, .. }]) => {
            let mut pairs = Vec::with_capacity(entries.len());
            for entry in entries {
                match entry {
                    Value::Struct(fields) if fields.len() == 2 => {
                        pairs.push((fields[0].1.clone(), fields[1].1.clone()));
                    }
                    _ => return Some(Err(Error::invalid_input("Map keys can not be NULL."))),
                }
            }
            build(pairs, returns)
        }
        (_, [Value::Map { key, value, entries }, rest @ ..]) => match (name, rest) {
            ("map_keys", []) => Ok(Value::List {
                element: (**key).clone(),
                values: entries.iter().map(|(key, _)| key.clone()).collect(),
            }),
            ("map_values", []) => Ok(Value::List {
                element: (**value).clone(),
                values: entries.iter().map(|(_, value)| value.clone()).collect(),
            }),
            ("map_entries", []) => {
                let values = entries
                    .iter()
                    .map(|(k, v)| {
                        Value::Struct(vec![("key".into(), k.clone()), ("value".into(), v.clone())])
                    })
                    .collect();
                Ok(Value::List { element: element(returns), values })
            }
            ("cardinality", []) => Ok(Value::UBigInt(entries.len() as u64)),
            ("map_extract", [needle]) => found(entries, needle).map(|at| Value::List {
                element: element(returns),
                values: at.map(|at| entries[at].1.clone()).into_iter().collect(),
            }),
            ("map_extract_value", [needle]) => {
                found(entries, needle).map(|at| at.map_or(Value::Null, |at| entries[at].1.clone()))
            }
            ("map_contains", [needle]) => {
                found(entries, needle).map(|at| Value::Boolean(at.is_some()))
            }
            ("map_contains_value", [needle]) => {
                let mut hit = false;
                for (_, value) in entries {
                    match same(value, needle) {
                        Ok(true) => hit = true,
                        Ok(false) => {}
                        Err(error) => return Some(Err(error)),
                    }
                }
                Ok(Value::Boolean(hit))
            }
            ("map_contains_entry", [needle, wanted]) => found(entries, needle).and_then(|at| {
                Ok(Value::Boolean(match at {
                    Some(at) => same(&entries[at].1, wanted)?,
                    None => false,
                }))
            }),
            _ => return None,
        },
        _ => return None,
    };
    Some(answer)
}

/// The element type of the list a call answers.
fn element(returns: &LogicalType) -> LogicalType {
    match returns {
        LogicalType::List(element) => (**element).clone(),
        _ => LogicalType::Null,
    }
}

/// Whether two values of one type are the same value, where a null is the same as a null.
fn same(one: &Value, other: &Value) -> Result<bool> {
    Ok(compare::order(one, other)? == Ordering::Equal)
}

/// Where a key is in a map.
fn found(entries: &[(Value, Value)], needle: &Value) -> Result<Option<usize>> {
    for (at, (key, _)) in entries.iter().enumerate() {
        if same(key, needle)? {
            return Ok(Some(at));
        }
    }
    Ok(None)
}

/// A map of these pairs, refused if a key is null or appears twice.
fn build(entries: Vec<(Value, Value)>, returns: &LogicalType) -> Result<Value> {
    let LogicalType::Map(key, value) = returns else {
        return Err(Error::internal(format!("a map call returning {returns}")));
    };
    if entries.iter().any(|(key, _)| key.is_null()) {
        return Err(Error::invalid_input("Map keys can not be NULL."));
    }
    if entries.len() <= 32 {
        for (at, (key, _)) in entries.iter().enumerate() {
            for (earlier, _) in &entries[..at] {
                if same(earlier, key)? {
                    return Err(Error::invalid_input("Map keys must be unique."));
                }
            }
        }
    } else {
        let mut keys: Vec<&Value> = entries.iter().map(|(key, _)| key).collect();
        let mut failed = None;
        keys.sort_by(|a, b| {
            compare::order(a, b).unwrap_or_else(|error| {
                failed = Some(error);
                Ordering::Equal
            })
        });
        if let Some(error) = failed {
            return Err(error);
        }
        for pair in keys.windows(2) {
            if same(pair[0], pair[1])? {
                return Err(Error::invalid_input("Map keys must be unique."));
            }
        }
    }
    Ok(Value::Map { key: key.clone(), value: value.clone(), entries })
}

/// Maps joined left to right, where a key seen again takes the later value in the earlier place,
/// and a null map is skipped. All of them null is null.
fn concat(args: &[Value], returns: &LogicalType) -> Result<Value> {
    let LogicalType::Map(key, value) = returns else {
        return Err(Error::internal(format!("map_concat returning {returns}")));
    };
    let mut entries: Vec<(Value, Value)> = Vec::new();
    let mut seen = false;
    for arg in args {
        let Value::Map { entries: held, .. } = arg else {
            continue;
        };
        seen = true;
        for (k, v) in held {
            match found(&entries, k)? {
                Some(at) => entries[at].1 = v.clone(),
                None => entries.push((k.clone(), v.clone())),
            }
        }
    }
    if !seen {
        return Ok(Value::Null);
    }
    Ok(Value::Map { key: key.clone(), value: value.clone(), entries })
}

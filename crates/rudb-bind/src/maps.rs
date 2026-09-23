//! Binding the calls that build a map and read one.
//!
//! A map's type is its key type and its value type, and every call here answers a type made out of
//! those two, a list of keys or of values, a list of key and value structs, one value or a list of
//! at most one. That is a template the signature table does not express, so these are settled here
//! the same way `struct_pack` and `list_aggr` are, and the plan records the call with the type it
//! works out. The kernels are in `rudb_kernels::maps`.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_plan::{Expr, ExprRef};

use crate::binder::Binder;

/// The key and value types of a map, when `ty` is one. A null is a map of nulls to nulls, which is
/// what `map_keys(NULL)` reads it as on the pin.
fn parts(ty: &LogicalType) -> Option<(LogicalType, LogicalType)> {
    match ty {
        LogicalType::Map(key, value) => Some(((**key).clone(), (**value).clone())),
        LogicalType::Null => Some((LogicalType::Null, LogicalType::Null)),
        _ => None,
    }
}

/// The element type of a list, or null for a null, which is how `map(NULL, NULL)` binds.
fn element(ty: &LogicalType) -> Option<LogicalType> {
    match ty {
        LogicalType::List(element) => Some((**element).clone()),
        LogicalType::Null => Some(LogicalType::Null),
        _ => None,
    }
}

fn map_of(key: LogicalType, value: LogicalType) -> LogicalType {
    LogicalType::Map(Box::new(key), Box::new(value))
}

/// The struct one entry of a map is read out as by `map_entries`.
fn entry(key: LogicalType, value: LogicalType) -> LogicalType {
    LogicalType::Struct(vec![Field::new("key", key), Field::new("value", value)])
}

/// The pin's refusal of a call that fits none of the ways it can be written.
fn no_match(written: &str, types: &[LogicalType], candidates: &[&str]) -> Error {
    let types: Vec<String> = types.iter().map(ToString::to_string).collect();
    let mut message = format!(
        "No function matches the given name and argument types '{}({})'. You might need to add \
         explicit type casts.\n\tCandidate functions:",
        written.to_lowercase(),
        types.join(", ")
    );
    for candidate in candidates {
        message.push_str("\n\t");
        message.push_str(candidate);
    }
    Error::binder(message)
}

impl Binder<'_> {
    /// A bound map call, or `None` when the call is not one of these.
    pub(crate) fn map_call(&mut self, written: &str, bound: &[ExprRef]) -> Result<Option<ExprRef>> {
        let name = written.to_ascii_lowercase();
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        // A subscript on a map is `map_extract_value`, and so is `(m).a`, which arrives as
        // `struct_extract`. On anything else both are left to the list and struct calls that
        // already take them.
        let subscript = matches!(
            name.as_str(),
            "array_extract" | "list_extract" | "list_element" | "struct_extract"
        );
        if subscript && !matches!(types.first(), Some(LogicalType::Map(_, _))) {
            return Ok(None);
        }
        let name = if subscript { "map_extract_value" } else { name.as_str() };
        match name {
            "map" => self.bind_map(bound, &types).map(Some),
            "map_from_entries" => {
                let entries = match types.as_slice() {
                    [LogicalType::List(entry)] => match &**entry {
                        LogicalType::Struct(fields) if fields.len() == 2 => {
                            Some((fields[0].ty.clone(), fields[1].ty.clone()))
                        }
                        LogicalType::Null => Some((LogicalType::Null, LogicalType::Null)),
                        _ => None,
                    },
                    [LogicalType::Null] => Some((LogicalType::Null, LogicalType::Null)),
                    _ => None,
                };
                let Some((key, value)) = entries else {
                    return Err(no_match(
                        written,
                        &types,
                        &["map_from_entries(col0 TUPLE(K, V)[]) -> MAP(K, V)"],
                    ));
                };
                Ok(Some(self.record(name, bound, map_of(key, value))))
            }
            "map_keys" | "map_values" | "map_entries" | "cardinality" => {
                let Some((key, value)) = types.first().and_then(parts).filter(|_| bound.len() == 1)
                else {
                    return Ok(None);
                };
                let returns = match name {
                    "map_keys" => LogicalType::list(key),
                    "map_values" => LogicalType::list(value),
                    "map_entries" => LogicalType::list(entry(key, value)),
                    _ => LogicalType::UBigInt,
                };
                Ok(Some(self.record(name, bound, returns)))
            }
            "map_extract" | "element_at" | "map_extract_value" | "map_contains" => {
                let [map, key] = bound else {
                    return Ok(None);
                };
                let Some((key_type, value)) = parts(&types[0]) else {
                    return Ok(None);
                };
                let key = self.map_key(name, *key, &key_type)?;
                let returns = match name {
                    "map_extract_value" => value,
                    "map_contains" => LogicalType::Boolean,
                    _ => LogicalType::list(value),
                };
                let recorded = if name == "element_at" { "map_extract" } else { name };
                Ok(Some(self.record(recorded, &[*map, key], returns)))
            }
            "map_contains_value" => {
                let [map, needle] = bound else {
                    return Ok(None);
                };
                let Some((_, value)) = parts(&types[0]) else {
                    return Ok(None);
                };
                let needle = self.checked_cast_to(*needle, &value, false)?;
                Ok(Some(self.record(name, &[*map, needle], LogicalType::Boolean)))
            }
            "map_contains_entry" => {
                let [map, key, needle] = bound else {
                    return Ok(None);
                };
                let Some((key_type, value)) = parts(&types[0]) else {
                    return Ok(None);
                };
                let key = self.map_key(name, *key, &key_type)?;
                let needle = self.checked_cast_to(*needle, &value, false)?;
                Ok(Some(self.record(name, &[*map, key, needle], LogicalType::Boolean)))
            }
            "map_concat" => {
                let mut returns: Option<LogicalType> = None;
                for ty in &types {
                    if *ty == LogicalType::Null {
                        continue;
                    }
                    if !matches!(ty, LogicalType::Map(_, _)) {
                        return Ok(None);
                    }
                    match &returns {
                        None => returns = Some(ty.clone()),
                        Some(first) if first != ty => {
                            return Err(Error::invalid_input(format!(
                                "'value' type of map differs between arguments, expected \
                                 '{first}', found '{ty}' instead"
                            )));
                        }
                        Some(_) => {}
                    }
                }
                let returns =
                    returns.unwrap_or_else(|| map_of(LogicalType::Null, LogicalType::Null));
                Ok(Some(self.record(name, bound, returns)))
            }
            _ => Ok(None),
        }
    }

    /// `map()` and `map(keys, values)`, the two forms the pin has.
    fn bind_map(&mut self, bound: &[ExprRef], types: &[LogicalType]) -> Result<ExprRef> {
        let candidates =
            ["\"map\"() -> MAP(\"NULL\", \"NULL\")", "\"map\"(col0 K[], col1 V[]) -> MAP(K, V)"];
        match (bound, types) {
            ([], []) => Ok(self.add_constant(Value::Map {
                key: Box::new(LogicalType::Null),
                value: Box::new(LogicalType::Null),
                entries: Vec::new(),
            })),
            ([_, _], [keys, values]) => {
                let (Some(key), Some(value)) = (element(keys), element(values)) else {
                    return Err(no_match("map", types, &candidates));
                };
                Ok(self.record("map", bound, map_of(key, value)))
            }
            _ => Err(no_match("map", types, &candidates)),
        }
    }

    /// The key a lookup is made with, brought to the map's key type.
    ///
    /// A string key into a map of anything but strings is cast, so `m['1']` finds the key 1 and
    /// `m['x']` is the cast's own refusal. Any other key into a map of strings is the pin's refusal
    /// to deduce the key type, since there it would need a cast the other way.
    fn map_key(&mut self, name: &str, key: ExprRef, key_type: &LogicalType) -> Result<ExprRef> {
        let written = self.plan().expr_type(key).clone();
        if *key_type == LogicalType::Varchar
            && !matches!(written, LogicalType::Varchar | LogicalType::Null)
        {
            let template = if name == "map_extract_value" {
                "map_extract_value(MAP(K, V), K) -> V".to_string()
            } else {
                format!("{name}(MAP(K, V), K)")
            };
            return Err(Error::binder(format!(
                "Cannot deduce template type 'K' in function: '{template}'\nType 'K' was inferred \
                 to be:\n - 'VARCHAR', from first occurrence\n - '{written}', which is \
                 incompatible with previously inferred type!"
            )));
        }
        if *key_type == LogicalType::Null {
            return Ok(key);
        }
        self.checked_cast_to(key, key_type, false)
    }

    /// The call, recorded under `name` with the type worked out for it.
    fn record(&mut self, name: &str, args: &[ExprRef], returns: LogicalType) -> ExprRef {
        let args = self.plan_mut().add_expr_list(args);
        let recorded = self.plan_mut().intern(name);
        self.add_expr(Expr::Function { name: recorded, args }, returns)
    }
}

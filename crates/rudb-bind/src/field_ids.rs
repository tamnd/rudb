//! `FIELD_IDS`, the option of a Parquet `COPY ... TO` that gives columns the field ids their
//! schema elements carry, read the way the pin's `FieldID::GetFieldIDs` reads it.
//!
//! The option is `'auto'`, which numbers every column and every column under one from zero, depth
//! first, or a struct with a field for each column that is given an id. A field is the id, as
//! anything that casts to an integer, or a struct with the id under `__duckdb_field_id` beside the
//! ids of the columns under the column. The element of a list is `element`, and the key and the
//! value of a map are `key` and `value`. Names are matched without regard to case.

use std::collections::HashSet;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_parquet::{FieldId, member_name};

use crate::statement::Written;

/// The name a column's own id goes under when the ids of the columns under it are given too.
const ID: &str = "__duckdb_field_id";

/// The field ids `FIELD_IDS` gives the columns `names`, of the types `types`, which are the ones
/// the file is written with.
///
/// `written` is what the option was written as, and `None` when it was written with no value.
pub(crate) fn field_ids(
    written: Option<&Written>,
    names: &[String],
    types: &[LogicalType],
) -> Result<Vec<FieldId>> {
    let Some(written) = written else {
        return Err(Error::binder("FIELD_IDS requires exactly one argument"));
    };
    if let Some(error) = &written.failed {
        return Err(error.clone());
    }
    let columns = names.iter().cloned().zip(types.iter().cloned()).collect::<Vec<_>>();
    if let Some(Value::Varchar(text)) = &written.value
        && text.eq_ignore_ascii_case("auto")
    {
        return Ok(numbered(&columns, &mut 0));
    }
    if let Some(name) = names.iter().find(|name| *name == ID) {
        return Err(Error::binder(format!(
            "Cannot have a column named \"{name}\" when writing FIELD_IDS"
        )));
    }
    match &written.value {
        Some(Value::Struct(fields)) => given(fields, &columns, &mut HashSet::new()),
        None if matches!(written.ty, LogicalType::Struct(_)) => Err(Error::not_implemented(
            "COPY TO with a FIELD_IDS struct that is not a constant is not supported yet",
        )),
        _ => Err(Error::binder(format!(
            "Expected FIELD_IDS to be a STRUCT, e.g., {{col1: 42, col2: {{{ID}: 43, nested_col: \
             44}}, col3: 44}}"
        ))),
    }
}

/// The ids of `'auto'`, which number the columns from `next` on, each column before the ones
/// under it.
fn numbered(columns: &[(String, LogicalType)], next: &mut i32) -> Vec<FieldId> {
    let mut out = Vec::with_capacity(columns.len());
    for (name, ty) in columns {
        let id = Some(*next);
        *next += 1;
        let children = under(ty).map(|under| numbered(&under, next)).unwrap_or_default();
        out.push(FieldId { name: name.clone(), id, children });
    }
    out
}

/// The ids a struct of `FIELD_IDS` gives the `columns` it is about, where `seen` holds the ids
/// given so far, since no two columns may share one.
fn given(
    fields: &[(String, Value)],
    columns: &[(String, LogicalType)],
    seen: &mut HashSet<u32>,
) -> Result<Vec<FieldId>> {
    let mut out = Vec::with_capacity(fields.len());
    for (key, value) in fields {
        let name = key.to_lowercase();
        if name == ID {
            continue;
        }
        let Some((_, ty)) = columns.iter().find(|(column, _)| column.eq_ignore_ascii_case(&name))
        else {
            // The pin lists the names out of a hash map, which for a few names is the reverse of
            // the order they went in.
            let names = columns.iter().rev().map(|(column, _)| column.as_str());
            return Err(Error::binder(format!(
                "Column name \"{name}\" specified in FIELD_IDS not found. Consider using \
                 WRITE_PARTITION_COLUMNS if this column is a partition column. Available column \
                 names: [{}]",
                names.collect::<Vec<_>>().join(", ")
            )));
        };
        let (id, nested) = match value {
            Value::Struct(members) => (
                members.iter().rev().find(|(member, _)| member == ID).map(|(_, id)| id),
                members.iter().any(|(member, _)| member != ID),
            ),
            id => (Some(id), false),
        };
        let id = id.map(|id| integer(id, seen)).transpose()?;
        let mut children = Vec::new();
        if nested && let Value::Struct(members) = value {
            let Some(under) = under(ty) else {
                return Err(Error::binder(format!(
                    "Column \"{name}\" with type \"{}\" cannot have a nested FIELD_IDS \
                     specification",
                    type_id(ty)
                )));
            };
            if let LogicalType::Struct(fields) = ty
                && fields.iter().any(|field| field.name == ID)
            {
                return Err(Error::binder(format!(
                    "Cannot have column named \"{ID}\" with FIELD_IDS"
                )));
            }
            children = given(members, &under, seen)?;
        }
        out.push(FieldId { name, id, children });
    }
    Ok(out)
}

/// The id a value of `FIELD_IDS` gives, cast to an integer the way the pin casts it, which makes
/// a NULL zero, and refused if an earlier column has it.
fn integer(value: &Value, seen: &mut HashSet<u32>) -> Result<i32> {
    let id = match rudb_kernels::cast::cast_value(value, &LogicalType::Integer, false) {
        Ok(Value::Integer(id)) => Some(id),
        Ok(_) => None,
        Err(error) => {
            return Err(Error::invalid_input(format!("Failed to cast value: {}", error.message())));
        }
    };
    if !seen.insert(id.unwrap_or_default() as u32) {
        let id = id.map_or_else(|| "NULL".to_string(), |id| id.to_string());
        return Err(Error::binder(format!("Duplicate field_id {id} found in FIELD_IDS")));
    }
    Ok(id.unwrap_or_default())
}

/// The columns under a column of the type `ty` that ids can be given to, or `None` for a type
/// that has none. An array has none here, although it is written as a list.
fn under(ty: &LogicalType) -> Option<Vec<(String, LogicalType)>> {
    match ty {
        LogicalType::List(element) => Some(vec![("element".to_string(), (**element).clone())]),
        LogicalType::Map(key, value) => Some(vec![
            ("key".to_string(), (**key).clone()),
            ("value".to_string(), (**value).clone()),
        ]),
        LogicalType::Struct(fields) => Some(
            fields
                .iter()
                .enumerate()
                .map(|(at, field)| (member_name(&field.name, at).into_owned(), field.ty.clone()))
                .collect(),
        ),
        _ => None,
    }
}

/// The name of the kind of type `ty` is, without what it is parameterised by, which is what the
/// pin names a type with in its errors about `FIELD_IDS`.
fn type_id(ty: &LogicalType) -> String {
    if let LogicalType::Array(..) = ty {
        return "ARRAY".to_string();
    }
    let written = ty.to_string();
    written.split('(').next().unwrap_or_default().to_string()
}

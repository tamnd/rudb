//! The list functions that look inside a list: searching it, deduplicating it, picking from it and
//! reshaping it.
//!
//! `list_position`, `list_contains`, `list_has_any`, `list_has_all`, `list_distinct`,
//! `list_unique`, `list_intersect`, `list_where`, `list_select`, `list_reverse`, `list_sort`,
//! `list_reverse_sort`, `flatten` and `list_resize`. The binder has already cast every list to one
//! element type and every argument to what it has to be, so nothing here converts a value, and each
//! answer was read off the pin case by case.
//!
//! Three of them answer something other than null for a null argument and are called above the
//! null rule. `list_position(l, NULL)` finds the first null element, since the search is `IS NOT
//! DISTINCT FROM` and not `=`, `list_resize(l, NULL)` is the empty list, and so is
//! `list_intersect(l, NULL)`. Everything else is null in and null out.
//!
//! Values are compared with [`order`], the one ordering every other operator uses, so a list of
//! doubles deduplicates `-0.0` and `0.0` the way a `GROUP BY` does. The pin keeps the elements of
//! `list_distinct` and `list_intersect` in the order of its hash table, which is not an order a
//! caller can rely on and is not one rudb copies. Both keep the order of first appearance here,
//! which is one of the orders the pin could have given.

use std::cmp::Ordering;

use rudb_common::{Error, LogicalType, Result, Value};

use crate::compare::order;

/// What the pin says when the mask of `list_where` or the indexes of `list_select` hold a null.
const NULL_PICK: &str = "NULLs are not allowed as list elements in the second input parameter.";

/// The answer for a call that has to see its null arguments, or `None` for any other call.
pub(crate) fn before_nulls(
    name: &str,
    args: &[Value],
    returns: &LogicalType,
) -> Option<Result<Value>> {
    Some(match (name, args) {
        ("list_position", [Value::Null, _]) => Ok(Value::Null),
        ("list_position", [Value::List { values, .. }, needle]) => position(values, needle),
        ("list_resize" | "list_intersect", [Value::Null, ..]) => Ok(Value::Null),
        ("list_intersect", [Value::List { .. }, Value::Null]) => listed(Vec::new(), returns),
        ("list_resize", [Value::List { values, .. }, size, filler @ ..]) => {
            resize(values, size, filler.first().unwrap_or(&Value::Null), returns)
        }
        _ => return None,
    })
}

/// The answer for a call none of whose arguments is null, or `None` for a name that is not here.
pub(crate) fn value(name: &str, args: &[Value], returns: &LogicalType) -> Option<Result<Value>> {
    let list = |values| listed(values, returns);
    Some(match (name, args) {
        ("list_contains", [Value::List { values, .. }, needle]) => {
            found(values, needle).map(|at| Value::Boolean(at.is_some()))
        }
        ("list_has_any", [Value::List { values, .. }, Value::List { values: wanted, .. }]) => {
            has_any(values, wanted).map(Value::Boolean)
        }
        ("list_has_all", [Value::List { values, .. }, Value::List { values: wanted, .. }]) => {
            has_any_missing(values, wanted).map(|missing| Value::Boolean(!missing))
        }
        ("list_distinct", [Value::List { values, .. }]) => distinct(values).and_then(&list),
        ("list_unique", [Value::List { values, .. }]) => {
            distinct(values).map(|kept| Value::UBigInt(kept.len() as u64))
        }
        ("list_intersect", [Value::List { values, .. }, Value::List { values: other, .. }]) => {
            intersect(values, other).and_then(&list)
        }
        ("list_where", [Value::List { values, .. }, Value::List { values: mask, .. }]) => {
            masked(values, mask).and_then(&list)
        }
        ("list_select", [Value::List { values, .. }, Value::List { values: indexes, .. }]) => {
            selected(values, indexes).and_then(&list)
        }
        ("list_sort", [Value::List { values, .. }, spelled @ ..]) => {
            let order = spelled.first().map(spelled_order).transpose();
            let nulls = spelled.get(1).map(spelled_nulls).transpose();
            match (order, nulls) {
                (Ok(order), Ok(nulls)) => {
                    sort(values, order.unwrap_or(false), nulls.unwrap_or(false)).and_then(list)
                }
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        }
        ("list_reverse_sort", [Value::List { values, .. }, spelled @ ..]) => {
            match spelled.first().map(spelled_nulls).transpose() {
                Ok(nulls) => sort(values, true, nulls.unwrap_or(false)).and_then(list),
                Err(error) => Err(error),
            }
        }
        ("list_reverse", [Value::List { values, .. }]) => {
            list(values.iter().rev().cloned().collect())
        }
        ("flatten", [Value::List { values, .. }]) => {
            let mut flat = Vec::new();
            for inner in values {
                if let Value::List { values: held, .. } = inner {
                    flat.extend(held.iter().cloned());
                }
            }
            list(flat)
        }
        _ => return None,
    })
}

/// A list of the element type the call answers in.
fn listed(values: Vec<Value>, returns: &LogicalType) -> Result<Value> {
    let LogicalType::List(element) = returns else {
        return Err(Error::internal(format!("a list function returning {returns}")));
    };
    Ok(Value::List { element: (**element).clone(), values })
}

/// Whether two values are the same value, with a null the same as another null.
fn same(left: &Value, right: &Value) -> Result<bool> {
    Ok(match (left.is_null(), right.is_null()) {
        (true, true) => true,
        (true, false) | (false, true) => false,
        (false, false) => order(left, right)? == Ordering::Equal,
    })
}

/// Where `needle` first is in `values`, nulls matching nulls.
fn found(values: &[Value], needle: &Value) -> Result<Option<usize>> {
    for (at, value) in values.iter().enumerate() {
        if same(value, needle)? {
            return Ok(Some(at));
        }
    }
    Ok(None)
}

/// `list_position`, one based, or null when the value is not there.
fn position(values: &[Value], needle: &Value) -> Result<Value> {
    Ok(match found(values, needle)? {
        Some(at) => Value::Integer(i32::try_from(at + 1).map_err(|_| {
            Error::out_of_range(format!("a list position of {} does not fit in INTEGER", at + 1))
        })?),
        None => Value::Null,
    })
}

/// The values that are not null, sorted, so that a lookup is a binary search.
fn sorted(values: &[Value]) -> Result<Vec<&Value>> {
    let mut held: Vec<&Value> = values.iter().filter(|value| !value.is_null()).collect();
    let mut failed = None;
    held.sort_by(|left, right| {
        order(left, right).unwrap_or_else(|error| {
            failed.get_or_insert(error);
            Ordering::Equal
        })
    });
    match failed {
        Some(error) => Err(error),
        None => Ok(held),
    }
}

/// Whether `needle`, which is not null, is in `haystack`, which [`sorted`] made.
fn contains(haystack: &[&Value], needle: &Value) -> Result<bool> {
    let mut failed = None;
    let hit = haystack
        .binary_search_by(|probe| {
            order(probe, needle).unwrap_or_else(|error| {
                failed.get_or_insert(error);
                Ordering::Equal
            })
        })
        .is_ok();
    match failed {
        Some(error) => Err(error),
        None => Ok(hit),
    }
}

/// `list_has_any`: whether some value that is not null is in both lists.
fn has_any(values: &[Value], wanted: &[Value]) -> Result<bool> {
    let haystack = sorted(values)?;
    for value in wanted.iter().filter(|value| !value.is_null()) {
        if contains(&haystack, value)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether some value of `wanted` that is not null is missing from `values`, which is `list_has_all`
/// turned over. A null in `wanted` is not asked about, so `list_has_all([1], [NULL])` is true.
fn has_any_missing(values: &[Value], wanted: &[Value]) -> Result<bool> {
    let haystack = sorted(values)?;
    for value in wanted.iter().filter(|value| !value.is_null()) {
        if !contains(&haystack, value)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The values that are not null, each once, in the order they first appear.
fn distinct(values: &[Value]) -> Result<Vec<Value>> {
    let mut at: Vec<usize> = (0..values.len()).filter(|&at| !values[at].is_null()).collect();
    let mut failed = None;
    // Sorted by value and then by position, so the first of each run of equal values is the one
    // that appeared first.
    at.sort_by(|&left, &right| match order(&values[left], &values[right]) {
        Ok(Ordering::Equal) => left.cmp(&right),
        Ok(ordering) => ordering,
        Err(error) => {
            failed.get_or_insert(error);
            Ordering::Equal
        }
    });
    if let Some(error) = failed {
        return Err(error);
    }
    let mut kept = Vec::with_capacity(at.len());
    for (index, &here) in at.iter().enumerate() {
        if index == 0 || order(&values[at[index - 1]], &values[here])? != Ordering::Equal {
            kept.push(here);
        }
    }
    kept.sort_unstable();
    Ok(kept.into_iter().map(|at| values[at].clone()).collect())
}

/// `list_intersect`: the values of the first list that are also in the second, each once.
fn intersect(values: &[Value], other: &[Value]) -> Result<Vec<Value>> {
    let haystack = sorted(other)?;
    let mut kept = Vec::new();
    for value in distinct(values)? {
        if contains(&haystack, &value)? {
            kept.push(value);
        }
    }
    Ok(kept)
}

/// `list_where`: the values whose place in the mask is true. A mask longer than the list picks
/// nulls past its end, so `list_where([1], [true, true])` is `[1, NULL]`.
fn masked(values: &[Value], mask: &[Value]) -> Result<Vec<Value>> {
    let mut kept = Vec::new();
    for (at, flag) in mask.iter().enumerate() {
        match flag {
            Value::Boolean(true) => kept.push(values.get(at).cloned().unwrap_or(Value::Null)),
            Value::Boolean(false) => {}
            Value::Null => return Err(Error::invalid_input(NULL_PICK)),
            other => {
                return Err(Error::internal(format!(
                    "list_where with a {} mask",
                    other.logical_type()
                )));
            }
        }
    }
    Ok(kept)
}

/// `list_select`: the values at the given one based places, and a null for a place that is not in
/// the list. A negative place does not count from the end here, unlike a subscript.
fn selected(values: &[Value], indexes: &[Value]) -> Result<Vec<Value>> {
    let mut kept = Vec::with_capacity(indexes.len());
    for index in indexes {
        if index.is_null() {
            return Err(Error::invalid_input(NULL_PICK));
        }
        let picked = index
            .as_i64()
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| index.checked_sub(1))
            .and_then(|at| values.get(at));
        kept.push(picked.cloned().unwrap_or(Value::Null));
    }
    Ok(kept)
}

/// `list_resize`: the list cut or padded to `size`, padding with `filler`. A null size is the empty
/// list, which is the pin's answer and not a null one.
fn resize(values: &[Value], size: &Value, filler: &Value, returns: &LogicalType) -> Result<Value> {
    let size = match size {
        Value::Null => 0,
        Value::UBigInt(size) => usize::try_from(*size).map_err(|_| {
            Error::out_of_range(format!("a list of {size} elements is too long to build"))
        })?,
        other => {
            return Err(Error::internal(format!("list_resize to a {}", other.logical_type())));
        }
    };
    let element = match returns {
        LogicalType::List(element) => element,
        _ => &LogicalType::Null,
    };
    let width = element.physical().size().max(1);
    if (size as u128) * (width as u128) > MAX_VECTOR_BYTES {
        return Err(Error::out_of_range(format!(
            "Cannot resize vector to {size} rows: maximum allowed vector size is 128.0 GiB"
        )));
    }
    let mut kept: Vec<Value> = values.iter().take(size).cloned().collect();
    kept.resize(size, filler.clone());
    listed(kept, returns)
}

/// The most bytes the pin lets one vector hold. A `list_resize` past it is refused before anything
/// is allocated, so a size of a few quintillion is an error and not an abort.
const MAX_VECTOR_BYTES: u128 = 1 << 37;

/// Whether a sort order spelled out as a string is descending.
fn spelled_order(spelled: &Value) -> Result<bool> {
    let spelled = spelled.to_string().to_uppercase();
    match spelled.as_str() {
        "ASC" | "ASCENDING" | "DEFAULT" | "ORDER_DEFAULT" => Ok(false),
        "DESC" | "DESCENDING" => Ok(true),
        _ => Err(unrecognized(&spelled, "OrderType")),
    }
}

/// Whether a null order spelled out as a string puts the nulls first.
fn spelled_nulls(spelled: &Value) -> Result<bool> {
    let spelled = spelled.to_string().to_uppercase();
    match spelled.as_str() {
        "NULLS FIRST" | "NULLS_FIRST" => Ok(true),
        "NULLS LAST" | "NULLS_LAST" | "DEFAULT" | "ORDER_DEFAULT" => Ok(false),
        _ => Err(unrecognized(&spelled, "OrderByNullType")),
    }
}

/// The pin's refusal of a name that is not one of an enum's values. The pin follows it with a line
/// of candidates picked by how close they are to what was written, which is left out here.
fn unrecognized(spelled: &str, kind: &str) -> Error {
    Error::not_implemented(format!(
        "Enum value: unrecognized value \"{spelled}\" for enum \"{kind}\""
    ))
}

/// `list_sort`: the values in order, with the nulls kept together at one end.
///
/// The nulls go last unless asked otherwise whichever way the rest are sorted, which is the pin's
/// default and not the reverse of an ascending sort.
fn sort(values: &[Value], descending: bool, nulls_first: bool) -> Result<Vec<Value>> {
    let mut held: Vec<Value> = values.iter().filter(|value| !value.is_null()).cloned().collect();
    let nulls = values.len() - held.len();
    let mut failed = None;
    held.sort_by(|left, right| {
        let ordering = order(left, right).unwrap_or_else(|error| {
            failed.get_or_insert(error);
            Ordering::Equal
        });
        if descending { ordering.reverse() } else { ordering }
    });
    if let Some(error) = failed {
        return Err(error);
    }
    let mut sorted = Vec::with_capacity(values.len());
    if nulls_first {
        sorted.resize(nulls, Value::Null);
    }
    sorted.append(&mut held);
    if !nulls_first {
        sorted.resize(values.len(), Value::Null);
    }
    Ok(sorted)
}

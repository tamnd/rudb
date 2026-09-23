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
use std::collections::HashSet;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Buffer, Data, Live, Validity, Vector, interleave};

use crate::compare::order;
use crate::datetime;
use crate::number::integral;

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
        ("range" | "generate_series", _) => ranged(name == "generate_series", args).and_then(list),
        ("list_grade_up", [Value::List { values, .. }, spelled @ ..]) => {
            let order = spelled.first().map(spelled_order).transpose();
            let nulls = spelled.get(1).map(spelled_nulls).transpose();
            match (order, nulls) {
                (Ok(order), Ok(nulls)) => {
                    graded(values, order.unwrap_or(false), nulls.unwrap_or(false)).and_then(list)
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

/// `range` and `generate_series` as scalars: the series from the start toward the stop as a list.
///
/// `generate_series` takes the stop when a step lands on it and `range` stops short of it. One
/// argument is the stop, with a start of zero and a step of one. A step of zero, or one that points
/// away from the stop, is an empty list rather than an error, which is the pin's answer.
fn ranged(inclusive: bool, args: &[Value]) -> Result<Vec<Value>> {
    if let [start, stop, Value::Interval { months, days, micros }] = args {
        return stepped(inclusive, start, stop, (*months, *days, *micros));
    }
    let whole = |value: &Value| {
        integral(value)
            .and_then(|held| i64::try_from(held).ok())
            .ok_or_else(|| Error::internal(format!("a range over a {}", value.logical_type())))
    };
    let (start, stop, step) = match args {
        [stop] => (0, whole(stop)?, 1),
        [start, stop] => (whole(start)?, whole(stop)?, 1),
        [start, stop, step] => (whole(start)?, whole(stop)?, whole(step)?),
        _ => return Err(Error::internal(format!("a range over {} arguments", args.len()))),
    };
    let count = series_length(start, stop, step, inclusive)?;
    // Every value is between the start and the stop, both of which are BIGINTs, so none of these
    // can leave the type.
    let mut at = start;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(Value::BigInt(at));
        at = at.wrapping_add(step);
    }
    Ok(values)
}

/// How many values an integer series has, counted wide so the gap between the two ends of BIGINT
/// does not overflow.
fn series_length(start: i64, stop: i64, step: i64, inclusive: bool) -> Result<usize> {
    if step == 0 || (start > stop && step > 0) || (start < stop && step < 0) {
        return Ok(0);
    }
    let apart = (i128::from(stop) - i128::from(start)).unsigned_abs();
    let by = i128::from(step).unsigned_abs();
    let mut count = apart / by;
    if inclusive || apart % by != 0 {
        count += 1;
    }
    usize::try_from(count).ok().filter(|&count| count <= MAX_SERIES).ok_or_else(too_long)
}

/// A series of moments, each one the last with the interval added, which is how the pin steps and
/// why a step of a month from the thirty first lands where adding a month would.
fn stepped(
    inclusive: bool,
    start: &Value,
    stop: &Value,
    (months, days, micros): (i32, i32, i64),
) -> Result<Vec<Value>> {
    let forward = months > 0 || days > 0 || micros > 0;
    let backward = months < 0 || days < 0 || micros < 0;
    if forward && backward {
        return Err(Error::invalid_input(
            "Interval with mix of negative/positive entries not supported",
        ));
    }
    let moment = |value: &Value| match value {
        Value::Timestamp(stamp) | Value::TimestampTz(stamp) => Ok(*stamp),
        other => Err(Error::internal(format!("a range from a {}", other.logical_type()))),
    };
    let end = moment(stop)?;
    let step = Value::Interval { months, days, micros };
    let mut values = Vec::new();
    let mut at = start.clone();
    if !forward && !backward {
        return Ok(values);
    }
    loop {
        let stamp = moment(&at)?;
        let past = if forward { stamp > end } else { stamp < end };
        if past || (stamp == end && !inclusive) {
            return Ok(values);
        }
        if values.len() == MAX_SERIES {
            return Err(too_long());
        }
        let next = datetime::shift(&at, &step, false)?;
        values.push(at);
        at = next;
    }
}

/// The longest list the pin builds for a series, which is the most entries a list can hold.
const MAX_SERIES: usize = u32::MAX as usize;

/// The pin's refusal of a series longer than [`MAX_SERIES`].
fn too_long() -> Error {
    Error::invalid_input("Lists larger than 2^32 elements are not supported")
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
fn sort(values: &[Value], descending: bool, nulls_first: bool) -> Result<Vec<Value>> {
    Ok(grade(values, descending, nulls_first)?.into_iter().map(|at| values[at].clone()).collect())
}

/// `list_grade_up`: the one based place of each value in the order `list_sort` would put it.
fn graded(values: &[Value], descending: bool, nulls_first: bool) -> Result<Vec<Value>> {
    grade(values, descending, nulls_first)?
        .into_iter()
        .map(|at| {
            Ok(Value::BigInt(
                i64::try_from(at + 1).map_err(|error| Error::internal(error.to_string()))?,
            ))
        })
        .collect()
}

/// The places of the values in sorted order, with the nulls kept together at one end.
///
/// The nulls go last unless asked otherwise whichever way the rest are sorted, which is the pin's
/// default and not the reverse of an ascending sort. The sort is stable, so equal values keep the
/// order they came in, which is what makes the grade of a list with repeats the pin's.
fn grade(values: &[Value], descending: bool, nulls_first: bool) -> Result<Vec<usize>> {
    let (mut held, nulls): (Vec<usize>, Vec<usize>) =
        (0..values.len()).partition(|&at| !values[at].is_null());
    let mut failed = None;
    held.sort_by(|&left, &right| {
        let ordering = order(&values[left], &values[right]).unwrap_or_else(|error| {
            failed.get_or_insert(error);
            Ordering::Equal
        });
        if descending { ordering.reverse() } else { ordering }
    });
    if let Some(error) = failed {
        return Err(error);
    }
    Ok(if nulls_first { [nulls, held].concat() } else { [held, nulls].concat() })
}

/// A loop over whole vectors for the list calls that have one, or `None` for a call that goes
/// through the row at a time path.
///
/// A list vector is entries over one child, so building a list, reversing one or searching one for
/// a constant can be a gather or a scan of the child with no `Value` made for any row. These are
/// the calls that were furthest behind the pin when measured, and each one here gives the same
/// answer as its arm in [`value`], which the tests in the facade check row for row.
pub(crate) fn vectorized<V: AsRef<Vector>>(
    name: &str,
    args: &[V],
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    match (name, args) {
        ("list_value", [_, ..]) => built(args, returns, rows),
        ("range" | "generate_series", [_, ..]) => series(name == "generate_series", args, rows),
        ("list_reverse", [list]) => reversed(list.as_ref()),
        ("length" | "array_length", [list]) => counted(list.as_ref()),
        ("list_distinct", [list]) => deduplicated(false, list.as_ref()),
        ("list_unique", [list]) => deduplicated(true, list.as_ref()),
        ("list_contains" | "list_position", [list, needle]) => {
            searched(name == "list_position", list.as_ref(), needle.as_ref())
        }
        ("list_sort" | "list_grade_up", [list, spelled @ ..]) => ordered(
            list.as_ref(),
            spelled.first().map(AsRef::as_ref),
            spelled.get(1).map(AsRef::as_ref),
            false,
            name == "list_grade_up",
        ),
        ("list_reverse_sort", [list, spelled @ ..]) => {
            ordered(list.as_ref(), None, spelled.first().map(AsRef::as_ref), true, false)
        }
        _ => Ok(None),
    }
}

/// `list_value` over columns: every argument laid end to end and read back a row at a time.
///
/// Left to the row path when the element is nested, because laying a nested column is a row at a
/// time there too, or when an argument is not already of the element type.
fn built<V: AsRef<Vector>>(
    args: &[V],
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    let LogicalType::List(element) = returns else {
        return Ok(None);
    };
    if nested_or_null(element) || args.iter().any(|arg| arg.as_ref().logical_type() != &**element) {
        return Ok(None);
    }
    let pieces: Vec<Vector> =
        args.iter().map(|arg| arg.as_ref().flatten()).collect::<Result<_>>()?;
    let width = args.len();
    let order: Vec<usize> =
        (0..rows).flat_map(|row| (0..width).map(move |at| at * rows + row)).collect();
    let child = interleave(element, &pieces, &order)?;
    let count = entry(width)?;
    let entries = (0..rows).map(|row| Ok((entry(row * width)?, count))).collect::<Result<_>>()?;
    Vector::list(entries, child).map(Some)
}

/// `range` and `generate_series` over integer columns, with every row's series written straight
/// into one child and no `Value` made for any element. A row with a null argument is a null list.
fn series<V: AsRef<Vector>>(inclusive: bool, args: &[V], rows: usize) -> Result<Option<Vector>> {
    if args.iter().any(|arg| arg.as_ref().logical_type() != &LogicalType::BigInt) {
        return Ok(None);
    }
    let flat: Vec<Vector> = args.iter().map(|arg| arg.as_ref().flatten()).collect::<Result<_>>()?;
    let mut columns = Vec::with_capacity(flat.len());
    for vector in &flat {
        let Some(Data::Int64(values)) = vector.data() else {
            return Ok(None);
        };
        columns.push((values.as_slice(), vector.validity().live()));
    }
    let mut entries = Vec::with_capacity(rows);
    let mut child = Vec::new();
    let mut live = vec![true; rows];
    for row in 0..rows {
        let mut held = [0_i64; 3];
        let mut null = false;
        for (at, (values, live)) in columns.iter().enumerate() {
            match values.get(row) {
                Some(&value) if live.at(row) => held[at] = value,
                _ => null = true,
            }
        }
        let at = entry(child.len())?;
        if null {
            entries.push((at, 0));
            live[row] = false;
            continue;
        }
        let (start, stop, step) = match columns.len() {
            1 => (0, held[0], 1),
            2 => (held[0], held[1], 1),
            _ => (held[0], held[1], held[2]),
        };
        let count = series_length(start, stop, step, inclusive)?;
        child.reserve(count);
        let mut value = start;
        for _ in 0..count {
            child.push(value);
            value = value.wrapping_add(step);
        }
        entries.push((at, entry(count)?));
    }
    let child = Vector::flat(LogicalType::BigInt, Data::Int64(Buffer::from(child)))?;
    let validity = Validity::from_iter(rows, |row| live[row]).normalize(rows);
    Ok(Some(Vector::list(entries, child)?.with_validity(validity)))
}

/// `length` of a list column, which is every entry's length with the column's nulls.
fn counted(list: &Vector) -> Result<Option<Vector>> {
    let Some((entries, _)) = list.list_parts() else {
        return Ok(None);
    };
    if !matches!(list.logical_type(), LogicalType::List(_)) {
        return Ok(None);
    }
    let lengths: Vec<i64> = entries.iter().map(|&(_, len)| i64::from(len)).collect();
    let answer = Vector::flat(LogicalType::BigInt, Data::Int64(Buffer::from(lengths)))?;
    Ok(Some(answer.with_validity(list.validity().clone())))
}

/// `list_reverse` over a column: one gather of the child with every row's run turned round.
fn reversed(list: &Vector) -> Result<Option<Vector>> {
    let Some((entries, child)) = list.list_parts() else {
        return Ok(None);
    };
    if !matches!(list.logical_type(), LogicalType::List(_)) {
        return Ok(None);
    }
    let live = list.validity().live();
    let mut indices = Vec::with_capacity(child.len());
    let mut placed = Vec::with_capacity(entries.len());
    for (row, &(start, len)) in entries.iter().enumerate() {
        let at = entry(indices.len())?;
        if live.at(row) {
            indices.extend((start..start + len).rev());
            placed.push((at, len));
        } else {
            placed.push((at, 0));
        }
    }
    let child = child.gather(&indices)?;
    Ok(Some(Vector::list(placed, child)?.with_validity(list.validity().clone())))
}

/// `list_contains` and `list_position` over an integer column with a constant needle, as one scan
/// of the child.
///
/// A null needle is left to the row path, since `list_position` finds a null element with it and
/// `list_contains` is null, and so is anything that is not a plain integer, where equality is not
/// the same thing as equal bits.
fn searched(position: bool, list: &Vector, needle: &Vector) -> Result<Option<Vector>> {
    let (Some((entries, child)), Some(wanted)) = (list.list_parts(), needle.constant_value())
    else {
        return Ok(None);
    };
    let plain = plain(child.logical_type());
    let Some(wanted) =
        integral(wanted).filter(|_| plain && needle.logical_type() == child.logical_type())
    else {
        return Ok(None);
    };
    let elements = child.validity().live();
    macro_rules! scan {
        ($($variant:ident),+) => {
            match child.data() {
                $(Some(Data::$variant(values)) => {
                    first_places(entries, values.as_slice(), elements, wanted)
                })+
                _ => return Ok(None),
            }
        };
    }
    let found = scan!(Int8, Int16, Int32, Int64, UInt8, UInt16, UInt32, UInt64);
    let rows = list.validity().live();
    if position {
        let validity = Validity::from_iter(entries.len(), |row| rows.at(row) && found[row] != 0);
        let data =
            Data::Int32(Buffer::from(found.iter().map(|&place| place as i32).collect::<Vec<_>>()));
        let answer = Vector::flat(LogicalType::Integer, data)?;
        return Ok(Some(answer.with_validity(validity.normalize(entries.len()))));
    }
    let data = Data::Bool(Buffer::from(found.iter().map(|&place| place != 0).collect::<Vec<_>>()));
    let answer = Vector::flat(LogicalType::Boolean, data)?;
    Ok(Some(answer.with_validity(list.validity().clone())))
}

/// `list_sort`, `list_reverse_sort` and `list_grade_up` over an integer column: each row's run of
/// the child sorted as indices, and the child gathered once in that order. A grade answers with the
/// places themselves and gathers nothing.
///
/// The order and null order are constants, which the binder insists on, so they are read once for
/// the whole vector. A null one is left to the row path, where it makes every row null. So is a
/// vector with no row that is not null, because the row path never reads the order for those and
/// so never refuses a bad one.
fn ordered(
    list: &Vector,
    order: Option<&Vector>,
    nulls: Option<&Vector>,
    reverse: bool,
    grade: bool,
) -> Result<Option<Vector>> {
    let Some((entries, child)) = list.list_parts() else {
        return Ok(None);
    };
    if !plain(child.logical_type()) || list.validity().count_valid(list.len()) == 0 {
        return Ok(None);
    }
    let spelled = |arg: Option<&Vector>| match arg.map(Vector::constant_value) {
        None => Some(None),
        Some(Some(value @ Value::Varchar(_))) => Some(Some(value.clone())),
        Some(_) => None,
    };
    let (Some(order), Some(nulls)) = (spelled(order), spelled(nulls)) else {
        return Ok(None);
    };
    let descending = reverse || order.as_ref().map(spelled_order).transpose()?.unwrap_or(false);
    let nulls_first = nulls.as_ref().map(spelled_nulls).transpose()?.unwrap_or(false);
    let rows = list.validity().live();
    let elements = child.validity().live();
    macro_rules! permute {
        ($($variant:ident),+) => {
            match child.data() {
                $(Some(Data::$variant(values)) => {
                    let values = values.as_slice();
                    permutation(entries, rows, elements, nulls_first, |left, right| {
                        let ordering = values[left as usize].cmp(&values[right as usize]);
                        if descending { ordering.reverse() } else { ordering }
                    })?
                })+
                _ => return Ok(None),
            }
        };
    }
    let (placed, indices) = permute!(Int8, Int16, Int32, Int64, UInt8, UInt16, UInt32, UInt64);
    let child = if grade {
        // A grade is each index less the start of its row's run, counted from one.
        let mut places = Vec::with_capacity(indices.len());
        for (&(at, len), &(start, _)) in placed.iter().zip(entries) {
            let run = &indices[at as usize..(at + len) as usize];
            places.extend(run.iter().map(|&index| i64::from(index - start) + 1));
        }
        Vector::flat(LogicalType::BigInt, Data::Int64(Buffer::from(places)))?
    } else {
        child.gather(&indices)?
    };
    Ok(Some(Vector::list(placed, child)?.with_validity(list.validity().clone())))
}

/// `list_distinct` and `list_unique` over an integer column, as one pass over each row's run that
/// keeps the first appearance of every value that is not null.
fn deduplicated(unique: bool, list: &Vector) -> Result<Option<Vector>> {
    let Some((entries, child)) = list.list_parts() else {
        return Ok(None);
    };
    if !plain(child.logical_type()) {
        return Ok(None);
    }
    let rows = list.validity().live();
    let elements = child.validity().live();
    macro_rules! keep {
        ($($variant:ident),+) => {
            match child.data() {
                $(Some(Data::$variant(values)) => {
                    let values = values.as_slice();
                    firsts(entries, rows, elements, |at| i128::from(values[at as usize]))?
                })+
                _ => return Ok(None),
            }
        };
    }
    let (placed, indices) = keep!(Int8, Int16, Int32, Int64, UInt8, UInt16, UInt32, UInt64);
    if unique {
        let counts: Vec<u64> = placed.iter().map(|&(_, len)| u64::from(len)).collect();
        let answer = Vector::flat(LogicalType::UBigInt, Data::UInt64(Buffer::from(counts)))?;
        return Ok(Some(answer.with_validity(list.validity().clone())));
    }
    let child = child.gather(&indices)?;
    Ok(Some(Vector::list(placed, child)?.with_validity(list.validity().clone())))
}

/// The new entries and the child indices that keep the first element of every `key` in each row's
/// run and drop the nulls, which is what [`distinct`] does with values.
///
/// A short run is checked against what it has kept so far, which for the lists people write is a
/// handful of comparisons and no allocation. A long one goes through a set.
fn firsts(
    entries: &[(u32, u32)],
    rows: Live<'_>,
    elements: Live<'_>,
    key: impl Fn(u32) -> i128,
) -> Result<Permuted> {
    const SHORT: u32 = 32;
    let mut indices = Vec::new();
    let mut placed = Vec::with_capacity(entries.len());
    let mut kept: Vec<i128> = Vec::new();
    let mut seen: HashSet<i128> = HashSet::new();
    for (row, &(start, len)) in entries.iter().enumerate() {
        let at = entry(indices.len())?;
        if !rows.at(row) {
            placed.push((at, 0));
            continue;
        }
        let from = indices.len();
        kept.clear();
        seen.clear();
        for index in start..start + len {
            if !elements.at(index as usize) {
                continue;
            }
            let value = key(index);
            let fresh = if len <= SHORT {
                let fresh = !kept.contains(&value);
                if fresh {
                    kept.push(value);
                }
                fresh
            } else {
                seen.insert(value)
            };
            if fresh {
                indices.push(index);
            }
        }
        placed.push((at, entry(indices.len() - from)?));
    }
    Ok((placed, indices))
}

/// A list column rearranged but not yet gathered: its new entries, and the child index each new
/// element is read from.
type Permuted = (Vec<(u32, u32)>, Vec<u32>);

/// The new entries and the child indices in order, for sorting every row's run with `compare`.
///
/// The nulls in a run are set aside, the rest are sorted stably, and the nulls go back in at the
/// front or the back, which is what [`sort`] does with values.
fn permutation(
    entries: &[(u32, u32)],
    rows: Live<'_>,
    elements: Live<'_>,
    nulls_first: bool,
    compare: impl Fn(u32, u32) -> Ordering,
) -> Result<Permuted> {
    let mut indices = Vec::new();
    let mut placed = Vec::with_capacity(entries.len());
    let mut nulls = Vec::new();
    for (row, &(start, len)) in entries.iter().enumerate() {
        let at = entry(indices.len())?;
        if !rows.at(row) {
            placed.push((at, 0));
            continue;
        }
        nulls.clear();
        let from = indices.len();
        for index in start..start + len {
            if elements.at(index as usize) {
                indices.push(index);
            } else {
                nulls.push(index);
            }
        }
        indices[from..].sort_by(|&left, &right| compare(left, right));
        if nulls_first {
            indices.splice(from..from, nulls.iter().copied());
        } else {
            indices.extend_from_slice(&nulls);
        }
        placed.push((at, len));
    }
    Ok((placed, indices))
}

/// Whether equal values of `ty` are equal bits, which is what lets a search or a sort compare the
/// child's native values instead of going through [`order`].
fn plain(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt
    )
}

/// The one based place in each row's run of the first element equal to `wanted` that is not null,
/// or 0 for none.
///
/// The needle is narrowed to the child's own type once, so the scan compares native values and a
/// child with no nulls is a plain search of each run. A needle that does not fit the type is in no
/// run at all.
fn first_places<T: Copy + PartialEq + TryFrom<i128>>(
    entries: &[(u32, u32)],
    values: &[T],
    elements: Live<'_>,
    wanted: i128,
) -> Vec<u32> {
    let Ok(wanted) = T::try_from(wanted) else {
        return vec![0; entries.len()];
    };
    let place = |at: Option<usize>| at.map_or(0, |at| at as u32 + 1);
    entries
        .iter()
        .map(|&(start, len)| {
            let start = start as usize;
            let run = &values[start..start + len as usize];
            match elements {
                Live::All => place(run.iter().position(|&value| value == wanted)),
                _ => place(
                    run.iter()
                        .enumerate()
                        .position(|(at, &value)| value == wanted && elements.at(start + at)),
                ),
            }
        })
        .collect()
}

/// Whether a list of `element` has to be laid a row at a time.
fn nested_or_null(element: &LogicalType) -> bool {
    matches!(
        element,
        LogicalType::Null
            | LogicalType::List(_)
            | LogicalType::Array(..)
            | LogicalType::Struct(_)
            | LogicalType::Map(..)
            | LogicalType::Union(_)
    )
}

/// A child offset as a list entry holds it.
fn entry(offset: usize) -> Result<u32> {
    u32::try_from(offset)
        .map_err(|_| Error::out_of_range(format!("a list child of {offset} elements")))
}

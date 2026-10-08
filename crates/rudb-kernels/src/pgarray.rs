//! The array functions of a PostgreSQL session that DuckDB does not have.
//!
//! A rudb list is an array of one dimension with the lower bound 1. A function here gives `0A000`
//! for an array that is not empty and has more than one dimension or another lower bound, where
//! PostgreSQL gives the array. An empty array has no dimensions and no bounds, so it is always a
//! list.

use std::cmp::Ordering;

use rudb_common::{Error, LogicalType, Result, SqlState, Value};
use rudb_pgtypes::{ArrayDim, MAXDIM};

use crate::compare::order;

/// `array_fill(value, dimensions [, lower_bounds])`, an array of `value` with the lengths of
/// `dimensions`. The value can be null, and the two arrays cannot.
pub const ARRAY_FILL: &str = "__rudb_pg_array_fill";

/// The value of a call of a kernel of this module, or `None` for any other name.
pub(crate) fn call(name: &str, args: &[Value], returns: &LogicalType) -> Result<Option<Value>> {
    if name != ARRAY_FILL {
        return Ok(None);
    }
    let (value, lengths, lowers) = match args {
        [value, lengths] => (value, lengths, None),
        [value, lengths, lowers] => (value, lengths, Some(lowers)),
        _ => return Err(Error::internal(format!("{name} with {} arguments", args.len()))),
    };
    let LogicalType::List(element) = returns else {
        return Err(Error::internal(format!("{name} that gives a {returns}")));
    };
    fill(value, lengths, lowers, element).map(Some)
}

/// `array_fill_internal`, which checks the arguments in this order.
fn fill(
    value: &Value,
    lengths: &Value,
    lowers: Option<&Value>,
    element: &LogicalType,
) -> Result<Value> {
    if lengths.is_null() || lowers.is_some_and(Value::is_null) {
        return Err(not_null("dimension array or low bound array cannot be null"));
    }
    let lengths = integers(lengths)?;
    if lengths.len() > MAXDIM {
        let message = format!(
            "number of array dimensions ({}) exceeds the maximum allowed ({MAXDIM})",
            lengths.len()
        );
        return Err(Error::invalid_input(message)
            .state(SqlState::PROGRAM_LIMIT_EXCEEDED)
            .unplaced());
    }
    let lowers = match lowers {
        Some(lowers) => integers(lowers)?,
        None => vec![1; lengths.len()],
    };
    if lowers.len() != lengths.len() {
        return Err(Error::invalid_input("wrong number of array subscripts")
            .state(SqlState::ARRAY_SUBSCRIPT_ERROR)
            .detail("Low bound array has different size than dimensions array.")
            .unplaced());
    }
    let dims: Vec<ArrayDim> =
        lengths.iter().zip(&lowers).map(|(&len, &lower)| ArrayDim { len, lower }).collect();
    let count = rudb_pgtypes::item_count(&dims).map_err(|error| Error::from(error).unplaced())?;
    rudb_pgtypes::check_bounds(&dims).map_err(|error| Error::from(error).unplaced())?;
    let element = element.clone();
    if count == 0 {
        return Ok(Value::List { element, values: Vec::new() });
    }
    if dims.len() > 1 {
        return Err(unsupported("arrays of more than one dimension are not supported"));
    }
    if lowers[0] != 1 {
        return Err(unsupported("arrays with a lower bound other than 1 are not supported"));
    }
    Ok(Value::List { element, values: vec![value.clone(); count] })
}

/// The `int4` elements of an array of dimensions or of lower bounds.
fn integers(array: &Value) -> Result<Vec<i32>> {
    let Value::List { values, .. } = array else {
        return Err(Error::internal(format!("array_fill over a {}", array.logical_type())));
    };
    values
        .iter()
        .map(|value| match value {
            Value::Null => Err(not_null("dimension values cannot be null")),
            value => value
                .as_i64()
                .and_then(|value| i32::try_from(value).ok())
                .ok_or_else(|| Error::internal(format!("a dimension of {value:?}"))),
        })
        .collect()
}

fn not_null(message: &str) -> Error {
    Error::invalid_input(message).state(SqlState::NULL_VALUE_NOT_ALLOWED).unplaced()
}

fn unsupported(message: &str) -> Error {
    Error::not_implemented(message).state(SqlState::FEATURE_NOT_SUPPORTED).unplaced()
}

/// The C functions of this module, sorted.
pub(crate) const SOURCES: &[&str] = &[
    "array_dims",
    "array_position",
    "array_position_start",
    "array_positions",
    "array_remove",
    "array_replace",
    "array_reverse",
    "array_sample",
    "array_shuffle",
    "array_sort",
    "array_sort_order",
    "array_sort_order_nulls_first",
    "array_to_text",
    "array_to_text_null",
    "trim_array",
    "width_bucket_array",
];

/// The C functions of this module that are not strict: they see a null argument.
pub(crate) const NULLS: &[&str] = &[
    "array_position",
    "array_position_start",
    "array_positions",
    "array_remove",
    "array_replace",
    "array_to_text_null",
];

/// The C functions of this module that write the elements of their array, which they take as
/// text, by the output function of the type of the elements.
pub(crate) const OUTPUTS: &[&str] = &["array_to_text", "array_to_text_null"];

/// The C functions of this module that are volatile, whose value is not decided by their
/// arguments.
pub(crate) const VOLATILE: &[&str] = &["array_sample", "array_shuffle"];

/// The value of the C function `src` of `pg_proc` over `args`, or `None` for another function.
pub(crate) fn proc_call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    use Value::{Boolean, Integer, Null, Varchar};
    if !SOURCES.contains(&src) {
        return Ok(None);
    }
    if src == "width_bucket_array" {
        return width_bucket(args);
    }
    // A function that is not strict gives a null for a null array.
    let Some((element, values)) = args.first().and_then(elements) else {
        return Ok(Some(Null));
    };
    let list = |values: Vec<Value>| Value::List { element: element.clone(), values };
    let value = match (src, &args[1..]) {
        ("array_dims", []) => dims(values),
        ("array_position", [search]) => position(element, values, search, 1)?,
        ("array_position_start", [search, Integer(start)]) => {
            position(element, values, search, *start)?
        }
        ("array_position_start", [_, Null]) => {
            return Err(Error::invalid_input("initial position must not be null")
                .state(SqlState::NULL_VALUE_NOT_ALLOWED)
                .unplaced());
        }
        ("array_positions", [search]) => {
            one_dimension(element, "searching for elements")?;
            let mut positions = Vec::new();
            for (at, value) in values.iter().enumerate() {
                if same(value, search)? {
                    positions.push(Integer(index(at + 1)?));
                }
            }
            Value::List { element: LogicalType::Integer, values: positions }
        }
        ("array_remove", [search]) => {
            one_dimension(element, "removing elements")?;
            let mut kept = Vec::with_capacity(values.len());
            for value in values {
                if !same(value, search)? {
                    kept.push(value.clone());
                }
            }
            list(kept)
        }
        ("array_replace", [search, replace]) => list(replaced(values, search, replace)?),
        ("array_to_text", [Varchar(separator)]) => Varchar(joined(values, separator, None)),
        ("array_to_text_null", [Varchar(separator), Varchar(null)]) => {
            Varchar(joined(values, separator, Some(null)))
        }
        ("array_to_text_null", [Varchar(separator), Null]) => {
            Varchar(joined(values, separator, None))
        }
        ("array_to_text_null", [Null, _]) => Null,
        ("array_reverse", []) => list(values.iter().rev().cloned().collect()),
        ("trim_array", [Integer(count)]) => {
            let kept =
                usize::try_from(*count).ok().and_then(|count| values.len().checked_sub(count));
            let Some(kept) = kept else {
                return Err(Error::invalid_input(format!(
                    "number of elements to trim must be between 0 and {}",
                    values.len()
                ))
                .state(SqlState::ARRAY_SUBSCRIPT_ERROR)
                .unplaced());
            };
            list(values[..kept].to_vec())
        }
        ("array_sort", []) => list(sorted(values, false, false)?),
        ("array_sort_order", [Boolean(descending)]) => {
            list(sorted(values, *descending, *descending)?)
        }
        ("array_sort_order_nulls_first", [Boolean(descending), Boolean(nulls_first)]) => {
            list(sorted(values, *descending, *nulls_first)?)
        }
        ("array_shuffle", []) => list(shuffled(values, values.len())),
        ("array_sample", [Integer(count)]) => {
            let Some(count) = usize::try_from(*count).ok().filter(|&count| count <= values.len())
            else {
                return Err(Error::invalid_input(format!(
                    "sample size must be between 0 and {}",
                    values.len()
                ))
                .state(SqlState::INVALID_PARAMETER_VALUE)
                .unplaced());
            };
            list(shuffled(values, count))
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// `array_to_text_internal`: the elements in order, the ones of an array of more dimensions too,
/// with `separator` between two of them. A null is `null` when there is one, and is left out
/// when there is not.
fn joined(values: &[Value], separator: &str, null: Option<&str>) -> String {
    fn write(
        values: &[Value],
        separator: &str,
        null: Option<&str>,
        out: &mut String,
        any: &mut bool,
    ) {
        for value in values {
            let text = match value {
                Value::List { values, .. } => {
                    write(values, separator, null, out, any);
                    continue;
                }
                Value::Varchar(text) => text.as_str(),
                _ => match null {
                    Some(null) => null,
                    None => continue,
                },
            };
            if *any {
                out.push_str(separator);
            }
            out.push_str(text);
            *any = true;
        }
    }
    let mut out = String::new();
    write(values, separator, null, &mut out, &mut false);
    out
}

/// `width_bucket(operand, thresholds)`: the number of thresholds that are not above the operand,
/// by a binary search over the sorted thresholds.
fn width_bucket(args: &[Value]) -> Result<Option<Value>> {
    let [operand, thresholds] = args else { return Ok(None) };
    let Some((element, values)) = elements(thresholds) else { return Ok(None) };
    if matches!(element, LogicalType::List(_)) {
        return Err(Error::invalid_input("thresholds must be one-dimensional array")
            .state(SqlState::ARRAY_SUBSCRIPT_ERROR)
            .unplaced());
    }
    if values.iter().any(Value::is_null) {
        return Err(Error::invalid_input("thresholds array must not contain NULLs")
            .state(SqlState::NULL_VALUE_NOT_ALLOWED)
            .unplaced());
    }
    let (mut left, mut right) = (0, values.len());
    while left < right {
        let middle = left + (right - left) / 2;
        match order(operand, &values[middle])? {
            Ordering::Less => right = middle,
            _ => left = middle + 1,
        }
    }
    Ok(Some(Value::Integer(index(left)?)))
}

/// The element type and the elements of an array, or `None` for a null.
fn elements(value: &Value) -> Option<(&LogicalType, &[Value])> {
    match value {
        Value::List { element, values } => Some((element, values)),
        _ => None,
    }
}

/// A position from 1 as an `integer`.
fn index(at: usize) -> Result<i32> {
    i32::try_from(at).map_err(|_| Error::internal("an array longer than an integer"))
}

/// The error of PostgreSQL for an array of more than one dimension where `doing` takes one.
fn one_dimension(element: &LogicalType, doing: &str) -> Result<()> {
    match element {
        LogicalType::List(_) => Err(Error::invalid_input(format!(
            "{doing} in multidimensional arrays is not supported"
        ))
        .state(SqlState::FEATURE_NOT_SUPPORTED)
        .unplaced()),
        _ => Ok(()),
    }
}

/// Whether an element is the value searched for, with a null the same as a null.
fn same(value: &Value, search: &Value) -> Result<bool> {
    match (value.is_null(), search.is_null()) {
        (true, true) => Ok(true),
        (false, false) => Ok(order(value, search)? == Ordering::Equal),
        _ => Ok(false),
    }
}

/// `array_dims`: the bounds of each dimension, as `[1:3]`, or a null for an empty array.
fn dims(values: &[Value]) -> Value {
    let mut text = String::new();
    let mut level = values;
    while !level.is_empty() {
        text.push_str(&format!("[1:{}]", level.len()));
        match elements(&level[0]) {
            Some((_, inner)) => level = inner,
            None => break,
        }
    }
    match text.is_empty() {
        true => Value::Null,
        false => Value::Varchar(text),
    }
}

/// `array_position`: the first position from `start` of an element that is the value searched
/// for, or a null.
fn position(element: &LogicalType, values: &[Value], search: &Value, start: i32) -> Result<Value> {
    one_dimension(element, "searching for elements")?;
    let skip = usize::try_from(start.saturating_sub(1)).unwrap_or(0);
    for (at, value) in values.iter().enumerate().skip(skip) {
        if same(value, search)? {
            return Ok(Value::Integer(index(at + 1)?));
        }
    }
    Ok(Value::Null)
}

/// `array_replace`, which replaces the elements of each dimension.
fn replaced(values: &[Value], search: &Value, replace: &Value) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        out.push(match value {
            Value::List { element, values } => {
                Value::List { element: element.clone(), values: replaced(values, search, replace)? }
            }
            value if same(value, search)? => replace.clone(),
            value => value.clone(),
        });
    }
    Ok(out)
}

/// `array_sort`, by the order of the element type.
fn sorted(values: &[Value], descending: bool, nulls_first: bool) -> Result<Vec<Value>> {
    let mut out = values.to_vec();
    let mut failed = None;
    out.sort_by(|left, right| match (left.is_null(), right.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let ordering = order(left, right).unwrap_or_else(|error| {
                failed.get_or_insert(error);
                Ordering::Equal
            });
            if descending { ordering.reverse() } else { ordering }
        }
    });
    match failed {
        Some(error) => Err(error),
        None => Ok(out),
    }
}

/// `array_shuffle_n`: the first `count` elements of the shuffle of Fisher and Yates from the front.
fn shuffled(values: &[Value], count: usize) -> Vec<Value> {
    let mut out = values.to_vec();
    let last = out.len().saturating_sub(1);
    for at in 0..count.min(last) {
        let span = (last - at) as u64;
        let other = at + usize::try_from(crate::random::unseeded_up_to(span)).unwrap_or(0);
        out.swap(at, other);
    }
    out.truncate(count);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(values: &[Option<i32>]) -> Value {
        let values = values.iter().map(|value| value.map_or(Value::Null, Value::Integer)).collect();
        Value::List { element: LogicalType::Integer, values }
    }

    fn fill(args: &[Value]) -> Result<Value> {
        let returns = LogicalType::List(Box::new(LogicalType::Integer));
        call(ARRAY_FILL, args, &returns).map(Option::unwrap)
    }

    fn state(result: Result<Value>) -> String {
        let error = result.unwrap_err();
        format!("{} {}", error.reported_state(), error.message())
    }

    #[test]
    fn array_fill_repeats_the_value_and_checks_the_dimensions_as_postgresql_does() {
        let seven = Value::Integer(7);
        let filled = fill(&[seven.clone(), ints(&[Some(3)])]).unwrap();
        assert_eq!(filled, ints(&[Some(7), Some(7), Some(7)]));
        assert_eq!(fill(&[Value::Null, ints(&[Some(2)])]).unwrap(), ints(&[None, None]));
        assert_eq!(fill(&[seven.clone(), ints(&[Some(0)]), ints(&[Some(5)])]).unwrap(), ints(&[]));
        assert_eq!(fill(&[seven.clone(), ints(&[Some(2), Some(0)])]).unwrap(), ints(&[]));
        for (args, expected) in [
            (vec![Value::Null], "22004 dimension array or low bound array cannot be null"),
            (vec![ints(&[None])], "22004 dimension values cannot be null"),
            (vec![ints(&[Some(-1)])], "54000 array size exceeds the maximum allowed (134217727)"),
            (
                vec![ints(&[Some(1); 7]), ints(&[Some(1)])],
                "54000 number of array dimensions (7) exceeds the maximum allowed (6)",
            ),
            (vec![ints(&[Some(2)]), ints(&[])], "2202E wrong number of array subscripts"),
            (
                vec![ints(&[Some(2)]), ints(&[Some(i32::MAX)])],
                "54000 array lower bound is too large: 2147483647",
            ),
            (
                vec![ints(&[Some(2), Some(2)])],
                "0A000 arrays of more than one dimension are not supported",
            ),
        ] {
            let mut all = vec![seven.clone()];
            all.extend(args);
            assert_eq!(state(fill(&all)), expected);
        }
    }

    fn run(src: &str, args: &[Value]) -> Value {
        proc_call(src, args).unwrap().unwrap()
    }

    #[test]
    fn the_search_functions_match_a_null_with_a_null() {
        let array = ints(&[Some(1), None, Some(1)]);
        assert_eq!(run("array_position", &[array.clone(), Value::Null]), Value::Integer(2));
        let start = [array.clone(), Value::Integer(1), Value::Integer(2)];
        assert_eq!(run("array_position_start", &start), Value::Integer(3));
        assert_eq!(
            run("array_positions", &[array.clone(), Value::Integer(1)]),
            ints(&[Some(1), Some(3)])
        );
        assert_eq!(run("array_remove", &[array.clone(), Value::Null]), ints(&[Some(1), Some(1)]));
        let replace = [array, Value::Integer(1), Value::Integer(9)];
        assert_eq!(run("array_replace", &replace), ints(&[Some(9), None, Some(9)]));
        assert_eq!(run("array_remove", &[Value::Null, Value::Integer(1)]), Value::Null);
    }

    #[test]
    fn the_order_functions_give_the_order_of_postgresql() {
        let array = ints(&[Some(3), None, Some(1)]);
        assert_eq!(
            run("array_sort", std::slice::from_ref(&array)),
            ints(&[Some(1), Some(3), None])
        );
        let descending = [array.clone(), Value::Boolean(true)];
        assert_eq!(run("array_sort_order", &descending), ints(&[None, Some(3), Some(1)]));
        assert_eq!(
            run("array_reverse", std::slice::from_ref(&array)),
            ints(&[Some(1), None, Some(3)])
        );
        assert_eq!(run("trim_array", &[array.clone(), Value::Integer(2)]), ints(&[Some(3)]));
        let error = proc_call("trim_array", &[array.clone(), Value::Integer(4)]).unwrap_err();
        assert_eq!(error.message(), "number of elements to trim must be between 0 and 3");
        assert_eq!(run("array_dims", &[array]), Value::Varchar("[1:3]".into()));
        assert_eq!(run("array_dims", &[ints(&[])]), Value::Null);
    }

    #[test]
    fn width_bucket_counts_the_thresholds_at_or_below_the_operand() {
        let thresholds = ints(&[Some(1), Some(4), Some(8)]);
        for (operand, bucket) in [(0, 0), (1, 1), (5, 2), (8, 3), (9, 3)] {
            let args = [Value::Integer(operand), thresholds.clone()];
            assert_eq!(width_bucket(&args).unwrap(), Some(Value::Integer(bucket)));
        }
    }

    #[test]
    fn a_shuffle_keeps_the_elements() {
        let array = ints(&[Some(1), Some(2), Some(3), Some(4)]);
        let Value::List { mut values, .. } = run("array_shuffle", &[array]) else { panic!() };
        values.sort_by_key(|value| match value {
            Value::Integer(value) => *value,
            _ => 0,
        });
        assert_eq!(
            Value::List { element: LogicalType::Integer, values },
            ints(&[Some(1), Some(2), Some(3), Some(4)])
        );
    }

    #[test]
    fn array_to_text_joins_the_elements_and_writes_or_leaves_out_a_null() {
        let text =
            |value: Option<&str>| value.map_or(Value::Null, |text| Value::Varchar(text.into()));
        let texts = |values: &[Option<&str>]| Value::List {
            element: LogicalType::Varchar,
            values: values.iter().map(|&value| text(value)).collect(),
        };
        let array = texts(&[Some("1"), None, Some("3")]);
        let comma = text(Some(","));
        assert_eq!(run("array_to_text", &[array.clone(), comma.clone()]), text(Some("1,3")));
        let star = text(Some("*"));
        let null = "array_to_text_null";
        assert_eq!(run(null, &[array.clone(), comma.clone(), star.clone()]), text(Some("1,*,3")));
        assert_eq!(run(null, &[array.clone(), comma.clone(), Value::Null]), text(Some("1,3")));
        assert_eq!(run(null, &[array.clone(), Value::Null, star.clone()]), Value::Null);
        assert_eq!(run(null, &[Value::Null, comma.clone(), star.clone()]), Value::Null);
        let rows = Value::List {
            element: LogicalType::List(Box::new(LogicalType::Varchar)),
            values: vec![texts(&[Some("1"), Some("2")]), texts(&[Some("3"), None])],
        };
        assert_eq!(run(null, &[rows, comma.clone(), text(Some("x"))]), text(Some("1,2,3,x")));
        assert_eq!(run("array_to_text", &[texts(&[None, Some("b")]), comma]), text(Some("b")));
    }
}

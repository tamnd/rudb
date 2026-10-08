//! The array functions of a PostgreSQL session that DuckDB does not have.
//!
//! A rudb list is an array of one dimension with the lower bound 1. A function here gives `0A000`
//! for an array that is not empty and has more than one dimension or another lower bound, where
//! PostgreSQL gives the array. An empty array has no dimensions and no bounds, so it is always a
//! list.

use rudb_common::{Error, LogicalType, Result, SqlState, Value};
use rudb_pgtypes::{ArrayDim, MAXDIM};

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
}

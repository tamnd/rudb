//! `generate_series` over `integer`, `bigint` and `numeric`, as `generate_series_step_int4`,
//! `generate_series_step_int8` and `generate_series_step_numeric` write it in `int.c`, `int8.c`
//! and `numeric.c`.
//!
//! The series starts at the start and adds the step until it passes the stop. A step of zero is
//! an error, and so are a start, a stop or a step of `numeric` that is not finite. An `integer` or
//! a `bigint` series stops before an addition that overflows. A `numeric` value has the display
//! scale of the sum that made it, so `generate_series(1, 2, 0.30)` gives `1` and then `1.30`.
//!
//! PostgreSQL gives the values one row at a time. A kernel here gives them as one array, and the
//! binder unnests it, as for the other functions of [`ROWS`].

use rudb_common::{Error, LogicalType, Result, SqlState, Value};
use rudb_pgtypes::{Numeric, NumericSign};

use crate::lists::MAX_SERIES;

/// The kernels of this module, sorted. They are not C functions of PostgreSQL: each one gives as
/// an array the rows of the C functions that [`ROWS`] maps to it.
pub(crate) const SOURCES: &[&str] = &["series_int4", "series_int8", "series_numeric"];

/// The C functions that return a set, each with the kernel that gives its rows as an array.
pub(crate) const ROWS: &[(&str, &str)] = &[
    ("generate_series_int4", "series_int4"),
    ("generate_series_step_int4", "series_int4"),
    ("generate_series_int8", "series_int8"),
    ("generate_series_step_int8", "series_int8"),
    ("generate_series_numeric", "series_numeric"),
    ("generate_series_step_numeric", "series_numeric"),
];

/// The value of the kernel `src` over `args`, or `None` for another kernel.
pub(crate) fn call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    let value = match (src, args) {
        ("series_int4", [Value::Integer(start), Value::Integer(stop), step @ ..]) => {
            let step = match step {
                [] => 1,
                [Value::Integer(step)] => *step,
                _ => return Ok(None),
            };
            let values = integers(*start, *stop, step, i32::checked_add)?;
            listed(LogicalType::Integer, values.into_iter().map(Value::Integer))
        }
        ("series_int8", [Value::BigInt(start), Value::BigInt(stop), step @ ..]) => {
            let step = match step {
                [] => 1,
                [Value::BigInt(step)] => *step,
                _ => return Ok(None),
            };
            let values = integers(*start, *stop, step, i64::checked_add)?;
            listed(LogicalType::BigInt, values.into_iter().map(Value::BigInt))
        }
        ("series_numeric", [Value::Numeric(start), Value::Numeric(stop), step @ ..]) => {
            let step = match step {
                [] => None,
                [Value::Numeric(step)] => Some(Numeric::from_bytes(step)),
                _ => return Ok(None),
            };
            let values =
                numerics(&Numeric::from_bytes(start), &Numeric::from_bytes(stop), step.as_ref())?;
            listed(LogicalType::Numeric, values.into_iter().map(|n| Value::Numeric(n.to_bytes())))
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn listed(element: LogicalType, values: impl Iterator<Item = Value>) -> Value {
    Value::List { element, values: values.collect() }
}

/// The error of PostgreSQL for an argument of the series that it refuses.
fn refused(message: &str) -> Error {
    Error::invalid_input(message).state(SqlState::INVALID_PARAMETER_VALUE).unplaced()
}

/// The error for a series longer than a list can hold.
fn too_long() -> Error {
    Error::invalid_input("Lists larger than 2^32 elements are not supported")
        .state(SqlState::PROGRAM_LIMIT_EXCEEDED)
        .unplaced()
}

/// The values from `start` toward `stop` by `step`, which stop before an addition that overflows.
fn integers<T: Copy + Ord + Default>(
    start: T,
    stop: T,
    step: T,
    add: fn(T, T) -> Option<T>,
) -> Result<Vec<T>> {
    let zero = T::default();
    if step == zero {
        return Err(refused("step size cannot equal zero"));
    }
    let mut values = Vec::new();
    let mut current = Some(start);
    while let Some(value) = current {
        if (step > zero && value > stop) || (step < zero && value < stop) {
            break;
        }
        if values.len() == MAX_SERIES {
            return Err(too_long());
        }
        values.push(value);
        current = add(value, step);
    }
    Ok(values)
}

/// The values from `start` toward `stop` by `step`, or by 1 with no step.
fn numerics(start: &Numeric, stop: &Numeric, step: Option<&Numeric>) -> Result<Vec<Numeric>> {
    for (value, name) in [(start, "start value"), (stop, "stop value")] {
        match value.sign() {
            NumericSign::NaN => return Err(refused(&format!("{name} cannot be NaN"))),
            NumericSign::Infinity | NumericSign::NegativeInfinity => {
                return Err(refused(&format!("{name} cannot be infinity")));
            }
            NumericSign::Positive | NumericSign::Negative => {}
        }
    }
    let one = Numeric::from_integer(1);
    let step = step.unwrap_or(&one);
    match step.sign() {
        NumericSign::NaN => return Err(refused("step size cannot be NaN")),
        NumericSign::Infinity | NumericSign::NegativeInfinity => {
            return Err(refused("step size cannot be infinity"));
        }
        NumericSign::Positive | NumericSign::Negative => {}
    }
    let zero = Numeric::from_integer(0);
    let direction = step.compare(&zero);
    if direction.is_eq() {
        return Err(refused("step size cannot equal zero"));
    }
    // The series goes on while the value is not past the stop in the direction of the step.
    let ahead = |current: &Numeric| current.compare(stop) != direction;
    let mut values = Vec::new();
    let mut current = start.clone();
    while ahead(&current) {
        if values.len() == MAX_SERIES {
            return Err(too_long());
        }
        let next = current.add(step).map_err(|error| Error::from(error).unplaced())?;
        values.push(std::mem::replace(&mut current, next));
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric(text: &str) -> Value {
        Value::Numeric(rudb_pgtypes::numeric_in(text, -1).unwrap().to_bytes())
    }

    fn texts(value: Value) -> Vec<String> {
        let Value::List { values, .. } = value else { panic!("{value:?}") };
        values.iter().map(ToString::to_string).collect()
    }

    fn series(src: &str, args: &[Value]) -> Result<Vec<String>> {
        call(src, args).map(|value| texts(value.unwrap()))
    }

    #[test]
    fn an_integer_series_stops_at_the_stop_or_before_an_overflow() {
        let int = Value::Integer;
        assert_eq!(series("series_int4", &[int(1), int(3)]).unwrap(), ["1", "2", "3"]);
        assert_eq!(series("series_int4", &[int(5), int(1), int(-2)]).unwrap(), ["5", "3", "1"]);
        assert!(series("series_int4", &[int(1), int(3), int(-1)]).unwrap().is_empty());
        let top = series("series_int4", &[int(i32::MAX - 1), int(i32::MAX), int(i32::MAX)]);
        assert_eq!(top.unwrap(), [(i32::MAX - 1).to_string()]);
        let error = series("series_int8", &[Value::BigInt(1), Value::BigInt(3), Value::BigInt(0)]);
        assert_eq!(error.unwrap_err().message(), "step size cannot equal zero");
    }

    #[test]
    fn a_numeric_series_has_the_scale_of_each_sum() {
        let made = series("series_numeric", &[numeric("1"), numeric("2"), numeric("0.30")]);
        assert_eq!(made.unwrap(), ["1", "1.30", "1.60", "1.90"]);
        let made = series("series_numeric", &[numeric("1.5"), numeric("3")]);
        assert_eq!(made.unwrap(), ["1.5", "2.5"]);
        let made = series("series_numeric", &[numeric("2"), numeric("1"), numeric("-0.5")]);
        assert_eq!(made.unwrap(), ["2", "1.5", "1.0"]);
    }

    #[test]
    fn a_numeric_series_refuses_a_value_that_is_not_finite() {
        let message =
            |args: &[Value]| series("series_numeric", args).unwrap_err().message().to_owned();
        assert_eq!(message(&[numeric("NaN"), numeric("2")]), "start value cannot be NaN");
        assert_eq!(message(&[numeric("0"), numeric("Infinity")]), "stop value cannot be infinity");
        let step = [numeric("1"), numeric("2"), numeric("-Infinity")];
        assert_eq!(message(&step), "step size cannot be infinity");
        assert_eq!(
            message(&[numeric("1"), numeric("2"), numeric("0.0")]),
            "step size cannot equal zero"
        );
    }
}

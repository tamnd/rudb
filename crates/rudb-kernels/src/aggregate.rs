//! The aggregate accumulators.
//!
//! One of these per group per aggregate, so a `GROUP BY` over a million distinct keys with three
//! aggregates in it holds three million of them. That is the reason the state is an enum of small
//! fixed cases rather than a boxed trait object: the hash table is going to hold these inline and a
//! pointer chase per row per aggregate is a cost that shows up on every grouped query there is.
//!
//! Null is skipped by every aggregate. `sum` over a column that is entirely null is null and not
//! zero, `count(x)` counts the rows where `x` is not null, and `count(*)` counts rows without
//! looking at anything. Those three are not variations on a theme, they are three different
//! questions, and the reason `count(*)` is a separate function rather than `count` with a star
//! argument is so the executor never has to work out which one it was handed.

use rudb_common::{Error, LogicalType, Result, Value};

use crate::compare::order;
use crate::number::{fit, integral, pow10, rescale};

/// A running aggregate.
#[derive(Debug, Clone)]
pub struct Accumulator {
    kind: Kind,
    returns: LogicalType,
    state: State,
}

/// Which aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// What the aggregate has seen so far.
#[derive(Debug, Clone)]
enum State {
    /// A row count, for `count` and `count(*)`.
    Counted(i64),
    /// A whole running total and whether anything landed in it.
    Whole { total: i128, seen: bool },
    /// A running total in floating point, and the count `avg` divides by.
    Real { total: f64, seen: i64 },
    /// A running total at a fixed decimal scale.
    Scaled { total: i128, scale: u8, seen: bool },
    /// The smallest or largest value so far.
    Extreme(Option<Value>),
}

impl Accumulator {
    /// A fresh accumulator for a named aggregate returning `returns`.
    ///
    /// # Errors
    ///
    /// If the name is not an aggregate this crate implements.
    pub fn new(name: &str, returns: &LogicalType) -> Result<Self> {
        let kind = match name {
            "count_star" => Kind::CountStar,
            "count" => Kind::Count,
            "sum" => Kind::Sum,
            "avg" => Kind::Avg,
            "min" => Kind::Min,
            "max" => Kind::Max,
            other => {
                return Err(Error::not_implemented(format!("the {other} aggregate")));
            }
        };
        let state = match kind {
            Kind::CountStar | Kind::Count => State::Counted(0),
            Kind::Avg => State::Real { total: 0.0, seen: 0 },
            Kind::Min | Kind::Max => State::Extreme(None),
            Kind::Sum => match returns {
                LogicalType::Decimal { scale, .. } => {
                    State::Scaled { total: 0, scale: *scale, seen: false }
                }
                LogicalType::Float | LogicalType::Double => State::Real { total: 0.0, seen: 0 },
                _ => State::Whole { total: 0, seen: false },
            },
        };
        Ok(Self { kind, returns: returns.clone(), state })
    }

    /// Folds one row in.
    ///
    /// # Errors
    ///
    /// If the argument count is wrong for the aggregate, if the value is not one the aggregate can
    /// accumulate, or if a whole running total overflows.
    pub fn update(&mut self, args: &[Value]) -> Result<()> {
        if self.kind == Kind::CountStar {
            if let State::Counted(count) = &mut self.state {
                *count += 1;
            }
            return Ok(());
        }
        let value = match args {
            [only] => only,
            _ => {
                return Err(Error::internal(format!("an aggregate over {} arguments", args.len())));
            }
        };
        if value.is_null() {
            return Ok(());
        }
        match &mut self.state {
            State::Counted(count) => *count += 1,
            State::Whole { total, seen } => {
                let whole = integral(value).ok_or_else(|| not_summable(value))?;
                *total = total.checked_add(whole).ok_or_else(|| {
                    Error::out_of_range("Overflow in the running total of a sum".to_string())
                })?;
                *seen = true;
            }
            State::Real { total, seen } => {
                *total += approximate_or_error(value)?;
                *seen += 1;
            }
            State::Scaled { total, scale, seen } => {
                let unscaled = at_scale(value, *scale).ok_or_else(|| not_summable(value))?;
                *total = total.checked_add(unscaled).ok_or_else(|| {
                    Error::out_of_range("Overflow in the running total of a sum".to_string())
                })?;
                *seen = true;
            }
            State::Extreme(held) => {
                let replace = match held {
                    None => true,
                    Some(current) => {
                        let ordering = order(value, current)?;
                        match self.kind {
                            Kind::Min => ordering.is_lt(),
                            _ => ordering.is_gt(),
                        }
                    }
                };
                if replace {
                    *held = Some(value.clone());
                }
            }
        }
        Ok(())
    }

    /// The aggregate's answer.
    ///
    /// # Errors
    ///
    /// If the running total does not fit the declared return type.
    pub fn finish(&self) -> Result<Value> {
        match &self.state {
            State::Counted(count) => Ok(Value::BigInt(*count)),
            State::Whole { total, seen } => {
                if !seen {
                    return Ok(Value::Null);
                }
                fit(*total, &self.returns).ok_or_else(|| {
                    Error::out_of_range(format!(
                        "a sum of {total} does not fit in {}",
                        self.returns
                    ))
                })
            }
            State::Real { total, seen } => {
                if *seen == 0 {
                    return Ok(Value::Null);
                }
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "the count of rows in one group is well inside the exact range"
                )]
                let answer = if self.kind == Kind::Avg { total / *seen as f64 } else { *total };
                if matches!(self.returns, LogicalType::Float) {
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "a declared FLOAT result is a FLOAT"
                    )]
                    return Ok(Value::Float(answer as f32));
                }
                Ok(Value::Double(answer))
            }
            State::Scaled { total, scale, seen } => {
                if !seen {
                    return Ok(Value::Null);
                }
                let width = match self.returns {
                    LogicalType::Decimal { width, .. } => width,
                    _ => rudb_common::MAX_DECIMAL_WIDTH,
                };
                Ok(Value::Decimal { unscaled: *total, width, scale: *scale })
            }
            State::Extreme(held) => Ok(held.clone().unwrap_or(Value::Null)),
        }
    }
}

fn not_summable(value: &Value) -> Error {
    Error::not_implemented(format!("summing a {}", value.logical_type()))
}

fn approximate_or_error(value: &Value) -> Result<f64> {
    crate::number::approximate(value).ok_or_else(|| not_summable(value))
}

/// A value as an unscaled integer at a fixed scale.
fn at_scale(value: &Value, scale: u8) -> Option<i128> {
    match *value {
        Value::Decimal { unscaled, scale: held, .. } => rescale(unscaled, held, scale),
        _ => integral(value).and_then(|whole| whole.checked_mul(pow10(scale))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, returns: &LogicalType, rows: &[Value]) -> Value {
        let mut accumulator = Accumulator::new(name, returns).expect("a known aggregate");
        for row in rows {
            accumulator.update(std::slice::from_ref(row)).expect("accumulates");
        }
        accumulator.finish().expect("finishes")
    }

    #[test]
    fn count_star_counts_rows_and_count_counts_values() {
        let mut stars = Accumulator::new("count_star", &LogicalType::BigInt).expect("known");
        for _ in 0..3 {
            stars.update(&[]).expect("no arguments");
        }
        assert_eq!(stars.finish().expect("finishes"), Value::BigInt(3));
        let counted = run(
            "count",
            &LogicalType::BigInt,
            &[Value::Integer(1), Value::Null, Value::Integer(3)],
        );
        assert_eq!(counted, Value::BigInt(2));
    }

    /// The distinction that makes `sum` over an empty group different from `count` over one.
    #[test]
    fn a_sum_of_nothing_is_null_and_a_count_of_nothing_is_zero() {
        assert_eq!(run("sum", &LogicalType::HugeInt, &[]), Value::Null);
        assert_eq!(run("sum", &LogicalType::HugeInt, &[Value::Null]), Value::Null);
        assert_eq!(run("count", &LogicalType::BigInt, &[]), Value::BigInt(0));
        assert_eq!(run("count_star", &LogicalType::BigInt, &[]), Value::BigInt(0));
    }

    #[test]
    fn a_sum_of_integers_accumulates_wider_than_it_reads() {
        let rows = vec![Value::Integer(i32::MAX); 4];
        let total = run("sum", &LogicalType::HugeInt, &rows);
        assert_eq!(total, Value::HugeInt(i128::from(i32::MAX) * 4));
    }

    #[test]
    fn an_average_divides_by_the_rows_it_saw_rather_than_the_rows_there_were() {
        let average =
            run("avg", &LogicalType::Double, &[Value::Integer(1), Value::Null, Value::Integer(3)]);
        assert_eq!(average, Value::Double(2.0));
    }

    #[test]
    fn min_and_max_skip_nulls_and_keep_the_value_rather_than_a_number() {
        let smallest = run(
            "min",
            &LogicalType::Varchar,
            &[Value::Varchar("b".into()), Value::Null, Value::Varchar("a".into())],
        );
        assert_eq!(smallest, Value::Varchar("a".into()));
        let largest = run(
            "max",
            &LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(7), Value::Integer(3)],
        );
        assert_eq!(largest, Value::Integer(7));
    }

    #[test]
    fn a_decimal_sums_at_its_own_scale() {
        let ty = LogicalType::decimal(10, 2).expect("a legal decimal");
        let total = run(
            "sum",
            &ty,
            &[
                Value::Decimal { unscaled: 250, width: 10, scale: 2 },
                Value::Decimal { unscaled: 125, width: 10, scale: 2 },
            ],
        );
        assert_eq!(total, Value::Decimal { unscaled: 375, width: 10, scale: 2 });
    }

    #[test]
    fn an_aggregate_nobody_has_written_says_which_one() {
        let error = Accumulator::new("median", &LogicalType::Double)
            .expect_err("median is not written yet");
        assert!(error.message().contains("the median aggregate"), "{error}");
    }
}

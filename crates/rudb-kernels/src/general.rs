//! The aggregates that are not a count, a total or an extreme.
//!
//! [`crate::aggregate::Accumulator`] keeps `count`, `sum`, `avg`, `min` and `max` in small inline
//! states because a grouped query holds one per group per call and those five are most of what
//! grouped queries ask for. Everything else lands here, behind one box, so that adding `list` or
//! `stddev` does not make every `count(*)` state in a hash table wider than it is now.
//!
//! These states fold values and not vectors. The vector paths in the aggregate module step aside
//! for them and hand over one row at a time, which is correct for all of them and fast for none,
//! and each one that turns out to matter can grow a vector path of its own later.
//!
//! Not every aggregate here skips nulls. `list` keeps them as elements and `first` and `last` can
//! answer with one, which is the pin's behavior and is why the null check lives in each arm rather
//! than in front of all of them.

use rudb_common::{Error, LogicalType, Result, Value};

use crate::number::{approximate, fit, integral};

/// A running aggregate that is not one of the five the aggregate module keeps inline.
#[derive(Debug, Clone)]
pub(crate) enum General {
    /// `list` and `array_agg`: every value in the order it arrived, nulls included.
    List { element: LogicalType, values: Vec<Value> },
    /// `first`, `last` and `any_value`, which keep one row's value.
    Pick { held: Option<Value>, pick: Pick },
    /// `bool_and` and `bool_or`.
    Logic { held: Option<bool>, all: bool },
    /// `bit_and`, `bit_or` and `bit_xor`, held wide and narrowed to the argument's type at the end.
    Bits { held: Option<i128>, op: BitOp, returns: LogicalType },
    /// `product`, in floating point the way the pin multiplies.
    Product { total: f64, seen: bool },
    /// The variance family, as a running count, mean and sum of squared differences.
    ///
    /// This is Welford's update and the pin's combine, step for step, because the order the
    /// arithmetic happens in is the last digit of the answer.
    Moments { count: u64, mean: f64, squared: f64, measure: Measure },
    /// `string_agg`, the text so far, whether anything has gone into it, and the separator it was
    /// joined with, which a combine has to put between the two halves.
    Joined { text: String, seen: bool, separator: String },
}

/// Which row [`General::Pick`] keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pick {
    /// The first row, even when it is null.
    First,
    /// The last row, even when it is null.
    Last,
    /// The first row that is not null.
    Any,
}

/// Which bitwise fold [`General::Bits`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BitOp {
    And,
    Or,
    Xor,
}

/// Which answer [`General::Moments`] gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Measure {
    VarSamp,
    VarPop,
    StddevSamp,
    StddevPop,
}

impl General {
    /// A fresh state for `name`, or `None` when the name is not one of these.
    pub(crate) fn new(name: &str, returns: &LogicalType) -> Option<Self> {
        let pick = |pick| Self::Pick { held: None, pick };
        let bits = |op| Self::Bits { held: None, op, returns: returns.clone() };
        let moments = |measure| Self::Moments { count: 0, mean: 0.0, squared: 0.0, measure };
        Some(match name {
            "list" => {
                let element = match returns {
                    LogicalType::List(element) => (**element).clone(),
                    _ => LogicalType::Null,
                };
                Self::List { element, values: Vec::new() }
            }
            "first" => pick(Pick::First),
            "last" => pick(Pick::Last),
            "any_value" => pick(Pick::Any),
            "bool_and" => Self::Logic { held: None, all: true },
            "bool_or" => Self::Logic { held: None, all: false },
            "bit_and" => bits(BitOp::And),
            "bit_or" => bits(BitOp::Or),
            "bit_xor" => bits(BitOp::Xor),
            "product" => Self::Product { total: 1.0, seen: false },
            "var_samp" => moments(Measure::VarSamp),
            "var_pop" => moments(Measure::VarPop),
            "stddev_samp" => moments(Measure::StddevSamp),
            "stddev_pop" => moments(Measure::StddevPop),
            "string_agg" => {
                Self::Joined { text: String::new(), seen: false, separator: String::new() }
            }
            _ => return None,
        })
    }

    /// Folds one row in. `args` is the row's arguments in call order.
    pub(crate) fn update(&mut self, args: &[Value]) -> Result<()> {
        let Some(value) = args.first() else {
            return Err(Error::internal("an aggregate over 0 arguments".to_string()));
        };
        match self {
            Self::List { values, .. } => values.push(value.clone()),
            Self::Pick { held, pick } => match pick {
                Pick::First => {
                    if held.is_none() {
                        *held = Some(value.clone());
                    }
                }
                Pick::Last => *held = Some(value.clone()),
                Pick::Any => {
                    if held.is_none() && !value.is_null() {
                        *held = Some(value.clone());
                    }
                }
            },
            _ if value.is_null() => {}
            Self::Logic { held, all } => {
                let Value::Boolean(flag) = *value else {
                    return Err(unexpected("bool_and", value));
                };
                let so_far = held.unwrap_or(*all);
                *held = Some(if *all { so_far && flag } else { so_far || flag });
            }
            Self::Bits { held, op, .. } => {
                let bits = integral(value).ok_or_else(|| unexpected("bit_and", value))?;
                *held = Some(match (*held, *op) {
                    (None, _) => bits,
                    (Some(so_far), BitOp::And) => so_far & bits,
                    (Some(so_far), BitOp::Or) => so_far | bits,
                    (Some(so_far), BitOp::Xor) => so_far ^ bits,
                });
            }
            Self::Product { total, seen } => {
                *total *= approximate(value).ok_or_else(|| unexpected("product", value))?;
                *seen = true;
            }
            Self::Moments { count, mean, squared, .. } => {
                let input = approximate(value).ok_or_else(|| unexpected("stddev", value))?;
                *count += 1;
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "the count of rows in one group is well inside the exact range"
                )]
                let differential = (input - *mean) / *count as f64;
                let next = *mean + differential;
                *squared += (input - next) * (input - *mean);
                *mean = next;
            }
            Self::Joined { text, seen, separator: kept } => {
                // A null separator drops the row, which is how the pin answers
                // `string_agg(x, NULL)` with null.
                let separator = match args.get(1) {
                    None => ",",
                    Some(Value::Varchar(separator)) => separator,
                    Some(_) => return Ok(()),
                };
                let Value::Varchar(piece) = value else {
                    return Err(unexpected("string_agg", value));
                };
                if *seen {
                    text.push_str(separator);
                } else {
                    separator.clone_into(kept);
                }
                text.push_str(piece);
                *seen = true;
            }
        }
        Ok(())
    }

    /// Folds another state for the same call into this one, as if its rows came after these.
    pub(crate) fn combine(&mut self, other: &Self) -> Result<()> {
        match (self, other) {
            (Self::List { values, .. }, Self::List { values: more, .. }) => {
                values.extend(more.iter().cloned());
            }
            (Self::Pick { held, pick }, Self::Pick { held: theirs, .. }) => {
                let take = match pick {
                    Pick::First | Pick::Any => held.is_none(),
                    Pick::Last => theirs.is_some(),
                };
                if take {
                    held.clone_from(theirs);
                }
            }
            (Self::Logic { held, all }, Self::Logic { held: theirs, .. }) => {
                *held = match (*held, *theirs) {
                    (Some(here), Some(there)) => {
                        Some(if *all { here && there } else { here || there })
                    }
                    (here, there) => here.or(there),
                };
            }
            (Self::Bits { held, op, .. }, Self::Bits { held: theirs, .. }) => {
                *held = match (*held, *theirs) {
                    (Some(here), Some(there)) => Some(match op {
                        BitOp::And => here & there,
                        BitOp::Or => here | there,
                        BitOp::Xor => here ^ there,
                    }),
                    (here, there) => here.or(there),
                };
            }
            (Self::Product { total, seen }, Self::Product { total: theirs, seen: any }) => {
                *total *= theirs;
                *seen |= any;
            }
            (
                Self::Moments { count, mean, squared, .. },
                Self::Moments { count: more, mean: theirs, squared: their_squared, .. },
            ) => {
                if *count == 0 {
                    (*count, *mean, *squared) = (*more, *theirs, *their_squared);
                } else if *more > 0 {
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "the count of rows in one group is well inside the exact range"
                    )]
                    let (here, there) = (*count as f64, *more as f64);
                    let total = here + there;
                    let delta = theirs - *mean;
                    *squared = their_squared + *squared + delta * delta * there * here / total;
                    *mean = (there * theirs + here * *mean) / total;
                    *count += more;
                }
            }
            (
                Self::Joined { text, seen, separator },
                Self::Joined { text: theirs, seen: any, separator: their_separator },
            ) => {
                if !*seen {
                    text.clone_from(theirs);
                    separator.clone_from(their_separator);
                    *seen = *any;
                } else if *any {
                    text.push_str(separator);
                    text.push_str(theirs);
                }
            }
            (here, there) => {
                return Err(Error::internal(format!(
                    "combining a {here:?} aggregate state with a {there:?} one"
                )));
            }
        }
        Ok(())
    }

    /// The answer.
    pub(crate) fn finish(&self) -> Result<Value> {
        Ok(match self {
            Self::List { values, .. } if values.is_empty() => Value::Null,
            Self::List { element, values } => {
                Value::List { element: element.clone(), values: values.clone() }
            }
            Self::Pick { held, .. } => held.clone().unwrap_or(Value::Null),
            Self::Logic { held, .. } => held.map_or(Value::Null, Value::Boolean),
            Self::Bits { held: None, .. } | Self::Product { seen: false, .. } => Value::Null,
            Self::Bits { held: Some(bits), returns, .. } => {
                let returns =
                    if *returns == LogicalType::Null { &LogicalType::BigInt } else { returns };
                fit(*bits, returns).ok_or_else(|| {
                    Error::out_of_range(format!(
                        "a bitwise aggregate of {bits} does not fit in {returns}"
                    ))
                })?
            }
            Self::Product { total, .. } => Value::Double(*total),
            Self::Moments { count, squared, measure, .. } => {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "the count of rows in one group is well inside the exact range"
                )]
                let rows = *count as f64;
                let sample = matches!(measure, Measure::VarSamp | Measure::StddevSamp);
                let variance = match (*count, sample) {
                    (0, _) | (1, true) => return Ok(Value::Null),
                    (1, false) => 0.0,
                    (_, true) => squared / (rows - 1.0),
                    (_, false) => squared / rows,
                };
                match measure {
                    Measure::VarSamp | Measure::VarPop => Value::Double(variance),
                    Measure::StddevSamp | Measure::StddevPop => Value::Double(variance.sqrt()),
                }
            }
            Self::Joined { seen: false, .. } => Value::Null,
            Self::Joined { text, .. } => Value::Varchar(text.clone()),
        })
    }
}

/// A value of a type the binder should not have let through to this aggregate.
fn unexpected(name: &str, value: &Value) -> Error {
    Error::internal(format!("{name} was handed a {}", value.logical_type()))
}

//! `EXPORT_STATE`, `finalize` and `combine`, which move an aggregate's running state out into a
//! value and back in.
//!
//! The pin writes a state out as a value of a layout each aggregate declares: a count is a
//! `UBIGINT`, an average is its count beside its sum, and the variance family is Welford's three
//! numbers. The layouts below are the pin's, so a state printed or cast here reads the same as one
//! printed there. Reading one back builds a fresh accumulator and puts the numbers into it, which
//! makes `finalize` the accumulator's own finish and `combine` its own combine, and so the same
//! answers the aggregate gives when nothing was exported at all.

use rudb_common::{Error, Field, LogicalType, Result, StateType, Value};

use super::{Accumulator, State, exactly, mean_bits, mean_real};
use crate::general::General;
use crate::number::{approximate, integral};
use crate::statistics::{Moment, Paired, Pairing, Powers};

/// What an aggregate's name ends with when its call exports its state.
pub const EXPORTED: &str = " EXPORT_STATE";

/// The private name `finalize` is stored under. The aggregate's name and the scale of a decimal
/// argument follow it, since a kernel sees the type it answers and not the type it was handed.
const FINALIZE: &str = "__rudb_finalize";

/// The layout a call of `name` over `arguments` answering `returns` exports its state in.
///
/// # Errors
///
/// The pin's error for an aggregate that has no layout, and a not implemented error for one whose
/// layout is not written here yet.
pub fn state_layout(
    name: &str,
    arguments: &[LogicalType],
    returns: &LogicalType,
) -> Result<LogicalType> {
    let field = |name: &str, ty: LogicalType| Field::new(name, ty);
    let count = || field("count", LogicalType::UBigInt);
    if let Some(measure) = Pairing::named(name) {
        return Ok(Paired::layout(measure));
    }
    if let Some(measure) = Moment::named(name) {
        return Ok(Powers::layout(measure));
    }
    Ok(match name {
        "count" | "count_star" => LogicalType::UBigInt,
        "sum" => match arguments.first() {
            Some(LogicalType::Decimal { .. }) => LogicalType::HugeInt,
            _ => returns.clone(),
        },
        "avg" => {
            let value = match arguments.first() {
                Some(LogicalType::Float | LogicalType::Double) => LogicalType::Double,
                Some(LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::UTinyInt) => {
                    LogicalType::BigInt
                }
                Some(LogicalType::Decimal { width, .. }) if *width <= 4 => LogicalType::BigInt,
                Some(LogicalType::Decimal { .. }) => LogicalType::HugeInt,
                Some(ty) if ty.is_integer() => LogicalType::HugeInt,
                _ => return Err(not_written(name)),
            };
            LogicalType::Struct(vec![count(), field("value", value)])
        }
        "min" | "max" | "bool_and" | "bool_or" | "bit_and" | "bit_or" | "bit_xor" | "product"
        | "count_if" => returns.clone(),
        "first" | "last" | "any_value" => {
            LogicalType::Struct(vec![field("value", returns.clone())])
        }
        "var_samp" | "var_pop" | "stddev_samp" | "stddev_pop" | "sem" => LogicalType::Struct(vec![
            count(),
            field("mean", LogicalType::Double),
            field("dsquared", LogicalType::Double),
        ]),
        "fsum" => LogicalType::Struct(vec![
            field("value", LogicalType::Double),
            field("err", LogicalType::Double),
        ]),
        "favg" => LogicalType::Struct(vec![
            count(),
            field("value", LogicalType::Double),
            field("err", LogicalType::Double),
        ]),
        "entropy" | "histogram" | "mode" | "approx_count_distinct" => {
            return Err(Error::not_implemented(format!(
                "Aggregate function \"\"{name}\"\" does not have a state type callback defined - \
                 cannot export state"
            )));
        }
        _ => return Err(not_written(name)),
    })
}

fn not_written(name: &str) -> Error {
    Error::not_implemented(format!("exporting the state of the {name} aggregate"))
}

/// The name a `finalize` of `state` is stored under, which `state_call` reads back.
pub fn finalize_name(state: &StateType) -> String {
    let scale = match state.arguments.first() {
        Some(LogicalType::Decimal { scale, .. }) => *scale,
        _ => 0,
    };
    format!("{FINALIZE} {} {scale}", state.function)
}

/// `finalize` or `combine` on one row, or `None` for any other name.
pub(crate) fn state_call(
    name: &str,
    args: &[Value],
    returns: &LogicalType,
) -> Result<Option<Value>> {
    if let Some(rest) = name.strip_prefix(FINALIZE) {
        let mut words = rest.split_whitespace();
        let (Some(function), Some(scale), [value]) = (words.next(), words.next(), args) else {
            return Err(Error::internal(format!("a finalize stored as {name}")));
        };
        if value.is_null() {
            return Ok(Some(Value::Null));
        }
        let scale =
            scale.parse().map_err(|_| Error::internal(format!("a finalize stored as {name}")))?;
        let accumulator = Accumulator::import(function, returns, scale, value)?;
        return accumulator.finish().map(Some);
    }
    if name != "combine" {
        return Ok(None);
    }
    let (LogicalType::AggregateState(state), [left, right]) = (returns, args) else {
        return Err(Error::internal(format!(
            "combine of {} values returning {returns}",
            args.len()
        )));
    };
    // A null is the empty state, which is what an aggregate that has seen nothing writes when its
    // layout has no other way to say so. Two of them combine to a null and not to what an empty
    // state writes, a `0` for a count, which is the pin's answer too.
    if left.is_null() && right.is_null() {
        return Ok(Some(Value::Null));
    }
    let scale = match state.arguments.first() {
        Some(LogicalType::Decimal { scale, .. }) => *scale,
        _ => 0,
    };
    let mut accumulator = Accumulator::import(&state.function, &state.returns, scale, left)?;
    accumulator.combine(&Accumulator::import(&state.function, &state.returns, scale, right)?)?;
    accumulator.export(&state.layout).map(Some)
}

/// The field of a struct value called `name`.
pub(crate) fn member<'a>(value: &'a Value, name: &str) -> Result<&'a Value> {
    let Value::Struct(fields) = value else {
        return Err(Error::internal(format!("an aggregate state {value:?} that is not a struct")));
    };
    fields
        .iter()
        .find(|(field, _)| field == name)
        .map(|(_, value)| value)
        .ok_or_else(|| Error::internal(format!("an aggregate state with no {name} in it")))
}

/// A struct type out of named fields, the shape every state layout with more than one number in
/// it takes.
pub(crate) fn shape(fields: &[(&str, LogicalType)]) -> LogicalType {
    LogicalType::Struct(fields.iter().map(|(name, ty)| Field::new(*name, ty.clone())).collect())
}

/// A struct value out of named fields, the value [`shape`] is the type of.
pub(crate) fn packed(fields: Vec<(&str, Value)>) -> Value {
    Value::Struct(fields.into_iter().map(|(name, value)| (name.to_string(), value)).collect())
}

/// A whole number held in a state.
pub(crate) fn whole(value: &Value) -> Result<i128> {
    integral(value).ok_or_else(|| Error::internal(format!("an aggregate state holding {value:?}")))
}

/// A floating point number held in a state.
pub(crate) fn real(value: &Value) -> Result<f64> {
    approximate(value)
        .ok_or_else(|| Error::internal(format!("an aggregate state holding {value:?}")))
}

/// A count held in a state.
pub(crate) fn counted(value: &Value) -> Result<u64> {
    u64::try_from(whole(value)?).map_err(|_| Error::internal("a negative count in a state"))
}

impl Accumulator {
    /// A fresh accumulator for a grouped operator, which for an exported call is the aggregate it
    /// exports rather than a wrapper around it.
    ///
    /// [`Accumulator::new`] wraps an exported call so that anything finishing it the ordinary way
    /// gets the state, and the wrapper is also what keeps it out of every fast path that folds a
    /// run of groups by the kind of their states. An operator that builds its states through here
    /// takes those paths for an exported call too, and must finish its states through
    /// [`Accumulator::finish_as`] so that the state is what comes out.
    ///
    /// # Errors
    ///
    /// If the name is not an aggregate this crate implements.
    pub fn folding(name: &str, returns: &LogicalType) -> Result<Self> {
        if let Some(inner) = name.strip_suffix(EXPORTED)
            && let LogicalType::AggregateState(state) = returns
        {
            return Self::new(inner, &state.returns);
        }
        Self::new(name, returns)
    }

    /// The answer of a state built by [`Accumulator::folding`] for a call returning `returns`, which
    /// is the state written out when the call exports one and the ordinary answer otherwise.
    ///
    /// # Errors
    ///
    /// The errors [`Accumulator::finish`] raises, and those of writing the state out.
    pub fn finish_as(&self, returns: &LogicalType) -> Result<Value> {
        let wrapped = matches!(&self.state, State::General(general)
            if matches!(**general, General::Exported { .. }));
        match returns {
            LogicalType::AggregateState(state) if !wrapped => self.export(&state.layout),
            _ => self.finish(),
        }
    }

    /// The state written out in `layout`, which [`state_layout`] gave for this call.
    ///
    /// # Errors
    ///
    /// If a total does not fit the layout, and an internal error for a state this does not write.
    pub(crate) fn export(&self, layout: &LogicalType) -> Result<Value> {
        match &self.state {
            State::Counted { count, .. } => {
                Ok(Value::UBigInt(u64::try_from(*count).unwrap_or_default()))
            }
            State::Whole { .. } | State::Real { .. } | State::Extreme { .. } => self.finish(),
            State::Scaled { total, seen, .. } => {
                Ok(if *seen { Value::HugeInt(*total) } else { Value::Null })
            }
            State::Mean { total, seen, exact, .. } => {
                let held = match layout {
                    LogicalType::Struct(fields) => fields.get(1).map(|field| &field.ty),
                    _ => None,
                };
                let value = match (held, *exact) {
                    (Some(LogicalType::Double), true) => Value::Double(exactly(*total)),
                    (Some(LogicalType::Double), false) => Value::Double(mean_real(*total)),
                    (Some(LogicalType::BigInt), true) => {
                        Value::BigInt(i64::try_from(*total).map_err(|_| super::overflowed())?)
                    }
                    (_, true) => Value::HugeInt(*total),
                    (_, false) => return Err(super::overflowed()),
                };
                let count = Value::UBigInt(u64::try_from(*seen).unwrap_or_default());
                Ok(Value::Struct(vec![("count".to_string(), count), ("value".to_string(), value)]))
            }
            State::General(general) => general.export(),
        }
    }

    /// A fresh accumulator for `function` answering `returns` holding the state `value` wrote,
    /// where `scale` is the scale of a decimal argument. A null state is an empty one.
    ///
    /// # Errors
    ///
    /// If `function` is not an aggregate, or `value` is not a state it writes.
    pub(crate) fn import(
        function: &str,
        returns: &LogicalType,
        scale: u8,
        value: &Value,
    ) -> Result<Self> {
        let mut accumulator = Self::new(function, returns)?;
        if value.is_null() {
            return Ok(accumulator);
        }
        match &mut accumulator.state {
            State::Counted { count, .. } => {
                *count = i64::try_from(counted(value)?).map_err(|_| super::overflowed())?;
            }
            State::Whole { total, seen, .. } | State::Scaled { total, seen, .. } => {
                *total = whole(value)?;
                *seen = true;
            }
            State::Real { total, seen, .. } => {
                *total = real(value)?;
                *seen = 1;
            }
            State::Mean { total, seen, exact, scale: held, .. } => {
                *seen = i64::try_from(counted(member(value, "count")?)?)
                    .map_err(|_| super::overflowed())?;
                *held = scale;
                match member(value, "value")? {
                    Value::Double(sum) => {
                        *total = mean_bits(*sum);
                        *exact = false;
                    }
                    sum => *total = whole(sum)?,
                }
            }
            State::Extreme { .. } => accumulator.update(std::slice::from_ref(value))?,
            State::General(general) => general.import(value)?,
        }
        Ok(accumulator)
    }
}

impl General {
    /// The state written out in the layout [`state_layout`] gives it.
    fn export(&self) -> Result<Value> {
        let named = |fields: Vec<(&str, Value)>| {
            Value::Struct(
                fields.into_iter().map(|(name, value)| (name.to_string(), value)).collect(),
            )
        };
        Ok(match self {
            Self::Pick { held, .. } => {
                held.as_ref().map_or(Value::Null, |held| named(vec![("value", held.clone())]))
            }
            Self::Logic { .. }
            | Self::Bits { .. }
            | Self::BitString { .. }
            | Self::Product { .. }
            | Self::CountIf { .. } => self.finish()?,
            Self::Moments { count, mean, squared, .. } => named(vec![
                ("count", Value::UBigInt(*count)),
                ("mean", Value::Double(*mean)),
                ("dsquared", Value::Double(*squared)),
            ]),
            Self::Kahan { value, err, count, average: true } => named(vec![
                ("count", Value::UBigInt(*count)),
                ("value", Value::Double(*value)),
                ("err", Value::Double(*err)),
            ]),
            Self::Kahan { count: 0, .. } => Value::Null,
            Self::Kahan { value, err, .. } => {
                named(vec![("value", Value::Double(*value)), ("err", Value::Double(*err))])
            }
            Self::Paired(state) => state.export(),
            Self::Powers(state) => state.export(),
            _ => return Err(Error::internal("exporting a state that has no layout")),
        })
    }

    /// Puts the state `value` wrote into this fresh one.
    fn import(&mut self, value: &Value) -> Result<()> {
        match self {
            Self::Pick { held, .. } => *held = Some(member(value, "value")?.clone()),
            Self::CountIf { count, seen } => {
                *count = whole(value)?;
                *seen = true;
            }
            Self::Logic { .. }
            | Self::Bits { .. }
            | Self::BitString { .. }
            | Self::Product { .. } => {
                self.update(std::slice::from_ref(value))?;
            }
            Self::Moments { count, mean, squared, .. } => {
                *count = counted(member(value, "count")?)?;
                *mean = real(member(value, "mean")?)?;
                *squared = real(member(value, "dsquared")?)?;
            }
            Self::Kahan { value: sum, err, count, average } => {
                *sum = real(member(value, "value")?)?;
                *err = real(member(value, "err")?)?;
                *count = if *average { counted(member(value, "count")?)? } else { 1 };
            }
            Self::Paired(state) => state.import(value)?,
            Self::Powers(state) => state.import(value)?,
            _ => return Err(Error::internal("importing a state that has no layout")),
        }
        Ok(())
    }
}

//! `EXPORT_STATE`, `finalize` and `combine`, which move an aggregate's running state out into a
//! value and back in.
//!
//! The pin writes a state out as a value of a layout each aggregate declares: a count is a
//! `UBIGINT`, an average is its count beside its sum, and the variance family is Welford's three
//! numbers. The layouts below are the pin's, so a state printed or cast here reads the same as one
//! printed there. Reading one back builds a fresh accumulator and puts the numbers into it, which
//! makes `finalize` the accumulator's own finish and `combine` its own combine, and so the same
//! answers the aggregate gives when nothing was exported at all.

use std::borrow::Cow;
use std::sync::Arc;

use rudb_common::{Error, Field, LogicalType, Result, StateType, Value};

use super::{Accumulator, State, exactly, mean_bits, mean_real, ordered_name};
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
        "list" | "string_agg" => returns.clone(),
        "quantile_cont" | "quantile_disc" | "median" => match arguments.first() {
            Some(ty) => LogicalType::List(Box::new(ty.clone())),
            None => return Err(not_written(name)),
        },
        "arg_min" | "arg_max" | "arg_min_null" | "arg_max_null" | "arg_min_nulls_last"
        | "arg_max_nulls_last" => match arguments {
            [arg, by] => {
                LogicalType::Struct(vec![field("arg", arg.clone()), field("by", by.clone())])
            }
            _ => return Err(no_layout(name)),
        },
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
        "entropy" | "histogram" | "mode" | "approx_count_distinct" => return Err(no_layout(name)),
        _ => return Err(not_written(name)),
    })
}

/// The layout of an ordered call's state, which is the rows it buffered, each a struct of the
/// columns it holds: the arguments and then the sort keys that are not one of them, named `v0`,
/// `v1` and on, the pin's names.
#[must_use]
pub fn ordered_layout(columns: &[LogicalType]) -> LogicalType {
    let fields =
        columns.iter().enumerate().map(|(at, ty)| Field::new(format!("v{at}"), ty.clone()));
    LogicalType::List(Box::new(LogicalType::Struct(fields.collect())))
}

/// The position of the first argument of `name` that its state keeps the constant of, which is
/// past the end for an aggregate that reads every argument on every row.
pub fn state_constants(name: &str) -> usize {
    match name {
        "string_agg" | "quantile_cont" | "quantile_disc" => 1,
        _ => usize::MAX,
    }
}

/// The pin's error for an aggregate that has no layout to export its state in.
fn no_layout(name: &str) -> Error {
    Error::not_implemented(format!(
        "Aggregate function \"\"{name}\"\" does not have a state type callback defined - cannot \
         export state"
    ))
}

fn not_written(name: &str) -> Error {
    Error::not_implemented(format!("exporting the state of the {name} aggregate"))
}

/// The name a `finalize` of `state` is stored under, which `state_call` reads back. The aggregate
/// comes last because an ordered one has spaces in its name.
pub fn finalize_name(state: &StateType) -> String {
    format!("{FINALIZE} {} {}", scale(state), accumulated(state))
}

/// The name of the aggregate that holds `state`, which for an ordered call is the ordered one.
fn accumulated(state: &StateType) -> Cow<'_, str> {
    if state.order.is_empty() {
        Cow::Borrowed(&state.function)
    } else {
        Cow::Owned(ordered_name(&state.function, &state.order))
    }
}

/// The scale of the decimal a state's call was made over, or 0 when it was not over a decimal.
fn scale(state: &StateType) -> u8 {
    match state.arguments.first() {
        Some(LogicalType::Decimal { scale, .. }) => *scale,
        _ => 0,
    }
}

/// A fresh accumulator for the call `state` came from holding the state `value` wrote.
fn imported(state: &StateType, value: &Value) -> Result<Accumulator> {
    let constant = state.constants.get(1).and_then(Option::as_ref);
    Accumulator::import(&accumulated(state), &state.returns, scale(state), value, constant)
}

/// `combine_aggr`, which folds the exported states of one call into one state of that call.
///
/// Each row is a state and how many times to fold it in, once when the call does not say. Every
/// fold is the aggregate's own combine, the running state first and the row's after it, so a list
/// keeps its rows in the order they arrived. A repeated state is combined that many times rather
/// than multiplied, which is what the pin does for every aggregate that has no shortcut of its own
/// and gives the same answer for the ones that do.
#[derive(Debug, Clone)]
pub(crate) struct Merge {
    held: Option<Box<Accumulator>>,
    state: Arc<StateType>,
}

impl Merge {
    pub(crate) fn new(state: Arc<StateType>) -> Self {
        Self { held: None, state }
    }

    /// Folds one row in: a state, and the number of times to fold it when there is one.
    pub(crate) fn update(&mut self, args: &[Value]) -> Result<()> {
        let Some(value) = args.first().filter(|value| !value.is_null()) else {
            return Ok(());
        };
        let times = match args.get(1) {
            None => 1,
            Some(Value::Null) => return Ok(()),
            Some(times) => whole(times)?,
        };
        if times < 0 {
            return Err(Error::invalid_input("combine_aggr multiplicity must be non-negative"));
        }
        if times == 0 {
            return Ok(());
        }
        let row = imported(&self.state, value)?;
        let held = match &mut self.held {
            Some(held) => held,
            empty => empty.insert(Box::new(imported(&self.state, &Value::Null)?)),
        };
        for _ in 0..times {
            held.combine(&row)?;
        }
        Ok(())
    }

    pub(crate) fn combine(&mut self, other: &Self) -> Result<()> {
        match (&mut self.held, &other.held) {
            (_, None) => {}
            (None, Some(theirs)) => self.held = Some(theirs.clone()),
            (Some(held), Some(theirs)) => held.combine(theirs)?,
        }
        Ok(())
    }

    /// The folded state written out. With no row to fold it is the aggregate's empty state, so a
    /// count of nothing still finalizes to 0 the way it does in the pin.
    pub(crate) fn finish(&self) -> Result<Value> {
        match &self.held {
            None => imported(&self.state, &Value::Null)?.export(&self.state.layout),
            Some(held) => held.export(&self.state.layout),
        }
    }
}

/// `finalize` or `combine` on one row, or `None` for any other name.
pub(crate) fn state_call(
    name: &str,
    args: &[Value],
    returns: &LogicalType,
) -> Result<Option<Value>> {
    if let Some(rest) = name.strip_prefix(FINALIZE) {
        let words = rest.trim_start().split_once(' ');
        let (Some((scale, function)), [value, constants @ ..]) = (words, args) else {
            return Err(Error::internal(format!("a finalize stored as {name}")));
        };
        if value.is_null() {
            return Ok(Some(Value::Null));
        }
        let scale =
            scale.parse().map_err(|_| Error::internal(format!("a finalize stored as {name}")))?;
        let constant = constants.get(1).filter(|constant| !constant.is_null());
        let accumulator = Accumulator::import(function, returns, scale, value, constant)?;
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
    // The pin folds the left state into the right one, so a list comes out as the right rows
    // followed by the left ones. Its `last` of a number keeps the right value when it has one,
    // the way `first` does, where the `last` of a string keeps the left one.
    let text = matches!(state.returns, LogicalType::Varchar | LogicalType::Blob);
    if state.function == "last" && state.order.is_empty() && !text {
        return Ok(Some(if right.is_null() { left.clone() } else { right.clone() }));
    }
    let mut accumulator = imported(state, right)?;
    accumulator.combine(&imported(state, left)?)?;
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
            State::Spread { count, mean, squared, .. } => Ok(Value::Struct(vec![
                ("count".to_string(), Value::UBigInt(*count)),
                ("mean".to_string(), Value::Double(*mean)),
                ("dsquared".to_string(), Value::Double(*squared)),
            ])),
            State::General(general) => general.export(layout),
        }
    }

    /// A fresh accumulator for `function` answering `returns` holding the state `value` wrote,
    /// where `scale` is the scale of a decimal argument and `constant` is the one the call was
    /// bound with, a separator or a fraction. A null state is an empty one.
    ///
    /// # Errors
    ///
    /// If `function` is not an aggregate, or `value` is not a state it writes.
    pub(crate) fn import(
        function: &str,
        returns: &LogicalType,
        scale: u8,
        value: &Value,
        constant: Option<&Value>,
    ) -> Result<Self> {
        let mut accumulator = Self::new(function, returns)?;
        if let State::General(general) = &mut accumulator.state {
            general.bind(constant);
        }
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
            State::Spread { count, mean, squared, .. } => {
                *count = counted(member(value, "count")?)?;
                *mean = real(member(value, "mean")?)?;
                *squared = real(member(value, "dsquared")?)?;
            }
            State::General(general) => general.import(value)?,
        }
        Ok(accumulator)
    }
}

impl General {
    /// The state written out in `layout`, which [`state_layout`] gave for it.
    fn export(&self, layout: &LogicalType) -> Result<Value> {
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
            Self::Arg { state, .. } => state.export()?,
            Self::Ordered { rows, .. } if rows.is_empty() => Value::Null,
            Self::Ordered { rows, .. } => {
                let element = match layout {
                    LogicalType::List(element) => (**element).clone(),
                    _ => LogicalType::Null,
                };
                let row = |row: &Vec<Value>| {
                    let fields = row.iter().enumerate();
                    Value::Struct(
                        fields.map(|(at, value)| (format!("v{at}"), value.clone())).collect(),
                    )
                };
                Value::List { element, values: rows.iter().map(row).collect() }
            }
            Self::List { .. } | Self::Joined { .. } => self.finish()?,
            Self::Merged(merge) => merge.finish()?,
            Self::Holistic { values, .. } if values.len() == 0 => Value::Null,
            Self::Holistic { values, .. } => {
                let element = match layout {
                    LogicalType::List(element) => (**element).clone(),
                    _ => LogicalType::Null,
                };
                Value::List { element, values: values.values() }
            }
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
            Self::Kahan { value: sum, err, count, average } => {
                *sum = real(member(value, "value")?)?;
                *err = real(member(value, "err")?)?;
                *count = if *average { counted(member(value, "count")?)? } else { 1 };
            }
            Self::Paired(state) => state.import(value)?,
            Self::Powers(state) => state.import(value)?,
            Self::Arg { state, .. } => state.import(member(value, "arg")?, member(value, "by")?)?,
            Self::Ordered { rows, .. } => {
                let Value::List { values: held, .. } = value else {
                    return Err(Error::internal(format!("an ordered state holding {value:?}")));
                };
                for row in held {
                    let Value::Struct(fields) = row else {
                        return Err(Error::internal(format!("an ordered state row {row:?}")));
                    };
                    rows.push(fields.iter().map(|(_, value)| value.clone()).collect());
                }
            }
            Self::List { values, .. } | Self::Holistic { values, .. } => {
                let Value::List { values: held, .. } = value else {
                    return Err(Error::internal(format!("a list state holding {value:?}")));
                };
                for value in held {
                    values.push(value);
                }
            }
            Self::Joined { text, seen, .. } => {
                let Value::Varchar(held) = value else {
                    return Err(Error::internal(format!("a string_agg state holding {value:?}")));
                };
                text.clone_from(held);
                *seen = true;
            }
            _ => return Err(Error::internal("importing a state that has no layout")),
        }
        Ok(())
    }

    /// Gives a fresh state the constant its call was bound with, which a state read back from
    /// its layout does not carry: the separator of a `string_agg` and the fraction of a quantile.
    fn bind(&mut self, constant: Option<&Value>) {
        match self {
            Self::Joined { separator, .. } => {
                *separator = match constant {
                    Some(Value::Varchar(text)) => text.clone(),
                    _ => ",".to_string(),
                };
            }
            Self::Holistic { fraction, .. } => *fraction = constant.cloned().map(Box::new),
            _ => {}
        }
    }
}

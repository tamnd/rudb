//! Turning a value of one type into a value of another.
//!
//! Every cast in a query is one the binder put there. Nothing here is reached because a user wrote
//! `CAST`, or rather that is only the smallest part of it: `WHERE counter > 5` casts a literal,
//! `a + b` over an `INTEGER` and a `BIGINT` casts the left side, and a `UNION` casts whichever side
//! is narrower. So this is on the path of almost every query, and the thing it has to be is exact
//! rather than fast, because the tier 0 interpreter is what tiers 1 and 2 are checked against.
//!
//! A failed cast raises, and a failed `TRY_CAST` produces null. That distinction is carried by the
//! error code rather than by a second set of functions: a conversion or a range failure is what
//! `TRY_CAST` swallows, and a cast between two types nobody has written the code for is not,
//! because turning "I have not implemented this" into a column of nulls is how a missing feature
//! becomes a wrong answer.
//!
//! # How the vectorized path is put together
//!
//! There are twenty numeric types here if a decimal counts once, and casting each of them to each
//! of the others is four hundred loops nobody is going to write or check. The way out is that an
//! integer and a decimal are the same thing seen twice: an exact number held as an integer, written
//! at a scale that happens to be zero when it is an integer. Once both sides are read that way, a
//! cast from `DECIMAL(9, 2)` to `BIGINT` and a cast from `INTEGER` to `DECIMAL(18, 4)` stop being
//! two problems and become one, which is moving a number from one scale to another and then fitting
//! it. What is left is four quadrants, exact or approximate on each side, and a read arm and a
//! write arm per physical layout, which is a number of loops that fits on a screen.
//!
//! The pivot is an `i128` for the exact quadrants and an `f64` for the approximate ones. That is one
//! extra pass over sixteen kilobytes that stays in L1 rather than one fused loop per type pair, and
//! it is the trade this file makes on purpose: the fused version is four hundred bodies and this one
//! is nine, in a file whose whole job is to be the answer every faster tier is checked against.
//!
//! Nothing on the fast path looks at which rows are null, and it does not have to. A null still
//! occupies a position, a vector writes a zero there, and zero converts to zero in every one of
//! these loops, so the run that comes out already holds at every null position the same zero the
//! row at a time path would have written. The mask is carried across untouched.

use std::cmp::Ordering;
use std::str::FromStr;

use rudb_common::{
    Error, ErrorCode, LogicalType, PhysicalType, Result, Value, civil_from_days, days_from_civil,
};
use rudb_vector::{Data, Form, Vector};

use crate::datetime::MICROS_PER_DAY;
use crate::fallback::{self, Kernel};
use crate::number::{approximate, digits, fit, integral, pow10, rescale};
use crate::shape::{identity, nulls_of};

/// Casts every value of a vector.
///
/// A cast to the type the vector already has is free. A constant vector costs one conversion
/// rather than one per row, which matters because a literal in a predicate is a constant vector
/// and the binder casts it on the way in. A flat or dictionary vector of one number type going to
/// another takes the sweep below, which never builds a [`Value`]. Everything else takes the loop at
/// the bottom of this function, which is the definition the rest of this file is written against.
///
/// # Errors
///
/// If a value cannot be represented in the target type and `try_cast` is false, or if the pair of
/// types is one this does not handle yet.
pub fn cast(input: &Vector, target: &LogicalType, try_cast: bool) -> Result<Vector> {
    if input.logical_type() == target {
        return Ok(input.clone());
    }
    if input.is_empty() {
        return Ok(Vector::constant(target.clone(), Value::Null, 0));
    }
    if input.form() == Form::Constant {
        let single = cast_value(&input.value_at(0), target, try_cast)?;
        return Ok(Vector::constant(target.clone(), single, input.len()));
    }
    if let Some(vector) = swept(input, target) {
        return Ok(vector);
    }
    // A cast reads one vector, so its form goes in both halves of the report rather than leaving a
    // column of zeros next to every row of it.
    fallback::record(Kernel::Cast, input.form(), input.form());
    let mut values = Vec::with_capacity(input.len());
    // row at a time: the path recorded on the line above, which exists to be correct for a
    // conversion `swept` does not cover and counts itself so that conversion shows up.
    for index in 0..input.len() {
        values.push(cast_value(&input.value_at(index), target, try_cast)?);
    }
    Vector::from_values(target.clone(), &values)
}

/// Every value of a vector converted without a [`Value`] being built for any of them, or `None`
/// when this is not a pair of types the loops here cover and `None` again when a value did not fit.
///
/// Answering `None` for a value that did not fit rather than raising is what keeps this honest. The
/// loop above raises with a message naming the value and the type it would not go into, and
/// `TRY_CAST` turns some of those failures into nulls and deliberately not others, and a second
/// copy of those two rules here would be a second copy of the part of this file that is hard to get
/// right. So the sweep only ever returns an answer it is sure of, and a vector with one bad value
/// in it costs one wasted pass and then goes through the same loop it always did.
///
/// That is also why `try_cast` is not a parameter. A sweep that came back with an answer converted
/// every value it was given, and a cast that raises nothing and a `TRY_CAST` that produces no nulls
/// are the same answer.
fn swept(input: &Vector, target: &LogicalType) -> Option<Vector> {
    let from = numeric(input.logical_type())?;
    let into = numeric(target)?;
    let rows = input.len();
    let physical = target.physical();
    let converted = match input.form() {
        Form::Flat => {
            let data = input.data()?;
            if data.len() < rows {
                return None;
            }
            convert_run(data, identity, rows, from, into, physical)?
        }
        Form::Dictionary => {
            let (codes, values) = input.dictionary_parts()?;
            if codes.len() < rows {
                return None;
            }
            // Every code is inside the dictionary because `Vector::dictionary` checks that on the
            // way in, so the gather below indexes without a bound of its own.
            convert_run(values.data()?, |index| codes[index] as usize, rows, from, into, physical)?
        }
        _ => return None,
    };
    Some(Vector::flat(target.clone(), converted).ok()?.with_validity(nulls_of(input)))
}

/// What a specialized loop needs to know about one side of a numeric cast.
#[derive(Clone, Copy)]
enum Numeric {
    /// An exact number at this scale, and the digits it has to fit into when it is a decimal.
    Exact { scale: u8, width: Option<u8> },
    /// A number held as a float, single precision rather than double.
    Approximate { single: bool },
}

/// The shape of a type for the loops below, or `None` for a type they do not cover.
///
/// `BOOLEAN` is not here even though a boolean reads as an integer, because as a target it converts
/// by comparing against zero rather than by fitting a width, and it would be the one arm that does
/// not follow the rule the rest of this is built on. `UHUGEINT` is not here because a value above
/// the `HUGEINT` range has no exact `i128` to pivot through, and no benchmark and no real schema
/// has a column of them. `DATE` and the timestamps are not here although they are held as integers,
/// because a cast between the two of them is a multiplication by the length of a day and a cast
/// from either of them to an integer is refused outright, and neither of those is what treating
/// them as exact numbers at scale zero would do.
fn numeric(ty: &LogicalType) -> Option<Numeric> {
    match *ty {
        LogicalType::TinyInt
        | LogicalType::SmallInt
        | LogicalType::Integer
        | LogicalType::BigInt
        | LogicalType::HugeInt
        | LogicalType::UTinyInt
        | LogicalType::USmallInt
        | LogicalType::UInteger
        | LogicalType::UBigInt => Some(Numeric::Exact { scale: 0, width: None }),
        LogicalType::Decimal { width, scale } => Some(Numeric::Exact { scale, width: Some(width) }),
        LogicalType::Float => Some(Numeric::Approximate { single: true }),
        LogicalType::Double => Some(Numeric::Approximate { single: false }),
        _ => None,
    }
}

/// The four quadrants, each one a read pass, a move and a write pass.
fn convert_run<M: Fn(usize) -> usize>(
    data: &Data,
    at: M,
    rows: usize,
    from: Numeric,
    into: Numeric,
    physical: PhysicalType,
) -> Option<Data> {
    match (from, into) {
        (Numeric::Exact { scale: was, .. }, Numeric::Exact { scale: now, width: None })
            if was == now =>
        {
            straight(data, at, rows, physical)
        }
        (Numeric::Exact { scale: was, .. }, Numeric::Exact { scale: now, width }) => {
            let mut run = exact_run(data, at, rows)?;
            restage(&mut run, was, now)?;
            exact_out(run, width, physical)
        }
        (Numeric::Exact { scale, .. }, Numeric::Approximate { single }) => {
            loosened(data, at, rows, scale, single)
        }
        (Numeric::Approximate { .. }, Numeric::Exact { scale, width }) => {
            let run = float_run(data, at, rows)?;
            exact_out(tighten(&run, scale)?, width, physical)
        }
        (Numeric::Approximate { .. }, Numeric::Approximate { single }) => {
            let run = float_run(data, at, rows)?;
            approximate_out(run, single)
        }
    }
}

/// The exact to exact case with nothing to do in between, which is every integer widening and
/// every integer narrowing in every query.
///
/// This one does not pivot. The other three quadrants gather into a run of `i128`, move that run to
/// the target scale and then fit it, which is three passes over sixteen kilobytes to do what is
/// here a load, a range check and a store. Integer to integer is common enough to be worth the
/// eighty one bodies the two macros below expand to, and it is the difference between this kernel
/// reading as a load and a store in a profile and reading as a memory bound copy.
///
/// Every pair of integer widths has a `TryFrom`, including each width with itself, so the eighty
/// one arms are the same three lines and the ones that cannot fail are a move once the compiler has
/// looked at them.
fn straight<M: Fn(usize) -> usize>(
    data: &Data,
    at: M,
    rows: usize,
    physical: PhysicalType,
) -> Option<Data> {
    macro_rules! fitted {
        ($values:expr, $variant:path, $ty:ty) => {{
            let values = $values;
            let mut out = Vec::with_capacity(rows);
            for index in 0..rows {
                out.push(<$ty>::try_from(values[at(index)]).ok()?);
            }
            $variant(out.into())
        }};
    }
    macro_rules! by_target {
        ($values:expr, $(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match physical {
                $(PhysicalType::$variant => fitted!($values, Data::$variant, $native),)+
                _ => return None,
            }
        };
    }
    // The source list and the target list are the same list, which is the point. A width added to
    // one of them and not the other is a cast that silently goes the row at a time way in one
    // direction and not the other, and it cannot happen when there is one list.
    macro_rules! by_source {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => {
                    rudb_vector::for_each_layout!(exact, by_target, values)
                })+
                _ => return None,
            }
        };
    }
    Some(rudb_vector::for_each_layout!(exact, by_source))
}

/// The exact numbers a run of data holds, read through one form's index mapping.
///
/// The mapping is a generic parameter rather than a `fn(usize) -> usize` held in a variable, which
/// is the difference between a read the compiler unrolls and an indirect call per row it cannot see
/// through. That difference was ten nanoseconds a row when `compare` was measured with the call in
/// it, and it is the reason no kernel in this crate keeps one.
fn exact_run<M: Fn(usize) -> usize>(data: &Data, at: M, rows: usize) -> Option<Vec<i128>> {
    // The `i128` arm widens an `i128` to an `i128`, which is the reflexive `From` and is a move.
    // Writing it out separately would be the same code with a chance of being different code.
    macro_rules! widened {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => {
                    (0..rows).map(|index| i128::from(values[at(index)])).collect()
                })+
                _ => return None,
            }
        };
    }
    Some(rudb_vector::for_each_layout!(exact, widened))
}

/// The approximate numbers a run of data holds, read through one form's index mapping.
fn float_run<M: Fn(usize) -> usize>(data: &Data, at: M, rows: usize) -> Option<Vec<f64>> {
    Some(match data {
        Data::Float32(values) => (0..rows).map(|index| f64::from(values[at(index)])).collect(),
        Data::Float64(values) => (0..rows).map(|index| values[at(index)]).collect(),
        _ => return None,
    })
}

/// Moves a whole run of exact numbers from one scale to another, in place.
///
/// Which of the three things to do is decided once, ahead of the loop, rather than by calling
/// [`rescale`] per value. At equal scales `rescale` multiplies by ten to the zero and checks that
/// product for overflow, and equal scales is every integer widening in every query, so leaving the
/// decision inside the loop would put a multiply and a branch on the most common conversion there
/// is. Going down rounds half away from zero, which is what `rescale` does and what DuckDB does.
///
/// The addition of half the factor cannot overflow. A value only reaches the third arm at a scale
/// above zero, a scale above zero means it came out of a decimal, a decimal carries at most thirty
/// eight digits, and ten to the thirty eighth plus half of it is still inside an `i128`.
fn restage(run: &mut [i128], was: u8, now: u8) -> Option<()> {
    match now.cmp(&was) {
        Ordering::Equal => {}
        Ordering::Greater => {
            let factor = pow10(now - was);
            for slot in run.iter_mut() {
                *slot = slot.checked_mul(factor)?;
            }
        }
        Ordering::Less => {
            let factor = pow10(was - now);
            let half = factor / 2;
            for slot in run.iter_mut() {
                let shifted = if *slot >= 0 { *slot + half } else { *slot - half };
                *slot = shifted / factor;
            }
        }
    }
    Some(())
}

/// The exact to approximate case, fused for the same reason [`straight`] is.
///
/// Nine source layouts is nine bodies, which is worth writing out for a conversion that every
/// average, every division and every comparison of an integer column against a written fraction
/// goes through. A `FLOAT` target still takes the second pass in [`approximate_out`], because the
/// answer it has to reach is the double narrowed rather than the source narrowed, and a single
/// rounding and two roundings are not always the same number.
///
/// Scale zero gets its own loop rather than dividing by a factor of one, because a division is a
/// division whatever it is by, and scale zero is every integer that ever goes to a double.
#[expect(
    clippy::cast_precision_loss,
    reason = "a wide integer past 2^53 losing digits is what a double is, and this is the float path"
)]
fn loosened<M: Fn(usize) -> usize>(
    data: &Data,
    at: M,
    rows: usize,
    scale: u8,
    single: bool,
) -> Option<Data> {
    macro_rules! doubles {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => doubles!(@run values),)+
                _ => return None,
            }
        };
        (@run $values:expr) => {{
            let values = $values;
            let mut out = Vec::with_capacity(rows);
            if scale == 0 {
                for index in 0..rows {
                    out.push(values[at(index)] as f64);
                }
            } else {
                let factor = pow10(scale) as f64;
                for index in 0..rows {
                    out.push(values[at(index)] as f64 / factor);
                }
            }
            out
        }};
    }
    let run: Vec<f64> = rudb_vector::for_each_layout!(exact, doubles);
    approximate_out(run, single)
}

/// A run of doubles as the exact numbers they round to at a scale, or `None` when one of them has
/// no such number.
///
/// The bound is the one the row at a time path checks, and a NaN or an infinity fails it because
/// every comparison against a NaN is false. A value that fails is not converted here at all, and
/// the whole vector goes back through the loop that knows how to say which value it was.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the bound checked on the line above is what decides whether the value fits"
)]
fn tighten(run: &[f64], scale: u8) -> Option<Vec<i128>> {
    let factor = pow10(scale) as f64;
    let mut out = Vec::with_capacity(run.len());
    for &number in run {
        let scaled = (number * factor).round();
        if !(-1.7014118346046923e38..=1.7014118346046923e38).contains(&scaled) {
            return None;
        }
        out.push(scaled as i128);
    }
    Some(out)
}

/// A run of exact numbers in the container the target type is held in, or `None` when one of them
/// does not fit.
///
/// The run is taken by value so that a `HUGEINT` target, where the container the values are already
/// in is the container they belong in, hands the same allocation straight on rather than copying
/// sixteen kilobytes to reach the same bytes.
fn exact_out(run: Vec<i128>, width: Option<u8>, physical: PhysicalType) -> Option<Data> {
    if let Some(width) = width {
        // This is `digits(whole) > width` with the counting taken out of the loop. `digits` divides
        // by ten until there is nothing left, up to thirty eight times, and a decimal column is
        // exactly where that would be paid on every row. Needing more digits than the width is the
        // same statement as being at or above ten to the width.
        let limit = pow10(width).unsigned_abs();
        if run.iter().any(|&whole| whole.unsigned_abs() >= limit) {
            return None;
        }
    }
    macro_rules! narrowed {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match physical {
                $(PhysicalType::$variant => {
                    let mut out = Vec::with_capacity(run.len());
                    for &whole in &run {
                        out.push(<$native>::try_from(whole).ok()?);
                    }
                    Data::$variant(out.into())
                })+
                // The run is a run of `i128` already, so the hugeint target is the one with nothing
                // to narrow, and it takes the run as it stands rather than copying it value by
                // value into a second one of the same width.
                PhysicalType::Int128 => Data::Int128(run.into()),
                _ => return None,
            }
        };
    }
    Some(rudb_vector::for_each_layout!(narrow, narrowed))
}

/// A run of doubles in the container the target type is held in, or `None` when narrowing one of
/// them to single precision turned a finite number into an infinity.
///
/// Taken by value for the same reason as [`exact_out`]: a `DOUBLE` target is already holding its
/// own answer and has nothing left to do but say so.
#[expect(
    clippy::cast_possible_truncation,
    reason = "narrowing to a float is what a cast to FLOAT is, and the line below catches the loss"
)]
fn approximate_out(run: Vec<f64>, single: bool) -> Option<Data> {
    if !single {
        return Some(Data::Float64(run.into()));
    }
    let mut out = Vec::with_capacity(run.len());
    for number in run {
        let narrowed = number as f32;
        if narrowed.is_infinite() && number.is_finite() {
            return None;
        }
        out.push(narrowed);
    }
    Some(Data::Float32(out.into()))
}

/// Casts one value.
///
/// Null casts to null of the target type, which is not a special case so much as the only sensible
/// reading: there is no value to convert and no conversion can fail.
///
/// # Errors
///
/// If the value cannot be represented in the target type and `try_cast` is false, or if the pair
/// of types is one this does not handle yet.
pub fn cast_value(value: &Value, target: &LogicalType, try_cast: bool) -> Result<Value> {
    if value.is_null() || matches!(target, LogicalType::Null) {
        return Ok(Value::Null);
    }
    if &value.logical_type() == target {
        return Ok(value.clone());
    }
    match convert(value, target) {
        Ok(converted) => Ok(converted),
        Err(error) if try_cast && recoverable(&error) => Ok(Value::Null),
        Err(error) => Err(error),
    }
}

/// Whether `TRY_CAST` turns this failure into a null.
fn recoverable(error: &Error) -> bool {
    matches!(error.code(), ErrorCode::Conversion | ErrorCode::OutOfRange)
}

fn convert(value: &Value, target: &LogicalType) -> Result<Value> {
    match target {
        LogicalType::Boolean => to_boolean(value),
        LogicalType::TinyInt
        | LogicalType::SmallInt
        | LogicalType::Integer
        | LogicalType::BigInt
        | LogicalType::HugeInt
        | LogicalType::UTinyInt
        | LogicalType::USmallInt
        | LogicalType::UInteger
        | LogicalType::UBigInt
        | LogicalType::UHugeInt => to_integer(value, target),
        LogicalType::Float => to_float(value),
        LogicalType::Double => to_double(value),
        LogicalType::Decimal { width, scale } => to_decimal(value, *width, *scale),
        LogicalType::Varchar => Ok(Value::Varchar(value.to_string())),
        LogicalType::Blob => to_blob(value),
        LogicalType::Date => to_date(value),
        LogicalType::Time => to_time(value),
        LogicalType::Timestamp => to_timestamp(value),
        other => {
            Err(Error::not_implemented(format!("a cast from {} to {other}", value.logical_type())))
        }
    }
}

/// The failure DuckDB reports for a number that does not fit, in the words DuckDB uses.
///
/// Both types are named the way a message names them, which is the integer they are stored in, so
/// 300 going into a `TINYINT` is `INT32` to `INT8`. This is the sentence for a value that was
/// already a number. A string that reads as a number too big for the target is a conversion
/// failure instead, and a decimal and a target decimal each have their own sentence below.
fn out_of_range(value: &Value, target: &LogicalType) -> Error {
    Error::conversion(format!(
        "Type {} with value {value} can't be cast because the value is out of range for the destination type {}",
        value.logical_type().physical_name(),
        target.physical_name()
    ))
}

/// The failure DuckDB reports for a string that does not read as the target type.
///
/// The source is the word `string` rather than the name of a type, because the message is written
/// where the value is already a run of bytes. A decimal target is the one that is spelled out in
/// full with its scale, and it is also the one that quotes the value with double quotes, which
/// looks like an accident upstream and is what the binary does.
fn not_convertible(text: &str, target: &LogicalType) -> Error {
    if matches!(target, LogicalType::Decimal { .. }) {
        return Error::conversion(format!("Could not convert string \"{text}\" to {target}"));
    }
    Error::conversion(format!("Could not convert string '{text}' to {}", target.physical_name()))
}

/// The failure DuckDB reports for a pair of types nothing casts between, in the words DuckDB uses.
///
/// It is a conversion failure and not an unimplemented one, which is what makes `TRY_CAST` answer
/// null for it the way DuckDB answers null. That does not soften what the module documentation
/// says about not turning a missing feature into a column of nulls: this is only reached from a
/// target that is built here, for a source DuckDB refuses as well, one measured statement at a
/// time. A target nobody has written the code for still raises out of `convert` and `TRY_CAST`
/// still does not swallow it.
fn no_cast(value: &Value, target: &LogicalType) -> Error {
    Error::conversion(format!("Unimplemented type for cast ({} -> {target})", value.logical_type()))
}

/// The failure DuckDB reports for a number that does not fit a decimal, in the words DuckDB uses.
///
/// There are two sentences and the source picks which one. A decimal that does not fit another
/// decimal is the `Casting value` one, everything else is the `Could not cast value` one, and a
/// float or a double is written with six digits after the point in it because the message is built
/// with the C `%f` that does that.
fn no_decimal(value: &Value, target: &LogicalType) -> Error {
    let written = match value {
        Value::Decimal { .. } => {
            return Error::conversion(format!(
                "Casting value \"{value}\" to type {target} failed: value is out of range!"
            ));
        }
        Value::Float(real) => format!("{real:.6}"),
        Value::Double(real) => format!("{real:.6}"),
        other => other.to_string(),
    };
    Error::conversion(format!("Could not cast value {written} to {target}"))
}

/// The failure DuckDB reports for a decimal that does not fit an integer, in the words DuckDB uses.
///
/// The number in it is the whole the decimal rounded to and not the decimal that was written, so
/// 999.9 going into a `TINYINT` is reported as 1000.
fn no_integer(whole: i128, target: &LogicalType) -> Error {
    Error::conversion(format!(
        "Failed to cast decimal value {whole} to type {}",
        target.physical_name()
    ))
}

fn to_boolean(value: &Value) -> Result<Value> {
    if let Value::Varchar(text) = value {
        return match text.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "1" => Ok(Value::Boolean(true)),
            "false" | "f" | "no" | "n" | "0" => Ok(Value::Boolean(false)),
            _ => Err(not_convertible(text, &LogicalType::Boolean)),
        };
    }
    match integral(value) {
        Some(whole) => Ok(Value::Boolean(whole != 0)),
        None => match approximate(value) {
            Some(number) => Ok(Value::Boolean(number != 0.0)),
            None => Err(no_cast(value, &LogicalType::Boolean)),
        },
    }
}

/// Three sources and three sentences. A string that will not read as a whole number and a string
/// that reads as one too big for the target are the same conversion failure upstream, a decimal
/// says which whole it rounded to, and a number that was already a number says it is out of range.
fn to_integer(value: &Value, target: &LogicalType) -> Result<Value> {
    if let Value::Varchar(text) = value {
        let whole = parse_integer(text).ok_or_else(|| not_convertible(text, target))?;
        return fit(whole, target).ok_or_else(|| not_convertible(text, target));
    }
    if let Value::Decimal { unscaled, scale, .. } = *value {
        let whole = rounded_decimal(unscaled, scale);
        return fit(whole, target).ok_or_else(|| no_integer(whole, target));
    }
    let whole = match integral(value) {
        Some(whole) => whole,
        None => rounded(value, target)?,
    };
    fit(whole, target).ok_or_else(|| out_of_range(value, target))
}

/// A decimal as a whole number, rounded half away from zero the way DuckDB rounds.
fn rounded_decimal(unscaled: i128, scale: u8) -> i128 {
    let factor = pow10(scale);
    let half = factor / 2;
    let shifted = if unscaled >= 0 { unscaled + half } else { unscaled - half };
    shifted / factor
}

/// A float as a whole number, rounded half away from zero the way DuckDB rounds.
fn rounded(value: &Value, target: &LogicalType) -> Result<i128> {
    let number = approximate(value).ok_or_else(|| no_cast(value, target))?;
    if !number.is_finite() {
        return Err(out_of_range(value, target));
    }
    let number = number.round();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the range check below is what decides whether the value fits"
    )]
    if (-1.7014118346046923e38..=1.7014118346046923e38).contains(&number) {
        Ok(number as i128)
    } else {
        Err(out_of_range(value, target))
    }
}

fn parse_integer(text: &str) -> Option<i128> {
    text.trim().parse::<i128>().ok()
}

/// A number too big for a float is an infinity when it was written as a string and a failure when
/// it was already a number, which is the string parser saturating rather than two opinions about
/// the same question. `'1e40'::FLOAT` is `inf` upstream and `1e40::FLOAT` is out of range.
fn to_float(value: &Value) -> Result<Value> {
    if let Value::Varchar(text) = value {
        let written =
            text.trim().parse::<f64>().map_err(|_| not_convertible(text, &LogicalType::Float))?;
        return Ok(Value::Float(narrowed(written)));
    }
    let number = approximate(value).ok_or_else(|| no_cast(value, &LogicalType::Float))?;
    let single = narrowed(number);
    if single.is_infinite() && number.is_finite() {
        return Err(out_of_range(value, &LogicalType::Float));
    }
    Ok(Value::Float(single))
}

/// A double as a float, which is where a number too big to be one becomes an infinity.
#[expect(
    clippy::cast_possible_truncation,
    reason = "narrowing to a float is what a cast to FLOAT is"
)]
fn narrowed(number: f64) -> f32 {
    number as f32
}

fn to_double(value: &Value) -> Result<Value> {
    let number = match value {
        Value::Varchar(text) => {
            text.trim().parse::<f64>().map_err(|_| not_convertible(text, &LogicalType::Double))?
        }
        _ => approximate(value).ok_or_else(|| no_cast(value, &LogicalType::Double))?,
    };
    Ok(Value::Double(number))
}

fn to_decimal(value: &Value, width: u8, scale: u8) -> Result<Value> {
    let target = LogicalType::Decimal { width, scale };
    if let Value::Varchar(text) = value {
        // A string keeps the conversion sentence whether it failed to read at all or read as a
        // number with too many digits, which is one sentence where a number gets two.
        let unscaled = parse_decimal(text, scale).ok_or_else(|| not_convertible(text, &target))?;
        if digits(unscaled) > width {
            return Err(not_convertible(text, &target));
        }
        return Ok(Value::Decimal { unscaled, width, scale });
    }
    let unscaled = match value {
        Value::Decimal { unscaled, scale: from, .. } => rescale(*unscaled, *from, scale),
        _ => match integral(value) {
            Some(whole) => whole.checked_mul(pow10(scale)),
            None => {
                let number = approximate(value).ok_or_else(|| no_cast(value, &target))?;
                if !number.is_finite() {
                    return Err(no_decimal(value, &target));
                }
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the width check below is what decides whether the value fits"
                )]
                let scaled = (number * pow10(scale) as f64).round() as i128;
                Some(scaled)
            }
        },
    };
    let unscaled = unscaled.ok_or_else(|| no_decimal(value, &target))?;
    if digits(unscaled) > width {
        return Err(no_decimal(value, &target));
    }
    Ok(Value::Decimal { unscaled, width, scale })
}

/// A written decimal at the given scale, with digits past the scale rounded away.
fn parse_decimal(text: &str, scale: u8) -> Option<i128> {
    let text = text.trim();
    let (sign, body) = match text.strip_prefix('-') {
        Some(rest) => (-1i128, rest),
        None => (1i128, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = match body.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (body, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    if !whole.bytes().chain(fraction.bytes()).all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let written: i128 = format!("{whole}{fraction}").parse().ok()?;
    let scaled = rescale(written, u8::try_from(fraction.len()).ok()?, scale)?;
    Some(sign * scaled)
}

/// Text to bytes, which is not the bytes of the text.
///
/// A blob prints with every byte that is not a printable ASCII character written as `\xNN`, and
/// reading one back has to undo that, so `'\x41'` is one byte and not four. Everything outside that
/// escape is taken as itself, and only if it is ASCII: a byte above 127 in the text has no reading
/// here that round trips, because the text it came from was UTF-8 and the blob is not, so DuckDB
/// refuses it and says which character it refused on. This is the one cast where being lenient
/// would quietly turn a two byte character into two bytes of a blob that nothing wrote.
fn to_blob(value: &Value) -> Result<Value> {
    let Value::Varchar(text) = value else {
        return Err(no_cast(value, &LogicalType::Blob));
    };
    let escape = |what: &str| {
        Error::conversion(format!(
            "Invalid hex escape code encountered in string -> blob conversion of string \"{text}\": {what}"
        ))
    };
    let source = text.as_bytes();
    let mut out = Vec::with_capacity(source.len());
    let mut at = 0;
    while at < source.len() {
        let byte = source[at];
        if byte == b'\\' {
            let Some(code) = source.get(at + 1..at + 4) else {
                return Err(escape("unterminated escape code at end of blob"));
            };
            let (high, low) = (hex(code[1]), hex(code[2]));
            match (code[0], high, low) {
                (b'x', Some(high), Some(low)) => out.push(high * 16 + low),
                _ => {
                    // The four bytes as they were written, which is what DuckDB puts here and is
                    // the only part of the message that says where in the string to look. Lossy
                    // because those four can cut a character in half, and a message is not worth
                    // a panic.
                    return Err(escape(&String::from_utf8_lossy(&source[at..at + 4])));
                }
            }
            at += 4;
            continue;
        }
        if !byte.is_ascii() {
            return Err(Error::conversion(format!(
                "Invalid byte encountered in STRING -> BLOB conversion of string \"{text}\". All non-ascii characters must be escaped with hex codes (e.g. \\xAA)"
            )));
        }
        out.push(byte);
        at += 1;
    }
    Ok(Value::Blob(out))
}

/// One hex digit as a number, either case.
fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn to_date(value: &Value) -> Result<Value> {
    match value {
        Value::Timestamp(micros) => i32::try_from(micros.div_euclid(MICROS_PER_DAY))
            .map(Value::Date)
            .map_err(|_| out_of_range(value, &LogicalType::Date)),
        Value::Varchar(text) => match parse_date(text) {
            Ok(days) => Ok(Value::Date(days)),
            Err(fault) => Err(fault.said("date", text, "(YYYY-MM-DD)")),
        },
        _ => Err(no_cast(value, &LogicalType::Date)),
    }
}

/// A `TIME`, which reads a written clock with far more slack than a date or a timestamp does.
///
/// The parser is upstream's and it is a different parser, not the same one with a different
/// message. There is one sentence for every way of failing, the seconds and the fraction are both
/// optional, a date in front is read and thrown away, and whatever is after the numbers is
/// ignored, so `'12:34:56 UTC'` and `'12:34:56abc'` are both twelve thirty four.
fn to_time(value: &Value) -> Result<Value> {
    match value {
        Value::Timestamp(micros) => Ok(Value::Time(micros.rem_euclid(MICROS_PER_DAY))),
        Value::Varchar(text) => parse_clock(text).map(Value::Time).ok_or_else(|| bad_time(text)),
        _ => Err(no_cast(value, &LogicalType::Time)),
    }
}

/// The one failure a written time has, which says the format even though it is the range sentence.
fn bad_time(text: &str) -> Error {
    Error::conversion(format!(
        "time field value out of range: \"{text}\", expected format is ([YYYY-MM-DD ]HH:MM:SS[.MS])"
    ))
}

fn to_timestamp(value: &Value) -> Result<Value> {
    match value {
        Value::Date(days) => Ok(Value::Timestamp(i64::from(*days) * MICROS_PER_DAY)),
        Value::Varchar(text) => match parse_timestamp(text) {
            Ok(micros) => Ok(Value::Timestamp(micros)),
            Err(fault) => Err(fault.said("timestamp", text, TIMESTAMP_FORMAT)),
        },
        _ => Err(no_cast(value, &LogicalType::Timestamp)),
    }
}

/// What the timestamp message says the format should have been, including the parts of it this
/// parser does not read yet, because the sentence is upstream's and not a description of this.
const TIMESTAMP_FORMAT: &str = "(YYYY-MM-DD HH:MM[:SS[.US]][±HH[:MM[:SS]]| ZONE])";

/// A read of a written date or time that says which way it went wrong when it did.
type Parsed<T> = std::result::Result<T, Fault>;

/// Which way a written date or timestamp was wrong, because DuckDB has a sentence for each.
///
/// Text that is not three numbers with dashes between them is a format failure and the message
/// says what the format should have been. Three numbers naming a day that does not exist is a
/// range failure and the message does not repeat the format. The time after a date follows the
/// same split and it is not the split anybody would guess: an hour past 24 is a range failure and
/// a minute past 59 is a format one, so `'2020-01-01 25:00:00'` and `'2020-01-01 10:61:00'` are
/// two different sentences upstream and are two different sentences here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    Format,
    Range,
}

impl Fault {
    /// The failure in DuckDB's words, where `what` is the word it uses for the type.
    fn said(self, what: &str, text: &str, format: &str) -> Error {
        match self {
            Self::Format => Error::conversion(format!(
                "invalid {what} field format: \"{text}\", expected format is {format}"
            )),
            Self::Range => {
                Error::conversion(format!("{what} field value out of range: \"{text}\""))
            }
        }
    }
}

/// `YYYY-MM-DD` as days since the epoch, with a time after it allowed and thrown away.
///
/// The time still has to be a time for the date to be a date, which is why `'2020-01-01 10:00'`
/// is the first of January and `'2020-01-01 abc'` is a format failure rather than a date with
/// something ignored after it.
fn parse_date(text: &str) -> Parsed<i32> {
    let (date, time) = split_time(text.trim());
    let days = parse_day(date)?;
    if let Some(time) = time {
        parse_time(time)?;
    }
    Ok(days)
}

/// `YYYY-MM-DD` with an optional `HH:MM:SS[.ffffff]` after it, as microseconds since the epoch.
fn parse_timestamp(text: &str) -> Parsed<i64> {
    let (date, time) = split_time(text.trim());
    let days = i64::from(parse_day(date)?);
    let micros = match time {
        None => 0,
        Some(time) => parse_time(time)?,
    };
    days.checked_mul(MICROS_PER_DAY).and_then(|start| start.checked_add(micros)).ok_or(Fault::Range)
}

/// The date and the time in a written timestamp, which are separated by a space or by a `T`.
fn split_time(text: &str) -> (&str, Option<&str>) {
    match text.split_once([' ', 'T']) {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    }
}

/// `YYYY-MM-DD` as days since the epoch.
fn parse_day(text: &str) -> Parsed<i32> {
    let mut parts = text.split('-');
    let year: i32 = field(parts.next())?;
    let month: u32 = field(parts.next())?;
    let day: u32 = field(parts.next())?;
    if parts.next().is_some() {
        return Err(Fault::Format);
    }
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return Err(Fault::Range);
    }
    let days = days_from_civil(year, month, day);
    // The two ends of the `i32` are what DuckDB keeps for its infinities, and past them
    // `days_from_civil` has wrapped, which the round trip is what catches. So the year that does
    // not fit says its day is out of range rather than answering with some other day.
    if days == i32::MAX || days == i32::MIN || civil_from_days(days) != (year, month, day) {
        return Err(Fault::Range);
    }
    Ok(days)
}

/// One field of a written date, which has to be there and has to be a number.
fn field<T: FromStr>(part: Option<&str>) -> Parsed<T> {
    part.ok_or(Fault::Format)?.parse().map_err(|_| Fault::Format)
}

/// How many days that month of that year has.
///
/// A date that names the thirty first of April is out of range upstream and was the first of May
/// here, which is a wrong answer and not only a wrong message.
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        _ => 28,
    }
}

/// `HH:MM:SS[.ffffff]` as microseconds since midnight.
///
/// Midnight at the end of the day is a time, so `'2020-01-01 24:00:00'` is the second of January,
/// and anything past it is out of range rather than badly written.
fn parse_time(text: &str) -> Parsed<i64> {
    let (clock, fraction) = match text.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (text, None),
    };
    let mut parts = clock.split(':');
    let hours: i64 = field(parts.next())?;
    let minutes: i64 = field(parts.next())?;
    let seconds: i64 = field(parts.next().or(Some("0")))?;
    if parts.next().is_some() {
        return Err(Fault::Format);
    }
    // The hour is checked before the arithmetic below rather than after it, because a written hour
    // is only bounded by what an `i64` holds and the multiply would be the one that overflowed.
    if !(0..=24).contains(&hours) {
        return Err(Fault::Range);
    }
    if !(0..60).contains(&minutes) || !(0..60).contains(&seconds) {
        return Err(Fault::Format);
    }
    let micros = match fraction {
        None => 0,
        Some(digits) => {
            if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(Fault::Format);
            }
            let padded = format!("{digits:0<6}");
            padded.get(..6).ok_or(Fault::Format)?.parse::<i64>().map_err(|_| Fault::Format)?
        }
    };
    let since_midnight = ((hours * 60 + minutes) * 60 + seconds) * 1_000_000 + micros;
    if since_midnight > MICROS_PER_DAY {
        return Err(Fault::Range);
    }
    Ok(since_midnight)
}

/// `[YYYY-MM-DD ]HH:MM[:SS[.ffffff]]` as microseconds since midnight, ignoring what follows.
///
/// This is the `TIME` parser and it is deliberately not `parse_time` above, because the rules are
/// not the same ones. A date in front is read and thrown away and a bare date is midnight, so
/// `'2020-01-02'` is `00:00:00`, but a date with nothing after the separator is refused, which is
/// why `'2020-01-02 '` fails and `' 12:34:56 '` does not. Every one of these was measured.
fn parse_clock(text: &str) -> Option<i64> {
    let text = text.trim_start();
    let clock = match split_time(text) {
        // A dash is a date only when it comes before any clock, because the offset on the end of
        // `'12:34:56-05'` is a dash too and that is a time and not a day.
        (day, rest) if day.contains('-') && !day.contains(':') => {
            parse_day(day).ok()?;
            match rest {
                // A day on its own is midnight, and a day with a separator and nothing after it is
                // a time that was not written.
                None => return Some(0),
                Some(rest) => rest,
            }
        }
        _ => text,
    };
    clock_micros(clock)
}

/// The clock itself, once any date in front of it has been dealt with.
fn clock_micros(text: &str) -> Option<i64> {
    let mut rest = text;
    let hours = number(&mut rest)?;
    if !eat(&mut rest, b':') {
        return None;
    }
    let minutes = number(&mut rest)?;
    let mut seconds = 0;
    let mut micros = 0;
    // A colon with nothing after it is not a failure, so `'12:34:'` is twelve thirty four, but a
    // colon with something after it that is not a number is. The dot is slacker still and anything
    // at all can follow it, which is how `'12:34:56.abc'` comes back as twelve thirty four and six.
    if eat(&mut rest, b':') && !rest.is_empty() {
        seconds = number(&mut rest)?;
        if eat(&mut rest, b'.') {
            micros = fraction(rest);
        }
    }
    if !(0..=24).contains(&hours) || !(0..60).contains(&minutes) || !(0..60).contains(&seconds) {
        return None;
    }
    let since_midnight = ((hours * 60 + minutes) * 60 + seconds) * 1_000_000 + micros;
    (since_midnight <= MICROS_PER_DAY).then_some(since_midnight)
}

/// The number at the front of `rest`, which is moved past it. A number too long to hold is `None`
/// rather than a wrap, which is the same answer an hour of 25 gets.
fn number(rest: &mut &str) -> Option<i64> {
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let (number, tail) = rest.split_at(digits);
    *rest = tail;
    number.parse().ok()
}

/// Whether `rest` starts with `byte`, which is moved past when it does.
fn eat(rest: &mut &str, byte: u8) -> bool {
    let Some(tail) = rest.strip_prefix(byte as char) else {
        return false;
    };
    *rest = tail;
    true
}

/// The microseconds a written fraction of a second is worth, reading six digits at the most and
/// throwing away whatever is after them, so `.1234567` is `.123456` and `.5` is half a second.
fn fraction(text: &str) -> i64 {
    let digits: String = text.chars().take_while(char::is_ascii_digit).take(6).collect();
    if digits.is_empty() {
        return 0;
    }
    format!("{digits:0<6}").parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cast_to(value: Value, target: &LogicalType) -> Result<Value> {
        cast_value(&value, target, false)
    }

    #[test]
    fn null_casts_to_null_and_never_fails() {
        let cast = cast_to(Value::Null, &LogicalType::Integer).expect("null casts");
        assert_eq!(cast, Value::Null);
    }

    #[test]
    fn a_number_that_fits_widens_and_one_that_does_not_says_so() {
        assert_eq!(
            cast_to(Value::Integer(7), &LogicalType::BigInt).expect("7 fits"),
            Value::BigInt(7)
        );
        let error = cast_to(Value::BigInt(40_000), &LogicalType::SmallInt)
            .expect_err("40000 is not a smallint");
        assert!(error.message().contains("out of range"), "{error}");
        assert_eq!(error.code(), ErrorCode::Conversion);
    }

    /// The whole reason the error code is checked rather than a second set of functions written.
    #[test]
    fn a_try_cast_that_does_not_fit_is_null_and_one_that_is_unimplemented_still_raises() {
        let fitted = cast_value(&Value::BigInt(40_000), &LogicalType::SmallInt, true)
            .expect("try_cast swallows the range failure");
        assert_eq!(fitted, Value::Null);
        let error = cast_value(&Value::Integer(1), &LogicalType::Interval, true)
            .expect_err("try_cast does not invent an interval");
        assert_eq!(error.code(), ErrorCode::NotImplemented);
    }

    #[test]
    fn a_float_casts_to_an_integer_by_rounding_rather_than_by_truncating() {
        assert_eq!(
            cast_to(Value::Double(1.5), &LogicalType::Integer).expect("rounds"),
            Value::Integer(2)
        );
        assert_eq!(
            cast_to(Value::Double(-1.5), &LogicalType::Integer).expect("rounds away from zero"),
            Value::Integer(-2)
        );
    }

    #[test]
    fn a_string_that_is_a_number_casts_and_one_that_is_not_does_not() {
        assert_eq!(
            cast_to(Value::Varchar(" 42 ".into()), &LogicalType::Integer).expect("42"),
            Value::Integer(42)
        );
        let error = cast_to(Value::Varchar("nope".into()), &LogicalType::Integer)
            .expect_err("nope is not a number");
        assert!(error.message().contains("Could not convert"), "{error}");
    }

    #[test]
    fn anything_prints_itself_when_it_casts_to_a_string() {
        assert_eq!(
            cast_to(Value::Boolean(true), &LogicalType::Varchar).expect("prints"),
            Value::Varchar("true".into())
        );
        assert_eq!(
            cast_to(Value::Date(0), &LogicalType::Varchar).expect("prints"),
            Value::Varchar("1970-01-01".into())
        );
    }

    #[test]
    fn a_decimal_keeps_its_value_across_a_change_of_scale() {
        let target = LogicalType::decimal(10, 2).expect("a legal decimal");
        let widened =
            cast_to(Value::Decimal { unscaled: 5, width: 4, scale: 1 }, &target).expect("rescales");
        assert_eq!(widened, Value::Decimal { unscaled: 50, width: 10, scale: 2 });
        let written =
            cast_to(Value::Varchar("3.14159".into()), &target).expect("rounds to two places");
        assert_eq!(written, Value::Decimal { unscaled: 314, width: 10, scale: 2 });
    }

    #[test]
    fn a_decimal_that_needs_more_digits_than_its_width_is_caught() {
        let target = LogicalType::decimal(3, 2).expect("a legal decimal");
        let error = cast_to(Value::Integer(100), &target).expect_err("100.00 needs five digits");
        assert_eq!(error.message(), "Could not cast value 100 to DECIMAL(3,2)");
    }

    /// Seven sentences, one per shape, every one of them read off `v2.0.0-dev84237 (cc7e7bac7f)`
    /// one statement at a time. The type names in them are the physical ones, which is why nothing
    /// here says INTEGER or TINYINT.
    #[test]
    fn a_failed_cast_says_the_sentence_duckdb_says() {
        let decimal = LogicalType::decimal(4, 1).expect("a legal decimal");
        let said = |value: Value, target: &LogicalType| {
            cast_to(value, target).expect_err("this does not cast").message().to_string()
        };
        assert_eq!(
            said(Value::Varchar("abc".into()), &LogicalType::TinyInt),
            "Could not convert string 'abc' to INT8"
        );
        assert_eq!(
            said(Value::Varchar("300".into()), &LogicalType::TinyInt),
            "Could not convert string '300' to INT8"
        );
        assert_eq!(
            said(Value::Varchar("abc".into()), &decimal),
            "Could not convert string \"abc\" to DECIMAL(4,1)"
        );
        assert_eq!(
            said(Value::Integer(300), &LogicalType::TinyInt),
            "Type INT32 with value 300 can't be cast because the value is out of range for the destination type INT8"
        );
        assert_eq!(
            said(Value::Decimal { unscaled: 9999, width: 4, scale: 1 }, &LogicalType::TinyInt),
            "Failed to cast decimal value 1000 to type INT8"
        );
        assert_eq!(
            said(Value::Integer(200_000), &decimal),
            "Could not cast value 200000 to DECIMAL(4,1)"
        );
        assert_eq!(
            said(Value::Double(1.5e30), &decimal),
            "Could not cast value 1499999999999999889089448902656.000000 to DECIMAL(4,1)"
        );
        assert_eq!(
            said(Value::Decimal { unscaled: 2_000_005, width: 7, scale: 1 }, &decimal),
            "Casting value \"200000.5\" to type DECIMAL(4,1) failed: value is out of range!"
        );
        assert_eq!(
            said(Value::Date(0), &LogicalType::Integer),
            "Unimplemented type for cast (DATE -> INTEGER)"
        );
    }

    /// A pair with no cast between it is a conversion failure here because it is one upstream, and
    /// upstream answers null for it under `TRY_CAST`. A target nobody has built is still the other
    /// kind, which is the distinction the module documentation is about.
    #[test]
    fn a_pair_with_no_cast_is_null_under_try_cast_and_a_missing_target_is_not() {
        let refused = cast_value(&Value::Date(0), &LogicalType::Integer, true)
            .expect("try_cast swallows a pair duckdb has no cast for");
        assert_eq!(refused, Value::Null);
        let error = cast_value(&Value::Integer(1), &LogicalType::Interval, true)
            .expect_err("try_cast does not invent an interval");
        assert_eq!(error.code(), ErrorCode::NotImplemented);
    }

    /// The number that does not fit a float is an infinity when it was written down and a failure
    /// when it was already a number, which is the string parser saturating rather than two
    /// opinions about the same question.
    #[test]
    fn a_written_number_too_big_for_a_float_is_an_infinity() {
        let written = cast_to(Value::Varchar("1e40".into()), &LogicalType::Float).expect("inf");
        assert_eq!(written, Value::Float(f32::INFINITY));
        let error =
            cast_to(Value::Double(1e40), &LogicalType::Float).expect_err("1e40 is not a float");
        assert!(error.message().contains("out of range"), "{error}");
    }

    #[test]
    fn a_written_date_and_a_written_timestamp_read_back() {
        assert_eq!(
            cast_to(Value::Varchar("2013-07-15".into()), &LogicalType::Date).expect("a date"),
            Value::Date(days_from_civil(2013, 7, 15))
        );
        let stamp =
            cast_to(Value::Varchar("2013-07-15 10:30:00.5".into()), &LogicalType::Timestamp)
                .expect("a timestamp");
        let expected = i64::from(days_from_civil(2013, 7, 15)) * MICROS_PER_DAY
            + 10 * 3_600_000_000
            + 30 * 60_000_000
            + 500_000;
        assert_eq!(stamp, Value::Timestamp(expected));
    }

    #[test]
    fn a_date_that_is_not_a_date_is_refused_rather_than_guessed_at() {
        for text in ["2013-07", "yesterday", "2013-07-15-01"] {
            let error = cast_to(Value::Varchar(text.into()), &LogicalType::Date)
                .expect_err("this is not a date");
            assert_eq!(
                error.message(),
                format!("invalid date field format: \"{text}\", expected format is (YYYY-MM-DD)")
            );
        }
        for text in ["2013-13-01", "2021-02-29", "2021-04-31"] {
            let error =
                cast_to(Value::Varchar(text.into()), &LogicalType::Date).expect_err("no such day");
            assert_eq!(error.message(), format!("date field value out of range: \"{text}\""));
        }
    }

    /// A written date is allowed to carry a time, which is thrown away, but it still has to be a
    /// time. The year that is a leap year has the day the year after it does not.
    #[test]
    fn a_date_takes_a_time_it_does_not_keep() {
        let kept = cast_to(Value::Varchar(" 2020-02-29 10:30:00 ".into()), &LogicalType::Date)
            .expect("a leap day with a time on it");
        assert_eq!(kept, Value::Date(days_from_civil(2020, 2, 29)));
        let error = cast_to(Value::Varchar("2020-02-29 10:70:00".into()), &LogicalType::Date)
            .expect_err("seventy minutes past ten is not a time");
        assert_eq!(
            error.message(),
            "invalid date field format: \"2020-02-29 10:70:00\", expected format is (YYYY-MM-DD)"
        );
    }

    /// Midnight at the end of the day is a time, and one second past it is not a time at all. The
    /// date keeps the day it was written with rather than the day that time rolls into, which the
    /// timestamp below does not.
    #[test]
    fn the_end_of_the_day_is_a_time_and_a_moment_after_it_is_not() {
        let midnight = cast_to(Value::Varchar("2020-01-01 24:00:00".into()), &LogicalType::Date)
            .expect("the end of the first is still the first");
        assert_eq!(midnight, Value::Date(days_from_civil(2020, 1, 1)));
        for (text, said) in [
            ("2020-01-01 24:00:01", "date field value out of range: \"2020-01-01 24:00:01\""),
            ("2020-01-01 25:00:00", "date field value out of range: \"2020-01-01 25:00:00\""),
            (
                "2020-01-01 10:00:60",
                "invalid date field format: \"2020-01-01 10:00:60\", expected format is (YYYY-MM-DD)",
            ),
        ] {
            let error = cast_to(Value::Varchar(text.into()), &LogicalType::Date)
                .expect_err("this is not a time");
            assert_eq!(error.message(), said);
        }
    }

    /// The timestamp says the same two things about itself that the date does, in its own words
    /// and with the long format string, and it does roll into the next day at the end of this one.
    #[test]
    fn a_timestamp_that_is_not_one_says_so_in_its_own_words() {
        let rolled = cast_to(Value::Varchar("2020-01-01 24:00:00".into()), &LogicalType::Timestamp)
            .expect("the end of the first is the start of the second");
        assert_eq!(
            rolled,
            Value::Timestamp(i64::from(days_from_civil(2020, 1, 2)) * MICROS_PER_DAY)
        );
        let missing = cast_to(Value::Varchar("2020-01-01".into()), &LogicalType::Timestamp)
            .expect("a day with no time on it is midnight");
        assert_eq!(
            missing,
            Value::Timestamp(i64::from(days_from_civil(2020, 1, 1)) * MICROS_PER_DAY)
        );
        for (text, said) in [
            ("2020-01-01 24:00:01", "timestamp field value out of range: \"2020-01-01 24:00:01\""),
            ("2021-02-29 10:00:00", "timestamp field value out of range: \"2021-02-29 10:00:00\""),
            (
                "abc",
                "invalid timestamp field format: \"abc\", expected format is (YYYY-MM-DD HH:MM[:SS[.US]][±HH[:MM[:SS]]| ZONE])",
            ),
        ] {
            let error = cast_to(Value::Varchar(text.into()), &LogicalType::Timestamp)
                .expect_err("this is not a timestamp");
            assert_eq!(error.message(), said);
        }
    }

    /// Every rule the TIME parser has, per #228, all of them measured against the pinned binary.
    ///
    /// The slack is the point. The seconds and the fraction are optional, a colon with nothing
    /// after it is not a failure, the fraction is cut at six digits rather than refused, a date in
    /// front is thrown away, and whatever is behind the numbers is ignored, which is what makes
    /// `'12:34:56 UTC'` a time here.
    #[test]
    fn a_written_time_is_read_with_all_the_slack_duckdb_reads_it_with() {
        let at = |hours: i64, minutes: i64, seconds: i64, micros: i64| {
            Value::Time(((hours * 60 + minutes) * 60 + seconds) * 1_000_000 + micros)
        };
        for (text, expected) in [
            ("12:34:56", at(12, 34, 56, 0)),
            ("12:34:56.123456", at(12, 34, 56, 123_456)),
            ("12:34:56.5", at(12, 34, 56, 500_000)),
            ("12:34:56.1234567", at(12, 34, 56, 123_456)),
            ("12:34", at(12, 34, 0, 0)),
            ("12:34:", at(12, 34, 0, 0)),
            ("12:34:56.", at(12, 34, 56, 0)),
            ("12:34:56.abc", at(12, 34, 56, 0)),
            ("1:2:3", at(1, 2, 3, 0)),
            ("0:0:0", at(0, 0, 0, 0)),
            (" 12:34:56 ", at(12, 34, 56, 0)),
            ("24:00:00", at(24, 0, 0, 0)),
            ("12:34:56 UTC", at(12, 34, 56, 0)),
            ("12:34:56+05:30", at(12, 34, 56, 0)),
            ("12:34:56-05", at(12, 34, 56, 0)),
            ("12:34:56abc", at(12, 34, 56, 0)),
            ("2024-01-02 03:04:05", at(3, 4, 5, 0)),
            ("2024-01-02T03:04:05", at(3, 4, 5, 0)),
            ("2020-01-02", at(0, 0, 0, 0)),
        ] {
            let time = cast_to(Value::Varchar(text.into()), &LogicalType::Time);
            assert_eq!(time.as_ref().ok(), Some(&expected), "{text}: {time:?}");
        }
    }

    /// One sentence for every way a written time can be wrong, which is not the split the date and
    /// the timestamp have, and it carries the format although it is the range wording.
    #[test]
    fn a_written_time_that_is_not_one_says_the_only_thing_duckdb_says_about_it() {
        for text in [
            "abc",
            "12",
            "24:00:01",
            "25:00:00",
            "10:70:00",
            "10:00:60",
            "12::56",
            "1234:56",
            "12:34:abc",
            "-01:00:00",
            "24:00:00.000001",
            "2024-13-02 03:04:05",
            "2020-01-02 ",
        ] {
            let error = cast_to(Value::Varchar(text.into()), &LogicalType::Time)
                .expect_err("this is not a time");
            assert_eq!(
                error.message(),
                format!(
                    "time field value out of range: \"{text}\", expected format is ([YYYY-MM-DD ]HH:MM:SS[.MS])"
                )
            );
        }
    }

    /// A timestamp keeps the clock and drops the day, and everything else has no cast at all.
    #[test]
    fn a_timestamp_casts_to_the_time_of_day_it_is() {
        let stamp = Value::Timestamp(i64::from(days_from_civil(2024, 1, 2)) * MICROS_PER_DAY + 5);
        assert_eq!(cast_to(stamp, &LogicalType::Time).expect("a time"), Value::Time(5));
        let before = Value::Timestamp(-1);
        assert_eq!(
            cast_to(before, &LogicalType::Time).expect("a time"),
            Value::Time(MICROS_PER_DAY - 1),
            "the last microsecond of 1969 is the last microsecond of the day"
        );
        for value in [Value::Date(0), Value::Integer(1)] {
            let written = value.logical_type();
            let error = cast_to(value, &LogicalType::Time).expect_err("no cast for this");
            assert_eq!(error.message(), format!("Unimplemented type for cast ({written} -> TIME)"));
        }
    }

    #[test]
    fn a_constant_vector_costs_one_conversion() {
        let input = Vector::constant(LogicalType::Integer, Value::Integer(3), 1024);
        let cast = cast(&input, &LogicalType::BigInt, false).expect("widens");
        assert_eq!(cast.form(), Form::Constant);
        assert_eq!(cast.len(), 1024);
        assert_eq!(cast.value_at(1000), Value::BigInt(3));
    }

    #[test]
    fn a_cast_to_the_type_it_already_is_is_the_same_vector() {
        let input = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Null, Value::Integer(3)],
        )
        .expect("three integers");
        let cast = cast(&input, &LogicalType::Integer, false).expect("free");
        assert_eq!(cast, input);
    }

    #[test]
    fn a_null_in_a_vector_stays_null_across_a_cast() {
        let input = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Null, Value::Integer(3)],
        )
        .expect("three integers");
        let cast = cast(&input, &LogicalType::Varchar, false).expect("prints");
        assert_eq!(cast.value_at(0), Value::Varchar("1".into()));
        assert_eq!(cast.value_at(1), Value::Null);
    }

    /// The row at a time path, copied here so that changing the one above cannot quietly change
    /// what the sweep is checked against.
    fn oracle(input: &Vector, target: &LogicalType, try_cast: bool) -> Result<Vector> {
        let mut values = Vec::with_capacity(input.len());
        for index in 0..input.len() {
            values.push(cast_value(&input.value_at(index), target, try_cast)?);
        }
        Vector::from_values(target.clone(), &values)
    }

    /// Both paths on one vector, which have to give the same answer or the same complaint.
    fn agrees(input: &Vector, target: &LogicalType) {
        let what = format!("{} to {target}", input.logical_type());
        match (cast(input, target, false), oracle(input, target, false)) {
            (Ok(fast), Ok(slow)) => assert_eq!(fast, slow, "{what}"),
            (Err(fast), Err(slow)) => assert_eq!(fast.message(), slow.message(), "{what}"),
            (Ok(fast), Err(slow)) => {
                panic!("{what}: the sweep answered {fast:?} and the loop said {slow}")
            }
            (Err(fast), Ok(slow)) => {
                panic!("{what}: the sweep said {fast} and the loop answered {slow:?}")
            }
        }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// A number for the generator below.
    ///
    /// Mostly small enough to fit every type here, and one time in eight large enough to fit a
    /// `BIGINT` and nothing narrower. The second half is the half that matters: a vector the sweep
    /// has to refuse is the case where this file is easiest to get wrong, and a generator that only
    /// made values that fit would test the easy half twice.
    fn small(rng: &mut Rng) -> i64 {
        if rng.below(8) == 0 {
            rng.below(300_000) as i64 - 150_000
        } else {
            rng.below(201) as i64 - 100
        }
    }

    /// One value of a type, inside that type's own range.
    fn sample(ty: &LogicalType, rng: &mut Rng) -> Value {
        match *ty {
            LogicalType::TinyInt => Value::TinyInt((small(rng) % 128) as i8),
            LogicalType::SmallInt => Value::SmallInt((small(rng) % 32_768) as i16),
            LogicalType::Integer => Value::Integer(small(rng) as i32),
            LogicalType::BigInt => Value::BigInt(small(rng)),
            LogicalType::HugeInt => Value::HugeInt(i128::from(small(rng))),
            LogicalType::UTinyInt => Value::UTinyInt((small(rng).unsigned_abs() % 256) as u8),
            LogicalType::USmallInt => Value::USmallInt((small(rng).unsigned_abs() % 65_536) as u16),
            LogicalType::UInteger => Value::UInteger(small(rng).unsigned_abs() as u32),
            LogicalType::UBigInt => Value::UBigInt(small(rng).unsigned_abs()),
            LogicalType::Float => Value::Float(small(rng) as f32 / 4.0),
            LogicalType::Double => Value::Double(small(rng) as f64 / 8.0),
            LogicalType::Decimal { width, scale } => {
                Value::Decimal { unscaled: i128::from(small(rng)) % pow10(width), width, scale }
            }
            ref other => panic!("the generator has no values for {other}"),
        }
    }

    /// Every numeric type this file claims, against every other, flat and through a dictionary, at
    /// three null densities. This is the test the rewrite rests on.
    #[test]
    fn every_numeric_pair_agrees_with_the_row_at_a_time_path() {
        // Most of the pairs below are pairs the sweep refuses, every refusal increments a process
        // wide counter, and the tests in `fallback` assert exact counts. This holds the same lock
        // they do so that a test that is about answers cannot fail a test that is about counting.
        let mut rng = Rng(0x5eed_cabb_a9e0_0001);
        let types: [LogicalType; 15] = [
            LogicalType::TinyInt,
            LogicalType::SmallInt,
            LogicalType::Integer,
            LogicalType::BigInt,
            LogicalType::HugeInt,
            LogicalType::UTinyInt,
            LogicalType::USmallInt,
            LogicalType::UInteger,
            LogicalType::UBigInt,
            LogicalType::Float,
            LogicalType::Double,
            // One decimal per physical container, because the container is what the write pass
            // matches on and a decimal that is two bytes wide and one that is sixteen take
            // different arms of it.
            LogicalType::decimal(4, 1).expect("a legal decimal"),
            LogicalType::decimal(9, 2).expect("a legal decimal"),
            LogicalType::decimal(18, 4).expect("a legal decimal"),
            LogicalType::decimal(30, 6).expect("a legal decimal"),
        ];
        let len = 37;
        for from in &types {
            for nulls in [0u64, 1, 3] {
                let values: Vec<Value> = (0..len)
                    .map(|_| {
                        if nulls > 0 && rng.below(nulls + 1) == 0 {
                            Value::Null
                        } else {
                            sample(from, &mut rng)
                        }
                    })
                    .collect();
                let flat = Vector::from_values(from.clone(), &values).expect("a flat vector");
                let codes: Vec<u32> = (0..len).map(|_| rng.below(len as u64) as u32).collect();
                let dictionary =
                    Vector::dictionary(codes, flat.clone()).expect("codes are in range");
                for into in &types {
                    // A cast to the type it already is hands the vector straight back, which for a
                    // dictionary means a dictionary, and the loop below always builds a flat one.
                    // Comparing those two would be comparing the shortcut against the definition of
                    // something else.
                    if into == from {
                        continue;
                    }
                    agrees(&flat, into);
                    agrees(&dictionary, into);
                }
            }
        }
    }

    /// The sweep is only worth having if it is the path a numeric cast actually takes, so this
    /// checks the counter rather than the answer.
    #[test]
    fn a_numeric_cast_does_not_reach_the_row_at_a_time_path_and_a_string_one_does() {
        fallback::reset();
        let input = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Null, Value::Integer(3)],
        )
        .expect("three integers");
        for target in [
            LogicalType::BigInt,
            LogicalType::Double,
            LogicalType::Float,
            LogicalType::decimal(18, 3).expect("a legal decimal"),
        ] {
            cast(&input, &target, false).expect("widens");
        }
        assert_eq!(fallback::count(Kernel::Cast, Form::Flat, Form::Flat), 0);
        cast(&input, &LogicalType::Varchar, false).expect("prints");
        assert_eq!(fallback::count(Kernel::Cast, Form::Flat, Form::Flat), 1);
        fallback::reset();
    }

    /// A vector the sweep refuses is a vector the loop below it has to explain, and `TRY_CAST` has
    /// to keep working through the refusal rather than being swallowed by it.
    #[test]
    fn one_value_that_does_not_fit_sends_the_whole_vector_back_to_the_loop() {
        let input = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(40_000), Value::Integer(3)],
        )
        .expect("three integers");
        let error =
            cast(&input, &LogicalType::SmallInt, false).expect_err("40000 is not a smallint");
        assert!(error.message().contains("40000"), "{error}");
        let tried = cast(&input, &LogicalType::SmallInt, true).expect("try_cast nulls it out");
        assert_eq!(tried.value_at(0), Value::SmallInt(1));
        assert_eq!(tried.value_at(1), Value::Null);
        assert_eq!(tried.value_at(2), Value::SmallInt(3));
    }

    /// Scale is the whole reason an integer and a decimal are read as one kind, so both directions
    /// of it get an assertion that names the number rather than a generated one that does not.
    #[test]
    fn moving_a_run_between_scales_rounds_the_way_one_value_at_a_time_rounds() {
        let two = LogicalType::decimal(9, 2).expect("a legal decimal");
        let input = Vector::from_values(
            two.clone(),
            &[
                Value::Decimal { unscaled: 155, width: 9, scale: 2 },
                Value::Decimal { unscaled: -155, width: 9, scale: 2 },
                Value::Decimal { unscaled: 100, width: 9, scale: 2 },
            ],
        )
        .expect("three decimals");
        let whole = cast(&input, &LogicalType::Integer, false).expect("rounds");
        assert_eq!(whole.value_at(0), Value::Integer(2));
        assert_eq!(whole.value_at(1), Value::Integer(-2));
        assert_eq!(whole.value_at(2), Value::Integer(1));
        let wider = cast(&input, &LogicalType::decimal(18, 5).expect("a legal decimal"), false)
            .expect("rescales up");
        assert_eq!(wider.value_at(0), Value::Decimal { unscaled: 155_000, width: 18, scale: 5 });
        let back = cast(&whole, &two, false).expect("rescales back");
        assert_eq!(back.value_at(0), Value::Decimal { unscaled: 200, width: 9, scale: 2 });
    }

    /// Text to bytes is not the bytes of the text. `\xNN` is one byte, an ASCII character is
    /// itself, and anything above 127 has no reading that round trips so it is refused.
    #[test]
    fn text_casts_to_a_blob_through_the_escapes_and_not_through_its_own_bytes() {
        let blob = |text: &str| cast_to(Value::Varchar(text.into()), &LogicalType::Blob);
        assert_eq!(blob("\\x41\\x42").expect("two escapes"), Value::Blob(b"AB".to_vec()));
        assert_eq!(blob("abc").expect("plain ascii"), Value::Blob(b"abc".to_vec()));
        assert_eq!(blob("").expect("the empty string"), Value::Blob(Vec::new()));
        assert_eq!(blob("\\xff\\x00").expect("either case, both ends"), Value::Blob(vec![255, 0]));
        assert_eq!(
            blob("a\\x0Ab").expect("an escape in the middle"),
            Value::Blob(b"a\nb".to_vec())
        );
    }

    /// The messages are DuckDB's word for word, since a query that fails the same way but says
    /// something else is still a difference someone has to reconcile.
    #[test]
    fn a_text_a_blob_cannot_read_says_which_part_it_could_not_read() {
        let blob = |text: &str| {
            cast_to(Value::Varchar(text.into()), &LogicalType::Blob).expect_err("not a blob")
        };
        assert!(blob("\\xZZ").message().contains("\\xZZ"), "{}", blob("\\xZZ"));
        assert!(blob("\\x4").message().contains("unterminated escape code at end of blob"));
        assert!(
            blob("é").message().contains("All non-ascii characters must be escaped"),
            "{}",
            blob("é")
        );
        assert_eq!(blob("\\xZZ").code(), ErrorCode::Conversion);
    }

    /// A dictionary keeps its nulls in the vector it points at, and a sweep that read the outer
    /// validity would report every row valid and hand back whatever sits at code zero.
    #[test]
    fn a_null_behind_a_dictionary_code_is_still_a_null_after_the_sweep() {
        let values = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(7), Value::Null, Value::Integer(9)],
        )
        .expect("three integers");
        let input = Vector::dictionary(vec![2, 1, 0, 1], values).expect("codes are in range");
        let widened = cast(&input, &LogicalType::BigInt, false).expect("widens");
        assert_eq!(widened.value_at(0), Value::BigInt(9));
        assert_eq!(widened.value_at(1), Value::Null);
        assert_eq!(widened.value_at(2), Value::BigInt(7));
        assert_eq!(widened.value_at(3), Value::Null);
    }
}

//! The scalar functions, which for M0 is arithmetic, the string functions and `LIKE`.
//!
//! One entry point rather than a function pointer per name, because the binder has already decided
//! which function this is and what its arguments were cast to, so all that is left is to do the
//! work. When the kernel generator in `spec/07-execution.md` section 7.3 arrives this becomes a
//! table lookup and the bodies below become the generated specializations, and the interface the
//! executor calls does not change.
//!
//! Null in, null out, for everything except `coalesce`. That rule is applied once here rather than
//! inside each function, which is the only way to be sure that a function added later does not
//! quietly forget it.
//!
//! Division by zero produces null rather than raising. That is DuckDB's behaviour and it is not
//! Postgres's, and it is one of the compatibility decisions that is worth a line of its own,
//! because a query that returns a row where another engine raises is a difference a user notices.
//!
//! # How the vectorized path is put together
//!
//! Same shape as `compare`, and for the same reason. [`call_values`] takes and returns owned
//! `Value`s, so a batch that goes through it once per row pays a match on the function name, a
//! clone of every argument and a push into a `Vec<Value>` that a second pass then packs. On a
//! string function it pays several heap allocations per row on top.
//!
//! [`call`] now decides the function, the physical layout and the form pair once per vector and
//! then runs a loop over slices. What it specializes is what a real query spends its time in:
//! integer, float and decimal arithmetic where the binder has already cast both sides to the result
//! type, the four `LIKE` spellings against a constant pattern, `length`, `lower`, `upper`, `not`,
//! unary minus, `abs` and string concatenation. Everything else falls through to the row at a time
//! loop, which is still here, is still correct, and increments a counter in [`crate::fallback`].
//!
//! Three details are worth knowing before reading the code.
//!
//! The fast path is taken only when every argument's logical type is the result type. That is the
//! normal case, because the binder inserts the casts, and it is what makes a native `checked_add`
//! on the run's own width exactly equivalent to the oracle's widen to `i128` and narrow back. Where
//! the types differ the fallback handles it, which is one of the things the counter is there to
//! tell us about.
//!
//! The body is not run at a row that is already null. That is not an optimization, it is
//! correctness: the value stored under a null is a zero, and adding two zeros is fine but dividing
//! by one is a null the oracle never produced and overflowing on one is an error the oracle never
//! raised.
//!
//! A `LIKE` pattern is compiled once per vector rather than once per row. `%google%`, which is
//! ClickBench query 21 over a hundred million rows, comes out as a substring search rather than as
//! a backtracking automaton, and the pattern stops being converted from a `Value` to a `String` to
//! a `Vec<char>` on every row.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Data, Form, StringColumn, Validity, Vector};

use crate::fallback::{self, Kernel};
use crate::number::{approximate, digits, fit, integral, pow10, rescale};
use crate::shape::{first, identity, nulls_of, single};

/// Calls a scalar function on a batch.
///
/// `returns` is the type the binder resolved the call to, and it is passed in rather than derived
/// because deriving it would mean consulting the signature table from inside a kernel, and the
/// signature table lives seven ranks above this crate.
///
/// # Errors
///
/// If the arguments are not all the same length, if the function is not one of the ones written
/// here, or if the call fails at some row.
pub fn call(name: &str, args: &[Vector], returns: &LogicalType) -> Result<Vector> {
    let rows = args.first().map_or(0, Vector::len);
    for (at, arg) in args.iter().enumerate() {
        if arg.len() != rows {
            return Err(Error::internal(format!(
                "argument {at} of {name} is {} rows and argument 0 is {rows}",
                arg.len()
            )));
        }
    }

    // Every argument constant is one call rather than 1024 of them. This is `3 * 4` surviving
    // constant folding, and it is also every correlated scalar the optimizer has already evaluated.
    if rows > 0 && !args.is_empty() && args.iter().all(|arg| arg.form() == Form::Constant) {
        let row: Vec<Value> = args.iter().map(|arg| arg.value_at(0)).collect();
        return Ok(Vector::constant(returns.clone(), call_values(name, &row, returns)?, rows));
    }

    if let Some(vector) = specialized(name, args, returns, rows)? {
        return Ok(vector);
    }

    // A unary function reports its one form on both sides of the table, because a column for the
    // argument that is not there would be a column of zeros in every row of the report.
    let left = args.first().map_or(Form::Flat, Vector::form);
    fallback::record(Kernel::Scalar, left, args.get(1).map_or(left, Vector::form));

    let mut row = Vec::with_capacity(args.len());
    let mut values = Vec::with_capacity(rows);
    // row at a time: the path recorded above, which is every function that has no vectorized form
    // yet, and counts itself so which functions those are shows up in the report.
    for index in 0..rows {
        row.clear();
        row.extend(args.iter().map(|arg| arg.value_at(index)));
        values.push(call_values(name, &row, returns)?);
    }
    Vector::from_values(returns.clone(), &values)
}

/// The result for a call this file has a loop for, or `None` to say it has not.
fn specialized(
    name: &str,
    args: &[Vector],
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    match args {
        [only] => unary(name, only, returns, rows),
        [left, right] => binary(name, left, right, returns, rows),
        _ => Ok(None),
    }
}

/// Runs `body` at every row that is not already null, and records the rows where the body itself
/// produced one.
///
/// Division by zero is the reason for the return value. It is the one thing in this file that turns
/// a valid input into a null output, so the driver has to be able to hear about it, and collecting
/// the indices costs nothing at all on the overwhelmingly common path where there are none.
fn over_valid(
    len: usize,
    base: Validity,
    mut body: impl FnMut(usize) -> Result<bool>,
) -> Result<Validity> {
    let mut became_null: Vec<usize> = Vec::new();
    match &base {
        Validity::AllValid => {
            for index in 0..len {
                if !body(index)? {
                    became_null.push(index);
                }
            }
        }
        // Nothing to compute. Every answer is null and the data is never touched, which is what a
        // projection of an expression over an all null column costs.
        Validity::AllInvalid => {}
        Validity::Mask(mask) => {
            for index in 0..len {
                if mask.get(index) && !body(index)? {
                    became_null.push(index);
                }
            }
        }
    }
    let mut validity = base;
    for index in became_null {
        validity = validity.with_null(index, len);
    }
    // An empty vector has no null to record, and `Vector::from_values` normalizes the empty mask it
    // builds to all valid, so a specialized empty result has to say the same thing.
    Ok(if len == 0 { Validity::AllValid } else { validity.normalize(len) })
}

/// The result vector, with the layout check that `Vector::flat` does kept rather than skipped.
fn finish(returns: &LogicalType, data: Data, validity: Validity) -> Result<Option<Vector>> {
    Ok(Some(Vector::flat(returns.clone(), data)?.with_validity(validity)))
}

/// A one argument call, for the functions with a loop.
fn unary(name: &str, arg: &Vector, returns: &LogicalType, rows: usize) -> Result<Option<Vector>> {
    let Some(data) = arg.data() else {
        return Ok(None);
    };
    let base = nulls_of(arg);
    match name {
        "not" => not_of(data, base, rows, returns),
        "-" | "abs" if arg.logical_type() == returns => {
            sign_of(name, data, base, rows, returns, arg)
        }
        "length" => length_of(data, base, rows, returns),
        "lower" | "upper" => fold_of(name, data, base, rows, returns),
        _ => Ok(None),
    }
}

/// `NOT`, which is one pass over a run of bytes.
fn not_of(
    data: &Data,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let Data::Bool(held) = data else {
        return Ok(None);
    };
    let mut out = vec![false; rows];
    let validity = over_valid(rows, base, |index| {
        out[index] = !held[index];
        Ok(true)
    })?;
    finish(returns, Data::Bool(out), validity)
}

/// Unary minus and `abs`, where the argument and the result are the same type.
fn sign_of(
    name: &str,
    data: &Data,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
    arg: &Vector,
) -> Result<Option<Vector>> {
    // Hoisted, because which of the two functions this is does not change from row to row and a
    // string comparison inside the loop would be most of what the loop costs.
    let negating = name == "-";
    macro_rules! runs {
        ($($variant:ident),+ $(,)?) => {
            match data {
                $(
                    Data::$variant(held) => {
                        let mut out = vec![0; rows];
                        let validity = over_valid(rows, base, |index| {
                            let value = held[index];
                            let computed =
                                if negating { value.checked_neg() } else { value.checked_abs() };
                            match computed {
                                Some(answer) => {
                                    out[index] = answer;
                                    Ok(true)
                                }
                                None => Err(overflow(
                                    Op::Subtract,
                                    returns,
                                    &Value::Integer(0),
                                    &arg.value_at(index),
                                )),
                            }
                        })?;
                        finish(returns, Data::$variant(out), validity)
                    }
                )+
                Data::Float32(held) => {
                    let mut out = vec![0.0f32; rows];
                    let validity = over_valid(rows, base, |index| {
                        out[index] = if negating { -held[index] } else { held[index].abs() };
                        Ok(true)
                    })?;
                    finish(returns, Data::Float32(out), validity)
                }
                Data::Float64(held) => {
                    let mut out = vec![0.0f64; rows];
                    let validity = over_valid(rows, base, |index| {
                        out[index] = if negating { -held[index] } else { held[index].abs() };
                        Ok(true)
                    })?;
                    finish(returns, Data::Float64(out), validity)
                }
                _ => Ok(None),
            }
        };
    }
    // The unsigned runs are left out on purpose. Negating a `UBIGINT` is an overflow at every row
    // but zero, so the fallback's error message is the right answer and a loop for it would be a
    // loop that exists to fail.
    runs!(Int8, Int16, Int32, Int64, Int128)
}

/// `length`, which counts characters rather than bytes.
fn length_of(
    data: &Data,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let (Data::Varlen(column), LogicalType::BigInt) = (data, returns) else {
        return Ok(None);
    };
    let mut out = vec![0i64; rows];
    let validity = over_valid(rows, base, |index| {
        let bytes = column.bytes(index).unwrap_or_default();
        // A character in UTF-8 is one lead byte and some continuation bytes, and a continuation
        // byte is the ones matching `0b10xx_xxxx`. Counting the bytes that are not continuations is
        // the same number `chars().count()` reaches and it never decodes anything.
        let characters = bytes.iter().filter(|byte| (**byte as i8) >= -0x40).count();
        out[index] = i64::try_from(characters).unwrap_or(i64::MAX);
        Ok(true)
    })?;
    finish(returns, Data::Int64(out), validity)
}

/// `lower` and `upper`.
fn fold_of(
    name: &str,
    data: &Data,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let (Data::Varlen(column), LogicalType::Varchar) = (data, returns) else {
        return Ok(None);
    };
    let lowering = name == "lower";
    let out = each_string(rows, &base, |index, into| {
        let text = column.get(index).unwrap_or_default();
        // `str::to_lowercase` rather than folding the characters into a buffer that is reused
        // across the vector, which would save the allocation. It is not the same function: the
        // string form knows that a final sigma lowercases to a different letter than a medial one
        // does, and the character form cannot know that. A saved allocation is not worth being
        // wrong about Greek. What the vectorized path removes here is the `Value` clone, the second
        // `to_string` and the packing pass, which was three allocations of the four.
        let folded = if lowering { text.to_lowercase() } else { text.to_uppercase() };
        into.push(&folded);
    });
    finish(returns, Data::Varlen(out), base.normalize(rows))
}

/// Builds a string column where every row gets a push, including the null ones.
///
/// A string column is append only and has no way to write row 7 without having written rows 0
/// through 6, so this is the shape a string producing kernel has to take rather than the shape
/// [`over_valid`] takes. A null pushes the empty string, which is what `Vector::from_values` writes
/// under a null and is therefore what keeps a specialized result equal to the oracle's.
fn each_string(
    rows: usize,
    base: &Validity,
    mut body: impl FnMut(usize, &mut StringColumn),
) -> StringColumn {
    let mut out = StringColumn::with_capacity(rows);
    for index in 0..rows {
        if base.is_valid(index) {
            body(index, &mut out);
        } else {
            out.push("");
        }
    }
    out
}

/// A two argument call, for the functions with a loop.
fn binary(
    name: &str,
    left: &Vector,
    right: &Vector,
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    if let Some(op) = arithmetic_op(name) {
        return arithmetic_of(op, left, right, returns);
    }
    match name {
        "/" => slash_of(left, right, returns),
        "||" => concat_of(left, right, returns),
        "~~" => like_of(left, right, returns, rows, false, false),
        "!~~" => like_of(left, right, returns, rows, false, true),
        "~~*" => like_of(left, right, returns, rows, true, false),
        "!~~*" => like_of(left, right, returns, rows, true, true),
        _ => Ok(None),
    }
}

/// The spelling of an arithmetic operator as the operator, and `None` for anything else.
fn arithmetic_op(name: &str) -> Option<Op> {
    match name {
        "+" => Some(Op::Add),
        "-" => Some(Op::Subtract),
        "*" => Some(Op::Multiply),
        "//" => Some(Op::Divide),
        "%" => Some(Op::Modulo),
        _ => None,
    }
}

/// The three form pairings a binary kernel in this file has a loop for.
///
/// Flat against flat, flat against constant and constant against flat, which is what an expression
/// over a column and a literal produces and is the overwhelming majority of what an expression tree
/// contains. A dictionary is not here and is counted instead, because arithmetic over a dictionary
/// wants to compute once per distinct value rather than once per row, and that is a different loop
/// rather than a different index mapping.
///
/// The mapping is a generic parameter and not a `fn(usize) -> usize` stored in a tuple. That is not
/// a style choice: a function pointer is an indirect call the compiler cannot see through, and two
/// of them per row was measured at ten nanoseconds a row on an integer addition, which is twenty
/// times what the addition costs. As a generic parameter each mapping is a zero sized type, the
/// call inlines to nothing, and the three pairings become three copies of the loop.
macro_rules! by_form {
    ($left:ident, $right:ident, $body:ident, $($rest:expr),* $(,)?) => {{
        if let (Some(one), Some(other)) = ($left.data(), $right.data()) {
            return $body(one, identity, other, identity, $($rest),*);
        }
        if let (Some(one), Some(value)) = ($left.data(), $right.constant_value()) {
            let Some(held) = single($right.logical_type(), value) else { return Ok(None) };
            let Some(other) = held.data() else { return Ok(None) };
            return $body(one, identity, other, first, $($rest),*);
        }
        if let (Some(value), Some(other)) = ($left.constant_value(), $right.data()) {
            let Some(held) = single($left.logical_type(), value) else { return Ok(None) };
            let Some(one) = held.data() else { return Ok(None) };
            return $body(one, first, other, identity, $($rest),*);
        }
        Ok(None)
    }};
}

/// `+`, `-`, `*`, `//` and `%`, on the types the binder has already made match.
fn arithmetic_of(
    op: Op,
    left: &Vector,
    right: &Vector,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    // Both sides already the result type is what makes a native operation on the run's own width
    // exactly the oracle's widen to `i128` and narrow back, rather than nearly it.
    if left.logical_type() != returns || right.logical_type() != returns {
        return Ok(None);
    }
    by_form!(left, right, arithmetic_runs, op, left, right, returns)
}

/// One optimistic pass over the two runs, with the operator hoisted out of the loop.
///
/// The step reports whether the row overflowed rather than raising, and the flag is accumulated
/// rather than branched on, so the loop has one exit and no error handling in it at all. That is
/// what lets it be the shape a compiler will unroll and, on the fixed width runs, vectorize.
///
/// The caller decides what an overflow means. In this file it means hand the whole vector back to
/// the row at a time path, which knows how to build the message and knows whether the row was null
/// and therefore never overflowed in the first place.
fn sweep<T, L, R, S>(out: &mut [T], a: &[T], at_left: L, b: &[T], at_right: R, step: S) -> bool
where
    T: Copy,
    L: Fn(usize) -> usize,
    R: Fn(usize) -> usize,
    S: Fn(T, T) -> (T, bool),
{
    let mut trouble = false;
    for (index, slot) in out.iter_mut().enumerate() {
        let (value, overflowed) = step(a[at_left(index)], b[at_right(index)]);
        *slot = value;
        trouble |= overflowed;
    }
    trouble
}

/// Writes the type's zero at every null position.
///
/// The optimistic loop computes at every row including the null ones, so whatever was stored under
/// a null comes out the other side as an answer nobody should read. Nobody does read it, but the
/// row at a time path writes a zero there and two vectors that hold different rubbish under their
/// nulls are not equal, and the property test compares vectors rather than answers on purpose.
fn blank<T: Copy + Default>(out: &mut [T], validity: &Validity) {
    match validity {
        Validity::AllValid => {}
        Validity::AllInvalid => out.fill(T::default()),
        Validity::Mask(mask) => {
            for (index, slot) in out.iter_mut().enumerate() {
                if !mask.get(index) {
                    *slot = T::default();
                }
            }
        }
    }
}

/// The arithmetic, once per form pairing, split by what the operator needs from the loop.
#[expect(
    clippy::too_many_arguments,
    reason = "two sides with an index each, the operator, the two vectors the error message needs \
              and the type of the answer, none of which is worth a struct that exists for three \
              calls"
)]
fn arithmetic_runs<L, R>(
    one: &Data,
    at_left: L,
    other: &Data,
    at_right: R,
    op: Op,
    left: &Vector,
    right: &Vector,
    returns: &LogicalType,
) -> Result<Option<Vector>>
where
    L: Fn(usize) -> usize,
    R: Fn(usize) -> usize,
{
    let rows = left.len();
    let base = nulls_of(left).and(&nulls_of(right), rows);
    // A decimal shares its run with the integer of the same width and the two want different
    // arithmetic, so `returns` and not the run is what decides, and it decides first.
    if matches!(returns, LogicalType::Decimal { .. }) {
        return decimal_runs(one, at_left, other, at_right, op, &base, left, right);
    }
    // Dividing has to look at the divisor before it computes and has to know whether the row was
    // already null before it calls a zero divisor a null, so it keeps the careful loop.
    if matches!(op, Op::Divide | Op::Modulo) {
        return guarded_runs(one, at_left, other, at_right, op, base, left, right, returns);
    }
    fast_runs(one, at_left, other, at_right, op, &base, returns, rows)
}

/// Adding, subtracting and multiplying, which is the arithmetic a scan spends its time in.
#[expect(
    clippy::too_many_arguments,
    reason = "two sides with an index each, the operator, the nulls and the type and length of the \
              answer, none of which is worth a struct that exists for three calls"
)]
fn fast_runs<L, R>(
    one: &Data,
    at_left: L,
    other: &Data,
    at_right: R,
    op: Op,
    base: &Validity,
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>>
where
    L: Fn(usize) -> usize,
    R: Fn(usize) -> usize,
{
    macro_rules! integers {
        ($($variant:ident => $native:ty),+ $(,)?) => {
            $(
                if let (Data::$variant(a), Data::$variant(b)) = (one, other) {
                    let mut out = vec![0 as $native; rows];
                    let trouble = match op {
                        Op::Add => {
                            sweep(&mut out, a, &at_left, b, &at_right, <$native>::overflowing_add)
                        }
                        Op::Subtract => {
                            sweep(&mut out, a, &at_left, b, &at_right, <$native>::overflowing_sub)
                        }
                        Op::Multiply => {
                            sweep(&mut out, a, &at_left, b, &at_right, <$native>::overflowing_mul)
                        }
                        // Never reached, because dividing was sent elsewhere before this function
                        // was called. Saying trouble rather than saying nothing keeps the answer
                        // right if that ever stops being true.
                        Op::Divide | Op::Modulo => true,
                    };
                    // A row that overflowed, or a null row holding something that looked like one,
                    // sends the whole vector back to the row at a time path. That path raises the
                    // error with the right two operands in the message, or does not raise at all
                    // because the row was null. Deciding it here would be a branch per row for a
                    // case that ends the query anyway.
                    if trouble {
                        return Ok(None);
                    }
                    blank(&mut out, base);
                    return finish(returns, Data::$variant(out), base.clone());
                }
            )+
        };
    }

    macro_rules! floats {
        ($variant:ident, $native:ty, $widen:expr, $narrow:expr) => {
            if let (Data::$variant(a), Data::$variant(b)) = (one, other) {
                let step = |x: $native, y: $native| {
                    let (x, y) = ($widen(x), $widen(y));
                    ($narrow(float_step(op, x, y)), false)
                };
                let mut out = vec![0 as $native; rows];
                // A float does not overflow, it reaches infinity, so the flag is never set and the
                // operator match can stay inside the step rather than outside the loop.
                let _ = sweep(&mut out, a, &at_left, b, &at_right, step);
                blank(&mut out, base);
                return finish(returns, Data::$variant(out), base.clone());
            }
        };
    }

    // A date and a timestamp are stored in the same runs the integers are, and arithmetic on them
    // is not integer arithmetic, so the run is not enough to decide by. The oracle answers a date
    // plus a date with a message saying it is not implemented, and that is the answer this path has
    // to leave it room to give.
    if returns.is_integer() {
        integers!(
            Int8 => i8, Int16 => i16, Int32 => i32, Int64 => i64, Int128 => i128,
            UInt8 => u8, UInt16 => u16, UInt32 => u32, UInt64 => u64, UInt128 => u128,
        );
    }
    floats!(Float64, f64, |x| x, |x| x);
    // Widened, computed and narrowed, which is what the oracle does. For one addition, subtraction
    // or multiplication the double rounding is exact, so this is the same bits either way, but
    // doing it the same way is how the property test stays an equality rather than a tolerance.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "arithmetic on a FLOAT column produces a FLOAT"
    )]
    {
        floats!(Float32, f32, f64::from, |x| x as f32);
    }
    Ok(None)
}

/// Dividing and taking a remainder, where a zero on the right is a null and not an answer.
#[expect(
    clippy::too_many_arguments,
    reason = "two sides with an index each, the operator, the nulls, the two vectors the error \
              message needs and the type of the answer, none of which is worth a struct that \
              exists for three calls"
)]
fn guarded_runs<L, R>(
    one: &Data,
    at_left: L,
    other: &Data,
    at_right: R,
    op: Op,
    base: Validity,
    left: &Vector,
    right: &Vector,
    returns: &LogicalType,
) -> Result<Option<Vector>>
where
    L: Fn(usize) -> usize,
    R: Fn(usize) -> usize,
{
    let rows = left.len();
    macro_rules! integers {
        ($($variant:ident),+ $(,)?) => {
            $(
                if let (Data::$variant(a), Data::$variant(b)) = (one, other) {
                    let mut out = vec![0; rows];
                    let validity = over_valid(rows, base, |index| {
                        let (x, y) = (a[at_left(index)], b[at_right(index)]);
                        if y == 0 {
                            return Ok(false);
                        }
                        let computed = if matches!(op, Op::Divide) {
                            x.checked_div(y)
                        } else {
                            // Wrapping rather than checked, because the one case they differ on is
                            // the smallest value of the type modulo negative one, where the checked
                            // form says overflow and the true answer, which is what the oracle
                            // reaches through `i128`, is zero.
                            Some(x.wrapping_rem(y))
                        };
                        match computed {
                            Some(answer) => {
                                out[index] = answer;
                                Ok(true)
                            }
                            None => Err(overflow(
                                op,
                                returns,
                                &left.value_at(index),
                                &right.value_at(index),
                            )),
                        }
                    })?;
                    return finish(returns, Data::$variant(out), validity);
                }
            )+
        };
    }

    macro_rules! floats {
        ($variant:ident, $native:ty, $widen:expr, $narrow:expr) => {
            if let (Data::$variant(a), Data::$variant(b)) = (one, other) {
                let mut out = vec![0 as $native; rows];
                let validity = over_valid(rows, base, |index| {
                    let (x, y) = (a[at_left(index)], b[at_right(index)]);
                    if y == 0.0 {
                        return Ok(false);
                    }
                    out[index] = $narrow(float_step(op, $widen(x), $widen(y)));
                    Ok(true)
                })?;
                return finish(returns, Data::$variant(out), validity);
            }
        };
    }

    if returns.is_integer() {
        integers!(Int8, Int16, Int32, Int64, Int128, UInt8, UInt16, UInt32, UInt64, UInt128);
    }
    floats!(Float64, f64, |x| x, |x| x);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "arithmetic on a FLOAT column produces a FLOAT"
    )]
    {
        floats!(Float32, f32, f64::from, |x| x as f32);
    }
    Ok(None)
}

/// Decimal arithmetic, which is integer arithmetic on the unscaled values plus a rescale and a
/// width check, and is rare enough on a scan to keep the careful loop for all five operators.
#[expect(
    clippy::too_many_arguments,
    reason = "two sides with an index each, the operator, the nulls and the two vectors the error \
              message needs, none of which is worth a struct that exists for three calls"
)]
fn decimal_runs<L, R>(
    one: &Data,
    at_left: L,
    other: &Data,
    at_right: R,
    op: Op,
    base: &Validity,
    left: &Vector,
    right: &Vector,
) -> Result<Option<Vector>>
where
    L: Fn(usize) -> usize,
    R: Fn(usize) -> usize,
{
    let returns = left.logical_type();
    let LogicalType::Decimal { width, scale } = *returns else {
        return Ok(None);
    };
    let rows = left.len();
    let guarding = matches!(op, Op::Divide | Op::Modulo);
    macro_rules! runs {
        ($($variant:ident => $native:ty),+ $(,)?) => {
            $(
                if let (Data::$variant(a), Data::$variant(b)) = (one, other) {
                    let mut out = vec![0 as $native; rows];
                    let validity = over_valid(rows, base.clone(), |index| {
                        let x = i128::from(a[at_left(index)]);
                        let y = i128::from(b[at_right(index)]);
                        if guarding && y == 0 {
                            return Ok(false);
                        }
                        let unscaled = match op {
                            Op::Add => x.checked_add(y),
                            Op::Subtract => x.checked_sub(y),
                            Op::Multiply => {
                                x.checked_mul(y).and_then(|wide| rescale(wide, scale * 2, scale))
                            }
                            Op::Modulo => x.checked_rem(y),
                            Op::Divide => {
                                x.checked_div(y).and_then(|whole| whole.checked_mul(pow10(scale)))
                            }
                        };
                        let fits = unscaled
                            .filter(|value| digits(*value) <= width)
                            .and_then(|value| <$native>::try_from(value).ok());
                        match fits {
                            Some(answer) => {
                                out[index] = answer;
                                Ok(true)
                            }
                            None => Err(overflow(
                                op,
                                returns,
                                &left.value_at(index),
                                &right.value_at(index),
                            )),
                        }
                    })?;
                    return finish(returns, Data::$variant(out), validity);
                }
            )+
        };
    }
    runs!(Int16 => i16, Int32 => i32, Int64 => i64, Int128 => i128);
    Ok(None)
}

/// One float operation, in the one place, so that `//` truncating cannot be got wrong twice.
fn float_step(op: Op, x: f64, y: f64) -> f64 {
    match op {
        Op::Add => x + y,
        Op::Subtract => x - y,
        Op::Multiply => x * y,
        Op::Divide => (x / y).trunc(),
        Op::Modulo => x % y,
    }
}

/// `/`, which the binder has already promoted both sides to `DOUBLE` for.
fn slash_of(left: &Vector, right: &Vector, returns: &LogicalType) -> Result<Option<Vector>> {
    if !matches!(returns, LogicalType::Double)
        || left.logical_type() != returns
        || right.logical_type() != returns
    {
        return Ok(None);
    }
    by_form!(left, right, slash_runs, left, right, returns)
}

/// The `/` loop itself, once per form pairing.
fn slash_runs<L, R>(
    one: &Data,
    at_left: L,
    other: &Data,
    at_right: R,
    left: &Vector,
    right: &Vector,
    returns: &LogicalType,
) -> Result<Option<Vector>>
where
    L: Fn(usize) -> usize,
    R: Fn(usize) -> usize,
{
    let (Data::Float64(a), Data::Float64(b)) = (one, other) else {
        return Ok(None);
    };
    let rows = left.len();
    let base = nulls_of(left).and(&nulls_of(right), rows);
    let mut out = vec![0.0f64; rows];
    let validity = over_valid(rows, base, |index| {
        let (x, y) = (a[at_left(index)], b[at_right(index)]);
        if y == 0.0 {
            return Ok(false);
        }
        out[index] = x / y;
        Ok(true)
    })?;
    finish(returns, Data::Float64(out), validity)
}

/// `||`, where both sides are already strings.
fn concat_of(left: &Vector, right: &Vector, returns: &LogicalType) -> Result<Option<Vector>> {
    if !matches!(returns, LogicalType::Varchar)
        || !matches!(left.logical_type(), LogicalType::Varchar)
        || !matches!(right.logical_type(), LogicalType::Varchar)
    {
        return Ok(None);
    }
    by_form!(left, right, concat_runs, left, right, returns)
}

/// The `||` loop itself, once per form pairing.
fn concat_runs<L, R>(
    one: &Data,
    at_left: L,
    other: &Data,
    at_right: R,
    left: &Vector,
    right: &Vector,
    returns: &LogicalType,
) -> Result<Option<Vector>>
where
    L: Fn(usize) -> usize,
    R: Fn(usize) -> usize,
{
    let (Data::Varlen(a), Data::Varlen(b)) = (one, other) else {
        return Ok(None);
    };
    let rows = left.len();
    let base = nulls_of(left).and(&nulls_of(right), rows);
    let mut joined = String::new();
    let out = each_string(rows, &base, |index, into| {
        joined.clear();
        joined.push_str(a.get(at_left(index)).unwrap_or_default());
        joined.push_str(b.get(at_right(index)).unwrap_or_default());
        into.push(&joined);
    });
    finish(returns, Data::Varlen(out), base.normalize(rows))
}

/// The four `LIKE` spellings, against a pattern that is the same on every row.
///
/// A pattern that varies per row is possible in SQL and is vanishingly rare, and it falls through
/// to the row at a time path where it is counted. Everything that matters is a literal.
fn like_of(
    text: &Vector,
    pattern: &Vector,
    returns: &LogicalType,
    rows: usize,
    fold_case: bool,
    negated: bool,
) -> Result<Option<Vector>> {
    if !matches!(returns, LogicalType::Boolean) {
        return Ok(None);
    }
    let (Some(Value::Varchar(spelling)), Some(Data::Varlen(column))) =
        (pattern.constant_value(), text.data())
    else {
        return Ok(None);
    };
    // The pattern is compiled once for the whole vector. This is the difference between ClickBench
    // query 21 doing a substring search per row and doing four allocations and a backtracking walk.
    let spelling = if fold_case { spelling.to_lowercase() } else { spelling.clone() };
    let compiled = Pattern::compile(&spelling);
    let base = nulls_of(text).and(&nulls_of(pattern), rows);

    let mut out = vec![false; rows];
    let mut characters: Vec<char> = Vec::new();
    let validity = over_valid(rows, base, |index| {
        let text = column.get(index).unwrap_or_default();
        // `str::to_lowercase` and not a character by character fold, for the same reason the
        // `lower` kernel uses it: the two functions disagree about a final sigma, and the oracle
        // this is checked against calls the string one.
        let folded = if fold_case { Some(text.to_lowercase()) } else { None };
        let text = folded.as_deref().unwrap_or(text);
        out[index] = compiled.holds(text, &mut characters) != negated;
        Ok(true)
    })?;
    finish(returns, Data::Bool(out), validity)
}

/// A `LIKE` pattern, after the shape of it has been looked at once.
///
/// The four literal shapes are the ones that appear in queries people actually write, and each of
/// them answers on bytes without decoding a character, which is correct because UTF-8 is self
/// synchronizing and a byte substring of a valid string is therefore a character substring of it.
/// Everything else keeps the backtracking walk.
#[derive(Debug)]
enum Pattern {
    /// No wildcard at all, so `LIKE` is `=`.
    Exact(String),
    /// `abc%`.
    Prefix(String),
    /// `%abc`.
    Suffix(String),
    /// `%abc%`, which is the one ClickBench spends its time in.
    Contains(String),
    /// Anything else, walked with one backtracking point.
    General(Vec<char>),
}

impl Pattern {
    fn compile(spelling: &str) -> Self {
        let plain = |text: &str| !text.contains('%') && !text.contains('_');
        if plain(spelling) {
            return Self::Exact(spelling.to_owned());
        }
        if let Some(inner) = spelling.strip_prefix('%').and_then(|rest| rest.strip_suffix('%')) {
            if plain(inner) {
                return Self::Contains(inner.to_owned());
            }
        }
        if let Some(rest) = spelling.strip_prefix('%') {
            if plain(rest) {
                return Self::Suffix(rest.to_owned());
            }
        }
        if let Some(head) = spelling.strip_suffix('%') {
            if plain(head) {
                return Self::Prefix(head.to_owned());
            }
        }
        Self::General(spelling.chars().collect())
    }

    /// Whether the pattern matches, reusing `characters` as the buffer the general walk needs so
    /// that a vector costs one allocation rather than one per row.
    fn holds(&self, text: &str, characters: &mut Vec<char>) -> bool {
        match self {
            Self::Exact(against) => text == against,
            Self::Prefix(against) => text.starts_with(against.as_str()),
            Self::Suffix(against) => text.ends_with(against.as_str()),
            Self::Contains(against) => text.contains(against.as_str()),
            Self::General(against) => {
                characters.clear();
                characters.extend(text.chars());
                like(characters, against)
            }
        }
    }
}

/// Calls a scalar function on one row.
///
/// # Errors
///
/// If the function is not one of the ones written here, or if the call fails.
pub fn call_values(name: &str, args: &[Value], returns: &LogicalType) -> Result<Value> {
    if name == "coalesce" {
        let found = args.iter().find(|value| !value.is_null());
        return Ok(found.cloned().unwrap_or(Value::Null));
    }
    if args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    match (name, args) {
        ("+", [only]) => Ok(only.clone()),
        ("-", [only]) => negate(only, returns),
        ("abs", [only]) => absolute(only, returns),
        ("not", [only]) => match only.as_bool() {
            Some(held) => Ok(Value::Boolean(!held)),
            None => Err(Error::internal(format!("not of a {}", only.logical_type()))),
        },
        ("+", [left, right]) => arithmetic(Op::Add, left, right, returns),
        ("-", [left, right]) => arithmetic(Op::Subtract, left, right, returns),
        ("*", [left, right]) => arithmetic(Op::Multiply, left, right, returns),
        ("%", [left, right]) => arithmetic(Op::Modulo, left, right, returns),
        ("//", [left, right]) => arithmetic(Op::Divide, left, right, returns),
        ("/", [left, right]) => divide(left, right),
        ("||", [left, right]) => Ok(Value::Varchar(format!("{left}{right}"))),
        ("lower", [only]) => Ok(Value::Varchar(only.to_string().to_lowercase())),
        ("upper", [only]) => Ok(Value::Varchar(only.to_string().to_uppercase())),
        ("length", [only]) => Ok(Value::BigInt(count_characters(only))),
        ("~~", [text, pattern]) => Ok(Value::Boolean(matches(text, pattern, false))),
        ("!~~", [text, pattern]) => Ok(Value::Boolean(!matches(text, pattern, false))),
        ("~~*", [text, pattern]) => Ok(Value::Boolean(matches(text, pattern, true))),
        ("!~~*", [text, pattern]) => Ok(Value::Boolean(!matches(text, pattern, true))),
        _ => Err(Error::not_implemented(format!(
            "the {name} function with {} arguments",
            args.len()
        ))),
    }
}

/// Which arithmetic, kept separate from the spelling so that the overflow message can name it the
/// way DuckDB names it.
#[derive(Debug, Clone, Copy)]
enum Op {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
}

impl Op {
    fn word(self) -> &'static str {
        match self {
            Self::Add => "addition",
            Self::Subtract => "subtraction",
            Self::Multiply => "multiplication",
            Self::Divide => "division",
            Self::Modulo => "modulo",
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "//",
            Self::Modulo => "%",
        }
    }
}

fn overflow(op: Op, ty: &LogicalType, left: &Value, right: &Value) -> Error {
    Error::out_of_range(format!(
        "Overflow in {} of {ty} ({left} {} {right})!",
        op.word(),
        op.symbol()
    ))
}

fn arithmetic(op: Op, left: &Value, right: &Value, ty: &LogicalType) -> Result<Value> {
    match ty {
        LogicalType::Float | LogicalType::Double => float_arithmetic(op, left, right, ty),
        LogicalType::Decimal { width, scale } => {
            decimal_arithmetic(op, left, right, *width, *scale)
        }
        other if other.is_integer() => integer_arithmetic(op, left, right, ty),
        other => Err(Error::not_implemented(format!("{} on {other}", op.word()))),
    }
}

fn integer_arithmetic(op: Op, left: &Value, right: &Value, ty: &LogicalType) -> Result<Value> {
    let (a, b) = match (integral(left), integral(right)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(Error::not_implemented(format!(
                "{} on {} and {}",
                op.word(),
                left.logical_type(),
                right.logical_type()
            )));
        }
    };
    if matches!(op, Op::Divide | Op::Modulo) && b == 0 {
        return Ok(Value::Null);
    }
    let wide = match op {
        Op::Add => a.checked_add(b),
        Op::Subtract => a.checked_sub(b),
        Op::Multiply => a.checked_mul(b),
        Op::Divide => a.checked_div(b),
        Op::Modulo => a.checked_rem(b),
    };
    wide.and_then(|whole| fit(whole, ty)).ok_or_else(|| overflow(op, ty, left, right))
}

fn float_arithmetic(op: Op, left: &Value, right: &Value, ty: &LogicalType) -> Result<Value> {
    let (a, b) = match (approximate(left), approximate(right)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(Error::not_implemented(format!(
                "{} on {} and {}",
                op.word(),
                left.logical_type(),
                right.logical_type()
            )));
        }
    };
    if matches!(op, Op::Divide | Op::Modulo) && b == 0.0 {
        return Ok(Value::Null);
    }
    let result = match op {
        Op::Add => a + b,
        Op::Subtract => a - b,
        Op::Multiply => a * b,
        Op::Divide => (a / b).trunc(),
        Op::Modulo => a % b,
    };
    if matches!(ty, LogicalType::Float) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "arithmetic on a FLOAT column produces a FLOAT"
        )]
        return Ok(Value::Float(result as f32));
    }
    Ok(Value::Double(result))
}

fn decimal_arithmetic(op: Op, left: &Value, right: &Value, width: u8, scale: u8) -> Result<Value> {
    let ty = LogicalType::Decimal { width, scale };
    let (a, b) = match (unscaled_at(left, scale), unscaled_at(right, scale)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(Error::not_implemented(format!(
                "{} on {} and {}",
                op.word(),
                left.logical_type(),
                right.logical_type()
            )));
        }
    };
    if matches!(op, Op::Divide | Op::Modulo) && b == 0 {
        return Ok(Value::Null);
    }
    let unscaled = match op {
        Op::Add => a.checked_add(b),
        Op::Subtract => a.checked_sub(b),
        Op::Multiply => a.checked_mul(b).and_then(|wide| rescale(wide, scale * 2, scale)),
        Op::Modulo => a.checked_rem(b),
        Op::Divide => a.checked_div(b).and_then(|whole| whole.checked_mul(pow10(scale))),
    };
    let unscaled = unscaled.ok_or_else(|| overflow(op, &ty, left, right))?;
    if digits(unscaled) > width {
        return Err(overflow(op, &ty, left, right));
    }
    Ok(Value::Decimal { unscaled, width, scale })
}

/// A value as an unscaled integer at the given scale, for the decimal path.
fn unscaled_at(value: &Value, scale: u8) -> Option<i128> {
    match *value {
        Value::Decimal { unscaled, scale: held, .. } => rescale(unscaled, held, scale),
        _ => integral(value).and_then(|whole| whole.checked_mul(pow10(scale))),
    }
}

/// `/`, which the binder has already promoted both sides to `DOUBLE` for.
fn divide(left: &Value, right: &Value) -> Result<Value> {
    let (a, b) = match (approximate(left), approximate(right)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(Error::not_implemented(format!(
                "division on {} and {}",
                left.logical_type(),
                right.logical_type()
            )));
        }
    };
    if b == 0.0 {
        return Ok(Value::Null);
    }
    Ok(Value::Double(a / b))
}

fn negate(value: &Value, ty: &LogicalType) -> Result<Value> {
    match value {
        Value::Float(v) => Ok(Value::Float(-v)),
        Value::Double(v) => Ok(Value::Double(-v)),
        Value::Decimal { unscaled, width, scale } => {
            Ok(Value::Decimal { unscaled: -unscaled, width: *width, scale: *scale })
        }
        _ => match integral(value) {
            Some(whole) => whole
                .checked_neg()
                .and_then(|negated| fit(negated, ty))
                .ok_or_else(|| overflow(Op::Subtract, ty, &Value::Integer(0), value)),
            None => Err(Error::not_implemented(format!("negating a {}", value.logical_type()))),
        },
    }
}

fn absolute(value: &Value, ty: &LogicalType) -> Result<Value> {
    match value {
        Value::Float(v) => Ok(Value::Float(v.abs())),
        Value::Double(v) => Ok(Value::Double(v.abs())),
        Value::Decimal { unscaled, width, scale } => {
            Ok(Value::Decimal { unscaled: unscaled.abs(), width: *width, scale: *scale })
        }
        _ => match integral(value) {
            Some(whole) => whole
                .checked_abs()
                .and_then(|positive| fit(positive, ty))
                .ok_or_else(|| overflow(Op::Subtract, ty, &Value::Integer(0), value)),
            None => Err(Error::not_implemented(format!("abs of a {}", value.logical_type()))),
        },
    }
}

/// `length`, which counts characters rather than bytes, the way DuckDB does.
fn count_characters(value: &Value) -> i64 {
    let text = match value.as_str() {
        Some(text) => text.chars().count(),
        None => value.to_string().chars().count(),
    };
    i64::try_from(text).unwrap_or(i64::MAX)
}

/// SQL `LIKE`, where `%` is any run and `_` is one character.
///
/// The loop is the standard one with a single backtracking point, which is linear on the patterns
/// that appear in practice and avoids the exponential blowup a naive recursion has on a pattern
/// like `%a%a%a%a%`. ClickBench query 21 is `LIKE '%google%'` over 100 million rows, so this is
/// somewhere the shape of the algorithm is going to matter.
fn matches(text: &Value, pattern: &Value, fold_case: bool) -> bool {
    let (text, pattern) = if fold_case {
        (text.to_string().to_lowercase(), pattern.to_string().to_lowercase())
    } else {
        (text.to_string(), pattern.to_string())
    };
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    like(&text, &pattern)
}

/// The `LIKE` walk itself, on characters that somebody else has already decoded and case folded.
///
/// Split out from [`matches`] so that the vectorized path can call it without going through a
/// `Value` and a `String` per row. It is the only copy of the algorithm, which is the point: a
/// second copy that drifted would be a wrong answer that only appears on one of the two paths.
fn like(text: &[char], pattern: &[char]) -> bool {
    let (mut at, mut against) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while at < text.len() {
        if against < pattern.len() && (pattern[against] == '_' || pattern[against] == text[at]) {
            at += 1;
            against += 1;
        } else if against < pattern.len() && pattern[against] == '%' {
            star = Some(against);
            resume = at;
            against += 1;
        } else if let Some(back) = star {
            against = back + 1;
            resume += 1;
            at = resume;
        } else {
            return false;
        }
    }
    while against < pattern.len() && pattern[against] == '%' {
        against += 1;
    }
    against == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn called(name: &str, args: &[Value], returns: &LogicalType) -> Value {
        call_values(name, args, returns).expect("this call is written")
    }

    #[test]
    fn null_in_is_null_out_for_everything_but_coalesce() {
        assert_eq!(
            called("+", &[Value::Integer(1), Value::Null], &LogicalType::Integer),
            Value::Null
        );
        assert_eq!(
            called("coalesce", &[Value::Null, Value::Integer(2)], &LogicalType::Integer),
            Value::Integer(2)
        );
        assert_eq!(
            called("coalesce", &[Value::Null, Value::Null], &LogicalType::Integer),
            Value::Null
        );
    }

    #[test]
    fn arithmetic_that_overflows_says_so_rather_than_wrapping() {
        let error =
            call_values("+", &[Value::Integer(i32::MAX), Value::Integer(1)], &LogicalType::Integer)
                .expect_err("2147483647 + 1 is not an integer");
        assert!(error.message().contains("Overflow in addition of INTEGER"), "{error}");
    }

    /// DuckDB returns null here where Postgres raises, and this is the line that records it.
    #[test]
    fn dividing_by_zero_is_null() {
        assert_eq!(
            called("/", &[Value::Integer(1), Value::Integer(0)], &LogicalType::Double),
            Value::Null
        );
        assert_eq!(
            called("//", &[Value::Integer(1), Value::Integer(0)], &LogicalType::Integer),
            Value::Null
        );
        assert_eq!(
            called("%", &[Value::Integer(1), Value::Integer(0)], &LogicalType::Integer),
            Value::Null
        );
    }

    #[test]
    fn a_division_is_a_double_even_when_both_sides_are_whole() {
        assert_eq!(
            called("/", &[Value::Integer(7), Value::Integer(2)], &LogicalType::Double),
            Value::Double(3.5)
        );
        assert_eq!(
            called("//", &[Value::Integer(7), Value::Integer(2)], &LogicalType::Integer),
            Value::Integer(3)
        );
    }

    #[test]
    fn decimals_add_at_their_own_scale_and_multiply_back_down_to_it() {
        let ty = LogicalType::decimal(10, 2).expect("a legal decimal");
        let two_fifty = Value::Decimal { unscaled: 250, width: 10, scale: 2 };
        let four = Value::Decimal { unscaled: 400, width: 10, scale: 2 };
        assert_eq!(
            called("+", &[two_fifty.clone(), four.clone()], &ty),
            Value::Decimal { unscaled: 650, width: 10, scale: 2 }
        );
        assert_eq!(
            called("*", &[two_fifty, four], &ty),
            Value::Decimal { unscaled: 1000, width: 10, scale: 2 }
        );
    }

    #[test]
    fn strings_join_and_fold() {
        assert_eq!(
            called(
                "||",
                &[Value::Varchar("ab".into()), Value::Varchar("cd".into())],
                &LogicalType::Varchar
            ),
            Value::Varchar("abcd".into())
        );
        assert_eq!(
            called("upper", &[Value::Varchar("aB".into())], &LogicalType::Varchar),
            Value::Varchar("AB".into())
        );
        assert_eq!(
            called("length", &[Value::Varchar("héllo".into())], &LogicalType::BigInt),
            Value::BigInt(5)
        );
    }

    #[test]
    fn like_matches_the_way_sql_says_it_does() {
        let text = Value::Varchar("google.com".into());
        for (pattern, expected) in [
            ("%google%", true),
            ("google%", true),
            ("%com", true),
            ("g_ogle.com", true),
            ("g__gle.com", true),
            ("goggle%", false),
            ("%GOOGLE%", false),
            ("google.com", true),
            ("%", true),
        ] {
            let held = called(
                "~~",
                &[text.clone(), Value::Varchar(pattern.into())],
                &LogicalType::Boolean,
            );
            assert_eq!(held, Value::Boolean(expected), "{pattern}");
        }
    }

    #[test]
    fn like_backtracks_rather_than_giving_up_at_the_first_star() {
        let text = Value::Varchar("aaaaaaab".into());
        let held = called("~~", &[text, Value::Varchar("%a%a%b".into())], &LogicalType::Boolean);
        assert_eq!(held, Value::Boolean(true));
    }

    #[test]
    fn the_case_folding_like_ignores_case_and_the_negated_ones_invert() {
        let text = Value::Varchar("Google".into());
        let pattern = Value::Varchar("%GOOGLE%".into());
        assert_eq!(
            called("~~*", &[text.clone(), pattern.clone()], &LogicalType::Boolean),
            Value::Boolean(true)
        );
        assert_eq!(called("!~~", &[text, pattern], &LogicalType::Boolean), Value::Boolean(true));
    }

    #[test]
    fn a_function_nobody_has_written_says_which_one() {
        let error = call_values("sqrt", &[Value::Double(4.0)], &LogicalType::Double)
            .expect_err("sqrt is not written yet");
        assert!(error.message().contains("the sqrt function"), "{error}");
    }

    #[test]
    fn a_batch_call_is_one_answer_per_row() {
        let left = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(2), Value::Null],
        )
        .expect("three rows");
        let right = Vector::constant(LogicalType::Integer, Value::Integer(10), 3);
        let sum = call("+", &[left, right], &LogicalType::Integer).expect("adds");
        assert_eq!(sum.value_at(0), Value::Integer(11));
        assert_eq!(sum.value_at(1), Value::Integer(12));
        assert_eq!(sum.value_at(2), Value::Null);
    }

    #[test]
    fn arguments_of_different_lengths_are_caught() {
        let left = Vector::constant(LogicalType::Integer, Value::Integer(1), 3);
        let right = Vector::constant(LogicalType::Integer, Value::Integer(1), 4);
        let error = call("+", &[left, right], &LogicalType::Integer).expect_err("ragged");
        assert!(error.message().contains("argument 1"), "{error}");
    }

    /// The row at a time path, kept as the oracle rather than deleted.
    ///
    /// This is the body [`call`] had before the specializations went in, written out here so that a
    /// test can run it on the same vectors the fast path is given. It returns a `Result` because
    /// overflow is an error and the two paths have to agree on that too, down to the message.
    fn oracle(name: &str, args: &[Vector], returns: &LogicalType) -> Result<Vector> {
        let rows = args.first().map_or(0, Vector::len);
        let mut row = Vec::with_capacity(args.len());
        let mut values = Vec::with_capacity(rows);
        for index in 0..rows {
            row.clear();
            row.extend(args.iter().map(|arg| arg.value_at(index)));
            values.push(call_values(name, &row, returns)?);
        }
        Vector::from_values(returns.clone(), &values)
    }

    /// Asserts that the two paths produce the same vector, not merely the same answers.
    ///
    /// Same vector means the same data, the same validity representation and the same filler at
    /// every null position, which is a much stronger statement than same answers and is free to
    /// check. An error has to match too, because a query that overflows on one path and not on the
    /// other is exactly the kind of difference nobody finds until a user reports it.
    fn agrees(name: &str, args: &[Vector], returns: &LogicalType) {
        let forms: Vec<Form> = args.iter().map(Vector::form).collect();
        let what = format!("{name} on {forms:?} returning {returns}");
        match (call(name, args, returns), oracle(name, args, returns)) {
            (Ok(fast), Ok(slow)) => assert_eq!(fast, slow, "{what}"),
            (Err(fast), Err(slow)) => assert_eq!(fast.message(), slow.message(), "{what}"),
            (fast, slow) => panic!("{what}: one path gave {fast:?} and the other gave {slow:?}"),
        }
    }

    /// A small deterministic generator, because a property test with no seed is a test that fails
    /// on somebody else's machine and passes on yours.
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

    /// One column of a given type, with roughly one row in `nulls` null, and zero deliberately
    /// frequent so that dividing by it is exercised rather than hoped for.
    fn sample(ty: &LogicalType, rows: usize, nulls: u64, rng: &mut Rng) -> Vector {
        let mut values = Vec::with_capacity(rows);
        for _ in 0..rows {
            if nulls > 0 && rng.below(nulls) == 0 {
                values.push(Value::Null);
                continue;
            }
            let small = rng.below(9) as i64 - 4;
            let edge = rng.below(32) == 0;
            values.push(match ty {
                LogicalType::TinyInt => Value::TinyInt(if edge { i8::MIN } else { small as i8 }),
                LogicalType::Integer => Value::Integer(if edge { i32::MAX } else { small as i32 }),
                LogicalType::BigInt => Value::BigInt(if edge { i64::MIN } else { small }),
                LogicalType::HugeInt => Value::HugeInt(i128::from(small)),
                LogicalType::UInteger => {
                    Value::UInteger(if edge { u32::MAX } else { small.unsigned_abs() as u32 })
                }
                LogicalType::Float => Value::Float(small as f32 / 2.0),
                LogicalType::Double => Value::Double(small as f64 / 2.0),
                LogicalType::Decimal { width, scale } => Value::Decimal {
                    unscaled: i128::from(small) * 37,
                    width: *width,
                    scale: *scale,
                },
                LogicalType::Boolean => Value::Boolean(small > 0),
                LogicalType::Varchar => Value::Varchar(text(rng)),
                other => panic!("the generator has nothing for a {other}"),
            });
        }
        Vector::from_values(ty.clone(), &values).expect("the generator builds legal columns")
    }

    /// A string, chosen so that the inline limit, the empty string, multi byte characters and the
    /// substring the `LIKE` patterns look for all turn up often.
    fn text(rng: &mut Rng) -> String {
        let words = [
            "",
            "google",
            "Google",
            "a google search",
            "GOOGLE",
            "goggle",
            "twelve bytes",
            "thirteen bytes",
            "π is two bytes and this string is not inline at all",
            "g",
        ];
        words[rng.below(words.len() as u64) as usize].to_owned()
    }

    /// The three form pairings that have a loop, as a pair of vectors built from one column.
    ///
    /// Constant against constant is not here on purpose: that pair returns a constant vector rather
    /// than a flat one, so it is right without being equal, and it has a test of its own below.
    fn pairings(left: &Vector, right: &Vector) -> Vec<(Vector, Vector)> {
        let rows = left.len();
        let as_constant = |vector: &Vector| {
            Vector::constant(vector.logical_type().clone(), vector.value_at(0), rows)
        };
        vec![
            (left.clone(), right.clone()),
            (left.clone(), as_constant(right)),
            (as_constant(left), right.clone()),
        ]
    }

    #[test]
    fn every_specialized_arithmetic_agrees_with_the_row_at_a_time_path() {
        let mut rng = Rng(0x5eed_1234_9abc_def1);
        let types = [
            LogicalType::TinyInt,
            LogicalType::Integer,
            LogicalType::BigInt,
            LogicalType::HugeInt,
            LogicalType::UInteger,
            LogicalType::Float,
            LogicalType::Double,
            LogicalType::decimal(10, 2).expect("a legal decimal"),
        ];
        for ty in &types {
            for nulls in [0, 7, 1] {
                let left = sample(ty, 96, nulls, &mut rng);
                let right = sample(ty, 96, nulls, &mut rng);
                for name in ["+", "-", "*", "//", "%"] {
                    for (one, other) in pairings(&left, &right) {
                        agrees(name, &[one, other], ty);
                    }
                }
                for name in ["-", "abs"] {
                    agrees(name, std::slice::from_ref(&left), ty);
                }
                if matches!(ty, LogicalType::Double) {
                    for (one, other) in pairings(&left, &right) {
                        agrees("/", &[one, other], ty);
                    }
                }
            }
        }
    }

    #[test]
    fn every_specialized_string_and_boolean_function_agrees_with_the_row_at_a_time_path() {
        let mut rng = Rng(0x1234_5eed_dead_beef);
        for nulls in [0, 7, 1] {
            let left = sample(&LogicalType::Varchar, 96, nulls, &mut rng);
            let right = sample(&LogicalType::Varchar, 96, nulls, &mut rng);
            agrees("length", std::slice::from_ref(&left), &LogicalType::BigInt);
            for name in ["lower", "upper"] {
                agrees(name, std::slice::from_ref(&left), &LogicalType::Varchar);
            }
            for (one, other) in pairings(&left, &right) {
                agrees("||", &[one, other], &LogicalType::Varchar);
            }
            // One of each pattern shape, so that the compiled form and the general walk are both
            // checked against the walk the oracle always takes.
            for spelling in ["google", "goo%", "%gle", "%oog%", "g_ogle", "%g%l%", "%", ""] {
                let pattern =
                    Vector::constant(LogicalType::Varchar, Value::Varchar(spelling.into()), 96);
                for name in ["~~", "!~~", "~~*", "!~~*"] {
                    agrees(name, &[left.clone(), pattern.clone()], &LogicalType::Boolean);
                }
            }
            let flags = sample(&LogicalType::Boolean, 96, nulls, &mut rng);
            agrees("not", std::slice::from_ref(&flags), &LogicalType::Boolean);
        }
    }

    /// A pattern that changes from row to row is legal SQL and has no loop, so it has to come out
    /// right through the fallback and it has to say that it did.
    #[test]
    fn a_pattern_that_varies_per_row_is_still_right_and_says_so() {
        let text = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("google".into()), Value::Varchar("goggle".into())],
        )
        .expect("two rows");
        let pattern = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("goo%".into()), Value::Varchar("goo%".into())],
        )
        .expect("two rows");
        // The counters are process wide and the harness runs tests in parallel, so the ones that
        // read a count take turns.
        let _turn = fallback::TURN.lock().expect("no test panics while holding this");
        let before = fallback::count(Kernel::Scalar, Form::Flat, Form::Flat);
        agrees("~~", &[text, pattern], &LogicalType::Boolean);
        assert!(fallback::count(Kernel::Scalar, Form::Flat, Form::Flat) > before);
    }

    #[test]
    fn a_call_where_every_argument_is_constant_costs_one_call() {
        let left = Vector::constant(LogicalType::Integer, Value::Integer(3), 1024);
        let right = Vector::constant(LogicalType::Integer, Value::Integer(4), 1024);
        let sum = call("+", &[left, right], &LogicalType::Integer).expect("adds");
        assert_eq!(sum.form(), Form::Constant);
        assert_eq!(sum.len(), 1024);
        assert_eq!(sum.value_at(1000), Value::Integer(7));
    }

    /// A dictionary has no loop here yet, and the thing that makes that safe is reading its nulls
    /// from the vector it points at rather than from its own validity.
    #[test]
    fn a_dictionary_argument_reads_its_nulls_from_the_values() {
        let values = Vector::from_values(
            LogicalType::Integer,
            &[Value::Null, Value::Integer(5), Value::Integer(6)],
        )
        .expect("three values");
        let codes = Vector::dictionary(vec![0, 1, 2, 1, 0], values).expect("a dictionary");
        let ten = Vector::constant(LogicalType::Integer, Value::Integer(10), 5);
        let sum = call("+", &[codes, ten], &LogicalType::Integer).expect("adds");
        assert_eq!(sum.value_at(0), Value::Null);
        assert_eq!(sum.value_at(1), Value::Integer(15));
        assert_eq!(sum.value_at(4), Value::Null);
    }

    #[test]
    fn an_empty_call_is_an_empty_answer() {
        let empty = Vector::from_values(LogicalType::Integer, &[]).expect("no rows");
        let sum = call("+", &[empty.clone(), empty], &LogicalType::Integer).expect("adds");
        assert_eq!(sum.len(), 0);
        assert_eq!(sum.validity(), &Validity::AllValid);
    }

    /// The one case where a native operation on the run's own width and the oracle's trip through
    /// `i128` could disagree, written down so that it stays checked.
    #[test]
    fn the_smallest_value_of_a_type_modulo_negative_one_is_zero_on_both_paths() {
        let left =
            Vector::from_values(LogicalType::TinyInt, &[Value::TinyInt(i8::MIN)]).expect("one row");
        let right = Vector::constant(LogicalType::TinyInt, Value::TinyInt(-1), 1);
        agrees("%", &[left.clone(), right.clone()], &LogicalType::TinyInt);
        let answer = call("%", &[left, right], &LogicalType::TinyInt).expect("modulo");
        assert_eq!(answer.value_at(0), Value::TinyInt(0));
    }
}

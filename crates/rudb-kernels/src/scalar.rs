//! The scalar functions, which for M0 is arithmetic, the string functions and `LIKE`.
//!
//! One entry point rather than a function pointer per name, because the binder has already decided
//! which function this is and what its arguments were cast to, so all that is left is to do the
//! work. When the kernel generator in `spec/07-execution.md` section 7.3 arrives this becomes a
//! table lookup and the bodies below become the generated specializations, and the interface the
//! executor calls does not change.
//!
//! Null in, null out, for everything except `coalesce` and `nullif`. That rule is applied once here
//! rather than inside each function, which is the only way to be sure that a function added later
//! does not quietly forget it. The two exceptions are the two functions whose whole job is to answer
//! something other than null when a null goes in.
//!
//! Dividing by zero is three behaviours and not one, which `divided_by_zero` writes out. `/` is
//! IEEE arithmetic on two doubles and answers an infinity or a nan, `//` raises whatever it was
//! given, and `%` raises on integers and decimals and answers a nan on floats. All three were
//! measured against the pinned binary rather than reasoned about, because a query that returns a
//! row where another engine raises is a difference a user notices.
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
//! by one raises an error the oracle never raised and overflowing on one raises another.
//!
//! A `LIKE` pattern is compiled once per vector rather than once per row. `%google%`, which is
//! ClickBench query 21 over a hundred million rows, comes out as a substring search rather than as
//! a backtracking automaton, and the pattern stops being converted from a `Value` to a `String` to
//! a `Vec<char>` on every row.

use memchr::memmem;
use rudb_common::{Error, LogicalType, Result, Value, civil_from_days, days_from_civil};
use rudb_vector::{Data, Form, StringColumn, Validity, Vector};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::cast;
use crate::compare::{self, Comparison};
use crate::datetime::{self, Count, Part};
use crate::fallback::{self, Kernel};
use crate::number::{approximate, beyond, digits, fit, integral, pow10, rescale};
use crate::prepare::{Hoisted, Recipe};
use crate::regexp;
use crate::shape::{first, identity, nulls_of, single};
use crate::subscript;
use crate::text;

/// How the call being evaluated is written, for the one error that quotes it.
///
/// DuckDB's division by zero message names the expression rather than the numbers in it, and the
/// expression it names is the bound one, so `a // 0` over a column says `a` and a decimal literal
/// says the digits the cast gave it. A kernel has the numbers and not the expression, so the caller
/// that has the plan hands this down.
///
/// It is a closure rather than a string because it is wanted only on the row that fails. Rendering
/// an expression once a chunk to carry it into an error that almost never happens is a cost every
/// chunk pays for a message nobody reads. `None` is a caller with no expression to name, which
/// quotes the two values instead.
pub type Written<'a> = Option<&'a dyn Fn() -> String>;

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
pub fn call<V: AsRef<Vector>>(
    name: &str,
    args: &[V],
    returns: &LogicalType,
    written: Written<'_>,
) -> Result<Vector> {
    run(name, &Hoisted::Nothing, args, returns, written)
}

/// Calls a scalar function on a batch, with the per query work already done.
///
/// The same function as [`call`] and the same answer. What the recipe saves is everything the call
/// would otherwise decide on every chunk from arguments that were literals in the query, which for
/// a regular expression is compiling it. See [`crate::prepare`].
///
/// # Errors
///
/// The same ones [`call`] reports, on the same inputs. A recipe never moves an error earlier.
pub fn call_prepared<V: AsRef<Vector>>(
    recipe: &Recipe,
    args: &[V],
    returns: &LogicalType,
    written: Written<'_>,
) -> Result<Vector> {
    run(recipe.name(), recipe.hoisted(), args, returns, written)
}

/// The body both entry points share.
fn run<V: AsRef<Vector>>(
    name: &str,
    hoisted: &Hoisted,
    args: &[V],
    returns: &LogicalType,
    written: Written<'_>,
) -> Result<Vector> {
    let rows = args.first().map_or(0, |arg| arg.as_ref().len());
    for (at, arg) in args.iter().enumerate() {
        if arg.as_ref().len() != rows {
            return Err(Error::internal(format!(
                "argument {at} of {name} is {} rows and argument 0 is {rows}",
                arg.as_ref().len()
            )));
        }
    }

    // A batch of no rows has the same answer whichever function this is and whichever form the
    // arguments came in, so there is nothing here to dispatch on. This is the same empty vector the
    // row at a time path at the end builds out of no values, and it used to be built there: an
    // empty batch fell past every path below and was recorded as a call that reached the row at a
    // time path, which is what `scalar constant against constant 1248` in the TPC-H fallback ledger
    // was. It was q19 evaluating `-` and `*` over an empty batch six hundred and twenty four times
    // each, with no row read at all.
    if rows == 0 {
        return Vector::from_values(returns.clone(), &[]);
    }

    // Every argument constant is one call rather than 1024 of them. This is `3 * 4` surviving
    // constant folding, and it is also every correlated scalar the optimizer has already evaluated.
    if !args.is_empty() && args.iter().all(|arg| arg.as_ref().form() == Form::Constant) {
        let row: Vec<Value> =
            args.iter().map(|arg| arg.as_ref().try_value_at(0)).collect::<Result<_>>()?;
        return Ok(Vector::constant(
            returns.clone(),
            call_values(name, &row, returns, written)?,
            rows,
        ));
    }

    if let Some(vector) = specialized(name, hoisted, args, returns, rows, written)? {
        return Ok(vector);
    }

    // A unary function reports its one form on both sides of the table, because a column for the
    // argument that is not there would be a column of zeros in every row of the report.
    let left = args.first().map_or(Form::Flat, |arg| arg.as_ref().form());
    fallback::record(Kernel::Scalar, left, args.get(1).map_or(left, |arg| arg.as_ref().form()));

    let mut row = Vec::with_capacity(args.len());
    let mut values = Vec::with_capacity(rows);
    // row at a time: the path recorded above, which is every function that has no vectorized form
    // yet, and counts itself so which functions those are shows up in the report.
    for index in 0..rows {
        row.clear();
        for arg in args {
            row.push(arg.as_ref().try_value_at(index)?);
        }
        values.push(call_values(name, &row, returns, written)?);
    }
    Vector::from_values(returns.clone(), &values)
}

/// The result for a call this file has a loop for, or `None` to say it has not.
fn specialized<V: AsRef<Vector>>(
    name: &str,
    hoisted: &Hoisted,
    args: &[V],
    returns: &LogicalType,
    rows: usize,
    written: Written<'_>,
) -> Result<Option<Vector>> {
    if regexp::is_regexp(name) {
        return regexp::vectorized(name, hoisted.regexp(), args, returns, rows);
    }
    match args {
        [only] => unary(name, only.as_ref(), returns, rows),
        [left, right] => {
            binary(name, hoisted, left.as_ref(), right.as_ref(), returns, rows, written)
        }
        _ => Ok(None),
    }
}

/// What a recipe can lift out of a call to `name`, given the arguments that were literals.
///
/// `None` says there was nothing to lift, which covers three cases that behave the same: the
/// function has no prepare step, the argument that would drive it is not a literal, and the literal
/// is one that does not compile. The third is the one worth stating, because it is what keeps a
/// recipe from moving an error to a place the query did not put it.
pub(crate) fn hoist(name: &str, literals: &[Option<Value>]) -> Option<Hoisted> {
    if regexp::is_regexp(name) {
        return regexp::hoist(name, literals).map(|call| Hoisted::Regexp(Box::new(call)));
    }
    let [_, Some(Value::Varchar(spelling))] = literals else {
        return None;
    };
    Like::of(name, spelling).map(Hoisted::Like)
}

/// Runs `body` at every row that is not already null, and hands back the nulls of the answer.
///
/// The answer has the nulls the arguments had and no others. Nothing in this file turns a valid
/// input into a null output: division by zero used to, which is what this used to collect indices
/// for, and it raises now, so the mask that comes out is the mask that went in.
pub(crate) fn over_valid(
    len: usize,
    base: Validity,
    mut body: impl FnMut(usize) -> Result<()>,
) -> Result<Validity> {
    match &base {
        Validity::AllValid => {
            for index in 0..len {
                body(index)?;
            }
        }
        // Nothing to compute. Every answer is null and the data is never touched, which is what a
        // projection of an expression over an all null column costs.
        Validity::AllInvalid => {}
        Validity::Mask(mask) => {
            for index in 0..len {
                if mask.get(index) {
                    body(index)?;
                }
            }
        }
    }
    // An empty vector has no null to record, and `Vector::from_values` normalizes the empty mask it
    // builds to all valid, so a specialized empty result has to say the same thing.
    Ok(if len == 0 { Validity::AllValid } else { base.normalize(len) })
}

/// The result vector, with the layout check that `Vector::flat` does kept rather than skipped.
pub(crate) fn finish(
    returns: &LogicalType,
    data: Data,
    validity: Validity,
) -> Result<Option<Vector>> {
    Ok(Some(Vector::flat(returns.clone(), data)?.with_validity(validity)))
}

/// A one argument call, for the functions with a loop.
///
/// The argument is read through an index mapping rather than out of a slice, for the reason given on
/// `by_form!`: a string column out of Parquet is dictionary encoded, and asking for `Vector::data`
/// here meant every one of these functions fell out of its loop and took the row at a time path on
/// the only files anybody runs. That was the second half of #288, and on the ClickBench file it is
/// `length`, `strlen` and the `lower` in front of a `LIKE`.
///
/// A column read out of a native table is a third shape: its bytes are in the file, so there is no
/// run of them to index into and `Vector::data` answers `None` whether it is asked of the vector or
/// of a dictionary's values. Every string function here used to decline on that and fall to the row
/// at a time path, which reads the same value and then pays for a `Value` and the boxed string
/// inside it as well. They go through [`Text`] now, which reads it either way.
fn unary(name: &str, arg: &Vector, returns: &LogicalType, rows: usize) -> Result<Option<Vector>> {
    let base = nulls_of(arg);
    match arg.form() {
        Form::Flat => {
            let Some(data) = arg.data() else {
                return Ok(None);
            };
            one_of(name, data, identity, base, rows, returns, arg)
        }
        // Text held as views into an arena, or read out of a file. Neither keeps a `Data` to index
        // into, so the vector is asked for the row and finds the bytes whichever way it holds them.
        Form::StringView => read_text(name, arg, base, rows, returns),
        Form::Dictionary | Form::Rle => {
            let Some((codes, values)) = arg.positions() else {
                return Ok(None);
            };
            if codes.len() < rows {
                return Ok(None);
            }
            match values.data() {
                // Every code is inside the dictionary because `Vector::dictionary` checks that on
                // the way in, so the gather needs no bound of its own.
                Some(data) => {
                    one_of(name, data, move |index| codes[index] as usize, base, rows, returns, arg)
                }
                // A dictionary whose values are read out of a file, which is what every string
                // column of a native table is. Following the code here would mean reading the
                // dictionary's row rather than the vector's, so the vector is asked for the row
                // instead and does the lookup on the way.
                None => read_text(name, arg, base, rows, returns),
            }
        }
        // There is no constant arm, and there is no point in one. A function whose every argument is
        // constant is answered by `call` in a single row before it reaches here, and a one argument
        // function has only the one argument to be constant. A sequence is a run of integers and
        // none of these is worth a loop over one of those, `abs` being the closest and nobody
        // writing it.
        _ => Ok(None),
    }
}

/// Which one argument function this is, once the argument's form has been turned into a mapping.
fn one_of<A: Fn(usize) -> usize>(
    name: &str,
    data: &Data,
    at: A,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
    arg: &Vector,
) -> Result<Option<Vector>> {
    match name {
        "__rudb_zero_to_null" => {
            let validity = Validity::from_iter(rows, |index| {
                base.is_valid(index) && approximate(&arg.value_at(index)) != Some(0.0)
            });
            Ok(Some(arg.clone().with_validity(validity)))
        }
        "not" => not_of(data, at, base, rows, returns),
        "-" | "abs" if arg.logical_type() == returns => {
            sign_of(name, data, at, base, rows, returns, arg)
        }
        "length" | "strlen" | "lower" | "upper" => match data {
            Data::Varlen(column) => text_of(name, &Text::Held { column, at }, base, rows, returns),
            _ => Ok(None),
        },
        "make_date" => made_date(data, at, base, rows, returns),
        "epoch_ms" => made_timestamp(data, at, base, rows, returns),
        name if datetime::is_interval(name) => made_interval(name, data, at, base, rows, returns),
        _ => Ok(None),
    }
}

/// Where a one argument string function reads its argument.
///
/// A string column that lives in a buffer is a run of bytes and an index into it, and a string
/// column that lives in a file is neither. `Vector::data` answers `None` for the second one, because
/// the bytes are in the file and the only way to a value is to ask the vector to read it, and every
/// string function here used to give up the moment it saw that. That is the form every column of a
/// native table arrives in, so all of them fell to the row at a time path, which does the same read
/// and then pays for a `Value` and the boxed string inside it on top of it. Measured on `Referer`
/// under `WHERE Referer <> ''`, which is eighty one million rows, `COUNT(LOWER(Referer))` took 1.98
/// seconds against 1.11 for `COUNT(LENGTH(Referer))`, and the only difference between the two was
/// that `length` had a special case written for this shape and `lower` did not. This is that special
/// case, written once for the shape rather than again for each function.
enum Text<'a, A> {
    /// The bytes are in a column, and `at` says which of its values a row wants.
    Held { column: &'a StringColumn, at: A },
    /// The bytes are behind a reader, and the vector follows a row to them itself.
    Read(&'a Vector),
}

impl<'a> Text<'a, fn(usize) -> usize> {
    /// Reading through the vector, for the forms that keep no bytes of their own.
    ///
    /// `None` for anything that is not text, because a vector holding no `Data` need not be holding
    /// bytes either, and `Vector::try_bytes_at` answers `None` for every row of one of those rather
    /// than failing, which would turn a length into a plausible looking zero.
    ///
    /// The mapping is a function pointer that nothing ever calls, since the `Read` arm has no
    /// mapping. It is here so the enum has a type to be, and naming it once here beats a turbofish
    /// at the call.
    fn read(vector: &'a Vector) -> Option<Self> {
        matches!(vector.logical_type(), LogicalType::Varchar).then_some(Text::Read(vector))
    }
}

impl<A: Fn(usize) -> usize> Text<'_, A> {
    /// The bytes of the value at `index`, and nothing where there is no value.
    ///
    /// # Errors
    ///
    /// Whatever reading the value out of storage raises.
    fn bytes(&self, index: usize) -> Result<&[u8]> {
        match self {
            Text::Held { column, at } => Ok(column.bytes(at(index)).unwrap_or_default()),
            Text::Read(vector) => Ok(vector.try_bytes_at(index)?.unwrap_or_default()),
        }
    }

    /// The value at `index` as text, and nothing where it is not valid UTF-8.
    ///
    /// `StringColumn::get` answers nothing for that too, so a kernel reading through here writes
    /// what one reading through a held column writes.
    ///
    /// # Errors
    ///
    /// Whatever reading the value out of storage raises.
    fn get(&self, index: usize) -> Result<&str> {
        Ok(std::str::from_utf8(self.bytes(index)?).unwrap_or_default())
    }

    /// How many bytes the value at `index` has, without reading them where that can be avoided.
    ///
    /// A source that keeps its values end to end knows the length from the two offsets that say
    /// where the value starts and stops, and those are what it would read the value through anyway.
    /// On `Referer` that is the difference between `COUNT(STRLEN(Referer))` costing 0.28 seconds and
    /// 147 MB and `COUNT(LENGTH(Referer))` costing 1.11 and 3,213 MB, because the second one has to
    /// look at the bytes to count characters and the first one does not.
    ///
    /// # Errors
    ///
    /// Whatever reading the length out of storage raises.
    fn len(&self, index: usize) -> Result<usize> {
        match self {
            Text::Held { column, at } => Ok(column.bytes(at(index)).unwrap_or_default().len()),
            Text::Read(vector) => Ok(vector.try_bytes_len_at(index)?.unwrap_or_default()),
        }
    }
}

/// The string functions, over a vector that reads its own values.
fn read_text(
    name: &str,
    arg: &Vector,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    match Text::read(arg) {
        Some(text) => text_of(name, &text, base, rows, returns),
        None => Ok(None),
    }
}

/// Which one argument string function this is, once its argument has been turned into a source.
fn text_of<A: Fn(usize) -> usize>(
    name: &str,
    text: &Text<'_, A>,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    match name {
        "length" => length_of(text, base, rows, returns),
        "strlen" => bytes_of(text, base, rows, returns),
        "lower" | "upper" => fold_of(name, text, base, rows, returns),
        _ => Ok(None),
    }
}

/// `make_date(days)`, which is the identity on the bytes.
///
/// A date is days since the epoch in an `i32` and so is the argument, so the whole function is the
/// logical type changing and the run of values staying exactly as it was. It is here rather than
/// left to the row at a time path because the ClickBench entry wraps a hundred million row column in
/// it, and a copy is the difference between that costing one pass over a run of integers and costing
/// a hundred million boxed values.
fn made_date<A: Fn(usize) -> usize>(
    data: &Data,
    at: A,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let (Data::Int32(days), LogicalType::Date) = (data, returns) else {
        return Ok(None);
    };
    let out: Vec<i32> = (0..rows).map(|index| days[at(index)]).collect();
    finish(returns, Data::Int32(out.into()), base.normalize(rows))
}

/// `epoch_ms(milliseconds)`, which is one multiply per row.
fn made_timestamp<A: Fn(usize) -> usize>(
    data: &Data,
    at: A,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let (Data::Int64(millis), LogicalType::Timestamp) = (data, returns) else {
        return Ok(None);
    };
    let mut out = vec![0i64; rows];
    let validity = over_valid(rows, base, |index| {
        out[index] = micros_of_millis(millis[at(index)])?;
        Ok(())
    })?;
    finish(returns, Data::Int64(out.into()), validity)
}

/// `to_seconds(count)` and the other twelve interval constructors, over a run of counts.
///
/// The benchmark view turns every stored `EventTime` into a timestamp through `to_seconds`, and on
/// the row at a time path that cost six times what DuckDB spends on it, most of it in making a
/// `Value` of each count and each interval. The arithmetic is [`datetime::Unit::count`], the same
/// one that path calls, so the answers and the errors are the ones it gives.
fn made_interval<A: Fn(usize) -> usize>(
    name: &str,
    data: &Data,
    at: A,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    if returns != &LogicalType::Interval {
        return Ok(None);
    }
    let unit = datetime::Unit::of(name)?;
    match data {
        Data::Float64(counts) => {
            counted(rows, base, returns, |index| unit.count(Count::Real(counts[at(index)])))
        }
        Data::Int64(counts) => counted(rows, base, returns, |index| {
            unit.count(Count::Whole(i128::from(counts[at(index)])))
        }),
        Data::Int32(counts) => counted(rows, base, returns, |index| {
            unit.count(Count::Whole(i128::from(counts[at(index)])))
        }),
        _ => Ok(None),
    }
}

/// The intervals `make` gives for every row that is not null.
fn counted(
    rows: usize,
    base: Validity,
    returns: &LogicalType,
    make: impl Fn(usize) -> Result<(i32, i32, i64)>,
) -> Result<Option<Vector>> {
    let mut out = vec![(0, 0, 0); rows];
    let validity = over_valid(rows, base, |index| {
        out[index] = make(index)?;
        Ok(())
    })?;
    finish(returns, Data::Interval(out.into()), validity)
}

/// Milliseconds since the epoch as microseconds since the epoch.
///
/// The overflow is upstream's sentence, which names the two units rather than the function, because
/// upstream reads the argument as a millisecond timestamp and then converts it.
fn micros_of_millis(millis: i64) -> Result<i64> {
    millis.checked_mul(1_000).ok_or_else(|| {
        Error::conversion("Could not convert Timestamp(MS) to Timestamp(US)".to_owned())
    })
}

/// `NOT`, which is one pass over a run of bytes.
fn not_of<A: Fn(usize) -> usize>(
    data: &Data,
    at: A,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let Data::Bool(held) = data else {
        return Ok(None);
    };
    let mut out = vec![false; rows];
    let validity = over_valid(rows, base, |index| {
        out[index] = !held[at(index)];
        Ok(())
    })?;
    finish(returns, Data::Bool(out.into()), validity)
}

/// Unary minus and `abs`, where the argument and the result are the same type.
fn sign_of<A: Fn(usize) -> usize>(
    name: &str,
    data: &Data,
    at: A,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
    arg: &Vector,
) -> Result<Option<Vector>> {
    // Hoisted, because which of the two functions this is does not change from row to row and a
    // string comparison inside the loop would be most of what the loop costs.
    let negating = name == "-";
    macro_rules! runs {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(
                    Data::$variant(held) => {
                        let mut out = vec![0; rows];
                        let validity = over_valid(rows, base, |index| {
                            let value = held[at(index)];
                            let computed =
                                if negating { value.checked_neg() } else { value.checked_abs() };
                            match computed {
                                Some(answer) => {
                                    out[index] = answer;
                                    Ok(())
                                }
                                None if negating => Err(negation_overflow()),
                                None => Err(abs_overflow(&arg.value_at(index))),
                            }
                        })?;
                        finish(returns, Data::$variant(out.into()), validity)
                    }
                )+
                Data::Float32(held) => {
                    let mut out = vec![0.0f32; rows];
                    let validity = over_valid(rows, base, |index| {
                        let value = held[at(index)];
                        out[index] = if negating { -value } else { value.abs() };
                        Ok(())
                    })?;
                    finish(returns, Data::Float32(out.into()), validity)
                }
                Data::Float64(held) => {
                    let mut out = vec![0.0f64; rows];
                    let validity = over_valid(rows, base, |index| {
                        let value = held[at(index)];
                        out[index] = if negating { -value } else { value.abs() };
                        Ok(())
                    })?;
                    finish(returns, Data::Float64(out.into()), validity)
                }
                _ => Ok(None),
            }
        };
    }
    // The unsigned runs are left out on purpose, which is what the `signed` group is for. Negating
    // a `UBIGINT` is an overflow at every row but zero, so the fallback's error message is the right
    // answer and a loop for it would be a loop that exists to fail.
    rudb_vector::for_each_layout!(signed, runs)
}

/// `length`, which counts characters rather than bytes.
fn length_of<A: Fn(usize) -> usize>(
    text: &Text<'_, A>,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    if returns != &LogicalType::BigInt {
        return Ok(None);
    }
    let mut out = vec![0i64; rows];
    let validity = over_valid(rows, base, |index| {
        let bytes = text.bytes(index)?;
        // A character in UTF-8 is one lead byte and some continuation bytes, and a continuation
        // byte is the ones matching `0b10xx_xxxx`. Counting the bytes that are not continuations is
        // the same number `chars().count()` reaches and it never decodes anything.
        let characters = bytes.iter().filter(|byte| (**byte as i8) >= -0x40).count();
        out[index] = i64::try_from(characters).unwrap_or(i64::MAX);
        Ok(())
    })?;
    finish(returns, Data::Int64(out.into()), validity)
}

/// `strlen`, which counts bytes where `length` counts characters.
///
/// The two differ on anything outside ASCII. `strlen('héllo')` is 6 and `length('héllo')` is 5,
/// which is why this is a function of its own upstream rather than another name for `length`.
/// ClickBench queries 28 and 29 are `AVG(STRLEN(URL))` and `AVG(STRLEN(Referer))` over a hundred
/// million rows, so it gets the vectorized path for the same reason `length` has one.
fn bytes_of<A: Fn(usize) -> usize>(
    text: &Text<'_, A>,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    if returns != &LogicalType::BigInt {
        return Ok(None);
    }
    let mut out = vec![0i64; rows];
    // A stored column answers the whole vector in one call where it can, which on ClickBench 28 is
    // the difference between a length costing a load and costing four calls. A vector with no nulls
    // has the argument's mask as its answer's, so there is nothing left to do after it.
    let whole = match text {
        Text::Read(vector) if matches!(base, Validity::AllValid) => {
            vector.try_bytes_lens(&mut out)?
        }
        _ => false,
    };
    if whole {
        return finish(returns, Data::Int64(out.into()), Validity::AllValid);
    }
    let validity = over_valid(rows, base, |index| {
        out[index] = i64::try_from(text.len(index)?).unwrap_or(i64::MAX);
        Ok(())
    })?;
    finish(returns, Data::Int64(out.into()), validity)
}

/// `lower` and `upper`.
fn fold_of<A: Fn(usize) -> usize>(
    name: &str,
    text: &Text<'_, A>,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    if returns != &LogicalType::Varchar {
        return Ok(None);
    }
    let lowering = name == "lower";
    let out = try_each_string(rows, &base, |index, into| {
        let value = text.get(index)?;
        // `str::to_lowercase` rather than folding the characters into a buffer that is reused
        // across the vector, which would save the allocation. It is not the same function: the
        // string form knows that a final sigma lowercases to a different letter than a medial one
        // does, and the character form cannot know that. A saved allocation is not worth being
        // wrong about Greek. What the vectorized path removes here is the `Value` clone, the second
        // `to_string` and the packing pass, which was three allocations of the four.
        let folded = if lowering { value.to_lowercase() } else { value.to_uppercase() };
        into.push(&folded);
        Ok(())
    })?;
    finish(returns, Data::Varlen(out), base.normalize(rows))
}

/// Builds a string column where every row gets a push, including the null ones.
///
/// A string column is append only and has no way to write row 7 without having written rows 0
/// through 6, so this is the shape a string producing kernel has to take rather than the shape
/// [`over_valid`] takes. A null pushes the empty string, which is what `Vector::from_values` writes
/// under a null and is therefore what keeps a specialized result equal to the oracle's.
pub(crate) fn each_string(
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

/// [`each_string`], for a body that reads its values out of storage and can fail doing it.
///
/// # Errors
///
/// Whatever `body` raises, at the first row that raises it.
fn try_each_string(
    rows: usize,
    base: &Validity,
    mut body: impl FnMut(usize, &mut StringColumn) -> Result<()>,
) -> Result<StringColumn> {
    let mut out = StringColumn::with_capacity(rows);
    for index in 0..rows {
        if base.is_valid(index) {
            body(index, &mut out)?;
        } else {
            out.push("");
        }
    }
    Ok(out)
}

/// A two argument call, for the functions with a loop.
fn binary(
    name: &str,
    hoisted: &Hoisted,
    left: &Vector,
    right: &Vector,
    returns: &LogicalType,
    rows: usize,
    written: Written<'_>,
) -> Result<Option<Vector>> {
    // Nested rather than a let chain, because the minimum supported Rust version is 1.85 and let
    // chains landed in 1.88.
    if matches!(name, "+" | "-") {
        if let Some(moved) = shift_of(name == "-", left, right, returns)? {
            return Ok(Some(moved));
        }
    }
    if let Some((op, floating_zero_errors)) = arithmetic_op(name) {
        return arithmetic_of(op, floating_zero_errors, left, right, returns, written);
    }
    match name {
        "/" => slash_of(left, right, returns),
        "||" => concat_of(left, right, returns),
        "~~" | "!~~" | "~~*" | "!~~*" => like_of(name, hoisted.like(), left, right, returns, rows),
        "date_part" | "date_trunc" => date_of(name, left, right, returns, rows),
        _ => Ok(None),
    }
}

/// The spelling of an arithmetic operator as the operator, and `None` for anything else.
fn arithmetic_op(name: &str) -> Option<(Op, bool)> {
    Some(match name {
        "+" => (Op::Add, false),
        "-" => (Op::Subtract, false),
        "*" => (Op::Multiply, false),
        "//" | "__rudb_checked_slash" => (Op::Divide, true),
        "%" => (Op::Modulo, false),
        "__rudb_checked_remainder" => (Op::Modulo, true),
        _ => return None,
    })
}

/// The form pairings a binary kernel in this file has a loop for.
///
/// Flat against flat, flat against constant and constant against flat are what an expression over a
/// column and a literal produces and are the overwhelming majority of what an expression tree
/// contains. The four pairings with a dictionary on one side are here because the kernel table
/// measured what leaving them out cost, which on `server3` was 73 to 105 nanoseconds a row against
/// 1.2 for the pairings that had a loop.
///
/// This file used to argue that a dictionary belongs in a different loop rather than a different
/// index mapping, because arithmetic over a dictionary wants to compute once per distinct value and
/// hand back a dictionary over the answers. That is still true and it is still the better loop. It
/// was an argument for writing it, though, and what it was actually being used for was an argument
/// for having no loop at all, at forty times the cost of the mapping that was already written.
///
/// Computing once per distinct value also has a question in it that the per row mapping does not,
/// and it is the reason that loop is worth doing carefully rather than quickly. A dictionary's
/// values can hold an entry no code refers to. Computing over the values array would evaluate that
/// entry, and if it overflows, a vector that has no overflowing row in it raises. The mapping here
/// only ever touches an entry some row points at, so it cannot invent an error the row at a time
/// path would not have produced.
///
/// The mapping is a generic parameter and not a `fn(usize) -> usize` stored in a tuple. That is not
/// a style choice: a function pointer is an indirect call the compiler cannot see through, and two
/// of them per row was measured at ten nanoseconds a row on an integer addition, which is twenty
/// times what the addition costs. As a generic parameter each mapping is a zero sized type and the
/// call inlines to nothing, at the price of one copy of the loop per pairing.
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
        if let (Some((codes, values)), Some(other)) = ($left.positions(), $right.data()) {
            let Some(one) = values.data() else { return Ok(None) };
            let at = move |index: usize| codes[index] as usize;
            return $body(one, at, other, identity, $($rest),*);
        }
        if let (Some(one), Some((codes, values))) = ($left.data(), $right.positions()) {
            let Some(other) = values.data() else { return Ok(None) };
            let at = move |index: usize| codes[index] as usize;
            return $body(one, identity, other, at, $($rest),*);
        }
        if let (Some((codes, values)), Some(value)) =
            ($left.positions(), $right.constant_value())
        {
            let Some(one) = values.data() else { return Ok(None) };
            let Some(held) = single($right.logical_type(), value) else { return Ok(None) };
            let Some(other) = held.data() else { return Ok(None) };
            let at = move |index: usize| codes[index] as usize;
            return $body(one, at, other, first, $($rest),*);
        }
        if let (Some(value), Some((codes, values))) =
            ($left.constant_value(), $right.positions())
        {
            let Some(other) = values.data() else { return Ok(None) };
            let Some(held) = single($left.logical_type(), value) else { return Ok(None) };
            let Some(one) = held.data() else { return Ok(None) };
            let at = move |index: usize| codes[index] as usize;
            return $body(one, first, other, at, $($rest),*);
        }
        Ok(None)
    }};
}

/// A timestamp with an interval added to it or taken off it, over runs of both.
///
/// This is the other half of the benchmark view's `EventTime`, the epoch plus the interval
/// `to_seconds` made, and it is [`datetime::shifted_stamp`] on each row, which is what the row at a
/// time path reaches through `datetime::shift`. A date or a time on the moving side is left to that
/// path, since neither comes back as the type it went in as.
fn shift_of(
    subtract: bool,
    left: &Vector,
    right: &Vector,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let stamped = |ty: &LogicalType| {
        matches!(ty, LogicalType::Timestamp | LogicalType::TimestampTz) && ty == returns
    };
    let interval = |ty: &LogicalType| matches!(ty, LogicalType::Interval);
    let (one, other) = (left.logical_type(), right.logical_type());
    let stamp_first = if stamped(one) && interval(other) {
        true
    } else if !subtract && interval(one) && stamped(other) {
        false
    } else {
        return Ok(None);
    };
    by_form!(left, right, shift_runs, subtract, stamp_first, left, right, returns)
}

/// The loop under [`shift_of`], once each side's form has been turned into a mapping.
#[expect(
    clippy::too_many_arguments,
    reason = "two sides with an index each, the direction, which side is the timestamp, and the \
              vectors and type the answer is built from"
)]
fn shift_runs<L, R>(
    one: &Data,
    at_left: L,
    other: &Data,
    at_right: R,
    subtract: bool,
    stamp_first: bool,
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
    let out = match (stamp_first, one, other) {
        (true, Data::Int64(stamps), Data::Interval(intervals)) => {
            shifted(stamps, at_left, intervals, at_right, subtract, base, rows)?
        }
        (false, Data::Interval(intervals), Data::Int64(stamps)) => {
            shifted(stamps, at_right, intervals, at_left, subtract, base, rows)?
        }
        _ => return Ok(None),
    };
    let (out, validity) = out;
    finish(returns, Data::Int64(out.into()), validity)
}

/// Each timestamp moved by the interval beside it, with the timestamps and the intervals each
/// read through their own mapping.
fn shifted<S, I>(
    stamps: &[i64],
    at_stamp: S,
    intervals: &[(i32, i32, i64)],
    at_interval: I,
    subtract: bool,
    base: Validity,
    rows: usize,
) -> Result<(Vec<i64>, Validity)>
where
    S: Fn(usize) -> usize,
    I: Fn(usize) -> usize,
{
    let sign = if subtract { -1 } else { 1 };
    let mut out = vec![0i64; rows];
    let validity = over_valid(rows, base, |index| {
        let (months, days, micros) = intervals[at_interval(index)];
        out[index] = datetime::shifted_stamp(
            stamps[at_stamp(index)],
            i64::from(months) * sign,
            i64::from(days) * sign,
            i128::from(micros) * i128::from(sign),
        )?;
        Ok(())
    })?;
    Ok((out, validity))
}

/// `+`, `-`, `*`, `//` and `%`, on the types the binder has already made match.
fn arithmetic_of(
    op: Op,
    floating_zero_errors: bool,
    left: &Vector,
    right: &Vector,
    returns: &LogicalType,
    written: Written<'_>,
) -> Result<Option<Vector>> {
    // Both sides already the result type is what makes a native operation on the run's own width
    // exactly the oracle's widen to `i128` and narrow back, rather than nearly it. A decimal
    // product is the exception: its two sides are the answer's width and keep their own scales, so
    // the runs still line up while the types do not, and what the loop needs is the width.
    let lined_up = match (returns, left.logical_type(), right.logical_type()) {
        (
            LogicalType::Decimal { width, .. },
            LogicalType::Decimal { width: one, .. },
            LogicalType::Decimal { width: other, .. },
        ) => one == width && other == width,
        (_, one, other) => one == returns && other == returns,
    };
    if !lined_up {
        return Ok(None);
    }
    by_form!(left, right, arithmetic_runs, op, floating_zero_errors, left, right, returns, written)
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
    floating_zero_errors: bool,
    left: &Vector,
    right: &Vector,
    returns: &LogicalType,
    written: Written<'_>,
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
        return decimal_runs(
            one, at_left, other, at_right, op, &base, left, right, returns, written,
        );
    }
    // Dividing has to look at the divisor before it computes, and has to know whether the row was
    // already null before it raises on a zero, so it keeps the careful loop.
    if matches!(op, Op::Divide | Op::Modulo) {
        return guarded_runs(
            one,
            at_left,
            other,
            at_right,
            op,
            floating_zero_errors,
            base,
            left,
            right,
            returns,
            written,
        );
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
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
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
                    return finish(returns, Data::$variant(out.into()), base.clone());
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
                return finish(returns, Data::$variant(out.into()), base.clone());
            }
        };
    }

    // A date and a timestamp are stored in the same runs the integers are, and arithmetic on them
    // is not integer arithmetic, so the run is not enough to decide by. The oracle answers a date
    // plus a date with a message saying it is not implemented, and that is the answer this path has
    // to leave it room to give.
    if returns.is_integer() {
        rudb_vector::for_each_layout!(integer, integers);
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

/// Dividing and taking a remainder, where a zero on the right is an error rather than an answer,
/// except on the float remainder, where it is a nan.
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
    floating_zero_errors: bool,
    base: Validity,
    left: &Vector,
    right: &Vector,
    returns: &LogicalType,
    written: Written<'_>,
) -> Result<Option<Vector>>
where
    L: Fn(usize) -> usize,
    R: Fn(usize) -> usize,
{
    let rows = left.len();
    macro_rules! integers {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            $(
                if let (Data::$variant(a), Data::$variant(b)) = (one, other) {
                    let mut out = vec![0; rows];
                    let validity = over_valid(rows, base, |index| {
                        let (x, y) = (a[at_left(index)], b[at_right(index)]);
                        if y == 0 {
                            return Err(divided_by_zero(
                                written,
                                op.symbol(),
                                &left.value_at(index),
                                &right.value_at(index),
                            ));
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
                                Ok(())
                            }
                            None => Err(overflow(
                                op,
                                returns,
                                &left.value_at(index),
                                &right.value_at(index),
                            )),
                        }
                    })?;
                    return finish(returns, Data::$variant(out.into()), validity);
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
                    if y == 0.0 && floating_zero_errors {
                        return Err(divided_by_zero(
                            written,
                            op.symbol(),
                            &left.value_at(index),
                            &right.value_at(index),
                        ));
                    }
                    out[index] = $narrow(float_step(op, $widen(x), $widen(y)));
                    Ok(())
                })?;
                return finish(returns, Data::$variant(out.into()), validity);
            }
        };
    }

    if returns.is_integer() {
        rudb_vector::for_each_layout!(integer, integers);
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
///
/// A product is the one that does not take its operands at the answer's scale. `DECIMAL(4,2) *
/// DECIMAL(4,2)` is a `DECIMAL(8,4)` and the two sides arrive holding two decimal places each, so
/// the unscaled values multiply straight into the answer. Everything else arrives at the answer's
/// own scale and is added, subtracted or divided there.
#[expect(
    clippy::too_many_arguments,
    reason = "two sides with an index each, the operator, the nulls, the type of the answer and \
              the two vectors the error message needs, none of which is worth a struct that \
              exists for three calls"
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
    returns: &LogicalType,
    written: Written<'_>,
) -> Result<Option<Vector>>
where
    L: Fn(usize) -> usize,
    R: Fn(usize) -> usize,
{
    let LogicalType::Decimal { width, scale } = *returns else {
        return Ok(None);
    };
    let (Some((_, left_scale)), Some((_, right_scale))) =
        (left.logical_type().decimal_shape(), right.logical_type().decimal_shape())
    else {
        return Ok(None);
    };
    let held = left_scale.saturating_add(right_scale);
    // Everything but a product wants both sides at the answer's scale, which is what the binder
    // casts them to. Anything else goes to the row at a time path, which rescales as it reads.
    if !matches!(op, Op::Multiply) && (left_scale != scale || right_scale != scale) {
        return Ok(None);
    }
    let rows = left.len();
    let guarding = matches!(op, Op::Divide | Op::Modulo);
    // The optimistic pass first, and the careful loop below only for what it hands back.
    if let Some(answer) =
        decimal_sweep(one, &at_left, other, &at_right, op, base, returns, width, scale, held, rows)?
    {
        return Ok(Some(answer));
    }
    // What the answer's width will not hold, worked out once for the whole run rather than by
    // counting the digits of every row. See [`number::beyond`], which is the whole argument.
    let limit = beyond(width);
    macro_rules! runs {
        ($($variant:ident => $native:ty),+ $(,)?) => {
            $(
                if let (Data::$variant(a), Data::$variant(b)) = (one, other) {
                    let mut out = vec![0 as $native; rows];
                    let validity = over_valid(rows, base.clone(), |index| {
                        let x = i128::from(a[at_left(index)]);
                        let y = i128::from(b[at_right(index)]);
                        if guarding && y == 0 {
                            return Err(divided_by_zero(
                                written,
                                op.symbol(),
                                &left.value_at(index),
                                &right.value_at(index),
                            ));
                        }
                        let unscaled = match op {
                            Op::Add => x.checked_add(y),
                            Op::Subtract => x.checked_sub(y),
                            Op::Multiply => {
                                x.checked_mul(y).and_then(|wide| rescale(wide, held, scale))
                            }
                            Op::Modulo => x.checked_rem(y),
                            // Nothing reaches this. `/` promotes both sides to DOUBLE and `//`
                            // does the same the moment a side is a decimal, which is DuckDB's
                            // rule and was measured, so a decimal run is never divided as a
                            // decimal. It is here because the match is over every operator.
                            Op::Divide => {
                                x.checked_div(y).and_then(|whole| whole.checked_mul(pow10(scale)))
                            }
                        };
                        let fits = unscaled
                            .filter(|value| value.unsigned_abs() < limit)
                            .and_then(|value| <$native>::try_from(value).ok());
                        match fits {
                            Some(answer) => {
                                out[index] = answer;
                                Ok(())
                            }
                            None => Err(overflow(
                                op,
                                returns,
                                &left.value_at(index),
                                &right.value_at(index),
                            )),
                        }
                    })?;
                    return finish(returns, Data::$variant(out.into()), validity);
                }
            )+
        };
    }
    runs!(Int16 => i16, Int32 => i32, Int64 => i64, Int128 => i128);
    Ok(None)
}

/// Adding, subtracting and multiplying a decimal in one pass on the run's own width.
///
/// The loop above widens every row to `i128`, computes, rescales, asks whether the answer fits and
/// asks the vector for the two operands as `Value`s the moment it does not, and it does all of that
/// inside a closure returning a `Result`, which is a shape nothing vectorizes and a compiler will
/// not unroll. That was written on the reading that decimal arithmetic is rare on a scan. It is not
/// rare on this workload: `l_extendedprice * (1 - l_discount)` is the argument of an aggregate in
/// eight of the twenty two TPC-H queries, so the pass runs over every lineitem row those queries
/// read, twice.
///
/// So the three operators that are one native instruction get the same treatment the integers get
/// in [`fast_runs`]: compute at the run's own width, accumulate whether any row went wrong rather
/// than branching on it, and hand the whole vector back to the loop above the moment one did. That
/// loop then produces the right answer or the right error message, so nothing here has to know how
/// to say what went wrong.
///
/// What "went wrong" means is two things at once. The native operation can wrap, and the answer can
/// be wider than the answer's type allows. Both have to be caught, and the first has to be caught
/// even though the second is the one the careful loop is checking, because a wrapped product is a
/// small number that would sail through a range check. The range is a pair of constants worked out
/// once per run, rather than [`beyond`]'s `u128` compared against a widened row, because the
/// answer's width always fits the run it is stored in: four digits in an `i16`, nine in an `i32`,
/// eighteen in an `i64` and thirty eight in an `i128` are each inside the type by a factor of at
/// least one and a half.
///
/// Dividing keeps the careful loop because a zero divisor is an error rather than an answer, and a
/// product whose operand scales do not already add up to the answer's keeps it because the rescale
/// is a division per row and the careful loop is already the place division lives.
#[expect(
    clippy::too_many_arguments,
    reason = "two sides with an index each, the operator, the nulls, and the four numbers \
              describing the answer, none of which is worth a struct that exists for one call"
)]
fn decimal_sweep<L, R>(
    one: &Data,
    at_left: L,
    other: &Data,
    at_right: R,
    op: Op,
    base: &Validity,
    returns: &LogicalType,
    width: u8,
    scale: u8,
    held: u8,
    rows: usize,
) -> Result<Option<Vector>>
where
    L: Fn(usize) -> usize,
    R: Fn(usize) -> usize,
{
    match op {
        Op::Add | Op::Subtract => {}
        Op::Multiply if held == scale => {}
        _ => return Ok(None),
    }
    macro_rules! runs {
        ($($variant:ident => $native:ty),+ $(,)?) => {
            $(
                if let (Data::$variant(a), Data::$variant(b)) = (one, other) {
                    // Refused rather than asserted, since a width the run cannot hold is a bug
                    // elsewhere and the careful loop answers it correctly either way.
                    let Some(cap) = <$native>::try_from(pow10(width)).ok() else {
                        return Ok(None);
                    };
                    let checked = |value: $native, overflowed: bool| {
                        (value, overflowed || value >= cap || value <= -cap)
                    };
                    let mut out = vec![0 as $native; rows];
                    let trouble = match op {
                        Op::Add => sweep(&mut out, a, &at_left, b, &at_right, |x, y| {
                            let (value, overflowed) = <$native>::overflowing_add(x, y);
                            checked(value, overflowed)
                        }),
                        Op::Subtract => sweep(&mut out, a, &at_left, b, &at_right, |x, y| {
                            let (value, overflowed) = <$native>::overflowing_sub(x, y);
                            checked(value, overflowed)
                        }),
                        Op::Multiply => sweep(&mut out, a, &at_left, b, &at_right, |x, y| {
                            let (value, overflowed) = <$native>::overflowing_mul(x, y);
                            checked(value, overflowed)
                        }),
                        // Never reached, because the match above sent them away already.
                        Op::Divide | Op::Modulo => true,
                    };
                    if trouble {
                        return Ok(None);
                    }
                    blank(&mut out, base);
                    // What [`over_valid`] returns for the same run, so that the two paths cannot
                    // produce vectors that differ in how they spell the same nulls.
                    let validity = if rows == 0 {
                        Validity::AllValid
                    } else {
                        base.clone().normalize(rows)
                    };
                    return finish(returns, Data::$variant(out.into()), validity);
                }
            )+
        };
    }
    runs!(Int16 => i16, Int32 => i32, Int64 => i64, Int128 => i128);
    Ok(None)
}

/// One float operation, in the one place, so that the two paths cannot disagree.
///
/// `Op::Divide` gets here from `//`, and it does not truncate. `//` is integer division only when
/// there are integers on both sides of it: `7.0 // 2.0` is 3.5 on the pinned binary and
/// `7.9 // 1.0` is 7.9, so once a side is a float it is `/` under another spelling. The truncation
/// that is left is the one the integer path does by dividing integers.
fn float_step(op: Op, x: f64, y: f64) -> f64 {
    match op {
        Op::Add => x + y,
        Op::Subtract => x - y,
        Op::Multiply => x * y,
        Op::Divide => x / y,
        Op::Modulo => x % y,
    }
}

/// `/`, which the binder has already promoted both sides to `DOUBLE` for.
fn slash_of(left: &Vector, right: &Vector, returns: &LogicalType) -> Result<Option<Vector>> {
    if !matches!(returns, LogicalType::Float | LogicalType::Double)
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
    let rows = left.len();
    let base = nulls_of(left).and(&nulls_of(right), rows);
    macro_rules! slash {
        ($variant:ident, $native:ty) => {
            if let (Data::$variant(a), Data::$variant(b)) = (one, other) {
                let mut out = vec![0.0 as $native; rows];
                let validity = over_valid(rows, base, |index| {
                    out[index] = a[at_left(index)] / b[at_right(index)];
                    Ok(())
                })?;
                return finish(returns, Data::$variant(out.into()), validity);
            }
        };
    }
    slash!(Float32, f32);
    slash!(Float64, f64);
    Ok(None)
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
///
/// The text is read through an index mapping and not straight out of a slice, for the reason given
/// on `by_form!`: a dictionary encoded column is the normal shape of a string column read out of
/// Parquet, and every string column in the published ClickBench file is one. Asking for a flat
/// vector here meant the real file never reached this loop at all. That was #288, and it cost six
/// times the whole predicate.
fn like_of(
    name: &str,
    prepared: Option<&Like>,
    text: &Vector,
    pattern: &Vector,
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    if !matches!(returns, LogicalType::Boolean) {
        return Ok(None);
    }
    // A pipeline that was built from a plan has already compiled this, and one that was not
    // compiles it here for the whole vector, which is still the difference between ClickBench
    // query 21 doing a substring search per row and doing four allocations and a backtracking walk.
    let held;
    let like = match prepared {
        Some(like) => like,
        None => {
            let Some(Value::Varchar(spelling)) = pattern.constant_value() else {
                return Ok(None);
            };
            let Some(built) = Like::of(name, spelling) else {
                return Ok(None);
            };
            held = built;
            &held
        }
    };
    let base = nulls_of(text).and(&nulls_of(pattern), rows);
    if let Some((codes, dictionary)) = text.stable_dictionary_parts() {
        return like_stable(dictionary, codes, like, base, rows, returns);
    }
    match text.form() {
        Form::Flat => {
            let Some(Data::Varlen(column)) = text.data() else {
                return Ok(None);
            };
            like_run(column, identity, like, base, rows, returns)
        }
        Form::Dictionary | Form::Rle => {
            let Some((codes, values)) = text.positions() else {
                return Ok(None);
            };
            if codes.len() < rows {
                return Ok(None);
            }
            let Some(Data::Varlen(column)) = values.data() else {
                return like_vector_run(values, &codes, like, base, rows, returns);
            };
            // Every code is inside the dictionary because `Vector::dictionary` checks that on the
            // way in, so the gather indexes without a bound of its own.
            if column.len() < rows {
                return like_over(column, &codes, like, base, rows, returns);
            }
            let at = move |index: usize| codes[index] as usize;
            like_run(column, at, like, base, rows, returns)
        }
        // There is no constant arm and there cannot be a useful one. This function needs the pattern
        // to be constant to get this far, so a constant text here would be a call whose every
        // argument is constant, and `call` answers one of those in a single row before it reaches
        // any of this.
        _ => Ok(None),
    }
}

/// A `LIKE` whose pattern has been looked at, which is everything about the call bar the text.
///
/// This is what [`crate::prepare`] lifts out of the per chunk path. It is small, and what it saves
/// per chunk is the walk over the spelling, the four allocations the walk can make and, for the
/// case folding spellings, a `to_lowercase` of the pattern.
#[derive(Debug)]
pub(crate) struct Like {
    /// The pattern, already folded to lower case where `fold_case` is set.
    compiled: Pattern,
    /// Whether the match ignores case, which is the `*` in the spelling.
    fold_case: bool,
    /// Whether the answer is inverted, which is the `!` in the spelling.
    negated: bool,
    stable: OnceLock<StableLike>,
}

/// How many dictionary values one decision of the stable memo covers.
///
/// It is what the native format puts in a payload block, so deciding a group reads one block of the
/// dictionary and reads all of it, in the order it decodes to. A dictionary that groups its values
/// some other way still gets the locality, since what this is really buying is a run of consecutive
/// values rather than a scatter, and it is a whole number of words either way.
const LIKE_GROUP: usize = 1024;

/// A `LIKE` answered once per distinct value, for a dictionary that outlives the chunk.
///
/// The memo used to be a byte a value holding unknown, no or yes, and the row loop probed it at a
/// random position once a row. On ClickBench `URL` that is a byte array of 18.3 million probed a
/// hundred million times, and eighteen megabytes across a dozen threads does not stay in the last
/// level cache, so the loop was paying a miss a row to answer a question it had already answered.
/// The substring search was not the cost: the same query with a pattern that matches nothing, and
/// the same query written as a prefix, both cost what `%google%` costs to the hundredth of a
/// second.
///
/// So the memo is two bits a value, one saying the value has been decided and one holding what was
/// decided, which is 4.6 MB for `URL` against the eighteen a byte a value took. Both bits live in
/// the same word of thirty two values, so the row loop reads one word and reads it once, and a
/// reader that sees the decided bit is looking at the answer that was written with it.
///
/// The memo also decides a whole group at a time rather than a value at a time. Deciding a group is
/// a walk over 1,024 consecutive dictionary values, which is one payload block read in the order it
/// decodes to rather than a scatter across a gigabyte of them.
///
/// A group is only worth deciding whole where the chunk asking is a scan rather than what a
/// selective filter left behind. A chunk of ten rows that happen to point at ten distant values has
/// no use for the ten thousand values either side of them, so it decides the ten. The rule is the
/// chunk holding at least a group's worth of rows, which is a guess about intent and not a fact
/// about the query, and it is wrong in the cheap direction: deciding one value at a time is what
/// this always did.
///
/// Two threads can decide the same value or the same group at the same time. They walk the same
/// values with the same pattern and reach the same answer, so the race is benign: the group write
/// and the single value write agree wherever they overlap, and neither can clear a bit the other
/// set. A decision is one `fetch_or` carrying both bits, released by the thread that made it and
/// acquired by the thread that skips the walk because of it.
#[derive(Debug)]
struct StableLike {
    dictionary: Arc<Vector>,
    /// Two bits a value, the low one of the pair saying the value was decided and the high one
    /// saying it matched, packed thirty two values to the word.
    state: Vec<AtomicU64>,
}

/// How many dictionary values one word of the memo holds, at two bits each.
const MEMO_VALUES: usize = 32;

impl StableLike {
    /// The word `code` lives in and how far into it the pair of bits sits.
    fn slot(code: usize) -> (usize, usize) {
        (code / MEMO_VALUES, code % MEMO_VALUES * 2)
    }

    /// Whether `code` matches, or `None` where nothing has decided it yet.
    ///
    /// One load for both bits, which is the whole point of packing them together: this runs once a
    /// row over a memo too big to stay in the last level cache, so a second array would be a second
    /// miss for a question the first one already answered.
    fn peek(&self, code: usize) -> Option<bool> {
        let (index, shift) = Self::slot(code);
        let word = self.state.get(index)?.load(Ordering::Acquire);
        ((word >> shift) & 1 == 1).then(|| (word >> (shift + 1)) & 1 == 1)
    }

    /// Decides one value, which is what a chunk too small to be a scan asks for.
    fn decide_one(&self, code: usize, like: &Like, characters: &mut Vec<char>) -> Result<()> {
        let held = like.holds_vector(&self.dictionary, code, characters)?;
        let (index, shift) = Self::slot(code);
        self.word(index)?.fetch_or((1 | u64::from(held) << 1) << shift, Ordering::Release);
        Ok(())
    }

    /// Decides every value of the group holding `code`, which reads one payload block in order.
    ///
    /// The walk goes through [`Vector::sweep_text`] rather than reading a value at a time, and that
    /// is the whole reason the group exists. A dictionary that reads out of a file decodes a block
    /// to answer for any value in it, and the reader that answers one value at a time has to keep
    /// every block it decoded, so a walk of the whole dictionary ends up holding the whole
    /// dictionary decoded: 4.2 GB for ClickBench `URL`, none of it read twice. A sweep hands the
    /// block over for the length of the call and drops it, so what is left behind is the two bits a
    /// value written here.
    ///
    /// The answers go into a buffer and are written a word at a time at the end rather than as they
    /// are decided, because a sweep may hand over less than a group at a time and the buffer is 256
    /// bytes. Nothing reads a bit before it is written, since a thread that finds the value
    /// undecided walks it itself.
    ///
    /// Two threads landing on the same group now decode the same block twice where one used to
    /// decode it and the other wait on the lock behind it. That is the trade and it is a good one:
    /// a scan hands each thread its own parts and the dictionary has seventeen thousand blocks, so
    /// the collision is rare, and what it costs when it happens is one block decoded twice rather
    /// than every block kept for the length of the query.
    fn decide_group(&self, code: usize, like: &Like, characters: &mut Vec<char>) -> Result<()> {
        let first = code / LIKE_GROUP * LIKE_GROUP;
        let last = (first + LIKE_GROUP).min(self.dictionary.len());
        if !like.fold_case {
            if let Pattern::Contains(finder) = &like.compiled {
                if finder.needle().len() >= 4
                    && !self.dictionary.text_block_might_contain(first, finder.needle())
                {
                    // A stored signature can only prove absence. Mark the whole group as decided,
                    // with the negated answer when this is NOT LIKE, without decoding its payload.
                    let word = if like.negated { u64::MAX } else { 0x5555_5555_5555_5555 };
                    for step in 0..(last - first).div_ceil(MEMO_VALUES) {
                        let remaining = (last - first - step * MEMO_VALUES).min(MEMO_VALUES);
                        let mask = if remaining == MEMO_VALUES {
                            u64::MAX
                        } else {
                            (1_u64 << (remaining * 2)) - 1
                        };
                        self.word(first / MEMO_VALUES + step)?
                            .fetch_or(word & mask, Ordering::Release);
                    }
                    return Ok(());
                }
            }
        }
        let mut bits = [0_u64; LIKE_GROUP / MEMO_VALUES];
        let mut at = first;
        while at < last {
            let stopped =
                self.dictionary.sweep_text(at, last, &mut |index: usize, text: &[u8]| {
                    let held = like.holds_loan(text, characters)?;
                    let (word, shift) = Self::slot(index - first);
                    bits[word] |= (1 | u64::from(held) << 1) << shift;
                    Ok(())
                })?;
            if stopped <= at {
                return Err(Error::internal("a dictionary sweep did not move"));
            }
            at = stopped;
        }
        for (step, word) in bits.iter().take((last - first).div_ceil(MEMO_VALUES)).enumerate() {
            self.word(first / MEMO_VALUES + step)?.fetch_or(*word, Ordering::Release);
        }
        Ok(())
    }

    /// The word of the memo at `index`.
    fn word(&self, index: usize) -> Result<&AtomicU64> {
        self.state
            .get(index)
            .ok_or_else(|| Error::internal("a stable dictionary code is out of range"))
    }
}

impl Like {
    /// The call `name` spells against `spelling`, or `None` when the name is not a `LIKE`.
    pub(crate) fn of(name: &str, spelling: &str) -> Option<Self> {
        let (fold_case, negated) = match name {
            "~~" => (false, false),
            "!~~" => (false, true),
            "~~*" => (true, false),
            "!~~*" => (true, true),
            _ => return None,
        };
        // Folding the pattern here rather than per chunk is the same answer, because the loop folds
        // the text on both paths and a fold of a fold is a fold.
        let folded;
        let spelling = if fold_case {
            folded = spelling.to_lowercase();
            &folded
        } else {
            spelling
        };
        Some(Self {
            compiled: Pattern::compile(spelling),
            fold_case,
            negated,
            stable: OnceLock::new(),
        })
    }

    /// Whether the string at `position` matches, negation included.
    ///
    /// One place rather than one per loop, because the two loops below differ in what they walk and
    /// not in what they decide, and `characters` is the buffer the general walk reuses so that a
    /// chunk costs one allocation and not one per value.
    fn holds_at(&self, column: &StringColumn, position: usize, characters: &mut Vec<char>) -> bool {
        if !self.fold_case && !matches!(self.compiled, Pattern::General(_)) {
            let text = column.bytes(position).unwrap_or_default();
            return self.compiled.holds_bytes(text) != self.negated;
        }
        let text = column.get(position).unwrap_or_default();
        // `str::to_lowercase` and not a character by character fold, for the same reason the
        // `lower` kernel uses it: the two functions disagree about a final sigma, and the oracle
        // this is checked against calls the string one.
        let folded = if self.fold_case { Some(text.to_lowercase()) } else { None };
        let text = folded.as_deref().unwrap_or(text);
        self.compiled.holds(text, characters) != self.negated
    }

    fn holds_vector(
        &self,
        vector: &Vector,
        position: usize,
        characters: &mut Vec<char>,
    ) -> Result<bool> {
        if !self.fold_case && !matches!(self.compiled, Pattern::General(_)) {
            let text = vector.try_bytes_at(position)?.unwrap_or_default();
            return Ok(self.compiled.holds_bytes(text) != self.negated);
        }
        let text = vector.try_text_at(position)?.unwrap_or_default();
        let folded = if self.fold_case { Some(text.to_lowercase()) } else { None };
        let text = folded.as_deref().unwrap_or(text);
        Ok(self.compiled.holds(text, characters) != self.negated)
    }

    /// The same answer for a value already in hand rather than one to be read out of a vector.
    ///
    /// What a sweep hands over is bytes on loan, so this is [`Self::holds_vector`] with the read
    /// taken out of it, including the reason the two halves are there: a pattern that neither folds
    /// case nor needs characters never looks at whether the bytes are UTF-8, and the two that do
    /// raise the same conversion error reading a value out of a vector raises.
    fn holds_loan(&self, text: &[u8], characters: &mut Vec<char>) -> Result<bool> {
        if !self.fold_case && !matches!(self.compiled, Pattern::General(_)) {
            return Ok(self.compiled.holds_bytes(text) != self.negated);
        }
        let text = std::str::from_utf8(text)
            .map_err(|error| Error::conversion(format!("invalid UTF-8 in VARCHAR: {error}")))?;
        let folded = if self.fold_case { Some(text.to_lowercase()) } else { None };
        let text = folded.as_deref().unwrap_or(text);
        Ok(self.compiled.holds(text, characters) != self.negated)
    }
}

/// The `LIKE` loop itself, once per form the text can arrive in.
///
/// The mapping is a generic parameter for the reason spelled out on [`by_form`], which is that a
/// function pointer here is an indirect call per row and this loop is one of the two or three that
/// ClickBench spends real time in.
fn like_run<A: Fn(usize) -> usize>(
    column: &StringColumn,
    at: A,
    like: &Like,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let mut out = vec![false; rows];
    let mut characters: Vec<char> = Vec::new();
    let validity = over_valid(rows, base, |index| {
        out[index] = like.holds_at(column, at(index), &mut characters);
        Ok(())
    })?;
    finish(returns, Data::Bool(out.into()), validity)
}

fn like_vector_run(
    values: &Vector,
    codes: &[u32],
    like: &Like,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let mut out = vec![false; rows];
    let mut characters = Vec::new();
    let validity = over_valid(rows, base, |index| {
        out[index] = like.holds_vector(values, codes[index] as usize, &mut characters)?;
        Ok(())
    })?;
    finish(returns, Data::Bool(out.into()), validity)
}

fn like_stable(
    dictionary: &Arc<Vector>,
    codes: &[u32],
    like: &Like,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let cache = like.stable.get_or_init(|| {
        let words = dictionary.len().div_ceil(MEMO_VALUES);
        StableLike {
            dictionary: Arc::clone(dictionary),
            state: (0..words).map(|_| AtomicU64::new(0)).collect(),
        }
    });
    if !Arc::ptr_eq(&cache.dictionary, dictionary) {
        return like_vector_run(dictionary, codes, like, base, rows, returns);
    }
    let bulk = rows >= LIKE_GROUP;
    let mut out = vec![false; rows];
    let mut characters = Vec::new();
    let validity = over_valid(rows, base, |index| {
        let code = codes[index] as usize;
        out[index] = match cache.peek(code) {
            Some(held) => held,
            None => {
                if bulk {
                    cache.decide_group(code, like, &mut characters)?;
                } else {
                    cache.decide_one(code, like, &mut characters)?;
                }
                cache
                    .peek(code)
                    .ok_or_else(|| Error::internal("a stable dictionary code is out of range"))?
            }
        };
        Ok(())
    })?;
    finish(returns, Data::Bool(out.into()), validity)
}

/// The same loop over a dictionary, answering once per distinct value instead of once per row.
///
/// A substring search is the most expensive thing any of these kernels does per value, and a
/// dictionary is a promise that the same value turns up again. ClickBench reads `URL` and `Title`
/// against a dictionary of a few tens of thousands over chunks of a million, so this is the same
/// answer for a fraction of the searches, and it is the difference between a filter that is linear
/// in rows and one that is linear in distinct values.
///
/// The caller only sends a chunk here when the dictionary is smaller than the chunk, because a
/// dictionary with more entries than the rows that point at it would have this searching values
/// nobody asked about.
fn like_over(
    column: &StringColumn,
    codes: &[u32],
    like: &Like,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let mut characters: Vec<char> = Vec::new();
    let answer: Vec<bool> =
        (0..column.len()).map(|value| like.holds_at(column, value, &mut characters)).collect();
    let mut out = vec![false; rows];
    let validity = over_valid(rows, base, |index| {
        // Every code is inside the dictionary for the reason the caller gives, so this indexes a
        // vector built to the dictionary's length without a bound of its own.
        out[index] = answer[codes[index] as usize];
        Ok(())
    })?;
    finish(returns, Data::Bool(out.into()), validity)
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
    ///
    /// The searcher is built here and not at the row, because `memmem::find` builds one out of the
    /// needle before it looks at the haystack: it picks the two rarest bytes and works out the
    /// stride from them. That is a few hundred instructions, which is more than searching a URL
    /// with a searcher that already exists, and a scan was paying it once a row.
    ///
    /// Boxed because a `Finder` is 288 bytes on x86_64, where it carries the packed pair prefilter,
    /// and every other arm of this is a `String`. Without the box the enum is 288 bytes wherever it
    /// is held, including inside the prepared expression tree, and one allocation per `LIKE` in a
    /// query is the cheaper of the two. It is 24 bytes on aarch64, which is why this only shows up
    /// when the build is on a Linux machine.
    Contains(Box<memmem::Finder<'static>>),
    /// Literal text with `%` between the pieces and no `_` anywhere, such as `%a%b%` or `a%b%c`.
    ///
    /// This is the shape the four above are the short cases of, and it covers the rest of them:
    /// `o_comment NOT LIKE '%special%requests%'` in TPC-H q13, and every `LIKE 'a%b%'` somebody
    /// writes to mean two things in order.
    ///
    /// Boxed for the reason [`Self::Contains`] is boxed, since it holds finders of its own.
    Segments(Box<Split>),
    /// Anything else, walked with one backtracking point.
    General(Vec<char>),
}

/// A `LIKE` pattern cut at its `%` signs.
///
/// The first piece is anchored at the start of the text and the last at the end, and the pieces
/// between them are looked for in order in what is left over. The two ends may be empty, which is
/// what a pattern starting or ending with `%` gives, and then that end is not anchored at all.
#[derive(Debug)]
struct Split {
    prefix: String,
    suffix: String,
    middles: Vec<memmem::Finder<'static>>,
}

impl Split {
    /// Whether the text matches, on bytes and without backtracking.
    ///
    /// Taking the leftmost occurrence of each middle piece is the right answer and not a guess.
    /// The pieces are separated by `%`, which stands for any run at all, so a match that puts a
    /// piece later can always be rewritten to put it earlier without disturbing the pieces before
    /// it, and leaving the most text over for the pieces after it can only help them. That is the
    /// whole of why this does not need the backtracking point [`like`] keeps: a `_` would break the
    /// argument, because then the run between two pieces has a length to satisfy, which is why
    /// [`Pattern::compile`] only builds this for a spelling with no `_` in it.
    ///
    /// Bytes rather than characters for the reason given on [`Pattern`]: a byte substring of valid
    /// UTF-8 found in valid UTF-8 starts and ends on a character boundary, because the leading byte
    /// of a sequence cannot appear inside another one.
    fn holds(&self, text: &[u8]) -> bool {
        if !text.starts_with(self.prefix.as_bytes()) || !text.ends_with(self.suffix.as_bytes()) {
            return false;
        }
        // The two ends are matched against the same text and may have found the same bytes, which
        // the pattern does not allow: there is a `%` between them and so they sit side by side at
        // the closest. A text shorter than the two of them together is the case where they did.
        let Some(end) = text.len().checked_sub(self.suffix.len()) else {
            return false;
        };
        if self.prefix.len() > end {
            return false;
        }
        let mut rest = &text[self.prefix.len()..end];
        for finder in &self.middles {
            let Some(at) = finder.find(rest) else {
                return false;
            };
            rest = &rest[at + finder.needle().len()..];
        }
        true
    }
}

impl Pattern {
    fn compile(spelling: &str) -> Self {
        let plain = |text: &str| !text.contains('%') && !text.contains('_');
        if plain(spelling) {
            return Self::Exact(spelling.to_owned());
        }
        if let Some(inner) = spelling.strip_prefix('%').and_then(|rest| rest.strip_suffix('%')) {
            if plain(inner) {
                return Self::Contains(Box::new(memmem::Finder::new(inner).into_owned()));
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
        // Everything left that has no `_` in it is literal text with `%` between the pieces, since
        // the four shapes above are the cases of that with one piece or two. A spelling with no `%`
        // either went to `Exact` already.
        if !spelling.contains('_') {
            let mut pieces: Vec<&str> = spelling.split('%').collect();
            if pieces.len() >= 2 {
                let suffix = pieces.pop().unwrap_or_default().to_owned();
                let prefix = pieces.remove(0).to_owned();
                // An empty piece is what two `%` in a row give, and two in a row mean what one
                // means, so dropping it is the same pattern with one fewer search per row.
                let middles = pieces
                    .into_iter()
                    .filter(|piece| !piece.is_empty())
                    .map(|piece| memmem::Finder::new(piece).into_owned())
                    .collect();
                return Self::Segments(Box::new(Split { prefix, suffix, middles }));
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
            Self::Contains(finder) => finder.find(text.as_bytes()).is_some(),
            Self::Segments(split) => split.holds(text.as_bytes()),
            Self::General(against) => {
                characters.clear();
                characters.extend(text.chars());
                like(characters, against)
            }
        }
    }

    fn holds_bytes(&self, text: &[u8]) -> bool {
        match self {
            Self::Exact(against) => text == against.as_bytes(),
            Self::Prefix(against) => text.starts_with(against.as_bytes()),
            Self::Suffix(against) => text.ends_with(against.as_bytes()),
            Self::Contains(finder) => finder.find(text).is_some(),
            Self::Segments(split) => split.holds(text),
            Self::General(_) => false,
        }
    }
}

/// `date_part` and `date_trunc`, against a part that is the same on every row.
///
/// The part is a string literal in every query anybody writes, and it is the whole of what the loop
/// would otherwise have to decide, so it is read once per vector. What is left per row is one
/// division or one call into the calendar, over a run of `i32` days or `i64` microseconds.
///
/// ClickBench query 43 groups a hundred million rows by `DATE_TRUNC('minute', EventTime)`, so this
/// is a loop whose shape shows up in a number somebody publishes.
///
/// The days or the microseconds are read through an index mapping for the same reason `unary` reads
/// its argument through one, which is #288: a date column out of Parquet can be dictionary encoded
/// and a date column that a filter has been through usually is.
fn date_of(
    name: &str,
    spec: &Vector,
    when: &Vector,
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    let Some(Value::Varchar(spelling)) = spec.constant_value() else {
        return Ok(None);
    };
    let truncating = name == "date_trunc";
    // A truncation keeps the type it was given, and a part is a bigint or the double the two
    // fractional parts make it. Anything else is a cast the binder put there, which the row at a
    // time path handles and counts.
    if truncating {
        if returns != when.logical_type() {
            return Ok(None);
        }
    } else if !matches!(returns, LogicalType::BigInt | LogicalType::Double) {
        return Ok(None);
    }
    let part = Part::parse(spelling)?;
    // An interval has no calendar in it, so it has fewer parts than a date does, and the check is
    // here rather than in the loop because it is the same answer on every row.
    let part = match when.logical_type() {
        LogicalType::Interval if !truncating => part.of_an_interval(spelling)?,
        _ => part,
    };
    let base = nulls_of(when).and(&nulls_of(spec), rows);
    match when.form() {
        Form::Flat => {
            let Some(data) = when.data() else {
                return Ok(None);
            };
            date_runs(part, data, identity, base, rows, returns, when, truncating)
        }
        Form::Dictionary | Form::Rle => {
            let Some((codes, values)) = when.positions() else {
                return Ok(None);
            };
            if codes.len() < rows {
                return Ok(None);
            }
            let Some(data) = values.data() else {
                return Ok(None);
            };
            let at = move |index: usize| codes[index] as usize;
            date_runs(part, data, at, base, rows, returns, when, truncating)
        }
        // No constant arm, because the part is constant in every query that reaches this at all and
        // a constant date under a constant part is one row of work that `call` has already done.
        _ => Ok(None),
    }
}

/// The four `date_part` and `date_trunc` loops, once the form has been turned into a mapping.
#[expect(
    clippy::too_many_arguments,
    reason = "the part, the days or microseconds and their mapping, the nulls, the row count, the \
              type of the answer, the vector whose type picks the arm and which of the two \
              functions this is"
)]
fn date_runs<A: Fn(usize) -> usize>(
    part: Part,
    data: &Data,
    at: A,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
    when: &Vector,
    truncating: bool,
) -> Result<Option<Vector>> {
    // A part that is read rather than truncated comes back as the type the binder decided, which is
    // a double when the part carries a fraction and a double as well when the specifier was not a
    // constant it could look at. So the reading arms come in pairs.
    let doubled = *returns == LogicalType::Double && !truncating;
    match (when.logical_type(), data, truncating) {
        (LogicalType::Date, Data::Int32(days), false) if doubled => {
            let mut out = vec![0f64; rows];
            let validity = over_valid(rows, base, |index| {
                out[index] = part.double_of_days(days[at(index)])?;
                Ok(())
            })?;
            finish(returns, Data::Float64(out.into()), validity)
        }
        (LogicalType::Timestamp, Data::Int64(micros), false) if doubled => {
            let mut out = vec![0f64; rows];
            let validity = over_valid(rows, base, |index| {
                out[index] = part.double_of_micros(micros[at(index)])?;
                Ok(())
            })?;
            finish(returns, Data::Float64(out.into()), validity)
        }
        (LogicalType::Interval, Data::Interval(fields), false) if doubled => {
            let mut out = vec![0f64; rows];
            let validity = over_valid(rows, base, |index| {
                let (months, days, micros) = fields[at(index)];
                out[index] = part.double_of_interval(months, days, micros)?;
                Ok(())
            })?;
            finish(returns, Data::Float64(out.into()), validity)
        }
        (LogicalType::Date, Data::Int32(days), false) => {
            let mut out = vec![0i64; rows];
            let validity = over_valid(rows, base, |index| {
                out[index] = part.of_days(days[at(index)])?;
                Ok(())
            })?;
            finish(returns, Data::Int64(out.into()), validity)
        }
        (LogicalType::Date, Data::Int32(days), true) => {
            let mut out = vec![0i32; rows];
            let validity = over_valid(rows, base, |index| {
                out[index] = part.truncate_days(days[at(index)])?;
                Ok(())
            })?;
            finish(returns, Data::Int32(out.into()), validity)
        }
        (LogicalType::Timestamp, Data::Int64(micros), false) => {
            let mut out = vec![0i64; rows];
            let validity = over_valid(rows, base, |index| {
                out[index] = part.of_micros(micros[at(index)])?;
                Ok(())
            })?;
            finish(returns, Data::Int64(out.into()), validity)
        }
        (LogicalType::Timestamp, Data::Int64(micros), true) => {
            let mut out = vec![0i64; rows];
            let validity = over_valid(rows, base, |index| {
                out[index] = part.truncate_micros(micros[at(index)])?;
                Ok(())
            })?;
            finish(returns, Data::Int64(out.into()), validity)
        }
        (LogicalType::Interval, Data::Interval(fields), false) => {
            let mut out = vec![0i64; rows];
            let validity = over_valid(rows, base, |index| {
                let (months, days, micros) = fields[at(index)];
                out[index] = part.of_interval(months, days, micros)?;
                Ok(())
            })?;
            finish(returns, Data::Int64(out.into()), validity)
        }
        (LogicalType::Interval, Data::Interval(fields), true) => {
            let mut out = vec![(0i32, 0i32, 0i64); rows];
            let validity = over_valid(rows, base, |index| {
                let (months, days, micros) = fields[at(index)];
                out[index] = part.truncate_interval(months, days, micros)?;
                Ok(())
            })?;
            finish(returns, Data::Interval(out.into()), validity)
        }
        _ => Ok(None),
    }
}

/// `date_part` and `date_trunc` on one row.
///
/// The answer type is passed in rather than worked out here, because a part that is read is a
/// bigint or a double and which one it is was decided at binding time. See `narrowed_part` in
/// `rudb-bind`.
fn date_value(name: &str, spec: &Value, when: &Value, returns: &LogicalType) -> Result<Value> {
    let Value::Varchar(spelling) = spec else {
        return Err(Error::internal(format!("{name} of a {} part", spec.logical_type())));
    };
    let part = Part::parse(spelling)?;
    let doubled = *returns == LogicalType::Double;
    match (name == "date_trunc", when) {
        (false, Value::Date(days)) if doubled => part.double_of_days(*days).map(Value::Double),
        (false, Value::Timestamp(micros) | Value::TimestampTz(micros)) if doubled => {
            part.double_of_micros(*micros).map(Value::Double)
        }
        (false, Value::Interval { months, days, micros }) if doubled => part
            .of_an_interval(spelling)?
            .double_of_interval(*months, *days, *micros)
            .map(Value::Double),
        (false, Value::Date(days)) => part.of_days(*days).map(Value::BigInt),
        (false, Value::Timestamp(micros) | Value::TimestampTz(micros)) => {
            part.of_micros(*micros).map(Value::BigInt)
        }
        (false, Value::Interval { months, days, micros }) => {
            part.of_an_interval(spelling)?.of_interval(*months, *days, *micros).map(Value::BigInt)
        }
        (true, Value::Date(days)) => part.truncate_days(*days).map(Value::Date),
        (true, Value::Timestamp(micros)) => part.truncate_micros(*micros).map(Value::Timestamp),
        // The truncation comes back zoned, because `date_trunc` answers the type it was handed and
        // the plan holds that type next to the value. Which moment it lands on is the calendar's
        // question and so is the session time zone's, the same as the shift in `datetime`.
        (true, Value::TimestampTz(micros)) => part.truncate_micros(*micros).map(Value::TimestampTz),
        (true, Value::Interval { months, days, micros }) => {
            let (months, days, micros) = part.truncate_interval(*months, *days, *micros)?;
            Ok(Value::Interval { months, days, micros })
        }
        // DuckDB has overloads for a time and for a timestamp with a time zone as well, and refuses
        // anything else at binding. This refuses the same set a step later, because the signature
        // table has one row per name and no way to say which types the row accepts.
        _ => Err(Error::binder(format!(
            "No function matches the given name and argument types '{name}(VARCHAR, {})'. You might need to add explicit type casts.",
            when.logical_type()
        ))),
    }
}

/// `make_date(days)` on one row.
fn made_date_value(days: &Value) -> Result<Value> {
    let Some(days) = days.as_i64() else {
        return Err(Error::internal(format!("make_date of a {}", days.logical_type())));
    };
    let fitted = i32::try_from(days)
        .map_err(|_| Error::conversion(format!("Date out of range: {days} days")))?;
    Ok(Value::Date(fitted))
}

/// `make_date(year, month, day)` on one row.
///
/// The check is a round trip rather than a calendar. Thirty February converts to the second of March
/// and converts back as the second of March, so a date that does not come back as what went in is a
/// date that was never there, and that catches the month length and the leap year without a table of
/// either. The message names the three numbers the way they were written, unpadded, which is what
/// the binary prints.
fn made_civil_value(year: &Value, month: &Value, day: &Value) -> Result<Value> {
    let (Some(year), Some(month), Some(day)) = (year.as_i64(), month.as_i64(), day.as_i64()) else {
        return Err(Error::internal("make_date of something that is not three numbers"));
    };
    let out_of_range = || Error::conversion(format!("Date out of range: {year}-{month}-{day}"));
    let (fitted, month, day) = match (i32::try_from(year), u32::try_from(month), u32::try_from(day))
    {
        (Ok(year), Ok(month), Ok(day)) => (year, month, day),
        _ => return Err(out_of_range()),
    };
    if !(1..=12).contains(&month) || day == 0 {
        return Err(out_of_range());
    }
    let days = days_from_civil(fitted, month, day);
    if civil_from_days(days) != (fitted, month, day) {
        return Err(out_of_range());
    }
    Ok(Value::Date(days))
}

/// One of the thirteen interval constructors on one row.
///
/// The signature has already cast the count to the type the function takes, so a DOUBLE here means
/// `to_seconds` or `to_milliseconds` and an integer means one of the other eleven, and the field the
/// count lands in is [`datetime::interval`]'s business rather than this one's.
fn interval_value(name: &str, count: &Value) -> Result<Value> {
    let count = match count {
        Value::Double(real) => Count::Real(*real),
        other => match integral(other) {
            Some(whole) => Count::Whole(whole),
            None => return Err(Error::internal(format!("{name} of a {}", other.logical_type()))),
        },
    };
    let (months, days, micros) = datetime::interval(name, count)?;
    Ok(Value::Interval { months, days, micros })
}

/// `trunc`, which drops what is after the point rather than rounding it.
///
/// The one difference from upstream is the decimal. `trunc(1.7)` is a `DECIMAL(2,0)` there and is a
/// `DECIMAL(2,1)` holding 1.0 here, because this table has one shape per name and no shape in it
/// drops a scale. Same number, and the interval rewrite never reaches that arm anyway, since every
/// call it writes goes through a DOUBLE.
fn truncated(value: &Value) -> Result<Value> {
    match value {
        Value::Float(real) => Ok(Value::Float(real.trunc())),
        Value::Double(real) => Ok(Value::Double(real.trunc())),
        Value::Decimal { unscaled, width, scale } => {
            let step = pow10(*scale);
            Ok(Value::Decimal { unscaled: unscaled / step * step, width: *width, scale: *scale })
        }
        _ if integral(value).is_some() => Ok(value.clone()),
        _ => Err(Error::not_implemented(format!("trunc of a {}", value.logical_type()))),
    }
}

/// `epoch_ms(milliseconds)` on one row.
fn made_timestamp_value(millis: &Value) -> Result<Value> {
    let Some(millis) = millis.as_i64() else {
        return Err(Error::internal(format!("epoch_ms of a {}", millis.logical_type())));
    };
    micros_of_millis(millis).map(Value::Timestamp)
}

/// Calls a scalar function on one row.
///
/// # Errors
///
/// If the function is not one of the ones written here, or if the call fails.
pub fn call_values(
    name: &str,
    args: &[Value],
    returns: &LogicalType,
    written: Written<'_>,
) -> Result<Value> {
    if let ("__rudb_zero_to_null", [value]) = (name, args) {
        return Ok(if approximate(value) == Some(0.0) { Value::Null } else { value.clone() });
    }
    if name == "coalesce" {
        let found = args.iter().find(|value| !value.is_null());
        return Ok(found.cloned().unwrap_or(Value::Null));
    }
    // `list_value` is above the null rule because a null argument is an element of the list rather
    // than an answer for the whole call. `[1, NULL]` is a list of two things on the pin and not null,
    // and the difference between a null list and a list holding a null is the difference this whole
    // family of types exists to keep.
    if name == "list_value" {
        let LogicalType::List(element) = returns else {
            return Err(Error::internal(format!("list_value returning {returns}")));
        };
        return Ok(Value::List { element: (**element).clone(), values: args.to_vec() });
    }
    // `nullif` is above the null rule for the same reason `coalesce` is. It is
    // `CASE WHEN a = b THEN NULL ELSE a END`, and `a = NULL` is null rather than true, so a null on
    // the right hands back the left value instead of blanking it: `nullif(1, NULL)` is 1 upstream.
    if let ("nullif", [left, right]) = (name, args) {
        if compare::compare_values(Comparison::Equal, left, right)?.as_bool() == Some(true) {
            return Ok(Value::Null);
        }
        // The two arguments were cast to the type they promote to so that the comparison happens
        // there, and the answer is the first argument's own type, so it goes back to where it
        // started. Promotion only ever widens, so this cast cannot fail and cannot lose anything.
        return cast::cast_value(left, returns, false);
    }
    // `concat` is the third one above the null rule and the only one of the three that is an
    // ordinary function rather than sugar. It drops a null argument rather than answering null for
    // the whole call, so `concat('a', 1, NULL)` is `a1` upstream, and the arguments reaching here
    // have already been cast to strings by the signature.
    if name == "concat" {
        let mut out = String::new();
        for value in args.iter().filter(|value| !value.is_null()) {
            out.push_str(&value.to_string());
        }
        return Ok(Value::Varchar(out));
    }
    // `list_concat` is the fourth one above the null rule and it follows `concat`'s rule rather than
    // the operator's. A null argument is a list with nothing in it here, so `list_concat([1], NULL)`
    // is `[1]` upstream while `[1] || NULL` is null, and the two spellings are not the same function
    // even though they answer the same thing whenever no argument is null.
    if name == "list_concat" {
        let LogicalType::List(element) = returns else {
            return Err(Error::internal(format!("list_concat returning {returns}")));
        };
        // Every argument was cast to the answer's type by the signature, so an element reaching here
        // is already the element type and nothing is cast a second time.
        let mut values = Vec::new();
        let mut seen = false;
        for value in args {
            let Value::List { values: held, .. } = value else {
                continue;
            };
            seen = true;
            values.extend(held.iter().cloned());
        }
        // Every argument was null, which is a null answer and not an empty list. `list_concat(NULL,
        // NULL)` is NULL upstream where `list_concat([], [])` is `[]`.
        if !seen {
            return Ok(Value::Null);
        }
        return Ok(Value::List { element: (**element).clone(), values });
    }
    if args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    match (name, args) {
        // Two lists joined end to end, which is the reading of this operator the arguments picked.
        // It is below the null rule and the named form is above it, which is the whole of the
        // difference between the two: `[1] || NULL` is null and `list_concat([1], NULL)` is `[1]`.
        ("||", [Value::List { values: left, .. }, Value::List { values: right, .. }]) => {
            let LogicalType::List(element) = returns else {
                return Err(Error::internal(format!("|| returning {returns}")));
            };
            let mut values = left.clone();
            values.extend(right.iter().cloned());
            Ok(Value::List { element: (**element).clone(), values })
        }
        ("+", [only]) => Ok(only.clone()),
        ("-", [only @ Value::Interval { .. }]) => datetime::negated(only),
        ("-", [only]) => negate(only, returns),
        ("abs", [only]) => absolute(only, returns),
        ("not", [only]) => match only.as_bool() {
            Some(held) => Ok(Value::Boolean(!held)),
            None => Err(Error::internal(format!("not of a {}", only.logical_type()))),
        },
        // Date arithmetic is above the numeric arithmetic because the two share a spelling, and
        // it is the argument types that tell them apart, the same way the signature does it.
        ("+" | "-", [left, right]) if datetime::is_shift(left, right) => {
            datetime::shift(left, right, name == "-")
        }
        ("+" | "-", [left @ Value::Interval { .. }, right @ Value::Interval { .. }]) => {
            datetime::combine(left, right, name == "-")
        }
        ("+" | "-", [left, right]) if datetime::is_counted(left, right) => {
            datetime::counted(left, right, name == "-")
        }
        ("-", [left @ Value::Date(_), right @ Value::Date(_)])
        | ("-", [left @ Value::Timestamp(_), right @ Value::Timestamp(_)])
        | ("-", [left @ Value::TimestampTz(_), right @ Value::TimestampTz(_)]) => {
            datetime::apart(left, right)
        }
        ("+", [left, right]) if datetime::is_joined(left, right) => datetime::joined(left, right),
        // The zero divisor is caught here rather than inside the scaling, because this is where
        // the written expression is and the sentence names the expression rather than the values.
        ("/", [left, right])
            if datetime::is_scale(left, right) && approximate(right) == Some(0.0) =>
        {
            // The symbol is a single slash and not [`Op::Divide`]'s double one, since this is
            // the one division by zero `/` reports rather than answering an infinity.
            Err(divided_by_zero(written, "/", left, right))
        }
        ("*" | "/", [left, right]) if datetime::is_scale(left, right) => {
            datetime::scaled(left, right, name == "/")
        }
        ("+", [left, right]) => arithmetic(Op::Add, left, right, returns, written),
        ("-", [left, right]) => arithmetic(Op::Subtract, left, right, returns, written),
        ("*", [left, right]) => arithmetic(Op::Multiply, left, right, returns, written),
        ("%", [left, right]) => arithmetic(Op::Modulo, left, right, returns, written),
        ("//", [left, right]) => arithmetic(Op::Divide, left, right, returns, written),
        ("__rudb_checked_slash", [left, right]) => {
            arithmetic(Op::Divide, left, right, returns, written)
        }
        ("__rudb_checked_remainder", [left, right]) => {
            if approximate(right) == Some(0.0) {
                Err(divided_by_zero(written, "%", left, right))
            } else {
                arithmetic(Op::Modulo, left, right, returns, written)
            }
        }
        ("/", [left, right]) => divide(left, right, returns),
        ("||", [left, right]) => Ok(Value::Varchar(format!("{left}{right}"))),
        ("lower", [only]) => Ok(Value::Varchar(only.to_string().to_lowercase())),
        ("upper", [only]) => Ok(Value::Varchar(only.to_string().to_uppercase())),
        // A list is counted at its top level, and a null element is an element.
        ("length" | "array_length", [Value::List { values, .. }]) => {
            Ok(Value::BigInt(elements(values)))
        }
        ("array_length", [Value::List { values, .. }, dimension]) => {
            // The signature cast the dimension to a BIGINT, so anything but 1 here is a number the
            // pin has no answer for either, and this is its sentence for it.
            if dimension.as_i64() != Some(1) {
                return Err(Error::not_implemented(
                    "array_length for lists with dimensions other than 1 not implemented",
                ));
            }
            Ok(Value::BigInt(elements(values)))
        }
        ("length", [only]) => Ok(Value::BigInt(count_characters(only))),
        ("strlen", [only]) => Ok(Value::BigInt(count_bytes(only))),
        ("substring" | "substr", [held, start]) => text::substring(held, start, None),
        ("substring" | "substr", [held, start, length]) => {
            text::substring(held, start, Some(length))
        }
        ("position" | "strpos" | "instr", [haystack, needle]) => text::position(haystack, needle),
        ("left" | "right", [held, count]) => text::end(name, held, count),
        ("replace", [held, needle, replacement]) => text::replace(held, needle, replacement),
        ("chr", [code]) => text::chr(code),
        ("trim" | "ltrim" | "rtrim", [only]) => text::trim(name, only, None),
        ("trim" | "ltrim" | "rtrim", [only, characters]) => {
            text::trim(name, only, Some(characters))
        }
        ("overlay", [held, replacement, start]) => text::overlay(held, replacement, start, None),
        ("overlay", [held, replacement, start, length]) => {
            text::overlay(held, replacement, start, Some(length))
        }
        ("~~", [text, pattern]) => Ok(Value::Boolean(matches(text, pattern, false))),
        ("!~~", [text, pattern]) => Ok(Value::Boolean(!matches(text, pattern, false))),
        ("~~*", [text, pattern]) => Ok(Value::Boolean(matches(text, pattern, true))),
        ("!~~*", [text, pattern]) => Ok(Value::Boolean(!matches(text, pattern, true))),
        ("date_part" | "date_trunc", [spec, when]) => date_value(name, spec, when, returns),
        ("age", [later, earlier]) => datetime::age(later, earlier),
        ("trunc", [only]) => truncated(only),
        (_, [count]) if datetime::is_interval(name) => interval_value(name, count),
        ("make_date", [days]) => made_date_value(days),
        ("make_date", [year, month, day]) => made_civil_value(year, month, day),
        ("epoch_ms", [millis]) => made_timestamp_value(millis),
        ("array_extract", [target, index]) => subscript::extract(target, index),
        ("array_slice", [target, begin, end]) => subscript::slice(target, begin, end, None),
        ("array_slice", [target, begin, end, step]) => {
            subscript::slice(target, begin, end, Some(step))
        }
        (_, [_, _, ..]) if regexp::is_regexp(name) => regexp::value(name, args),
        _ => Err(Error::not_implemented(format!(
            "the {name} function with {} arguments",
            args.len()
        ))),
    }
}

/// Which arithmetic, kept separate from the spelling so that the overflow message can name it the
/// way DuckDB names it.
///
#[derive(Debug, Clone, Copy)]
pub(crate) enum Op {
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

/// The overflow message, which is DuckDB's sentence down to the punctuation on the end.
///
/// There is more shape to it than there looks. The type is the physical integer rather than the SQL
/// name, a decimal prints its operands unscaled, an integer ends the sentence on `!` and a decimal
/// on `;`, a decimal subtraction is called `subtract` where everything else is called by the noun,
/// and a decimal multiplication ends on a hint instead of punctuation. All of it was measured
/// against the pinned binary, and every arithmetic message in the engine comes through here so
/// there is one place to keep it right.
pub(crate) fn overflow(op: Op, ty: &LogicalType, left: &Value, right: &Value) -> Error {
    let decimal = matches!(ty, LogicalType::Decimal { .. });
    let (left, right) = if decimal {
        (unscaled(left), unscaled(right))
    } else {
        (left.to_string(), right.to_string())
    };
    let word = if decimal && matches!(op, Op::Subtract) { "subtract" } else { op.word() };
    Error::out_of_range(format!(
        "Overflow in {word} of {} ({left} {} {right}){}",
        ty.physical_name(),
        op.symbol(),
        ending(op, ty)
    ))
}

/// The sentence DuckDB says when a divisor is zero, which is not every divisor by zero.
///
/// `/` never gets here: it promotes to a double and answers an infinity or a nan. `//` always gets
/// here, whatever it was given. `%` gets here on integers and decimals and not on floats, where the
/// IEEE answer is a nan. The sentence names the expression rather than the numbers, so a caller with
/// the plan hands one down and a caller without one quotes the two values, which is the same text
/// whenever the expression was folded to a pair of constants.
fn divided_by_zero(written: Written<'_>, symbol: &str, left: &Value, right: &Value) -> Error {
    let quoted = written.map_or_else(|| format!("({left} {symbol} {right})"), |render| render());
    Error::invalid_input(format!(
        "Division by zero in expression {quoted}. Use TRY(...) to return NULL for this expression, \
         or SET null_on_division_by_zero=true to return NULL for all divisions by zero."
    ))
}

/// `abs` says it differently: `Overflow on abs(-2147483648)`, with no type in it and no punctuation
/// on the end.
fn abs_overflow(value: &Value) -> Error {
    Error::out_of_range(format!("Overflow on abs({value})"))
}

/// Negation says it differently again, and names neither the type nor the value it was given.
///
/// This used to come out of [`overflow`] as the subtraction `0 - -2147483648`, which reads like an
/// expression nobody wrote and is not what upstream says. There are only two ways to reach it, and
/// both are a value nothing can widen: a column, where the constant folder has no value to look at
/// until the loop is already running, and a `HUGEINT`, where there is no wider signed type to move
/// to. Per #264.
pub(crate) fn negation_overflow() -> Error {
    Error::out_of_range("Overflow in negation of numeric value!")
}

/// The digits a decimal holds, rather than the number it means.
///
/// Upstream says `(999999999999 * 999999999999)` for a pair of DECIMAL(38,2) values, so the point
/// is gone from the operands the same way the scale is gone from the type name.
fn unscaled(value: &Value) -> String {
    match value {
        Value::Decimal { unscaled, .. } => unscaled.to_string(),
        other => other.to_string(),
    }
}

/// What the sentence ends on.
///
/// A decimal multiplication ends on advice rather than punctuation, and which advice depends on
/// whether there is a wider decimal to move to. At 38 digits there is not one, so the only way out
/// is to give up scale.
fn ending(op: Op, ty: &LogicalType) -> &'static str {
    let Some(width) = ty.decimal_storage() else {
        return "!";
    };
    match op {
        Op::Multiply if width == 38 => {
            ". You might want to add an explicit cast to a decimal with a smaller scale."
        }
        Op::Multiply => ". You might want to add an explicit cast to a bigger decimal.",
        _ => ";",
    }
}

fn arithmetic(
    op: Op,
    left: &Value,
    right: &Value,
    ty: &LogicalType,
    written: Written<'_>,
) -> Result<Value> {
    match ty {
        LogicalType::Float | LogicalType::Double => float_arithmetic(op, left, right, ty, written),
        LogicalType::Decimal { width, scale } => {
            decimal_arithmetic(op, left, right, *width, *scale, written)
        }
        other if other.is_integer() => integer_arithmetic(op, left, right, ty, written),
        other => Err(Error::not_implemented(format!("{} on {other}", op.word()))),
    }
}

fn integer_arithmetic(
    op: Op,
    left: &Value,
    right: &Value,
    ty: &LogicalType,
    written: Written<'_>,
) -> Result<Value> {
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
        return Err(divided_by_zero(written, op.symbol(), left, right));
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

fn float_arithmetic(
    op: Op,
    left: &Value,
    right: &Value,
    ty: &LogicalType,
    written: Written<'_>,
) -> Result<Value> {
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
    // `//` raises on a zero divisor whatever it was given and `%` does not, so a float remainder
    // by zero is the IEEE answer and a float `//` by zero is the error. Measured both ways.
    if matches!(op, Op::Divide) && b == 0.0 {
        return Err(divided_by_zero(written, op.symbol(), left, right));
    }
    let result = float_step(op, a, b);
    if matches!(ty, LogicalType::Float) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "arithmetic on a FLOAT column produces a FLOAT"
        )]
        return Ok(Value::Float(result as f32));
    }
    Ok(Value::Double(result))
}

fn decimal_arithmetic(
    op: Op,
    left: &Value,
    right: &Value,
    width: u8,
    scale: u8,
    written: Written<'_>,
) -> Result<Value> {
    let ty = LogicalType::Decimal { width, scale };
    if matches!(op, Op::Multiply) {
        return decimal_product(left, right, width, scale, &ty);
    }
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
        return Err(divided_by_zero(written, op.symbol(), left, right));
    }
    let unscaled = match op {
        Op::Add => a.checked_add(b),
        Op::Subtract => a.checked_sub(b),
        Op::Multiply => unreachable!("a product is handled above"),
        Op::Modulo => a.checked_rem(b),
        Op::Divide => a.checked_div(b).and_then(|whole| whole.checked_mul(pow10(scale))),
    };
    let unscaled = unscaled.ok_or_else(|| overflow(op, &ty, left, right))?;
    if digits(unscaled) > width {
        return Err(overflow(op, &ty, left, right));
    }
    Ok(Value::Decimal { unscaled, width, scale })
}

/// A product, which multiplies the two unscaled values as they are rather than at the same scale.
///
/// The answer's scale is the two scales added together, so `1.50 * 1.50` is 150 times 150 written
/// with four decimal places, which is 2.2500 and is what DuckDB answers. Lifting both sides to the
/// answer's scale first would multiply the same number by ten thousand and then divide it back,
/// which is the same answer for small values and an overflow for large ones.
fn decimal_product(
    left: &Value,
    right: &Value,
    width: u8,
    scale: u8,
    ty: &LogicalType,
) -> Result<Value> {
    let (a, b) = match (unscaled_and_scale(left), unscaled_and_scale(right)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(Error::not_implemented(format!(
                "multiplication on {} and {}",
                left.logical_type(),
                right.logical_type()
            )));
        }
    };
    let held = a.1.saturating_add(b.1);
    let unscaled =
        a.0.checked_mul(b.0)
            .and_then(|wide| rescale(wide, held, scale))
            .ok_or_else(|| overflow(Op::Multiply, ty, left, right))?;
    if digits(unscaled) > width {
        return Err(overflow(Op::Multiply, ty, left, right));
    }
    Ok(Value::Decimal { unscaled, width, scale })
}

/// A value as the integer it is written with and the scale it is written at.
fn unscaled_and_scale(value: &Value) -> Option<(i128, u8)> {
    match *value {
        Value::Decimal { unscaled, scale, .. } => Some((unscaled, scale)),
        _ => integral(value).map(|whole| (whole, 0)),
    }
}

/// A value as an unscaled integer at the given scale, for the decimal path.
fn unscaled_at(value: &Value, scale: u8) -> Option<i128> {
    match *value {
        Value::Decimal { unscaled, scale: held, .. } => rescale(unscaled, held, scale),
        _ => integral(value).and_then(|whole| whole.checked_mul(pow10(scale))),
    }
}

/// `/`, which the binder has already promoted both sides to the result type.
fn divide(left: &Value, right: &Value, returns: &LogicalType) -> Result<Value> {
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
    // No guard. `/` uses IEEE arithmetic, so a zero divisor is an infinity or a nan and never an
    // error, whatever the arguments were written as. Two floats keep their width upstream.
    if returns == &LogicalType::Float {
        #[expect(clippy::cast_possible_truncation, reason = "the resolved result is FLOAT")]
        return Ok(Value::Float((a / b) as f32));
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
                .ok_or_else(negation_overflow),
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
                .ok_or_else(|| abs_overflow(value)),
            None => Err(Error::not_implemented(format!("abs of a {}", value.logical_type()))),
        },
    }
}

/// `length`, which counts characters rather than bytes, the way DuckDB does.
/// How many elements a list holds, as the BIGINT `length` answers with.
fn elements(values: &[Value]) -> i64 {
    i64::try_from(values.len()).unwrap_or(i64::MAX)
}

fn count_characters(value: &Value) -> i64 {
    let text = match value.as_str() {
        Some(text) => text.chars().count(),
        None => value.to_string().chars().count(),
    };
    i64::try_from(text).unwrap_or(i64::MAX)
}

/// `strlen`, which counts bytes rather than characters, the way DuckDB does.
fn count_bytes(value: &Value) -> i64 {
    let bytes = match value.as_str() {
        Some(text) => text.len(),
        None => value.to_string().len(),
    };
    i64::try_from(bytes).unwrap_or(i64::MAX)
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
        // The wildcard is tested before the literal, and the order is the whole of the correctness
        // here. A `%` in a pattern is always a wildcard, so a pattern `%` sitting over a string that
        // happens to hold a `%` must not take the equal branch and eat one character of each. It did
        // for a while, and the effect was that `'a%b' LIKE '%a%'` came back false, which is issue
        // #279. Encoded URLs are full of percent signs, so the queries it was wrong for were the
        // ordinary ones.
        if against < pattern.len() && pattern[against] == '%' {
            star = Some(against);
            resume = at;
            against += 1;
        } else if against < pattern.len()
            && (pattern[against] == '_' || pattern[against] == text[at])
        {
            at += 1;
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
        call_values(name, args, returns, None).expect("this call is written")
    }

    /// What `nullif` does with the nulls and with a comparison that was cast. Per #306.
    #[test]
    fn nullif_blanks_a_pair_that_matches_and_keeps_the_left_one_otherwise() {
        let integer = LogicalType::Integer;
        assert_eq!(
            called("nullif", &[Value::Integer(2), Value::Integer(2)], &integer),
            Value::Null
        );
        assert_eq!(
            called("nullif", &[Value::Integer(1), Value::Integer(2)], &integer),
            Value::Integer(1)
        );
        // `1 = NULL` is null and not true, so the left value comes back rather than being blanked.
        assert_eq!(
            called("nullif", &[Value::Integer(1), Value::Null], &integer),
            Value::Integer(1)
        );
        assert_eq!(called("nullif", &[Value::Null, Value::Integer(1)], &integer), Value::Null);
        // The binder casts both sides to what they promote to and the answer goes back to the first
        // argument's type, which is the one thing this function has to do that the others do not.
        let decimal = |unscaled| Value::Decimal { unscaled, width: 11, scale: 1 };
        assert_eq!(
            called("nullif", &[decimal(20), decimal(25)], &integer),
            Value::Integer(2),
            "2 is not 2.5, and what comes back is the 2 the query wrote"
        );
        assert_eq!(called("nullif", &[decimal(20), decimal(20)], &integer), Value::Null);
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
        let error = call_values(
            "+",
            &[Value::Integer(i32::MAX), Value::Integer(1)],
            &LogicalType::Integer,
            None,
        )
        .expect_err("2147483647 + 1 is not an integer");
        assert_eq!(error.message(), "Overflow in addition of INT32 (2147483647 + 1)!");
    }

    /// A zero divisor is three different things depending on the operator, and this is the line
    /// that records which is which. Per #262.
    #[test]
    fn a_zero_divisor_is_an_infinity_on_slash_and_an_error_on_the_other_two() {
        assert_eq!(
            called("/", &[Value::Double(1.0), Value::Double(0.0)], &LogicalType::Double),
            Value::Double(f64::INFINITY)
        );
        let error =
            call_values("//", &[Value::Integer(1), Value::Integer(0)], &LogicalType::Integer, None)
                .expect_err("1 // 0 raises");
        assert_eq!(
            error.message(),
            "Division by zero in expression (1 // 0). Use TRY(...) to return NULL for this \
             expression, or SET null_on_division_by_zero=true to return NULL for all divisions by \
             zero."
        );
        let error =
            call_values("%", &[Value::Integer(1), Value::Integer(0)], &LogicalType::Integer, None)
                .expect_err("1 % 0 raises");
        assert!(error.message().starts_with("Division by zero in expression (1 % 0)."), "{error}");
        // The float remainder is the exception. IEEE says nan and so does the pinned binary.
        let remainder =
            called("%", &[Value::Double(1.0), Value::Double(0.0)], &LogicalType::Double);
        assert!(matches!(remainder, Value::Double(answer) if answer.is_nan()), "{remainder}");
    }

    /// The caller with a plan hands down the expression, and the message quotes that rather than
    /// the two values.
    #[test]
    fn a_named_expression_is_what_the_message_quotes() {
        let written = || "(a // 0)".to_string();
        let error = call_values(
            "//",
            &[Value::Integer(7), Value::Integer(0)],
            &LogicalType::Integer,
            Some(&written),
        )
        .expect_err("7 // 0 raises");
        assert!(error.message().starts_with("Division by zero in expression (a // 0)."), "{error}");
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

    /// `//` over floats does not truncate, which was measured: `7.0 // 2.0` is 3.5 on the pinned
    /// binary and `7.9 // 1.0` is 7.9. Once a side is not an integer it is `/` under another
    /// spelling, and the truncation that is left is the one dividing two integers does.
    #[test]
    fn integer_division_over_floats_divides_and_does_not_truncate() {
        let slash = |left, right| {
            called("//", &[Value::Double(left), Value::Double(right)], &LogicalType::Double)
        };
        assert_eq!(slash(7.0, 2.0), Value::Double(3.5));
        assert_eq!(slash(7.5, 2.0), Value::Double(3.75));
        assert_eq!(slash(7.9, 1.0), Value::Double(7.9));
        assert_eq!(slash(-7.9, 1.0), Value::Double(-7.9));
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
        // The one string where the two disagree, and the reason `strlen` is not an alias. Upstream
        // says 6 here and 5 above.
        assert_eq!(
            called("strlen", &[Value::Varchar("héllo".into())], &LogicalType::BigInt),
            Value::BigInt(6)
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

    /// A percent sign in the string is a character and a percent sign in the pattern is a wildcard.
    ///
    /// The pairs worth reading together are `ax%b` against `a%b`, where the match is at the same
    /// offset and the only difference is the character after it, and `a%` against `a%b`, where the
    /// pattern is the same and the string grows by one. The percent sign in the text is the part
    /// that was once wrong, and it is wrong in the same way whichever of the compiled shapes or the
    /// backtracking walk answers, so the list runs across both.
    #[test]
    fn a_percent_sign_in_the_string_is_a_character_and_not_a_wildcard() {
        for (text, pattern, expected) in [
            ("a%b", "%a%", true),
            ("ax%b", "%a%", true),
            ("a%%b", "%a%", true),
            ("a%b", "a%", true),
            ("a%", "a%", true),
            ("%a", "%", true),
            ("%%", "%", true),
            ("%", "%", true),
            ("a%b", "%b%", true),
            ("a%b", "%a%b%", true),
            ("a%b", "_%_", true),
            ("http://x/google%2F12.15", "%google%", true),
            // One of the nine URLs that went missing out of ClickBench q21, cut down to the part
            // that matters, which is the `%` sitting immediately after the word the pattern wants.
            ("amalgama-lab.com.ua/google%2F12.15&he=900&Select", "%google%", true),
            ("a%", "%a", false),
            ("a%b", "%a", false),
            ("%b", "a%", false),
            // The other direction. A `%` in the text stands for itself there too, so it does not
            // stand in for the two characters the pattern is asking for.
            ("goo%gle", "google", false),
            ("goo%gle", "%google%", false),
        ] {
            let held = called(
                "~~",
                &[Value::Varchar(text.into()), Value::Varchar(pattern.into())],
                &LogicalType::Boolean,
            );
            assert_eq!(held, Value::Boolean(expected), "{text} LIKE {pattern}");
        }
    }

    /// The compiled shapes and the walk have to agree, since a query reaches one or the other
    /// depending only on whether the pattern is a literal the binder could fold.
    ///
    /// `%a%` compiles to a substring search and `%a%%` does not, and they are the same question.
    /// That pair is what #279 was: the fast path was right and the walk was wrong, so the answer
    /// depended on where the pattern came from rather than on what it said.
    #[test]
    fn the_compiled_shapes_and_the_walk_answer_the_same_question() {
        let mut characters = Vec::new();
        for text in ["a%b", "ax%b", "%ab", "ab%", "a%", "%", "ab", ""] {
            for (fast, slow) in [("%a%", "%a%%"), ("a%", "a%%"), ("%b", "%%b"), ("ab", "ab")] {
                assert_eq!(
                    Pattern::compile(fast).holds(text, &mut characters),
                    Pattern::compile(slow).holds(text, &mut characters),
                    "{text:?} against {fast} and {slow}"
                );
            }
        }
    }

    /// Every pattern of literal text cut at its `%` signs, against every string of the same shape.
    ///
    /// The greedy search [`Split::holds`] does is the whole of the claim that it can drop the
    /// backtracking point, and the strings that catch a greedy search out are the ones where a
    /// piece appears more than once, which is what an alphabet of two letters and a wildcard gives
    /// at almost every length. A percent sign is in the alphabet on both sides so that the pattern
    /// runs `%` together and the text holds one as a character of its own.
    #[test]
    fn the_segment_search_answers_what_the_backtracking_walk_answers() {
        let mut alphabet = vec![String::new()];
        let mut words = vec![String::new()];
        for _ in 0..4 {
            alphabet = alphabet
                .iter()
                .flat_map(|word| ['a', 'b', '%'].map(|letter| format!("{word}{letter}")))
                .collect();
            words.extend(alphabet.iter().cloned());
        }
        let mut characters = Vec::new();
        let mut seen = 0_usize;
        for pattern in &words {
            let compiled = Pattern::compile(pattern);
            if !matches!(compiled, Pattern::Segments(_)) {
                continue;
            }
            seen += 1;
            let spelling: Vec<char> = pattern.chars().collect();
            for text in &words {
                let walked = like(&text.chars().collect::<Vec<char>>(), &spelling);
                assert_eq!(
                    compiled.holds(text, &mut characters),
                    walked,
                    "{text:?} LIKE {pattern:?}"
                );
                assert_eq!(
                    compiled.holds_bytes(text.as_bytes()),
                    walked,
                    "{text:?} LIKE {pattern:?} on bytes"
                );
            }
        }
        assert!(seen > 20, "the segment shape was reached {seen} times");
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

    /// A dictionary encoded text reaches the compiled loop instead of falling out of it.
    ///
    /// This is the one thing the property test cannot say, because `agrees` is happy when the row at
    /// a time path answers for both sides. Per #288 every string column in the published ClickBench
    /// file is dictionary encoded, so `like_of` handing back `None` here is the difference between
    /// the predicate costing a compiled walk and costing a `Value` per row, which over twenty
    /// million rows was measured at 13.8 seconds against 2.2.
    #[test]
    fn a_dictionary_or_constant_text_reaches_the_compiled_like() {
        let values = Vector::from_values(
            LogicalType::Varchar,
            &[
                Value::Varchar("a google search".into()),
                Value::Varchar("goggle".into()),
                Value::Null,
            ],
        )
        .expect("builds");
        let text = Vector::dictionary(vec![0, 1, 2, 0], values).expect("codes are in range");
        let pattern = Vector::constant(LogicalType::Varchar, Value::Varchar("%google%".into()), 4);
        let answer =
            binary("~~", &Hoisted::Nothing, &text, &pattern, &LogicalType::Boolean, 4, None)
                .expect("the call is written")
                .expect("a dictionary text has a loop of its own");
        let rows: Vec<Value> = (0..4).map(|row| answer.value_at(row)).collect();
        assert_eq!(
            rows,
            [Value::Boolean(true), Value::Boolean(false), Value::Null, Value::Boolean(true)]
        );

        // A constant text is not the other arm, because there is no other arm. `call` answers a
        // call whose every argument is constant in one row, and a constant text under a pattern
        // that is not constant does not get past the pattern check.
        let text = Vector::constant(LogicalType::Varchar, Value::Varchar("google".into()), 4);
        assert!(
            binary("~~", &Hoisted::Nothing, &text, &pattern, &LogicalType::Boolean, 4, None)
                .expect("the call is written")
                .is_none()
        );
        assert_eq!(
            call("~~", &[text, pattern], &LogicalType::Boolean, None)
                .expect("the call is written")
                .value_at(0),
            Value::Boolean(true)
        );
    }

    /// Answering per distinct value and answering per row give the same column.
    ///
    /// Both dictionary arms are reached here, because the arm is chosen on whether the dictionary is
    /// shorter than the chunk and the two dictionaries below are on either side of the same row
    /// count. Both are compared against the flat answer over the same values, which is the loop that
    /// was there before either of them.
    #[test]
    fn answering_a_like_per_distinct_value_agrees_with_answering_it_per_row() {
        let seen = [
            Value::Varchar("a google search".into()),
            Value::Varchar("goggle".into()),
            Value::Null,
            Value::Varchar("GOOGLE".into()),
            Value::Varchar("".into()),
            Value::Varchar("google.com/google".into()),
        ];
        for spelling in ["%google%", "%GOOGLE%", "goggle", "g%e", "%le", "go%"] {
            for name in ["~~", "!~~", "~~*"] {
                let pattern =
                    Vector::constant(LogicalType::Varchar, Value::Varchar(spelling.into()), 12);
                let answer = |text: Vector| {
                    let out = binary(
                        name,
                        &Hoisted::Nothing,
                        &text,
                        &pattern,
                        &LogicalType::Boolean,
                        12,
                        None,
                    )
                    .expect("the call is written")
                    .expect("text in this form has a loop of its own");
                    (0..12).map(|row| out.value_at(row)).collect::<Vec<Value>>()
                };
                let codes: Vec<u32> = (0..12).map(|row| (row % seen.len()) as u32).collect();
                let rows: Vec<Value> =
                    codes.iter().map(|&code| seen[code as usize].clone()).collect();
                let flat = Vector::from_values(LogicalType::Varchar, &rows).expect("builds");
                let values = Vector::from_values(LogicalType::Varchar, &seen).expect("builds");
                // Six values under twelve rows, which answers once per value and gathers.
                let short = Vector::dictionary(codes, values).expect("codes are in range");
                // Twelve values under twelve rows, which is not shorter than the chunk and walks it
                // a row at a time instead.
                let long = Vector::dictionary(
                    (0..12).collect(),
                    Vector::from_values(LogicalType::Varchar, &rows).expect("builds"),
                )
                .expect("codes are in range");
                let want = answer(flat);
                assert_eq!(answer(short), want, "{name} {spelling} over a short dictionary");
                assert_eq!(answer(long), want, "{name} {spelling} over a long one");
            }
        }
    }

    /// A stable dictionary `LIKE` answers the same whichever way the memo filled.
    ///
    /// The memo fills a whole group of `LIKE_GROUP` values where the chunk is big enough to be a
    /// scan and fills the one value it was asked about where it is not, so the two have to agree
    /// over the same dictionary, and a memo a scan filled has to answer for a chunk that comes after
    /// it. The dictionary here is 2,500 values, which is two whole groups and a part of a third, and
    /// the codes step by a prime so a chunk lands in every group without repeating a code in order.
    /// The big chunk runs first so the small one reads decisions it did not make.
    #[test]
    fn a_stable_dictionary_like_agrees_whichever_way_the_memo_filled() {
        let values: Vec<Value> = (0..2_500)
            .map(|index| match index % 7 {
                0 => Value::Null,
                1 => Value::Varchar(format!("http://google.com/{index}")),
                2 => Value::Varchar(format!("http://goggle.com/{index}")),
                _ => Value::Varchar(format!("row {index}")),
            })
            .collect();
        let dictionary =
            Arc::new(Vector::from_values(LogicalType::Varchar, &values).expect("builds"));
        let like = Like::of("~~", "%google%").expect("a literal pattern compiles");
        for rows in [2_000_usize, 64] {
            let codes: Vec<u32> =
                (0..rows).map(|row| ((row * 991) % values.len()) as u32).collect();
            let picked: Vec<Value> =
                codes.iter().map(|&code| values[code as usize].clone()).collect();
            let flat = Vector::from_values(LogicalType::Varchar, &picked).expect("builds");
            let pattern =
                Vector::constant(LogicalType::Varchar, Value::Varchar("%google%".into()), rows);
            let answer = |text: &Vector| {
                like_of("~~", Some(&like), text, &pattern, &LogicalType::Boolean, rows)
                    .expect("the call is written")
                    .expect("text in this form has a loop of its own")
            };
            let column = Vector::stable_dictionary(codes, Arc::clone(&dictionary))
                .expect("codes are in range");
            let want = answer(&flat);
            let got = answer(&column);
            for row in 0..rows {
                assert_eq!(got.value_at(row), want.value_at(row), "{rows} rows, row {row}");
            }
        }
    }

    /// The one argument kernels and the date ones enter their loop on a dictionary.
    ///
    /// The same argument as the `LIKE` test above. `agrees` is satisfied when the row at a time path
    /// answers for both sides, which is the bug rather than the test of it, so this asserts the loop
    /// is entered at all. Per #288 `strlen` is the one ClickBench cares about most here, since q28
    /// and q29 are `AVG(STRLEN(URL))` and `AVG(STRLEN(Referer))` and both of those columns are
    /// dictionary encoded in the published file.
    #[test]
    fn a_dictionary_argument_reaches_the_one_argument_loops() {
        let values = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("héllo".into()), Value::Varchar(String::new()), Value::Null],
        )
        .expect("builds");
        let arg = Vector::dictionary(vec![0, 1, 2, 0], values).expect("codes are in range");
        let read = |name: &str| {
            let answer = unary(name, &arg, &LogicalType::BigInt, 4)
                .expect("the call is written")
                .expect("a dictionary argument has a loop of its own");
            (0..4).map(|row| answer.value_at(row)).collect::<Vec<_>>()
        };
        // `length` counts characters and `strlen` counts bytes, and the first word is five of one
        // and six of the other, so this says which loop ran as well as that one did.
        assert_eq!(
            read("length"),
            [Value::BigInt(5), Value::BigInt(0), Value::Null, Value::BigInt(5)]
        );
        assert_eq!(
            read("strlen"),
            [Value::BigInt(6), Value::BigInt(0), Value::Null, Value::BigInt(6)]
        );

        let days = Vector::from_values(
            LogicalType::Date,
            &[Value::Date(0), Value::Date(16_000), Value::Null],
        )
        .expect("builds");
        let when = Vector::dictionary(vec![0, 1, 2, 1], days).expect("codes are in range");
        let part = Vector::constant(LogicalType::Varchar, Value::Varchar("year".into()), 4);
        let years = date_of("date_part", &part, &when, &LogicalType::BigInt, 4)
            .expect("the call is written")
            .expect("a dictionary date has a loop of its own");
        assert_eq!(
            (0..4).map(|row| years.value_at(row)).collect::<Vec<_>>(),
            [Value::BigInt(1970), Value::BigInt(2013), Value::Null, Value::BigInt(2013)]
        );
    }

    #[test]
    fn a_function_nobody_has_written_says_which_one() {
        let error = call_values("sqrt", &[Value::Double(4.0)], &LogicalType::Double, None)
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
        let sum = call("+", &[left, right], &LogicalType::Integer, None).expect("adds");
        assert_eq!(sum.value_at(0), Value::Integer(11));
        assert_eq!(sum.value_at(1), Value::Integer(12));
        assert_eq!(sum.value_at(2), Value::Null);
    }

    #[test]
    fn arguments_of_different_lengths_are_caught() {
        let left = Vector::constant(LogicalType::Integer, Value::Integer(1), 3);
        let right = Vector::constant(LogicalType::Integer, Value::Integer(1), 4);
        let error = call("+", &[left, right], &LogicalType::Integer, None).expect_err("ragged");
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
            values.push(call_values(name, &row, returns, None)?);
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
        match (call(name, args, returns, None), oracle(name, args, returns)) {
            // The debug rendering rather than the vectors themselves, because a nan is a real
            // answer here now that a float remainder by zero is one, and no nan equals any nan.
            // Comparing the text is the stronger check everywhere else too, since it tells a
            // negative zero from a positive one where `==` does not.
            (Ok(fast), Ok(slow)) => assert_eq!(format!("{fast:?}"), format!("{slow:?}"), "{what}"),
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
                // The edge here is the largest value the width holds, so that a sum carries out of
                // the width and a product overflows the run it is computed in, which is what
                // [`decimal_sweep`] hands back to the careful loop and is otherwise never reached.
                LogicalType::Decimal { width, scale } => Value::Decimal {
                    unscaled: if edge {
                        (pow10(*width) - 1) * if small < 0 { -1 } else { 1 }
                    } else {
                        i128::from(small) * 37
                    },
                    width: *width,
                    scale: *scale,
                },
                LogicalType::Boolean => Value::Boolean(small > 0),
                LogicalType::Varchar => Value::Varchar(text(rng)),
                // Roughly the years 600 to 3300 either side of the epoch, and nine thousand years
                // of timestamps, because a calendar that is only ever asked about this decade is a
                // calendar whose leap years and week numbers are never asked about at all.
                LogicalType::Date => Value::Date(rng.below(1_000_000) as i32 - 500_000),
                LogicalType::Timestamp => {
                    Value::Timestamp(rng.next() as i64 % 300_000_000_000_000_000)
                }
                other => panic!("the generator has nothing for a {other}"),
            });
        }
        Vector::from_values(ty.clone(), &values).expect("the generator builds legal columns")
    }

    /// A string, chosen so that the inline limit, the empty string, multi byte characters and the
    /// substring the `LIKE` patterns look for all turn up often.
    ///
    /// Three of these hold a `%` or a `_`, which are the pattern characters, because a text is not a
    /// pattern and nothing in it is special. That the list held neither is why #279 got past this
    /// test for as long as it did. The compiled forms and the general walk disagreed about a `%` in
    /// the text and there was no text here to disagree over, so a property test that compares the
    /// two forms against each other on every string it can think of never thought of one.
    ///
    /// Holding a `%` somewhere is not enough on its own, which is why `goo%gle` is here as well.
    /// The two forms only part company when the pattern's `%` lands on the text's `%`, and against
    /// the patterns this test uses that needs the text to have one exactly where `goo%` has one. The
    /// other two go through all eight patterns agreeing either way, so with those alone the wildcard
    /// branch can be moved back below the literal one and this test still passes.
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
            "google%2F12",
            "goo_gle%",
            "goo%gle",
        ];
        words[rng.below(words.len() as u64) as usize].to_owned()
    }

    /// The form pairings that have a loop, as a pair of vectors built from one column.
    ///
    /// Constant against constant is not here on purpose: that pair returns a constant vector rather
    /// than a flat one, so it is right without being equal, and it has a test of its own below.
    ///
    /// The dictionary is built over the column itself with codes that repeat and run backwards, so
    /// a null in the column is a null under several codes and the loop cannot pass by reading the
    /// rows in order. Its last entry is deliberately unreferenced, which is the case where computing
    /// once per distinct value and computing once per row are allowed to disagree about whether
    /// something overflowed.
    /// The one argument forms that have a loop, as a vector each, over the one column.
    ///
    /// A constant is not here, and the reason is the one `pairings` gives for leaving out constant
    /// against constant. A call whose every argument is constant is answered by `call` in a single
    /// row and comes back as a constant vector, which is right without being equal to the flat
    /// vector the oracle builds, so `agrees` is the wrong test for it.
    fn forms(arg: &Vector) -> Vec<Vector> {
        let rows = arg.len();
        let codes: Vec<u32> = (0..rows).map(|index| (rows - 1 - index) as u32 / 2).collect();
        vec![arg.clone(), Vector::dictionary(codes, arg.clone()).expect("codes are in range")]
    }

    fn pairings(left: &Vector, right: &Vector) -> Vec<(Vector, Vector)> {
        let rows = left.len();
        let as_constant = |vector: &Vector| {
            Vector::constant(vector.logical_type().clone(), vector.value_at(0), rows)
        };
        let as_dictionary = |vector: &Vector| {
            let codes: Vec<u32> = (0..rows).map(|index| (rows - 1 - index) as u32 / 2).collect();
            Vector::dictionary(codes, vector.clone()).expect("codes are in range")
        };
        vec![
            (left.clone(), right.clone()),
            (left.clone(), as_constant(right)),
            (as_constant(left), right.clone()),
            (as_dictionary(left), right.clone()),
            (left.clone(), as_dictionary(right)),
            (as_dictionary(left), as_constant(right)),
            (as_constant(left), as_dictionary(right)),
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
            // One decimal per storage width, because the sweep works out its range from the width
            // and the four ranges are four different constants in four different types.
            LogicalType::decimal(4, 2).expect("a legal decimal"),
            LogicalType::decimal(9, 4).expect("a legal decimal"),
            LogicalType::decimal(10, 2).expect("a legal decimal"),
            LogicalType::decimal(18, 6).expect("a legal decimal"),
            LogicalType::decimal(38, 4).expect("a legal decimal"),
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
                    for arg in forms(&left) {
                        agrees(name, std::slice::from_ref(&arg), ty);
                    }
                }
                // The shape a bound product actually has, which the loop above cannot produce
                // because it gives both sides and the answer one type. A real product takes its
                // two sides at the answer's width with their own scales, which add up to the
                // answer's, so the unscaled values multiply straight into the answer and there is
                // no rescale. That is the case `l_extendedprice * (1 - l_discount)` is, it is the
                // only case [`decimal_sweep`] computes a product in, and with the sides and the
                // answer all one type it is never reached.
                if let LogicalType::Decimal { width, scale } = ty {
                    let doubled = LogicalType::decimal(*width, scale.saturating_mul(2))
                        .expect("a scale of twice a legal one is inside the width");
                    for (one, other) in pairings(&left, &right) {
                        agrees("*", &[one, other], &doubled);
                    }
                }
                if matches!(ty, LogicalType::Double) {
                    for (one, other) in pairings(&left, &right) {
                        agrees("/", &[one, other], ty);
                    }
                }
            }
        }
    }

    /// A string column whose bytes are behind a reader rather than in a buffer.
    ///
    /// Which is what every string column of a native table is. What matters about it here is what
    /// it does not have: `Vector::data` answers `None` for a vector over one of these, and that is
    /// the whole of why the string functions used to decline on a real file.
    #[derive(Debug)]
    struct Kept(Vec<Vec<u8>>);

    impl rudb_vector::TextSource for Kept {
        fn len(&self) -> usize {
            self.0.len()
        }

        fn bytes_at(&self, index: usize) -> Result<Option<&[u8]>> {
            Ok(self.0.get(index).map(Vec::as_slice))
        }

        fn footprint(&self) -> usize {
            self.0.iter().map(Vec::len).sum()
        }
    }

    /// The string functions take the vectorized path over a column that is read rather than held.
    ///
    /// Per #1019. `agrees` cannot make this assertion, because a function that declines is answered
    /// by the row at a time path and the row at a time path is the oracle, so the two agree exactly
    /// when nothing is specialized at all. What has to be checked is that `unary` hands back a
    /// vector rather than `None`, and then separately that the vector it hands back is the right
    /// one.
    ///
    /// Both shapes a file gives, since a native table writes a column either way: the values read
    /// straight through, and the values behind a dictionary of codes, which is the one ClickBench
    /// produces and the one that was falling through ninety seven thousand times a query.
    #[test]
    fn a_string_function_over_a_column_that_is_read_rather_than_held_stays_vectorized() {
        let values = ["Ärger", "b", "", "Straße", "http://EXAMPLE.com/Q"];
        let kept = Kept(values.iter().map(|text| text.as_bytes().to_vec()).collect());
        let read = Vector::external_text(LogicalType::Varchar, Arc::new(kept))
            .expect("the source is text");
        let coded = Vector::dictionary(vec![4, 0, 2, 1, 3, 0, 4], read.clone())
            .expect("every code names a value");
        for arg in [read, coded] {
            for (name, returns) in [
                ("lower", LogicalType::Varchar),
                ("upper", LogicalType::Varchar),
                ("length", LogicalType::BigInt),
                ("strlen", LogicalType::BigInt),
            ] {
                let form = arg.form();
                let taken = unary(name, &arg, &returns, arg.len())
                    .expect("the call is written")
                    .unwrap_or_else(|| panic!("{name} on {form:?} took the row at a time path"));
                let want = oracle(name, std::slice::from_ref(&arg), &returns)
                    .expect("the row at a time path answers");
                assert_eq!(format!("{taken:?}"), format!("{want:?}"), "{name} on {form:?}");
            }
        }
    }

    #[test]
    fn every_specialized_string_and_boolean_function_agrees_with_the_row_at_a_time_path() {
        let mut rng = Rng(0x1234_5eed_dead_beef);
        for nulls in [0, 7, 1] {
            let left = sample(&LogicalType::Varchar, 96, nulls, &mut rng);
            let right = sample(&LogicalType::Varchar, 96, nulls, &mut rng);
            for arg in forms(&left) {
                for name in ["length", "strlen"] {
                    agrees(name, std::slice::from_ref(&arg), &LogicalType::BigInt);
                }
                for name in ["lower", "upper"] {
                    agrees(name, std::slice::from_ref(&arg), &LogicalType::Varchar);
                }
            }
            for (one, other) in pairings(&left, &right) {
                agrees("||", &[one, other], &LogicalType::Varchar);
            }
            // One of each pattern shape, so that the compiled form and the general walk are both
            // checked against the walk the oracle always takes. Through `pairings` because a text
            // that is a dictionary has a loop of its own now, and per #288 the form a ClickBench
            // string column actually arrives in is that one and not the flat one.
            for spelling in ["google", "goo%", "%gle", "%oog%", "g_ogle", "%g%l%", "%", ""] {
                // A flat column of the one spelling, and `pairings` is what makes it constant. A
                // pattern that arrives here already constant would ask `pairings` for constant
                // against constant, which is the one pair it leaves out.
                let column = vec![Value::Varchar(spelling.into()); 96];
                let pattern = Vector::from_values(LogicalType::Varchar, &column).expect("builds");
                for name in ["~~", "!~~", "~~*", "!~~*"] {
                    for (text, pattern) in pairings(&left, &pattern) {
                        agrees(name, &[text, pattern], &LogicalType::Boolean);
                    }
                }
            }
            let flags = sample(&LogicalType::Boolean, 96, nulls, &mut rng);
            for arg in forms(&flags) {
                agrees("not", std::slice::from_ref(&arg), &LogicalType::Boolean);
            }
        }
    }

    /// Every part, on both the types that have a loop, against the row at a time path.
    ///
    /// The part is what decides which piece of calendar arithmetic runs, so a test that only asks
    /// for the minute is a test of one branch out of twenty. An era does not truncate and both
    /// paths have to refuse it with the same words, which is a thing `agrees` checks for free.
    #[test]
    fn every_part_of_a_date_agrees_with_the_row_at_a_time_path() {
        const PARTS: &[&str] = &[
            "year",
            "month",
            "day",
            "hour",
            "minute",
            "second",
            "millisecond",
            "microsecond",
            "week",
            "quarter",
            "dayofweek",
            "isodow",
            "dayofyear",
            "decade",
            "century",
            "millennium",
            "era",
            "isoyear",
            "yearweek",
        ];
        let mut rng = Rng(0xdead_beef_1234_5eed);
        for nulls in [0, 7, 1] {
            for ty in [LogicalType::Date, LogicalType::Timestamp] {
                let when = sample(&ty, 96, nulls, &mut rng);
                for spelling in PARTS {
                    let part = Vector::constant(
                        LogicalType::Varchar,
                        Value::Varchar((*spelling).to_owned()),
                        96,
                    );
                    for arg in forms(&when) {
                        agrees("date_part", &[part.clone(), arg.clone()], &LogicalType::BigInt);
                        agrees("date_trunc", &[part.clone(), arg], &ty);
                    }
                }
            }
        }
    }

    /// A part that changes from row to row has no loop, the same way a `LIKE` pattern that changes
    /// from row to row has none, and it still has to be right.
    #[test]
    fn a_part_that_varies_per_row_is_still_right() {
        let part = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("minute".into()), Value::Varchar("hour".into())],
        )
        .expect("two rows");
        let when = Vector::from_values(
            LogicalType::Timestamp,
            &[Value::Timestamp(13 * 3_600_000_000 + 45 * 60_000_000), Value::Timestamp(0)],
        )
        .expect("two rows");
        let found =
            call("date_part", &[part, when], &LogicalType::BigInt, None).expect("two parts");
        assert_eq!(found.value_at(0), Value::BigInt(45));
        assert_eq!(found.value_at(1), Value::BigInt(0));
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
        // The counters are per thread in a test build, so this reads its own and nothing else's.
        let before = fallback::count(Kernel::Scalar, Form::Flat, Form::Flat);
        agrees("~~", &[text, pattern], &LogicalType::Boolean);
        assert!(fallback::count(Kernel::Scalar, Form::Flat, Form::Flat) > before);
    }

    #[test]
    fn a_call_where_every_argument_is_constant_costs_one_call() {
        let left = Vector::constant(LogicalType::Integer, Value::Integer(3), 1024);
        let right = Vector::constant(LogicalType::Integer, Value::Integer(4), 1024);
        let sum = call("+", &[left, right], &LogicalType::Integer, None).expect("adds");
        assert_eq!(sum.form(), Form::Constant);
        assert_eq!(sum.len(), 1024);
        assert_eq!(sum.value_at(1000), Value::Integer(7));
    }

    /// An empty batch is still a call, and a plan that hands one down often hands down a lot of
    /// them. TPC-H q19 does it 624 times for `-` and again for `*`, and every one of those used to
    /// be counted as work the row at a time path had to do, which made the ledger read as if a
    /// third of the scalar fallbacks in the whole benchmark were real.
    #[test]
    fn a_call_on_an_empty_batch_costs_nothing_and_says_so() {
        for forms in [
            vec![
                Vector::constant(LogicalType::Integer, Value::Integer(3), 0),
                Vector::constant(LogicalType::Integer, Value::Integer(4), 0),
            ],
            vec![
                Vector::from_values(LogicalType::Integer, &[]).expect("no rows"),
                Vector::constant(LogicalType::Integer, Value::Integer(4), 0),
            ],
        ] {
            let left = forms[0].form();
            let right = forms[1].form();
            let before = fallback::count(Kernel::Scalar, left, right);
            let sum = call("+", &forms, &LogicalType::Integer, None).expect("adds nothing");
            assert_eq!(sum.len(), 0);
            assert_eq!(fallback::count(Kernel::Scalar, left, right), before);
        }
    }

    /// A dictionary's nulls come from the vector it points at rather than from its own validity,
    /// and the code at a null position still indexes a real entry. This was the property that made
    /// falling through to the row at a time path safe, and it is the property the loop needs now
    /// that there is one.
    #[test]
    fn a_dictionary_argument_reads_its_nulls_from_the_values() {
        let values = Vector::from_values(
            LogicalType::Integer,
            &[Value::Null, Value::Integer(5), Value::Integer(6)],
        )
        .expect("three values");
        let codes = Vector::dictionary(vec![0, 1, 2, 1, 0], values).expect("a dictionary");
        let ten = Vector::constant(LogicalType::Integer, Value::Integer(10), 5);
        let sum = call("+", &[codes, ten], &LogicalType::Integer, None).expect("adds");
        assert_eq!(sum.value_at(0), Value::Null);
        assert_eq!(sum.value_at(1), Value::Integer(15));
        assert_eq!(sum.value_at(4), Value::Null);
    }

    /// The two the ClickBench entry is written in terms of. The days are the ones the real data
    /// holds, since the whole point of these is that the column stores an integer and every query
    /// in the set reads a date.
    #[test]
    fn a_number_becomes_a_date_and_a_timestamp() {
        assert_eq!(
            called("make_date", &[Value::Integer(16_000)], &LogicalType::Date),
            Value::Date(16_000)
        );
        assert_eq!(
            called(
                "make_date",
                &[Value::Integer(2013), Value::Integer(7), Value::Integer(1)],
                &LogicalType::Date
            ),
            Value::Date(days_from_civil(2013, 7, 1))
        );
        assert_eq!(
            called("epoch_ms", &[Value::BigInt(1_600_000_000_000)], &LogicalType::Timestamp),
            Value::Timestamp(1_600_000_000_000_000)
        );
        assert_eq!(
            called("epoch_ms", &[Value::BigInt(-1)], &LogicalType::Timestamp),
            Value::Timestamp(-1_000)
        );
    }

    /// The round trip check, which is the whole of the calendar this function needs. Thirty
    /// February is the case that a month length table would be written for.
    #[test]
    fn a_day_that_is_not_in_its_month_is_a_date_out_of_range() {
        for (year, month, day, written) in [
            (2013, 13, 1, "2013-13-1"),
            (2013, 2, 30, "2013-2-30"),
            (0, 0, 0, "0-0-0"),
            (2013, 7, 0, "2013-7-0"),
        ] {
            let error = call_values(
                "make_date",
                &[Value::Integer(year), Value::Integer(month), Value::Integer(day)],
                &LogicalType::Date,
                None,
            )
            .expect_err("a date that is not a date");
            assert_eq!(error.message(), format!("Date out of range: {written}"));
        }
        // The leap day itself is a date, which is the other half of the round trip check.
        assert_eq!(
            called(
                "make_date",
                &[Value::Integer(2024), Value::Integer(2), Value::Integer(29)],
                &LogicalType::Date
            ),
            Value::Date(days_from_civil(2024, 2, 29))
        );
    }

    /// Milliseconds so large that microseconds do not hold them, which is the one way this can fail
    /// on data that bound.
    #[test]
    fn milliseconds_that_do_not_fit_in_microseconds_say_which_two_units_they_are() {
        let error =
            call_values("epoch_ms", &[Value::BigInt(i64::MAX)], &LogicalType::Timestamp, None)
                .expect_err("that is not a timestamp");
        assert_eq!(error.message(), "Could not convert Timestamp(MS) to Timestamp(US)");
    }

    /// The loop and the row at a time path over the same column, including the nulls, since the
    /// date one hands back the argument's own run of bytes and a mistake there would be invisible
    /// in the values and wrong in the validity.
    #[test]
    fn the_loops_for_the_two_constructors_agree_with_the_row_at_a_time_path() {
        let days = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(0), Value::Null, Value::Integer(16_000), Value::Integer(-1)],
        )
        .expect("four days");
        for arg in forms(&days) {
            agrees("make_date", &[arg], &LogicalType::Date);
        }
        let millis = Vector::from_values(
            LogicalType::BigInt,
            &[Value::BigInt(0), Value::Null, Value::BigInt(1_600_000_000_000), Value::BigInt(-1)],
        )
        .expect("four stamps");
        for arg in forms(&millis) {
            agrees("epoch_ms", &[arg], &LogicalType::Timestamp);
        }
        let overflowing =
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(i64::MAX)]).expect("one row");
        for arg in forms(&overflowing) {
            agrees("epoch_ms", &[arg], &LogicalType::Timestamp);
        }
    }

    /// The interval constructors and a timestamp moved by an interval, which is how the benchmark
    /// view turns a stored count of seconds into `EventTime`, on both paths and in every form,
    /// with a count too large to be an interval and a move past the last timestamp among them.
    #[test]
    fn the_loops_for_counted_intervals_and_moved_timestamps_agree_with_the_row_at_a_time_path() {
        let seconds = Vector::from_values(
            LogicalType::Double,
            &[Value::Double(1_373_000_000.0), Value::Null, Value::Double(2.7), Value::Double(-0.5)],
        )
        .expect("four counts");
        for arg in forms(&seconds) {
            agrees("to_seconds", &[arg], &LogicalType::Interval);
        }
        let days = Vector::from_values(
            LogicalType::BigInt,
            &[Value::BigInt(3), Value::Null, Value::BigInt(-40)],
        )
        .expect("three counts");
        for arg in forms(&days) {
            agrees("to_days", std::slice::from_ref(&arg), &LogicalType::Interval);
            agrees("to_months", &[arg], &LogicalType::Interval);
        }
        let huge = Vector::from_values(LogicalType::Double, &[Value::Double(1e300)]).expect("one");
        for arg in forms(&huge) {
            agrees("to_seconds", &[arg], &LogicalType::Interval);
        }
        let intervals = Vector::from_values(
            LogicalType::Interval,
            &[
                Value::Interval { months: 0, days: 0, micros: 1_373_000_000_000_000 },
                Value::Null,
                Value::Interval { months: 1, days: 1, micros: -5 },
                Value::Interval { months: 0, days: 0, micros: i64::MAX },
            ],
        )
        .expect("four intervals");
        let stamps = Vector::from_values(
            LogicalType::Timestamp,
            &[
                Value::Timestamp(0),
                Value::Timestamp(86_400_000_000),
                Value::Null,
                Value::Timestamp(-1),
            ],
        )
        .expect("four stamps");
        let epoch = Vector::constant(LogicalType::Timestamp, Value::Timestamp(0), 4);
        for interval in forms(&intervals) {
            for stamp in forms(&stamps).into_iter().chain([epoch.clone()]) {
                let pair = [stamp.clone(), interval.clone()];
                agrees("+", &pair, &LogicalType::Timestamp);
                agrees("-", &pair, &LogicalType::Timestamp);
                agrees("+", &[interval.clone(), stamp], &LogicalType::Timestamp);
            }
        }
    }

    #[test]
    fn an_empty_call_is_an_empty_answer() {
        let empty = Vector::from_values(LogicalType::Integer, &[]).expect("no rows");
        let sum = call("+", &[empty.clone(), empty], &LogicalType::Integer, None).expect("adds");
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
        let answer = call("%", &[left, right], &LogicalType::TinyInt, None).expect("modulo");
        assert_eq!(answer.value_at(0), Value::TinyInt(0));
    }
}

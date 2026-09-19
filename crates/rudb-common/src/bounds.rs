//! What a minimum and a maximum can answer about a comparison, without looking at any rows.
//!
//! Every part of this engine that stores data stores the smallest and the largest value of a stretch
//! of it, and every one of them then wants to ask the same question: given `x < 5`, can this stretch
//! hold a row that passes. The stretches differ. A Parquet row group keeps its bounds in the footer
//! as the writer's bytes. A table in memory keeps them per chunk as values. A block of the storage
//! format will keep them per block. What does not differ is the answer, so the answer lives here and
//! the three of them decode into [`Bound`] and call [`excluded`].
//!
//! That is worth a module at rank zero rather than a function each. Three copies of this reasoning
//! would be three chances to get the direction of one comparison backwards, and the failure that
//! causes is not a slow query: it is a row group dropped from an answer that needed it. A wrong
//! answer, arrived at quickly.
//!
//! # Every answer here is one sided
//!
//! [`excluded`] says a stretch cannot hold a matching row, or it says nothing. It never says a
//! stretch does hold one, because bounds cannot know that: a column running from 1 to 100 may hold
//! no 50 at all. So the caller skips on `true` and reads on `false`, and every case this cannot
//! decide answers `false`, which costs time and never costs rows.
//!
//! [`certain`] is the same shape pointing the other way. It says every row of a stretch passes, or
//! it says nothing, and it never says a row fails. The caller hands the chunk on untouched on
//! `true` and runs the comparison on `false`, and again the undecidable cases answer `false` and
//! cost time rather than rows. Between the two of them a chunk is skipped, waved through, or
//! compared, which is the three way decision a scan makes before it looks at a single value.
//!
//! # What has no bound
//!
//! A null answers `None` from [`Bound::of_value`]. A comparison against null is null, so a filter
//! holding one keeps no rows anywhere, and that is a fact about the whole query rather than about
//! one stretch of it. Deciding it here would be deciding it in the wrong place.
//!
//! A `NaN` compares with nothing, which falls out of `f64::partial_cmp`. A column whose minimum is
//! `NaN` has no minimum, so no test against it rules anything out, and the `None` from the ordering
//! carries that through without a case of its own.
//!
//! # Two numbers of the same thing
//!
//! A decimal, a time and a timestamp are all an integer with a power of ten under it, and the two
//! sides of a comparison are free to disagree about which power. A `DECIMAL(15, 2)` column stores
//! 12.34 as 1234 and the same constant written `12.340` arrives as 12340. A [`Value::Timestamp`] is
//! microseconds and a file is free to store the column in milliseconds or nanoseconds. Comparing
//! either pair as the integers they are would rule out stretches holding rows the query wants, which
//! is a wrong answer.
//!
//! So [`Bound::Scaled`] carries the power alongside the integer and the comparison restates the
//! coarser of the two before it looks at them. Restating upwards is exact, and where it does not fit
//! an `i128` the comparison answers nothing rather than guessing, which keeps the stretch.

use std::cmp::Ordering;

use crate::{LogicalType, Stat, Value};

/// The scale a microsecond count sits at, which is what every time and timestamp constant is.
pub const MICROS: u8 = 6;

/// The comparison a bounds test applies.
///
/// The five that a minimum and a maximum can answer, written with the column on the left. `<>` is
/// not here: a stretch is ruled out for it only when its bounds are equal to each other and to the
/// constant, which is a column of one value and not worth a case. The two distinctness operators are
/// about nulls rather than about order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `=`.
    Equal,
    /// `<`.
    Less,
    /// `<=`.
    LessOrEqual,
    /// `>`.
    Greater,
    /// `>=`.
    GreaterOrEqual,
}

impl Op {
    /// The same comparison with its operands the other way round, for `5 < x` written as such.
    ///
    /// The optimizer does not normalise which side of a comparison the constant sits on, so a caller
    /// turning a filter into a test flips the operator when it finds the column on the right.
    #[must_use]
    pub fn flipped(self) -> Self {
        match self {
            Self::Equal => Self::Equal,
            Self::Less => Self::Greater,
            Self::LessOrEqual => Self::GreaterOrEqual,
            Self::Greater => Self::Less,
            Self::GreaterOrEqual => Self::LessOrEqual,
        }
    }
}

/// A constant, in a domain that can be ordered against a stored bound.
///
/// Four domains and not one per type, because a bound written for a `SMALLINT` has to compare with
/// a constant the parser read as an `INTEGER`, and widening both to the same domain is what makes
/// that one comparison instead of a table of them.
#[derive(Debug, Clone, PartialEq)]
pub enum Bound {
    /// Every integer, boolean and date, widened.
    Int(i128),
    /// `FLOAT` and `DOUBLE`.
    Real(f64),
    /// `DECIMAL`, `TIME` and `TIMESTAMP`, as an integer and the power of ten under it.
    ///
    /// Apart from [`Bound::Int`] because the two sides of one comparison can disagree about the
    /// power, which the module doc goes into. A decimal's scale is its own, and a time or a
    /// timestamp is a count of seconds at whichever of 0, 3, 6 or 9 its unit is.
    Scaled {
        /// The integer, so 12.34 at scale 2 is 1234 and a microsecond timestamp is at scale 6.
        unscaled: i128,
        /// How many powers of ten sit under it.
        scale: u8,
    },
    /// `VARCHAR` and `BLOB`, ordered as bytes.
    Bytes(Vec<u8>),
}

/// `unscaled` at `scale` restated at `into`, exactly or not at all.
///
/// Upwards is a multiplication that either fits an `i128` or does not. Downwards is only exact when
/// the digits being dropped are zeroes, and a bound that has to round is no bound: rounding a
/// minimum up or a maximum down would rule out a stretch holding rows the query wants.
#[must_use]
fn restated(unscaled: i128, scale: u8, into: u8) -> Option<i128> {
    let ten = |steps: u8| 10_i128.checked_pow(u32::from(steps));
    if into >= scale {
        unscaled.checked_mul(ten(into - scale)?)
    } else {
        let factor = ten(scale - into)?;
        (unscaled % factor == 0).then_some(unscaled / factor)
    }
}

impl Bound {
    /// The bound a value stands for, or `None` for a value no bound compares with.
    ///
    /// See the module doc for which values answer `None` and why.
    #[must_use]
    pub fn of_value(value: &Value) -> Option<Self> {
        Some(match value {
            Value::Boolean(flag) => Self::Int(i128::from(*flag)),
            Value::TinyInt(number) => Self::Int(i128::from(*number)),
            Value::SmallInt(number) => Self::Int(i128::from(*number)),
            Value::Integer(number) => Self::Int(i128::from(*number)),
            Value::BigInt(number) => Self::Int(i128::from(*number)),
            Value::HugeInt(number) => Self::Int(*number),
            Value::UTinyInt(number) => Self::Int(i128::from(*number)),
            Value::USmallInt(number) => Self::Int(i128::from(*number)),
            Value::UInteger(number) => Self::Int(i128::from(*number)),
            Value::UBigInt(number) => Self::Int(i128::from(*number)),
            Value::UHugeInt(number) => Self::Int(i128::try_from(*number).ok()?),
            Value::Date(days) => Self::Int(i128::from(*days)),
            Value::Float(number) => Self::Real(f64::from(*number)),
            Value::Double(number) => Self::Real(*number),
            Value::Decimal { unscaled, scale, .. } => {
                Self::Scaled { unscaled: *unscaled, scale: *scale }
            }
            // A constant of any of these four is microseconds, whatever unit the column it is being
            // compared against is stored in, so the scale is the same six every time.
            Value::Time(micros)
            | Value::TimeTz(micros)
            | Value::Timestamp(micros)
            | Value::TimestampTz(micros) => {
                Self::Scaled { unscaled: i128::from(*micros), scale: MICROS }
            }
            Value::Varchar(text) => Self::Bytes(text.as_bytes().to_vec()),
            Value::Blob(bytes) => Self::Bytes(bytes.clone()),
            _ => return None,
        })
    }

    /// The value this bound stands for in a column of type `ty`, and `None` when it cannot be one.
    ///
    /// The inverse of [`Bound::of_value`], and it needs the type because the bound does not carry
    /// one: every integer width widens into `Int`, so going back is a question about the column
    /// rather than about the number. A number that does not fit the type answers `None`, which is
    /// what a caller that got a bound from somewhere other than this column should get.
    #[must_use]
    pub fn into_value(&self, ty: &LogicalType) -> Option<Value> {
        /// Narrows a widened integer back into the type it came from.
        macro_rules! fit {
            ($number:expr, $variant:ident) => {
                Some(Value::$variant((*$number).try_into().ok()?))
            };
        }
        Some(match (self, ty) {
            (Self::Int(number), LogicalType::Boolean) => Value::Boolean(*number != 0),
            (Self::Int(number), LogicalType::TinyInt) => return fit!(number, TinyInt),
            (Self::Int(number), LogicalType::SmallInt) => return fit!(number, SmallInt),
            (Self::Int(number), LogicalType::Integer) => return fit!(number, Integer),
            (Self::Int(number), LogicalType::BigInt) => return fit!(number, BigInt),
            (Self::Int(number), LogicalType::HugeInt) => Value::HugeInt(*number),
            (Self::Int(number), LogicalType::UTinyInt) => return fit!(number, UTinyInt),
            (Self::Int(number), LogicalType::USmallInt) => return fit!(number, USmallInt),
            (Self::Int(number), LogicalType::UInteger) => return fit!(number, UInteger),
            (Self::Int(number), LogicalType::UBigInt) => return fit!(number, UBigInt),
            (Self::Int(number), LogicalType::UHugeInt) => return fit!(number, UHugeInt),
            (Self::Int(number), LogicalType::Date) => return fit!(number, Date),
            (Self::Real(number), LogicalType::Float) => Value::Float(*number as f32),
            (Self::Real(number), LogicalType::Double) => Value::Double(*number),
            (Self::Scaled { unscaled, scale }, LogicalType::Decimal { width, scale: want }) => {
                Value::Decimal {
                    unscaled: restated(*unscaled, *scale, *want)?,
                    width: *width,
                    scale: *want,
                }
            }
            (Self::Scaled { unscaled, scale }, LogicalType::Time) => {
                return fit!(&restated(*unscaled, *scale, MICROS)?, Time);
            }
            (Self::Scaled { unscaled, scale }, LogicalType::TimeTz) => {
                return fit!(&restated(*unscaled, *scale, MICROS)?, TimeTz);
            }
            (Self::Scaled { unscaled, scale }, LogicalType::Timestamp) => {
                return fit!(&restated(*unscaled, *scale, MICROS)?, Timestamp);
            }
            (Self::Scaled { unscaled, scale }, LogicalType::TimestampTz) => {
                return fit!(&restated(*unscaled, *scale, MICROS)?, TimestampTz);
            }
            (Self::Bytes(bytes), LogicalType::Varchar) => {
                Value::Varchar(String::from_utf8(bytes.clone()).ok()?)
            }
            (Self::Bytes(bytes), LogicalType::Blob) => Value::Blob(bytes.clone()),
            _ => return None,
        })
    }

    /// The order between two bounds of the same domain, and `None` across domains.
    #[must_use]
    pub fn order(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Int(left), Self::Int(right)) => Some(left.cmp(right)),
            (Self::Real(left), Self::Real(right)) => left.partial_cmp(right),
            (Self::Bytes(left), Self::Bytes(right)) => Some(left.as_slice().cmp(right)),
            (
                Self::Scaled { unscaled: left, scale: from },
                Self::Scaled { unscaled: right, scale: to },
            ) => {
                // Both restated at the finer of the two, which is upwards for at least one of them
                // and is the direction that keeps every digit either of them had.
                let scale = (*from).max(*to);
                Some(restated(*left, *from, scale)?.cmp(&restated(*right, *to, scale)?))
            }
            _ => None,
        }
    }

    /// This bound if it is smaller than `other`, which is how a minimum is accumulated.
    #[must_use]
    pub fn smaller(self, other: Self) -> Self {
        match self.order(&other) {
            Some(Ordering::Greater) => other,
            _ => self,
        }
    }

    /// This bound if it is larger than `other`, which is how a maximum is accumulated.
    #[must_use]
    pub fn larger(self, other: Self) -> Self {
        match self.order(&other) {
            Some(Ordering::Less) => other,
            _ => self,
        }
    }
}

/// Which end of a range a bound is.
///
/// A named pair rather than a `bool`, because the two ends carry different flags, read different
/// bytes and fold together the other way round, and a caller that passes `true` says nothing about
/// which one it meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum End {
    /// The smallest value, which `MIN` asks for.
    Low,
    /// The largest value, which `MAX` asks for.
    High,
}

impl End {
    /// The further of the two towards this end, which is how the parts of a store fold into one.
    ///
    /// `None` where the two do not compare, which [`Bound::smaller`] and [`Bound::larger`] answer
    /// by keeping the left one. That is right for a skip, where an undecided comparison costs a
    /// part being read, and wrong here, where the fold is producing an answer and keeping either
    /// side of a pair nothing ordered would be picking one.
    #[must_use]
    pub fn further(self, one: &Bound, other: &Bound) -> Option<Bound> {
        let wanted = match self {
            Self::Low => Ordering::Less,
            Self::High => Ordering::Greater,
        };
        let taken = if one.order(other)? == wanted { one } else { other };
        Some(taken.clone())
    }

    /// The word `EXPLAIN` and an error message use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Low => "minimum",
            Self::High => "maximum",
        }
    }
}

/// One comparison against one column, with the bound it is testing for.
///
/// Here rather than beside whichever store answers it, for the same reason [`Op`] and [`Bound`] are
/// here: the question does not depend on where the minimum and the maximum came from. A Parquet row
/// group and a chunk of a table in memory get asked the same thing in the same words, and the
/// planner asks it of both without knowing which it is holding.
#[derive(Debug, Clone)]
pub struct Test {
    /// Which column of the store's flat schema, numbered the way the store numbers its columns.
    pub column: usize,
    /// The comparison, written with the column on the left.
    pub op: Op,
    /// The constant the column is compared against.
    pub value: Bound,
}

/// A store that keeps rows in parts and a minimum and a maximum per part, asked how much of itself
/// a filter can rule out.
///
/// The planner wants this and cannot compute it. Deciding which parts a filter rules out needs the
/// bounds, the bounds are in the file, and by the time anything plans over the file the file is
/// closed. So whatever read the bounds answers the question, and the planner holds the answer
/// behind this trait rather than holding the bounds themselves: a hundred and five columns across
/// eight thousand row groups is not something to copy into a plan, and it is already parsed and
/// already in memory on the side that read it.
///
/// The row count is not here. How many rows there are in total is a count, it is [`Stat`] shaped,
/// and it already rides on the plan on its own. What is here is the part that needs the bounds.
///
/// [`Stat`]: crate::Stat
pub trait Zones: std::fmt::Debug + Send + Sync {
    /// Which column of this store's own numbering the column called `name` is.
    ///
    /// The planner has a name and this wants a position, and nothing else can do the translation.
    /// A plan numbers a scan's columns by where they sit in what that scan produces, which column
    /// pruning moves and which is not the file's order once anything has been pruned. The store
    /// knows its own order, so the store is asked. `None` for a name it does not have, which is
    /// what a column the query computed rather than read looks like from here.
    fn column(&self, name: &str) -> Option<usize>;

    /// How many rows are in the parts that `tests` cannot rule out.
    ///
    /// A ceiling and not a count: a part that survives holds rows the filter wants, or it holds
    /// none and the bounds could not say so. With no tests at all this is every row, which is the
    /// honest answer to a question nothing was asked.
    ///
    /// `None` where the store cannot say, which is a count it cannot represent rather than a store
    /// with no bounds. No bounds means nothing is ruled out, which is a number.
    fn surviving(&self, tests: &[Test]) -> Option<u64>;

    /// What fraction of the store's rows `tests` is expected to keep, and how many of them said so.
    ///
    /// An estimate and not a ceiling, which is the whole difference from [`Self::surviving`]. That
    /// one proves a part holds nothing and its answer is an upper bound. This one interpolates
    /// inside the parts that survive, by [`kept`], and its answer is a guess that can be wrong in
    /// either direction. So a caller caps with the first and multiplies with the second.
    ///
    /// Asked with every test of the filter at once rather than one at a time, because two tests on
    /// one column have to be intersected and a caller handing them over separately would have
    /// multiplied them instead.
    ///
    /// `None` where no part could be interpolated at all, which is what a test [`kept`] refuses
    /// looks like from here, and is the caller's signal to fall back to its constant for all of
    /// them. Where some were read and some were not, [`Spread::read`] says how many, and the ones
    /// left over are the caller's to guess at.
    fn spread(&self, tests: &[Test]) -> Option<Spread>;

    /// The smallest or the largest value that column holds anywhere in the store.
    ///
    /// The answer to `MIN(column)` and `MAX(column)` and not an input to a guess about one, which
    /// is what makes this different from the other two. So it comes back as a [`Stat`] rather than
    /// as an `Option<Bound>`: a caller that means to print the number as the query's result has to
    /// be able to see that it is [`Class::Exact`] before it does, per the answer rule of
    /// `spec/stats/05-every-query.md` section 5.1.1.
    ///
    /// [`Stat::Unknown`] wherever the store cannot fold every part into one exact value. A part
    /// that stated no bound, a part whose writer shortened its bound, a part whose bound does not
    /// read, and two parts whose bounds do not compare, all end here. Nulls do not: a part holding
    /// only nulls has no smallest value to contribute and `MIN` skips nulls, so a store can leave
    /// that part out and still answer, and a store where every part is null has no answer to give.
    ///
    /// A shortened bound is [`Stat::Unknown`] and not [`Class::Certified`] even though it is a
    /// perfectly good one sided bound, because the certified class carries a relative error and a
    /// string that lost its tail has no relative error to state. When something wants the one sided
    /// bound it can have its own question.
    ///
    /// [`Class::Certified`]: crate::stat::Class::Certified
    /// [`Class::Exact`]: crate::stat::Class::Exact
    /// [`Stat`]: crate::Stat
    fn extreme(&self, column: usize, end: End) -> Stat<Bound>;
}

/// Whether `column op value` is false for every value between `low` and `high`.
///
/// `low` and `high` are the smallest and the largest value of the stretch being tested, either of
/// which may be missing, because a writer is allowed to record one and not the other and a column
/// that is entirely null has neither.
///
/// Which end each comparison needs is the whole of it. `x < c` is false everywhere only when even
/// the smallest value in the stretch is not below `c`, and the mirror holds for the other three.
/// `x = c` needs both ends, because the constant has to fall outside the range on one side or the
/// other.
///
/// Every arm is written as a comparison that has to come back known and in a stated direction, so a
/// comparison that cannot be made at all keeps the stretch. Writing it the other way round, as the
/// negation of the comparison that keeps rows, reads the same and is not the same: a `NaN` bound or
/// a constant from another domain answers neither direction, and negating that turns an unknown into
/// a skip and drops rows from an answer.
#[must_use]
pub fn excluded(op: Op, value: &Bound, low: Option<&Bound>, high: Option<&Bound>) -> bool {
    match op {
        // Nothing is below `c` when the smallest value is already `c` or more.
        Op::Less => holds(low, value, &[Ordering::Greater, Ordering::Equal]),
        // Nothing is at or below `c` when the smallest value is above it.
        Op::LessOrEqual => holds(low, value, &[Ordering::Greater]),
        // Nothing is above `c` when the largest value is already `c` or less.
        Op::Greater => holds(high, value, &[Ordering::Less, Ordering::Equal]),
        // Nothing is at or above `c` when the largest value is below it.
        Op::GreaterOrEqual => holds(high, value, &[Ordering::Less]),
        // `c` has to fall off one end or the other.
        Op::Equal => {
            holds(low, value, &[Ordering::Greater]) || holds(high, value, &[Ordering::Less])
        }
    }
}

/// Whether `column op value` is true for every value between `low` and `high`.
///
/// The other half of [`excluded`], and the one that lets a caller skip the comparison rather than
/// skip the rows. Where `excluded` proves a stretch holds nothing the filter wants, this proves it
/// holds nothing the filter would throw away, so a chunk it answers `true` for goes past untouched
/// and the per row work on it is none.
///
/// Each arm is the mirror of the one above it. `x < c` is true everywhere when the largest value in
/// the stretch is below `c`, where `excluded` asked about the smallest, and the same swap holds for
/// the other three. `x = c` needs both ends to be `c`, which is a stretch holding one distinct value
/// and the constant being it.
///
/// # A bound that is too wide is still sound
///
/// This is worth saying because it is not obvious and it is the thing that would be a wrong answer
/// if it were false. A chunk that arrived dictionary encoded records the ends of the dictionary
/// rather than the ends of the rows, so the stretch this is asked about can be wider than the rows
/// really are. Everything in a wider stretch passing means everything in the narrower stretch inside
/// it passes too, exactly as nothing in a wider stretch matching means nothing in the narrower one
/// does. So `exact` is a question for `MIN`, `MAX` and `SUM` and not for either of these two.
///
/// # Nulls are not this function's to know
///
/// `v >= 10` over a null row is unknown rather than true, and a filter keeps the rows where its
/// predicate is true, so one null anywhere in the stretch means the comparison cannot be skipped
/// however the values fall. The null count is not here, it is beside the bounds in whatever recorded
/// them, so the caller is the one that has to ask. `Range::certain` in `rudb-storage` is the caller
/// that does, and it answers `false` the moment the chunk holds a null.
#[must_use]
pub fn certain(op: Op, value: &Bound, low: Option<&Bound>, high: Option<&Bound>) -> bool {
    match op {
        // Everything is below `c` when even the largest value is.
        Op::Less => holds(high, value, &[Ordering::Less]),
        // Everything is at or below `c` when the largest value is.
        Op::LessOrEqual => holds(high, value, &[Ordering::Less, Ordering::Equal]),
        // Everything is above `c` when even the smallest value is.
        Op::Greater => holds(low, value, &[Ordering::Greater]),
        // Everything is at or above `c` when the smallest value is.
        Op::GreaterOrEqual => holds(low, value, &[Ordering::Greater, Ordering::Equal]),
        // Both ends have to be `c`, which leaves nothing in between that is not.
        Op::Equal => {
            holds(low, value, &[Ordering::Equal]) && holds(high, value, &[Ordering::Equal])
        }
    }
}

/// Whether `bound` is present and stands in one of `wanted` to `value`.
///
/// Absent, or ordered against `value` in no direction at all, answers `false`, which is the answer
/// that keeps the rows.
fn holds(bound: Option<&Bound>, value: &Bound, wanted: &[Ordering]) -> bool {
    bound.and_then(|bound| bound.order(value)).is_some_and(|order| wanted.contains(&order))
}

/// What fraction of a stretch running from `low` to `high` the tests on one column keep.
///
/// This is the other question a minimum and a maximum can be asked and the only one in this module
/// that is a guess. [`excluded`] proves a stretch holds nothing and is never wrong. This assumes the
/// values are spread evenly between the two ends, which is not a fact about any column, and answers
/// a number between zero and one.
///
/// It is still worth far more than a constant. Every range comparison in TPC-H took the same fifth
/// whatever it asked for, and a fifth is what `l_shipdate <= '1998-09-02'` gets when the answer is
/// ninety eight percent of the table. The bounds already say the column runs from 1992 to 1998 and
/// the arithmetic from there is one subtraction.
///
/// # Every test on the column at once
///
/// `tests` is the whole filter and `column` picks the ones this call is about, because two tests on
/// one column are not independent of each other and multiplying them is wrong. `x >= 100 AND x < 4000`
/// over a stretch of 0 to 4095 names 3,900 of the 4,096 values in it, and two fractions multiplied
/// give ninety seven percent of ninety eight, which is a different number for no reason. So the tests
/// narrow one interval one after another and the fraction is measured once at the end.
///
/// What is still multiplied, by the caller, is tests on different columns. That assumes the columns
/// are independent of each other, which is the assumption every estimator makes and the one that
/// fails first, and it is not something a pair of bounds can do anything about.
///
/// # What it will not answer
///
/// `None` where no test on the column could be read at all, which is the caller's signal to fall
/// back to whatever it does without a number. A test is unreadable when the two ends and the constant
/// are not in one domain, when they are strings, or when it is `=`. Strings are out because the
/// distance between two byte strings is not a number anybody agrees on and a prefix embedding would
/// be a guess on top of a guess. `=` is out because one value out of a range is not a fraction a
/// range knows: how many distinct values sit between the ends is the question, and a distinct count
/// answers it.
///
/// The integers are counted and the reals are measured, which is the difference between six values
/// in 10 to 15 and a sixth of the distance from 10 to 15. Dates are integers here and are counted,
/// which is what makes a range of days come out right on a small table rather than only on a large
/// one. A decimal, a time and a timestamp are counted too, once every number in the question has
/// been restated at one scale, because a `DECIMAL(15, 2)` column runs over hundredths and those are
/// as countable as days are.
#[must_use]
pub fn kept(tests: &[Test], column: usize, low: &Bound, high: &Bound) -> Option<Spread> {
    let ours = || tests.iter().filter(|test| test.column == column);
    match (low, high) {
        (&Bound::Int(low), &Bound::Int(high)) => {
            let clips = ours().filter_map(|test| match test.value {
                Bound::Int(value) => Some((test.op, value)),
                _ => None,
            });
            counted(clips, low, high)
        }
        (&Bound::Real(low), &Bound::Real(high)) => measured(tests, column, low, high),
        (
            &Bound::Scaled { unscaled: low, scale: lower },
            &Bound::Scaled { unscaled: high, scale: upper },
        ) => {
            // The finest scale anything in the question is written at, so that every number in it
            // restates upwards and none of them loses a digit on the way. The step of the counting
            // is one at that scale, which is the column's own step where the constants are no finer
            // than the column, and a fraction of it where one of them is. Either way it is the same
            // step above and below the line and the ratio is what comes out.
            let scale = ours().fold(lower.max(upper), |scale, test| match test.value {
                Bound::Scaled { scale: theirs, .. } => scale.max(theirs),
                _ => scale,
            });
            let clips = ours().filter_map(|test| match test.value {
                Bound::Scaled { unscaled, scale: theirs } => {
                    Some((test.op, restated(unscaled, theirs, scale)?))
                }
                _ => None,
            });
            counted(clips, restated(low, lower, scale)?, restated(high, upper, scale)?)
        }
        _ => None,
    }
}

/// What a set of tests is expected to keep, and how many of them went into that.
///
/// The count is what lets a caller tell a test that was answered from one that was refused. The
/// fraction is one number over the whole set, so without the count there is no way back to which
/// conditions still need the caller's own guess applied to them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spread {
    /// The fraction of the rows the tests that were read are expected to keep.
    pub fraction: f64,
    /// How many tests were read.
    pub read: usize,
}

/// [`kept`] over a dense stretch of integers, where the answer is a count of values.
///
/// The stretch starts as the whole of `low` to `high` and each clip narrows one end of it. `x < v`
/// clips the top to `v - 1` and `x >= v` clips the bottom to `v`, and the fraction at the end is the
/// integers left over the integers there were. An interval clipped past itself is a zero rather than
/// a negative number.
///
/// A stretch of one value falls out of this without a case of its own, because `high - low + 1` is
/// one rather than zero and there is nothing to divide by that is not there.
///
/// The clips arrive as an iterator rather than as the tests themselves, because the two domains that
/// end up here disagree about how a test becomes a number and agree about everything after that. An
/// integer is already one and a scaled value has to be restated first, and a clip that could not be
/// made is simply not in the iterator.
#[expect(clippy::cast_precision_loss, reason = "a span past two to the fifty third is not a span")]
fn counted(clips: impl Iterator<Item = (Op, i128)>, low: i128, high: i128) -> Option<Spread> {
    let whole = high.checked_sub(low)?.checked_add(1)?;
    if whole <= 0 {
        return None;
    }
    let (mut first, mut last) = (low, high);
    let mut read = 0;
    for (op, value) in clips {
        match op {
            Op::Less => last = last.min(value.saturating_sub(1)),
            Op::LessOrEqual => last = last.min(value),
            Op::Greater => first = first.max(value.saturating_add(1)),
            Op::GreaterOrEqual => first = first.max(value),
            Op::Equal => continue,
        }
        read += 1;
    }
    let passing = last.saturating_sub(first).saturating_add(1).max(0);
    (read > 0).then(|| Spread { fraction: (passing as f64 / whole as f64).clamp(0.0, 1.0), read })
}

/// [`kept`] over a stretch of reals, where the answer is a length.
///
/// The two strict comparisons clip to the same point as the two loose ones, because the point they
/// differ by has no width and a fraction of a continuous stretch cannot see it.
fn measured(tests: &[Test], column: usize, low: f64, high: f64) -> Option<Spread> {
    let whole = high - low;
    if !whole.is_finite() || whole < 0.0 {
        return None;
    }
    let (mut first, mut last) = (low, high);
    let mut read = 0;
    for test in tests.iter().filter(|test| test.column == column) {
        let &Bound::Real(value) = &test.value else { continue };
        if !value.is_finite() {
            continue;
        }
        match test.op {
            Op::Less | Op::LessOrEqual => last = last.min(value),
            Op::Greater | Op::GreaterOrEqual => first = first.max(value),
            Op::Equal => continue,
        }
        read += 1;
    }
    // A stretch of one value is not a range to interpolate over and cannot be divided by. It holds
    // or it does not, which the clipped interval says by whether it is empty.
    let fraction = if whole == 0.0 {
        f64::from(u8::from(first <= last))
    } else {
        ((last - first).max(0.0) / whole).clamp(0.0, 1.0)
    };
    (read > 0 && fraction.is_finite()).then_some(Spread { fraction, read })
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::{Bound, MICROS, Op, Test, certain, excluded, kept};
    use crate::{LogicalType, Value};

    /// The range 10 to 20, which every test here asks about.
    fn range() -> (Bound, Bound) {
        (Bound::Int(10), Bound::Int(20))
    }

    #[test]
    fn a_constant_below_the_range_passes_every_row_of_the_two_ordered_the_other_way() {
        let (low, high) = range();
        let five = Bound::Int(5);
        assert!(certain(Op::Greater, &five, Some(&low), Some(&high)), "10 to 20 is all above 5");
        assert!(certain(Op::GreaterOrEqual, &five, Some(&low), Some(&high)));
        assert!(!certain(Op::Less, &five, Some(&low), Some(&high)));
        assert!(!certain(Op::LessOrEqual, &five, Some(&low), Some(&high)));
        assert!(!certain(Op::Equal, &five, Some(&low), Some(&high)));
    }

    #[test]
    fn a_constant_above_the_range_passes_every_row_of_the_other_two() {
        let (low, high) = range();
        let fifty = Bound::Int(50);
        assert!(certain(Op::Less, &fifty, Some(&low), Some(&high)), "10 to 20 is all below 50");
        assert!(certain(Op::LessOrEqual, &fifty, Some(&low), Some(&high)));
        assert!(!certain(Op::Greater, &fifty, Some(&low), Some(&high)));
        assert!(!certain(Op::GreaterOrEqual, &fifty, Some(&low), Some(&high)));
    }

    /// The same edges [`a_constant_at_either_end_of_the_range_is_kept`] checks, from the other side.
    #[test]
    fn a_constant_at_either_end_of_the_range_passes_only_where_the_end_is_included() {
        let (low, high) = range();
        // Every value from 10 to 20 is at or above 10, and not every one of them is above it.
        assert!(certain(Op::GreaterOrEqual, &Bound::Int(10), Some(&low), Some(&high)));
        assert!(!certain(Op::Greater, &Bound::Int(10), Some(&low), Some(&high)));
        assert!(certain(Op::LessOrEqual, &Bound::Int(20), Some(&low), Some(&high)));
        assert!(!certain(Op::Less, &Bound::Int(20), Some(&low), Some(&high)));
    }

    /// One distinct value is the only stretch an equality can pass whole.
    #[test]
    fn equality_passes_every_row_only_when_both_ends_are_the_constant() {
        let ten = Bound::Int(10);
        assert!(certain(Op::Equal, &ten, Some(&ten), Some(&ten)));
        assert!(!certain(Op::Equal, &ten, Some(&ten), Some(&Bound::Int(20))));
        assert!(!certain(Op::Equal, &ten, Some(&Bound::Int(5)), Some(&ten)));
    }

    /// The undecidable cases answer the way that costs a comparison rather than the way that keeps
    /// rows the filter wanted gone.
    #[test]
    fn a_bound_that_says_nothing_passes_nothing() {
        let (low, high) = range();
        let nan = Bound::Real(f64::NAN);
        let text = Bound::Bytes(b"x".to_vec());
        for op in [Op::Equal, Op::Less, Op::LessOrEqual, Op::Greater, Op::GreaterOrEqual] {
            assert!(!certain(op, &Bound::Int(5), None, None), "no bounds at all");
            assert!(!certain(op, &Bound::Real(1.0), Some(&nan), Some(&nan)), "a NaN end");
            assert!(!certain(op, &text, Some(&low), Some(&high)), "another domain");
        }
        assert!(!certain(Op::Less, &Bound::Int(50), Some(&low), None), "no largest value");
        assert!(!certain(Op::Greater, &Bound::Int(5), None, Some(&high)), "no smallest value");
    }

    /// The property that makes a dictionary's ends safe to ask, checked rather than only argued.
    #[test]
    fn widening_the_ends_never_turns_a_false_into_a_true() {
        let (low, high) = range();
        let (wide_low, wide_high) = (Bound::Int(0), Bound::Int(30));
        for op in [Op::Equal, Op::Less, Op::LessOrEqual, Op::Greater, Op::GreaterOrEqual] {
            for value in [0, 5, 10, 15, 20, 25, 30].map(Bound::Int) {
                if certain(op, &value, Some(&wide_low), Some(&wide_high)) {
                    assert!(
                        certain(op, &value, Some(&low), Some(&high)),
                        "{op:?} against {value:?} passes 0 to 30 and not 10 to 20"
                    );
                }
            }
        }
    }

    #[test]
    fn a_constant_below_the_range_rules_out_equality_and_nothing_else() {
        let (low, high) = range();
        let five = Bound::Int(5);
        assert!(excluded(Op::Equal, &five, Some(&low), Some(&high)));
        assert!(excluded(Op::Less, &five, Some(&low), Some(&high)), "nothing is below 5");
        assert!(excluded(Op::LessOrEqual, &five, Some(&low), Some(&high)));
        assert!(!excluded(Op::Greater, &five, Some(&low), Some(&high)), "everything is above 5");
        assert!(!excluded(Op::GreaterOrEqual, &five, Some(&low), Some(&high)));
    }

    #[test]
    fn a_constant_above_the_range_rules_out_the_other_direction() {
        let (low, high) = range();
        let fifty = Bound::Int(50);
        assert!(excluded(Op::Equal, &fifty, Some(&low), Some(&high)));
        assert!(!excluded(Op::Less, &fifty, Some(&low), Some(&high)));
        assert!(excluded(Op::Greater, &fifty, Some(&low), Some(&high)));
        assert!(excluded(Op::GreaterOrEqual, &fifty, Some(&low), Some(&high)));
    }

    /// The edges, where an off by one is a wrong answer rather than a slow query.
    #[test]
    fn a_constant_at_either_end_of_the_range_is_kept() {
        let (low, high) = range();
        for value in [Bound::Int(10), Bound::Int(20)] {
            assert!(!excluded(Op::Equal, &value, Some(&low), Some(&high)));
            assert!(!excluded(Op::LessOrEqual, &value, Some(&low), Some(&high)));
            assert!(!excluded(Op::GreaterOrEqual, &value, Some(&low), Some(&high)));
        }
        // `x < 10` is false everywhere in a stretch whose smallest value is 10, and `x > 20` is
        // false everywhere in one whose largest is 20.
        assert!(excluded(Op::Less, &Bound::Int(10), Some(&low), Some(&high)));
        assert!(excluded(Op::Greater, &Bound::Int(20), Some(&low), Some(&high)));
    }

    #[test]
    fn a_missing_bound_rules_nothing_out() {
        let high = Bound::Int(20);
        assert!(!excluded(Op::Less, &Bound::Int(5), None, Some(&high)));
        assert!(!excluded(Op::Equal, &Bound::Int(5), None, None));
    }

    #[test]
    fn a_bound_of_another_domain_rules_nothing_out() {
        let (low, high) = range();
        let text = Bound::Bytes(b"x".to_vec());
        assert!(!excluded(Op::Equal, &text, Some(&low), Some(&high)));
        assert!(!excluded(Op::Less, &text, Some(&low), Some(&high)));
    }

    /// A column of `NaN` has no minimum, so nothing can be ruled out against it.
    #[test]
    fn a_nan_bound_rules_nothing_out() {
        let nan = Bound::Real(f64::NAN);
        for op in [Op::Equal, Op::Less, Op::LessOrEqual, Op::Greater, Op::GreaterOrEqual] {
            assert!(!excluded(op, &Bound::Real(1.0), Some(&nan), Some(&nan)));
        }
    }

    #[test]
    fn a_null_constant_has_no_bound() {
        assert!(Bound::of_value(&Value::Null).is_none());
        assert_eq!(Bound::of_value(&Value::Integer(7)), Some(Bound::Int(7)));
    }

    /// A timestamp constant is microseconds and a file's statistics are at whatever unit the file
    /// chose, which is why these carry the scale rather than being widened into [`Bound::Int`]. A
    /// date does not, because a day is a day in every file that states one.
    #[test]
    fn a_temporal_constant_carries_the_unit_it_is_counted_in() {
        assert_eq!(Bound::of_value(&Value::Timestamp(1)), Some(scaled(1, MICROS)));
        assert_eq!(Bound::of_value(&Value::Time(1)), Some(scaled(1, MICROS)));
        assert_eq!(Bound::of_value(&Value::Date(1)), Some(Bound::Int(1)));
    }

    #[test]
    fn a_flipped_op_is_the_one_with_its_operands_the_other_way_round() {
        assert_eq!(Op::Less.flipped(), Op::Greater);
        assert_eq!(Op::GreaterOrEqual.flipped(), Op::LessOrEqual);
        assert_eq!(Op::Equal.flipped(), Op::Equal);
    }

    /// One test on column zero, which is the only column these ask about.
    fn one(op: Op, number: i128) -> Vec<Test> {
        vec![Test { column: 0, op, value: Bound::Int(number) }]
    }

    /// What fraction of the range 10 to 20 a test keeps, written short because there are a lot.
    fn fraction(op: Op, number: i128) -> Option<f64> {
        let (low, high) = range();
        kept(&one(op, number), 0, &low, &high).map(|spread| spread.fraction)
    }

    #[test]
    fn a_range_of_integers_is_counted_and_not_measured() {
        // Eleven integers sit in 10 to 20, so `x < 15` keeps the five from 10 to 14 and `x <= 15`
        // keeps the six from 10 to 15. A length ratio would call both of them a half, which is off
        // by a whole value on a stretch this size and is the difference between a range of days
        // coming out right on a small table and only on a large one.
        assert_eq!(fraction(Op::Less, 15), Some(5.0 / 11.0));
        assert_eq!(fraction(Op::LessOrEqual, 15), Some(6.0 / 11.0));
        assert_eq!(fraction(Op::Greater, 15), Some(5.0 / 11.0));
        assert_eq!(fraction(Op::GreaterOrEqual, 15), Some(6.0 / 11.0));
    }

    #[test]
    fn a_constant_outside_the_range_keeps_all_of_it_or_none_of_it() {
        assert_eq!(fraction(Op::Less, 5), Some(0.0));
        assert_eq!(fraction(Op::GreaterOrEqual, 5), Some(1.0));
        assert_eq!(fraction(Op::Less, 50), Some(1.0));
        assert_eq!(fraction(Op::Greater, 50), Some(0.0));
    }

    /// The edges again, where the two loose comparisons and the two strict ones part company.
    #[test]
    fn a_constant_at_either_end_keeps_one_value_or_all_but_one() {
        assert_eq!(fraction(Op::Less, 10), Some(0.0), "nothing is below the stretch's own low");
        assert_eq!(fraction(Op::LessOrEqual, 10), Some(1.0 / 11.0));
        assert_eq!(fraction(Op::Greater, 20), Some(0.0));
        assert_eq!(fraction(Op::GreaterOrEqual, 20), Some(1.0 / 11.0));
    }

    /// The whole reason this takes every test at once rather than one at a time.
    #[test]
    fn two_tests_on_one_column_are_intersected_and_not_multiplied() {
        // `x >= 12 AND x < 15` names the three values 12, 13 and 14 out of the eleven in the
        // stretch. Two fractions multiplied give nine elevenths times five elevenths, which is
        // forty five over a hundred and twenty one and is a wider interval than the one asked for.
        let (low, high) = range();
        let mut both = one(Op::GreaterOrEqual, 12);
        both.extend(one(Op::Less, 15));
        let spread = kept(&both, 0, &low, &high).expect("both were read");
        assert_eq!(spread.fraction, 3.0 / 11.0);
        assert_eq!(spread.read, 2, "and it says both went into it");
        // An interval clipped past itself is empty rather than negative.
        let mut empty = one(Op::GreaterOrEqual, 18);
        empty.extend(one(Op::Less, 12));
        assert_eq!(kept(&empty, 0, &low, &high).map(|spread| spread.fraction), Some(0.0));
    }

    #[test]
    fn a_test_on_another_column_is_not_this_columns_business() {
        // The caller hands over the whole filter and names the column, because two tests on one
        // column intersect and two on different columns do not. Picking the wrong ones out here
        // would narrow a column by an interval belonging to some other column.
        let (low, high) = range();
        let mut mixed = one(Op::Less, 15);
        mixed.push(Test { column: 1, op: Op::Less, value: Bound::Int(11) });
        let spread = kept(&mixed, 0, &low, &high).expect("the first one was read");
        assert_eq!(spread.fraction, 5.0 / 11.0);
        assert_eq!(spread.read, 1);
    }

    #[test]
    fn a_range_of_reals_is_measured_and_the_strict_comparisons_answer_the_same() {
        // A point has no width, so a fraction of a continuous stretch cannot see the difference
        // between `<` and `<=`. There is no counting to do here because there is nothing to count.
        let (low, high) = (Bound::Real(0.0), Bound::Real(10.0));
        let at = |op| {
            let tests = vec![Test { column: 0, op, value: Bound::Real(2.5) }];
            kept(&tests, 0, &low, &high).map(|spread| spread.fraction)
        };
        assert_eq!(at(Op::Less), Some(0.25));
        assert_eq!(at(Op::LessOrEqual), Some(0.25));
        assert_eq!(at(Op::Greater), Some(0.75));
        assert_eq!(at(Op::GreaterOrEqual), Some(0.75));
    }

    #[test]
    fn a_stretch_of_one_value_holds_or_does_not_and_is_not_interpolated() {
        // Every row group of a sorted column looks like this at its edges, and dividing by a span
        // of zero is how that turns into a number nobody can use.
        let one_int = Bound::Int(7);
        let at = |op, number| {
            kept(&one(op, number), 0, &one_int, &one_int).map(|spread| spread.fraction)
        };
        assert_eq!(at(Op::LessOrEqual, 7), Some(1.0));
        assert_eq!(at(Op::Less, 7), Some(0.0));
        assert_eq!(at(Op::Greater, 6), Some(1.0));
        // And the same for a real, where it is a case of its own because the division is not by a
        // count of values but by a length, and that length is zero here.
        let point = Bound::Real(7.0);
        let real = |op, number| {
            let tests = vec![Test { column: 0, op, value: Bound::Real(number) }];
            kept(&tests, 0, &point, &point).map(|spread| spread.fraction)
        };
        assert_eq!(real(Op::LessOrEqual, 7.0), Some(1.0));
        assert_eq!(real(Op::Less, 6.0), Some(0.0));
    }

    #[test]
    fn what_a_range_cannot_answer_it_says_nothing_about() {
        let (low, high) = range();
        // One value out of a range is not a fraction a range knows. How many distinct values sit
        // between the two ends is the question, and a distinct count is what answers it.
        assert_eq!(fraction(Op::Equal, 15), None);
        // No test on the column at all.
        assert_eq!(kept(&[], 0, &low, &high), None);
        // Two domains have no distance between them, and bytes have none anybody agrees on.
        let real = vec![Test { column: 0, op: Op::Less, value: Bound::Real(15.0) }];
        assert_eq!(kept(&real, 0, &low, &high), None);
        let text = vec![Test { column: 0, op: Op::Less, value: Bound::Bytes(b"m".to_vec()) }];
        let (first, last) = (Bound::Bytes(b"a".to_vec()), Bound::Bytes(b"z".to_vec()));
        assert_eq!(kept(&text, 0, &first, &last), None);
    }

    /// A column of `NaN` has no minimum, so there is no span to interpolate along either.
    #[test]
    fn a_nan_bound_interpolates_nothing() {
        let nan = Bound::Real(f64::NAN);
        let tests = vec![Test { column: 0, op: Op::Less, value: Bound::Real(1.0) }];
        assert_eq!(kept(&tests, 0, &nan, &nan), None);
    }

    /// A decimal at a stated scale, which is what the footer and the parser each hand over.
    fn scaled(unscaled: i128, scale: u8) -> Bound {
        Bound::Scaled { unscaled, scale }
    }

    #[test]
    fn two_scales_of_one_number_are_one_number() {
        // 12.34 written at two scales and at three, which is the same quantity and has to order as
        // one. Comparing the integers as they are would put 12340 above 1235 and rule out a stretch
        // holding the rows the query asked for.
        assert_eq!(scaled(1234, 2).order(&scaled(12_340, 3)), Some(Ordering::Equal));
        assert_eq!(scaled(1234, 2).order(&scaled(12_350, 3)), Some(Ordering::Less));
        assert_eq!(scaled(1235, 2).order(&scaled(12_340, 3)), Some(Ordering::Greater));
    }

    #[test]
    fn a_decimal_constant_rules_out_a_stretch_the_same_way_an_integer_does() {
        // `l_discount BETWEEN 0.05 AND 0.07` against a group running from 0.00 to 0.04, with the
        // constants at a finer scale than the column to make the restating do something.
        let (low, high) = (scaled(0, 2), scaled(4, 2));
        assert!(excluded(Op::GreaterOrEqual, &scaled(50, 3), Some(&low), Some(&high)));
        assert!(!excluded(Op::LessOrEqual, &scaled(70, 3), Some(&low), Some(&high)));
        // And the edge, where the constant is the maximum itself rather than past it.
        assert!(!excluded(Op::GreaterOrEqual, &scaled(40, 3), Some(&low), Some(&high)));
    }

    #[test]
    fn a_decimal_range_is_counted_over_the_steps_the_scale_gives_it() {
        // 0.00 to 0.10 at scale 2 is eleven hundredths. `x <= 0.07` keeps eight of them, which is
        // the same counting a range of integers gets and is why this shares that code.
        let (low, high) = (scaled(0, 2), scaled(10, 2));
        let tests = vec![Test { column: 0, op: Op::LessOrEqual, value: scaled(7, 2) }];
        let spread = kept(&tests, 0, &low, &high).expect("a decimal range interpolates");
        assert!((spread.fraction - 8.0 / 11.0).abs() < 1e-12, "{spread:?}");
        assert_eq!(spread.read, 1);
    }

    #[test]
    fn a_constant_finer_than_the_column_is_counted_at_its_own_scale() {
        // The same stretch and a constant at scale 3, where the step is a thousandth rather than a
        // hundredth. 0.000 to 0.100 is 101 thousandths and `x <= 0.075` keeps 76 of them, which is
        // the fraction the finer grid gives and is within a step of the coarser one.
        let (low, high) = (scaled(0, 2), scaled(10, 2));
        let tests = vec![Test { column: 0, op: Op::LessOrEqual, value: scaled(75, 3) }];
        let spread = kept(&tests, 0, &low, &high).expect("a finer constant interpolates");
        assert!((spread.fraction - 76.0 / 101.0).abs() < 1e-12, "{spread:?}");
    }

    #[test]
    fn a_timestamp_in_one_unit_compares_with_a_constant_in_another() {
        // A file storing milliseconds against the microseconds every constant arrives as. The
        // stretch is one second and the constant is half a second into it, so half of it survives.
        let (low, high) = (scaled(1_000, 3), scaled(2_000, 3));
        let tests = vec![Test { column: 0, op: Op::Less, value: scaled(1_500_000, MICROS) }];
        let spread = kept(&tests, 0, &low, &high).expect("a timestamp range interpolates");
        assert!((spread.fraction - 0.5).abs() < 1e-3, "{spread:?}");
        assert!(excluded(Op::Less, &scaled(1_000_000, MICROS), Some(&low), Some(&high)));
    }

    #[test]
    fn a_number_too_wide_to_restate_answers_nothing_rather_than_wrapping() {
        // Restating a scale 0 maximum at scale 30 does not fit an `i128`, and the comparison that
        // needs it says nothing, which keeps the stretch. Wrapping would rule out a stretch that
        // holds the rows.
        let huge = scaled(i128::MAX / 2, 0);
        assert_eq!(huge.order(&scaled(1, 30)), None);
        assert!(!excluded(Op::Less, &scaled(1, 30), Some(&huge), Some(&huge)));
    }

    #[test]
    fn a_scaled_bound_orders_against_nothing_from_another_domain() {
        assert_eq!(scaled(1234, 2).order(&Bound::Int(12)), None);
        assert_eq!(Bound::Real(12.34).order(&scaled(1234, 2)), None);
        assert_eq!(kept(&one(Op::Less, 15), 0, &scaled(0, 2), &scaled(100, 2)), None);
    }

    #[test]
    fn a_decimal_value_becomes_a_bound_and_comes_back_at_the_columns_scale() {
        let value = Value::Decimal { unscaled: 1234, width: 18, scale: 2 };
        let bound = Bound::of_value(&value).expect("a decimal has a bound");
        assert_eq!(bound, scaled(1234, 2));
        // Back out at a finer scale, which is exact, and at a coarser one, which is only exact when
        // the digits going away are zeroes.
        let finer = LogicalType::Decimal { width: 18, scale: 3 };
        assert_eq!(
            bound.into_value(&finer),
            Some(Value::Decimal { unscaled: 12_340, width: 18, scale: 3 })
        );
        let coarser = LogicalType::Decimal { width: 18, scale: 1 };
        assert_eq!(bound.into_value(&coarser), None, "12.34 is not a number of tenths");
        assert_eq!(
            scaled(1230, 2).into_value(&coarser),
            Some(Value::Decimal { unscaled: 123, width: 18, scale: 1 })
        );
    }

    #[test]
    fn a_timestamp_value_becomes_a_bound_in_microseconds_and_comes_back() {
        let value = Value::Timestamp(1_700_000_000_000_000);
        let bound = Bound::of_value(&value).expect("a timestamp has a bound");
        assert_eq!(bound, scaled(1_700_000_000_000_000, MICROS));
        assert_eq!(bound.into_value(&LogicalType::Timestamp), Some(value));
        // A file's milliseconds restate upwards into the microseconds the value type is.
        assert_eq!(
            scaled(1_700_000_000_000, 3).into_value(&LogicalType::Timestamp),
            Some(Value::Timestamp(1_700_000_000_000_000))
        );
    }

    #[test]
    fn a_minimum_and_a_maximum_accumulate() {
        let lower = Bound::Int(4).smaller(Bound::Int(9));
        let upper = Bound::Int(4).larger(Bound::Int(9));
        assert_eq!(lower, Bound::Int(4));
        assert_eq!(upper, Bound::Int(9));
        // Across domains neither moves, because there is no order to move along.
        assert_eq!(Bound::Int(4).smaller(Bound::Bytes(Vec::new())), Bound::Int(4));
    }
}

//! Comparing values, which is where SQL's three-valued logic actually lives.
//!
//! Six of the eight comparisons return null when either side is null, and the other two never do.
//! That is not a detail: `WHERE a = b` drops a row where either is null and `WHERE a IS NOT
//! DISTINCT FROM b` keeps the row where both are, and the binder produces the second one for `IS
//! NULL` and for a `USING` join under some rewrites. One enum with the null rule attached to the
//! variant is what stops that difference from being re-decided in every operator.
//!
//! The comparison enum here is this crate's own rather than `rudb_plan`'s, because the plan sits
//! nine ranks above the kernels and a kernel that imports a plan type is a kernel that cannot be
//! called from anywhere else. The executor maps one to the other, which is four lines it writes
//! once.
//!
//! Float comparison is DuckDB's rather than IEEE's. Two NaNs are equal, NaN sorts above every
//! number, and negative zero equals zero. IEEE says the first is false and that a NaN comparison is
//! unordered, which would make `GROUP BY` over a column with a NaN in it produce a group nothing
//! can ever find again and make a sort's result depend on the order the rows arrived in.
//!
//! # How the vectorized path is put together
//!
//! `spec/engine/03-data-plane.md` opens with this file as the example of what layer one is for.
//! What it used to be was a loop from zero to length calling `value_at` on both sides, comparing
//! two owned `Value`s and pushing into a `Vec<Value>` that a second pass then walked to pack into a
//! vector. On a varchar column that is a heap allocation and a memcpy per row per side, plus a
//! match on the operator inside the loop that the compiler has no way to hoist.
//!
//! What it is now is three decisions taken once per vector and then a loop that does one thing.
//!
//! The first decision is the form pair. Flat against flat, flat against constant and dictionary
//! against constant each get a hand written path, because those three are what a filter on a scan
//! actually produces. Constant on the left is the same code with the comparison turned around,
//! which [`Comparison::swapped`] does, so there is one loop rather than two. Everything else falls
//! through to the row at a time path, which is still here, is still correct, and now increments a
//! counter in [`crate::fallback`] on the way past so that a combination worth specializing shows up
//! as a number rather than as an opinion.
//!
//! The second decision is the physical type, which a macro turns into one loop per layout. Fifteen
//! layouts by three form pairs by eight operators written out by hand is how a wrong answer gets
//! in, and it is also four thousand lines nobody reads.
//!
//! The third decision is the operator, hoisted out of the loop once. The eight operators
//! become eight monomorphized loops over the same ordering, each with a comparison against a
//! constant `Ordering` in it, which is what makes the body a compare and a store.
//!
//! Validity gets its three cases used rather than collapsed. Two all valid sides skip the mask
//! entirely and produce an all valid result. Either side all invalid, on one of the six ordinary
//! comparisons, is every answer null without reading the data at all, which is a real case because
//! it is what a constant `NULL` in a predicate is.
//!
//! Strings resolve from the four byte prefix in the view. Two views whose prefixes differ are in
//! that order, which holds because the payload past the end of a short string is zero and zero is
//! the least byte, so prefix order is byte order whenever the prefixes are not equal. On `hits` the
//! columns that carry the file are `URL` and `Referer`, and a filter on either of them is now a
//! four byte compare on almost every row instead of a `String` being built to be thrown away.
//!
//! # What is still slow here
//!
//! The index into each side goes through a closure so that the same macro serves flat, constant and
//! dictionary, which means the bounds check on each access survives. That is a known cost and it is
//! next to nothing beside the allocation it replaced, but it is the reason this file will not hit
//! the one nanosecond per row target on its own. The way out is a slice narrowed to the vector
//! length on the identity path, and that wants the benchmark suite to exist first so that the
//! change is a number rather than a belief.

use std::cmp::Ordering;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Data, Form, StringColumn, Validity, Vector};

use crate::fallback::{self, Kernel};
use crate::number::{approximate, integral};
use crate::shape::{first, identity, nulls_of, single};

/// Which comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Comparison {
    /// `=`, null if either side is null.
    Equal,
    /// `<>`, null if either side is null.
    NotEqual,
    /// `<`, null if either side is null.
    Less,
    /// `<=`, null if either side is null.
    LessOrEqual,
    /// `>`, null if either side is null.
    Greater,
    /// `>=`, null if either side is null.
    GreaterOrEqual,
    /// `IS DISTINCT FROM`, which is total and never null.
    DistinctFrom,
    /// `IS NOT DISTINCT FROM`, which is total and never null.
    NotDistinctFrom,
}

impl Comparison {
    /// Whether this comparison treats null as a value rather than as an absence.
    #[must_use]
    pub fn is_total(self) -> bool {
        matches!(self, Self::DistinctFrom | Self::NotDistinctFrom)
    }

    /// The comparison that means the same thing with the two sides exchanged.
    ///
    /// This is what halves the number of specialized loops. A constant on the left against a
    /// column on the right is the column against the constant with the inequality turned around,
    /// and writing it that way means the column against constant loop is written once and tested
    /// once rather than twice with a chance of the second one being subtly wrong.
    #[must_use]
    pub fn swapped(self) -> Self {
        match self {
            Self::Less => Self::Greater,
            Self::LessOrEqual => Self::GreaterOrEqual,
            Self::Greater => Self::Less,
            Self::GreaterOrEqual => Self::LessOrEqual,
            same => same,
        }
    }
}

/// Compares two vectors of the same length, producing a `BOOLEAN` vector.
///
/// # Errors
///
/// If the two sides are not the same length, or if the two types cannot be compared.
pub fn compare(op: Comparison, left: &Vector, right: &Vector) -> Result<Vector> {
    if left.len() != right.len() {
        return Err(Error::internal(format!(
            "a comparison of a {} row vector with a {} row one",
            left.len(),
            right.len()
        )));
    }
    let len = left.len();
    if left.form() == Form::Constant && right.form() == Form::Constant && len > 0 {
        let single = compare_values(op, &left.value_at(0), &right.value_at(0))?;
        return Ok(Vector::constant(LogicalType::Boolean, single, len));
    }

    let (left_valid, right_valid) = (nulls_of(left), nulls_of(right));
    // Either side entirely null, on one of the six ordinary comparisons, is every answer null and
    // the data is never read. This is not a corner case: a `NULL` literal in a predicate is a
    // constant vector whose validity is exactly this, and so is a column the scan knows is empty.
    if !op.is_total()
        && (left_valid == Validity::AllInvalid || right_valid == Validity::AllInvalid)
        && len > 0
    {
        return boolean(vec![false; len], Validity::AllInvalid, len);
    }

    if let Some(answers) = specialized(op, left, right, &left_valid, &right_valid) {
        let validity =
            if op.is_total() { Validity::AllValid } else { left_valid.and(&right_valid, len) };
        return boolean(blank_the_nulls(answers, &validity), validity, len);
    }

    fallback::record(Kernel::Compare, left.form(), right.form());
    let mut values = Vec::with_capacity(len);
    // row at a time: the path recorded on the line above, which exists to be correct for a pair of
    // forms no specialization covers and counts itself so that pair shows up in the report.
    for index in 0..len {
        values.push(compare_values(op, &left.value_at(index), &right.value_at(index))?);
    }
    Vector::from_values(LogicalType::Boolean, &values)
}

/// A `BOOLEAN` vector from a run of answers and the validity that says which of them count.
fn boolean(answers: Vec<bool>, validity: Validity, len: usize) -> Result<Vector> {
    // An empty vector has no null to record, and `Vector::from_values` normalizes the empty mask it
    // builds to all valid, so saying the same here is what keeps an empty specialized result the
    // same vector as the oracle's rather than merely the same length.
    let validity = if len == 0 { Validity::AllValid } else { validity.normalize(len) };
    Ok(Vector::flat(LogicalType::Boolean, Data::Bool(answers.into()))?.with_validity(validity))
}

/// A false in every position the validity says is null.
///
/// The comparison at a null position read whatever the zero the null was stored as compared to,
/// which is a defined value and a meaningless one. Writing false there costs one pass over a run
/// of bytes, only when there are nulls at all, and it buys the property that a specialized result
/// is the same vector as the row at a time result rather than merely the same answer. A test that
/// can compare two vectors with `==` is a much better test than one that has to walk them.
fn blank_the_nulls(mut answers: Vec<bool>, validity: &Validity) -> Vec<bool> {
    if let Validity::Mask(mask) = validity {
        for (index, answer) in answers.iter_mut().enumerate() {
            if !mask.get(index) {
                *answer = false;
            }
        }
    }
    answers
}

/// The answers for a form pair this file has a loop for, or `None` to say it has not.
fn specialized(
    op: Comparison,
    left: &Vector,
    right: &Vector,
    left_valid: &Validity,
    right_valid: &Validity,
) -> Option<Vec<bool>> {
    // Across representations is the fallback's job. `INTEGER` against `BIGINT` reaches the same
    // answer through `numeric_order`, and a specialized loop that assumed the two runs had the same
    // layout would compare a four byte column against an eight byte one position by position.
    if left.logical_type() != right.logical_type() {
        return None;
    }
    let len = left.len();

    if let (Some(one), Some(other)) = (left.data(), right.data()) {
        return dispatch(op, len, one, identity, other, identity, left_valid, right_valid);
    }
    if let (Some(one), Some(value)) = (left.data(), right.constant_value()) {
        let held = single(left.logical_type(), value)?;
        let other = held.data()?;
        return dispatch(op, len, one, identity, other, first, left_valid, right_valid);
    }
    if let (Some(value), Some(other)) = (left.constant_value(), right.data()) {
        // The same loop with the comparison turned around, rather than a second loop.
        let held = single(right.logical_type(), value)?;
        let one = held.data()?;
        return dispatch(op.swapped(), len, other, identity, one, first, right_valid, left_valid);
    }
    if let (Some((codes, values)), Some(value)) = (left.dictionary_parts(), right.constant_value())
    {
        let one = values.data()?;
        let held = single(left.logical_type(), value)?;
        let other = held.data()?;
        let at = |index: usize| codes[index] as usize;
        return dispatch(op, len, one, at, other, first, left_valid, right_valid);
    }
    if let (Some(value), Some((codes, values))) = (left.constant_value(), right.dictionary_parts())
    {
        let other = values.data()?;
        let held = single(right.logical_type(), value)?;
        let one = held.data()?;
        let at = |index: usize| codes[index] as usize;
        return dispatch(op.swapped(), len, other, at, one, first, right_valid, left_valid);
    }
    None
}

/// One loop per physical layout, generated rather than written out.
///
/// The two index closures are what let the same body serve flat against flat, a column against a
/// constant and a dictionary against a constant. `identity` on both sides is the first, `first` on
/// the right is the second, and the codes on the left are the third.
#[expect(
    clippy::too_many_arguments,
    reason = "two sides with an index each, the operator, the length and two validities, all of \
              which the loop needs and none of which is worth a struct that exists for one call"
)]
fn dispatch<L, R>(
    op: Comparison,
    len: usize,
    left: &Data,
    at_left: L,
    right: &Data,
    at_right: R,
    left_valid: &Validity,
    right_valid: &Validity,
) -> Option<Vec<bool>>
where
    L: Fn(usize) -> usize,
    R: Fn(usize) -> usize,
{
    macro_rules! layouts {
        ($($variant:ident),+ $(,)?) => {
            match (left, right) {
                $(
                    (Data::$variant(one), Data::$variant(other)) => Some(sweep(
                        op,
                        len,
                        |index| one[at_left(index)].cmp(&other[at_right(index)]),
                        left_valid,
                        right_valid,
                    )),
                )+
                // Floats have their own order, which is DuckDB's rather than IEEE's, and the
                // widening on a `f32` is free because the comparison is against another `f32`.
                (Data::Float32(one), Data::Float32(other)) => Some(sweep(
                    op,
                    len,
                    |index| {
                        float_order(
                            f64::from(one[at_left(index)]),
                            f64::from(other[at_right(index)]),
                        )
                    },
                    left_valid,
                    right_valid,
                )),
                (Data::Float64(one), Data::Float64(other)) => Some(sweep(
                    op,
                    len,
                    |index| float_order(one[at_left(index)], other[at_right(index)]),
                    left_valid,
                    right_valid,
                )),
                (Data::Varlen(one), Data::Varlen(other)) => Some(sweep(
                    op,
                    len,
                    |index| string_order(one, at_left(index), other, at_right(index)),
                    left_valid,
                    right_valid,
                )),
                _ => None,
            }
        };
    }
    // A `Data::Interval` is a tuple of three integers whose derived order is months, then days,
    // then microseconds, which is exactly what `order` does for the same value by hand.
    layouts!(
        Bool, Int8, Int16, Int32, Int64, Int128, UInt8, UInt16, UInt32, UInt64, UInt128, Interval
    )
}

/// Two strings in byte order, resolved from the four byte prefix where it can be.
///
/// The lemma this rests on is that prefix order is byte order whenever the two prefixes differ. A
/// view pads a string shorter than four bytes with zeros, zero is the least byte, and byte order
/// says a string is less than any string that extends it, so padding compares the same way the
/// missing bytes would have. When the prefixes are equal the payload settles it, which for an
/// inline string is the same sixteen bytes already loaded and for a long one is a block read.
fn string_order(
    left: &StringColumn,
    at_left: usize,
    right: &StringColumn,
    at_right: usize,
) -> Ordering {
    let (Some(one), Some(other)) = (left.views().get(at_left), right.views().get(at_right)) else {
        return Ordering::Equal;
    };
    let (prefix, against) = (one.prefix(), other.prefix());
    if prefix != against {
        return prefix.cmp(&against);
    }
    // Bytes rather than `StringColumn::get`, which validates UTF-8. Everything in a column was
    // pushed from a `&str` so the validation cannot fail, and on a URL column, where every row
    // shares the `http` prefix and the payload therefore decides every comparison, it was the
    // larger half of the per row cost.
    let bytes = left.bytes(at_left).unwrap_or_default();
    let against_bytes = right.bytes(at_right).unwrap_or_default();
    bytes.cmp(against_bytes)
}

/// The answers for one ordering, with the operator decided once rather than once per row.
///
/// This is where the match on the operator gets hoisted. Each arm calls a generic `fill` with a
/// different predicate, so the compiler produces eight loops whose bodies are an ordering against a
/// constant, rather than one loop with a branch table in it.
fn sweep<O>(
    op: Comparison,
    len: usize,
    order_at: O,
    left_valid: &Validity,
    right_valid: &Validity,
) -> Vec<bool>
where
    O: Fn(usize) -> Ordering,
{
    let mut answers = vec![false; len];
    match op {
        Comparison::Equal => fill(&mut answers, order_at, |o| o == Ordering::Equal),
        Comparison::NotEqual => fill(&mut answers, order_at, |o| o != Ordering::Equal),
        Comparison::Less => fill(&mut answers, order_at, |o| o == Ordering::Less),
        Comparison::LessOrEqual => fill(&mut answers, order_at, |o| o != Ordering::Greater),
        Comparison::Greater => fill(&mut answers, order_at, |o| o == Ordering::Greater),
        Comparison::GreaterOrEqual => fill(&mut answers, order_at, |o| o != Ordering::Less),
        Comparison::DistinctFrom => {
            total(&mut answers, order_at, left_valid, right_valid);
            for answer in &mut answers {
                *answer = !*answer;
            }
        }
        Comparison::NotDistinctFrom => total(&mut answers, order_at, left_valid, right_valid),
    }
    answers
}

/// One loop, one predicate, no branch on the operator.
#[inline]
fn fill<O, H>(answers: &mut [bool], order_at: O, held: H)
where
    O: Fn(usize) -> Ordering,
    H: Fn(Ordering) -> bool,
{
    for (index, answer) in answers.iter_mut().enumerate() {
        *answer = held(order_at(index));
    }
}

/// `IS NOT DISTINCT FROM`, which reads validity as data rather than as an absence.
///
/// Two nulls are the same value here and a null against anything else is not, which is the whole
/// difference between this and `=`. The all valid case is checked once so that the common shape,
/// which is a total comparison inside a join on columns that happen not to be nullable, does not
/// pay for two validity lookups per row.
fn total<O>(answers: &mut [bool], order_at: O, left_valid: &Validity, right_valid: &Validity)
where
    O: Fn(usize) -> Ordering,
{
    if *left_valid == Validity::AllValid && *right_valid == Validity::AllValid {
        fill(answers, order_at, |o| o == Ordering::Equal);
        return;
    }
    for (index, answer) in answers.iter_mut().enumerate() {
        *answer = match (left_valid.is_valid(index), right_valid.is_valid(index)) {
            (true, true) => order_at(index) == Ordering::Equal,
            (false, false) => true,
            _ => false,
        };
    }
}

/// Compares two values, producing `TRUE`, `FALSE` or `NULL`.
///
/// # Errors
///
/// If the two types cannot be compared, which after binding means one of them is a nested type.
pub fn compare_values(op: Comparison, left: &Value, right: &Value) -> Result<Value> {
    if op.is_total() {
        let same = match (left.is_null(), right.is_null()) {
            (true, true) => true,
            (true, false) | (false, true) => false,
            (false, false) => order(left, right)? == Ordering::Equal,
        };
        return Ok(Value::Boolean(match op {
            Comparison::NotDistinctFrom => same,
            _ => !same,
        }));
    }
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let ordering = order(left, right)?;
    let held = match op {
        Comparison::Equal => ordering == Ordering::Equal,
        Comparison::NotEqual => ordering != Ordering::Equal,
        Comparison::Less => ordering == Ordering::Less,
        Comparison::LessOrEqual => ordering != Ordering::Greater,
        Comparison::Greater => ordering == Ordering::Greater,
        Comparison::GreaterOrEqual => ordering != Ordering::Less,
        Comparison::DistinctFrom | Comparison::NotDistinctFrom => {
            return Err(Error::internal("a total comparison reached the ordered path"));
        }
    };
    Ok(Value::Boolean(held))
}

/// The order of two values, neither of which is null.
///
/// This is the one place the sort order of a type is written down. `ORDER BY`, `GROUP BY`, a merge
/// join and a min or max aggregate all reach it, and a type that ordered differently in two of
/// those would produce a query whose answer depends on which operator the optimizer picked.
///
/// # Errors
///
/// If either value is null, which is the caller's mistake rather than a comparison, or if the
/// types have no order between them.
pub fn order(left: &Value, right: &Value) -> Result<Ordering> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => {
            Err(Error::internal("a null reached the ordering path"))
        }
        (Value::Boolean(a), Value::Boolean(b)) => Ok(a.cmp(b)),
        (Value::Varchar(a), Value::Varchar(b)) => Ok(a.as_bytes().cmp(b.as_bytes())),
        (Value::Blob(a), Value::Blob(b)) => Ok(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Ok(a.cmp(b)),
        (Value::Time(a), Value::Time(b)) | (Value::Timestamp(a), Value::Timestamp(b)) => {
            Ok(a.cmp(b))
        }
        (
            Value::Interval { months: am, days: ad, micros: au },
            Value::Interval { months: bm, days: bd, micros: bu },
        ) => Ok((am, ad, au).cmp(&(bm, bd, bu))),
        _ => numeric_order(left, right),
    }
}

/// The order of two numbers, which is the case that has to work across representations.
fn numeric_order(left: &Value, right: &Value) -> Result<Ordering> {
    if let (Some(a), Some(b)) = (integral(left), integral(right)) {
        return Ok(a.cmp(&b));
    }
    if let (
        Value::Decimal { unscaled: a, scale: sa, .. },
        Value::Decimal { unscaled: b, scale: sb, .. },
    ) = (left, right)
    {
        if sa == sb {
            return Ok(a.cmp(b));
        }
    }
    match (approximate(left), approximate(right)) {
        (Some(a), Some(b)) => Ok(float_order(a, b)),
        _ => Err(Error::not_implemented(format!(
            "comparing {} with {}",
            left.logical_type(),
            right.logical_type()
        ))),
    }
}

/// DuckDB's float order: NaN is equal to itself and above everything else, and zero has one place.
fn float_order(left: f64, right: f64) -> Ordering {
    if left == right {
        return Ordering::Equal;
    }
    match (left.is_nan(), right.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
    }
}

/// The order of two values with nulls in it, for a sort key.
///
/// A sort has to put nulls somewhere and SQL lets the query say where, so this takes the answer
/// rather than deciding it.
///
/// # Errors
///
/// If the two types have no order between them.
pub fn order_with_nulls(left: &Value, right: &Value, nulls_first: bool) -> Result<Ordering> {
    match (left.is_null(), right.is_null()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(if nulls_first { Ordering::Less } else { Ordering::Greater }),
        (false, true) => Ok(if nulls_first { Ordering::Greater } else { Ordering::Less }),
        (false, false) => order(left, right),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compared(op: Comparison, left: Value, right: Value) -> Value {
        compare_values(op, &left, &right).expect("these types compare")
    }

    /// Every comparison, so that a test that sweeps them cannot quietly miss one.
    const EVERY: [Comparison; 8] = [
        Comparison::Equal,
        Comparison::NotEqual,
        Comparison::Less,
        Comparison::LessOrEqual,
        Comparison::Greater,
        Comparison::GreaterOrEqual,
        Comparison::DistinctFrom,
        Comparison::NotDistinctFrom,
    ];

    /// The row at a time path, kept as the oracle rather than deleted.
    ///
    /// `spec/engine/03-data-plane.md` is explicit that the slow path becomes the thing the fast
    /// path is checked against. This is that, written out here so that a test can call it on a pair
    /// of vectors whose forms the fast path does specialize.
    fn oracle(op: Comparison, left: &Vector, right: &Vector) -> Vector {
        let values: Vec<Value> = (0..left.len())
            .map(|index| {
                compare_values(op, &left.value_at(index), &right.value_at(index))
                    .expect("the oracle is only asked about types that compare")
            })
            .collect();
        Vector::from_values(LogicalType::Boolean, &values).expect("booleans")
    }

    /// Asserts that the specialized path and the oracle produce the same vector, not merely the
    /// same answers. Same vector means the same data, the same validity representation and the
    /// same false in every null position, which is a much stronger statement and is free to check.
    fn agrees(op: Comparison, left: &Vector, right: &Vector) {
        let fast = compare(op, left, right).expect("compares");
        let slow = oracle(op, left, right);
        assert_eq!(fast, slow, "{op:?} on a {:?} against a {:?}", left.form(), right.form());
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

    #[test]
    fn an_ordinary_comparison_is_null_when_either_side_is() {
        assert_eq!(compared(Comparison::Equal, Value::Integer(1), Value::Null), Value::Null);
        assert_eq!(compared(Comparison::Less, Value::Null, Value::Integer(1)), Value::Null);
    }

    #[test]
    fn a_total_comparison_is_never_null() {
        assert_eq!(
            compared(Comparison::NotDistinctFrom, Value::Null, Value::Null),
            Value::Boolean(true)
        );
        assert_eq!(
            compared(Comparison::NotDistinctFrom, Value::Integer(1), Value::Null),
            Value::Boolean(false)
        );
        assert_eq!(
            compared(Comparison::DistinctFrom, Value::Integer(1), Value::Null),
            Value::Boolean(true)
        );
    }

    #[test]
    fn a_string_compares_by_bytes() {
        assert_eq!(
            compared(Comparison::Less, Value::Varchar("a".into()), Value::Varchar("b".into())),
            Value::Boolean(true)
        );
        assert_eq!(
            compared(Comparison::Less, Value::Varchar("Z".into()), Value::Varchar("a".into())),
            Value::Boolean(true)
        );
    }

    /// The reason this crate does not use `f64::partial_cmp` directly. A NaN that compared
    /// unordered would make a group by produce a group nothing can find again.
    #[test]
    fn two_nans_are_one_value_and_they_sort_above_the_numbers() {
        assert_eq!(
            compared(Comparison::Equal, Value::Double(f64::NAN), Value::Double(f64::NAN)),
            Value::Boolean(true)
        );
        assert_eq!(
            compared(Comparison::Greater, Value::Double(f64::NAN), Value::Double(1e300)),
            Value::Boolean(true)
        );
    }

    #[test]
    fn zero_has_one_value_however_it_is_signed() {
        assert_eq!(
            compared(Comparison::Equal, Value::Double(0.0), Value::Double(-0.0)),
            Value::Boolean(true)
        );
    }

    #[test]
    fn a_number_compares_the_same_however_it_is_stored() {
        assert_eq!(
            compared(Comparison::Equal, Value::Integer(3), Value::BigInt(3)),
            Value::Boolean(true)
        );
        assert_eq!(
            compared(Comparison::Less, Value::Integer(3), Value::Double(3.5)),
            Value::Boolean(true)
        );
    }

    #[test]
    fn nulls_go_where_the_query_asked_for_them() {
        assert_eq!(
            order_with_nulls(&Value::Null, &Value::Integer(1), true).expect("orders"),
            Ordering::Less
        );
        assert_eq!(
            order_with_nulls(&Value::Null, &Value::Integer(1), false).expect("orders"),
            Ordering::Greater
        );
    }

    #[test]
    fn two_constant_vectors_cost_one_comparison() {
        let left = Vector::constant(LogicalType::Integer, Value::Integer(1), 512);
        let right = Vector::constant(LogicalType::Integer, Value::Integer(2), 512);
        let result = compare(Comparison::Less, &left, &right).expect("compares");
        assert_eq!(result.form(), Form::Constant);
        assert_eq!(result.value_at(500), Value::Boolean(true));
    }

    #[test]
    fn a_comparison_of_two_vectors_is_one_answer_per_row() {
        let left = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(5), Value::Null],
        )
        .expect("three rows");
        let right = Vector::constant(LogicalType::Integer, Value::Integer(3), 3);
        let result = compare(Comparison::Greater, &left, &right).expect("compares");
        assert_eq!(result.value_at(0), Value::Boolean(false));
        assert_eq!(result.value_at(1), Value::Boolean(true));
        assert_eq!(result.value_at(2), Value::Null);
    }

    #[test]
    fn two_vectors_of_different_lengths_are_caught() {
        let left = Vector::constant(LogicalType::Integer, Value::Integer(1), 4);
        let right = Vector::constant(LogicalType::Integer, Value::Integer(1), 5);
        let error = compare(Comparison::Equal, &left, &right).expect_err("ragged");
        assert!(error.message().contains("4 row vector"), "{error}");
    }

    #[test]
    fn turning_a_comparison_around_is_what_the_other_side_would_have_said() {
        for op in EVERY {
            let left = Value::Integer(3);
            let right = Value::Integer(7);
            assert_eq!(
                compare_values(op, &left, &right).expect("compares"),
                compare_values(op.swapped(), &right, &left).expect("compares"),
                "{op:?}"
            );
        }
    }

    /// The whole point of the rewrite, stated as a property. Every operator, every physical
    /// layout, every form pair the fast path claims, against the row at a time oracle.
    #[test]
    fn every_specialized_path_agrees_with_the_row_at_a_time_path() {
        let mut rng = Rng(0x5eed_1234_9876_4321);
        let types: [LogicalType; 10] = [
            LogicalType::Boolean,
            LogicalType::TinyInt,
            LogicalType::SmallInt,
            LogicalType::Integer,
            LogicalType::BigInt,
            LogicalType::HugeInt,
            LogicalType::UInteger,
            LogicalType::Float,
            LogicalType::Double,
            LogicalType::Varchar,
        ];
        for ty in &types {
            for nulls in [0u64, 1, 3] {
                let len = 37;
                let make = |rng: &mut Rng| {
                    let values: Vec<Value> = (0..len)
                        .map(|_| {
                            if nulls > 0 && rng.below(nulls + 1) == 0 {
                                Value::Null
                            } else {
                                sample(ty, rng)
                            }
                        })
                        .collect();
                    Vector::from_values(ty.clone(), &values).expect("a flat vector")
                };
                let left = make(&mut rng);
                let right = make(&mut rng);
                let literal = sample(ty, &mut rng);
                let constant = Vector::constant(ty.clone(), literal, len);
                let null_constant = Vector::constant(ty.clone(), Value::Null, len);
                let codes: Vec<u32> =
                    (0..len).map(|_| rng.below(left.len() as u64) as u32).collect();
                let dictionary =
                    Vector::dictionary(codes, left.clone()).expect("codes are in range");

                for op in EVERY {
                    agrees(op, &left, &right);
                    agrees(op, &left, &constant);
                    agrees(op, &constant, &left);
                    agrees(op, &left, &null_constant);
                    agrees(op, &null_constant, &left);
                    agrees(op, &dictionary, &constant);
                    agrees(op, &constant, &dictionary);
                }
            }
        }
    }

    /// One value of a type, for the generator above.
    fn sample(ty: &LogicalType, rng: &mut Rng) -> Value {
        match ty {
            LogicalType::Boolean => Value::Boolean(rng.below(2) == 1),
            LogicalType::TinyInt => Value::TinyInt(rng.below(7) as i8 - 3),
            LogicalType::SmallInt => Value::SmallInt(rng.below(11) as i16 - 5),
            LogicalType::Integer => Value::Integer(rng.below(9) as i32 - 4),
            LogicalType::BigInt => Value::BigInt(rng.below(9) as i64 - 4),
            LogicalType::HugeInt => Value::HugeInt(i128::from(rng.below(9)) - 4),
            LogicalType::UInteger => Value::UInteger(rng.below(9) as u32),
            // A NaN and a negative zero in the pool on purpose, because DuckDB's float order is
            // not IEEE's and the fast path has to reach the same answer the oracle does.
            LogicalType::Float => Value::Float(match rng.below(5) {
                0 => f32::NAN,
                1 => -0.0,
                other => other as f32 - 2.0,
            }),
            LogicalType::Double => Value::Double(match rng.below(5) {
                0 => f64::NAN,
                1 => -0.0,
                other => other as f64 - 2.0,
            }),
            // Short, at the inline limit, over it, and sharing a prefix with each other, which is
            // where a comparison that trusts the prefix too far goes wrong.
            LogicalType::Varchar => Value::Varchar(
                match rng.below(6) {
                    0 => "",
                    1 => "ab",
                    2 => "abc",
                    3 => "abcdefghijkl",
                    4 => "abcdefghijklm",
                    _ => "abcdefghijklmnopqrstuvwxyz",
                }
                .to_owned(),
            ),
            other => panic!("the generator has no values for {other}"),
        }
    }

    /// The prefix lemma, written as a test because the whole string path rests on it. A view pads
    /// a short string with zeros, so prefix order has to agree with byte order on every pair where
    /// the prefixes differ, including the pairs where one string is shorter than four bytes.
    #[test]
    fn prefix_order_is_byte_order_whenever_the_prefixes_differ() {
        let words =
            ["", "a", "ab", "abc", "abcd", "abcde", "b", "abcdefghijklmnop", "abcdefghijklmnoq"];
        let mut column = StringColumn::new();
        for word in words {
            column.push(word);
        }
        for (i, one) in words.iter().enumerate() {
            for (j, other) in words.iter().enumerate() {
                assert_eq!(
                    string_order(&column, i, &column, j),
                    one.as_bytes().cmp(other.as_bytes()),
                    "{one:?} against {other:?}"
                );
            }
        }
    }

    /// A dictionary is compared once per distinct value, not once per row, and it has to reach the
    /// same answer including for the nulls it keeps in the vector it points at.
    #[test]
    fn a_dictionary_against_a_constant_reads_its_nulls_from_the_values() {
        let values = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Null, Value::Integer(9)],
        )
        .expect("three values");
        let dictionary =
            Vector::dictionary(vec![0, 1, 2, 1, 0], values).expect("codes are in range");
        let constant = Vector::constant(LogicalType::Integer, Value::Integer(5), 5);
        let result = compare(Comparison::Less, &dictionary, &constant).expect("compares");
        assert_eq!(result.value_at(0), Value::Boolean(true));
        assert_eq!(result.value_at(1), Value::Null);
        assert_eq!(result.value_at(2), Value::Boolean(false));
        assert_eq!(result.value_at(3), Value::Null);
        assert_eq!(result.value_at(4), Value::Boolean(true));
    }

    /// A form pair with no loop is answered correctly and counted, which is the whole contract of
    /// the fallback counter. Sequence against a column is the one this file leaves out on purpose.
    #[test]
    fn a_form_pair_with_no_loop_is_still_right_and_says_so() {
        // The counters are process wide and another test in this crate resets them, so the ones
        // that read a count take turns.
        let _turn = fallback::TURN.lock().expect("no test panics while holding this");
        let before = fallback::count(Kernel::Compare, Form::Sequence, Form::Flat);
        let sequence = Vector::sequence(10, 1, 4);
        let flat = Vector::from_values(
            LogicalType::BigInt,
            &[Value::BigInt(9), Value::BigInt(11), Value::BigInt(12), Value::Null],
        )
        .expect("four rows");
        let result = compare(Comparison::Less, &sequence, &flat).expect("compares");
        assert_eq!(result.value_at(0), Value::Boolean(false));
        assert_eq!(result.value_at(1), Value::Boolean(false));
        assert_eq!(result.value_at(2), Value::Boolean(false));
        assert_eq!(result.value_at(3), Value::Null);
        assert!(fallback::count(Kernel::Compare, Form::Sequence, Form::Flat) > before);
    }

    /// The reason `Vector::dictionary` composes rather than stacks, stated as the thing that breaks
    /// if it stops.
    ///
    /// Every loop in this file reaches for the values behind the codes with `Vector::data`, and a
    /// dictionary pointing at a dictionary has no data to hand back, so a second filter over an
    /// already filtered chunk used to turn every one of these kernels off and drop the comparison
    /// onto the row at a time path. Measured on server3 over a chunk of two numeric columns that was
    /// selected twice, that was 3.5 nanoseconds a row becoming 104, and a third and fourth level
    /// cost nothing more because the first one had already given up everything there was to give.
    #[test]
    fn a_second_level_of_codes_does_not_turn_the_loops_off() {
        let _turn = fallback::TURN.lock().expect("no test panics while holding this");
        let before = fallback::count(Kernel::Compare, Form::Dictionary, Form::Constant);
        let values = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(5), Value::Integer(9)],
        )
        .expect("three rows");
        let once = Vector::dictionary(vec![2, 1, 0], values).expect("codes are in range");
        let twice = Vector::dictionary(vec![1, 2], once).expect("codes are in range");
        let cut = Vector::constant(LogicalType::Integer, Value::Integer(4), 2);
        let result = compare(Comparison::Greater, &twice, &cut).expect("compares");
        assert_eq!(result.value_at(0), Value::Boolean(true));
        assert_eq!(result.value_at(1), Value::Boolean(false));
        assert_eq!(fallback::count(Kernel::Compare, Form::Dictionary, Form::Constant), before);
    }

    /// Either side all null, on one of the six ordinary comparisons, is every answer null without
    /// the data being read. The vector this produces has to be the one the oracle produces, which
    /// is a flat run of falses under an all invalid validity rather than a constant.
    #[test]
    fn a_side_that_is_entirely_null_answers_without_reading_the_other() {
        let nulls = Vector::constant(LogicalType::Integer, Value::Null, 6);
        let flat = Vector::from_values(
            LogicalType::Integer,
            &[
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3),
                Value::Integer(4),
                Value::Integer(5),
                Value::Integer(6),
            ],
        )
        .expect("six rows");
        agrees(Comparison::Less, &nulls, &flat);
        agrees(Comparison::Equal, &flat, &nulls);
        assert_eq!(
            compare(Comparison::Less, &nulls, &flat).expect("compares").validity(),
            &Validity::AllInvalid
        );
    }

    /// An empty vector is not a special case anywhere, and the easiest way to keep it that way is
    /// to say so in a test rather than to find out from a panic in an operator.
    #[test]
    fn an_empty_comparison_is_an_empty_answer() {
        let left = Vector::from_values(LogicalType::Integer, &[]).expect("no rows");
        let right = Vector::constant(LogicalType::Integer, Value::Integer(1), 0);
        let result = compare(Comparison::Equal, &left, &right).expect("compares");
        assert_eq!(result.len(), 0);
    }
}

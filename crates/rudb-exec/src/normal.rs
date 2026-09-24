//! A sort key written as one byte string whose `memcmp` order is the sort order.
//!
//! The second half of what the note on #63 asks for and the remaining item on #1210. The first
//! half was the payload, which #1233 took out of the sort: a row is the chunk it arrived in and its
//! place in that chunk, so the comparator moves two pairs of numbers rather than sixteen columns.
//! What it still moved was the key, as a `Vec<Value>` a row, and for lineitem on three keys that is
//! one heap allocation a row and a walk through the `Value` enum on every comparison.
//!
//! A normalized key is the usual answer and it is the one DuckDB uses. Every key of a row is
//! written into one fixed width buffer, in priority order, in an encoding where comparing the bytes
//! unsigned and left to right gives the same answer the type's own comparison gives. Then the
//! comparator is a byte compare over a fixed array, the direction and the null placement are
//! already folded in, and the key is a field of the row rather than a pointer to somewhere else.
//!
//! # What goes in a key
//!
//! One tag byte then the payload, per key.
//!
//! The tag says whether the value is there, and which way round depends on where the query wants
//! its nulls: `NULLS FIRST` writes 0 for a null and 1 for a value, `NULLS LAST` the other way. It
//! is the one part `DESC` does not touch, because `ORDER BY x DESC NULLS LAST` is not `ORDER BY x
//! NULLS FIRST` reversed and reversing the whole key would move the nulls with it. A null writes
//! zeros for its payload and is never inverted, so two nulls compare equal whatever the direction
//! is and the tag is what separates a null from a value.
//!
//! The payload is big endian, which is what makes a byte compare read the high end first. A signed
//! integer has its sign bit flipped, so -1 becomes 0x7fff... and 0 becomes 0x8000..., which puts
//! the negatives below the positives where two's complement puts them above. An unsigned integer
//! needs nothing. `DESC` inverts every payload byte of that key and only that key, so a key list
//! can mix directions.
//!
//! # What does not go in a key
//!
//! A type with no fixed width, which is `VARCHAR` and `BLOB`, as the value itself. What goes in
//! instead is the string's rank among the strings of its key, which orders the way the bytes do and
//! fits in four, but is only known once every row is in, so [`ranked_layout`] is a second layout
//! the sort writes at the end rather than a wider [`layout`]. A list that does not fit even then
//! takes the old path, whole, rather than normalizing the keys around it.
//!
//! A `FLOAT` or a `DOUBLE`, for a different reason. The IEEE total order that makes floats memcmp
//! comparable separates -0.0 from 0.0 and orders NaNs by their payload, and DuckDB's order does
//! neither: a NaN is equal to itself, above everything else, and zero has one place. Encoding those
//! three rules into bytes is possible and it is not obviously worth a wrong answer if it is got
//! subtly wrong, so floats take the old path too.
//!
//! An `INTERVAL`, because its order is over the months, days and microseconds folded together and
//! not over the triple as stored. A `UUID`, a `BIT` and the nested types, because none of them has
//! an order this has checked. Anything wider than [`WIDTH`] all together, which is two `HUGEINT`s
//! or a great many small ones.
//!
//! Everything left is the case that matters: the integers, the dates, the times, the timestamps and
//! the decimals. A clustered load on `date_trunc('month', l_shipdate), l_orderkey, l_linenumber` is
//! nineteen bytes of this.

#[cfg(test)]
use rudb_common::Value;
use rudb_common::{Error, LogicalType, PhysicalType, Result};
use rudb_plan::SortKey;
use rudb_vector::{Data, Validity, Vector};

/// The widest normalized key, in bytes.
///
/// Twenty four rather than sixteen so that the three key clustered layout fits, which is a date, a
/// `BIGINT` and an `INTEGER` with a tag each and comes to nineteen. Rather than thirty two because
/// the buffer sits in every row of the sort and the bytes are paid whether the key fills them or
/// not, and a key list that does not fit takes a path that still works.
pub(crate) const WIDTH: usize = 24;

/// One row's keys, in priority order, ready to compare with a byte compare.
pub(crate) type Normal = [u8; WIDTH];

/// How wide each key of a list encodes, or `None` when the list has no normalized form.
///
/// `None` for a key of a type with no fixed width order, and for a list that is wider than
/// [`WIDTH`] all together. Either way the caller keeps the `Value` path, which handles everything.
pub(crate) fn layout(types: &[LogicalType]) -> Option<Vec<usize>> {
    fitted(types, wide)
}

/// How wide each key encodes when every string key is written as its rank among the strings of
/// the sort, and whether it is one of those, or `None` when the list still has no normalized form.
///
/// A string orders by its bytes, so where it falls among the other strings of the same key is all
/// the sort needs of it, and that is a number that fits in four bytes. The number is only known
/// once every row is in, which is why this is a second layout rather than a wider [`layout`]: the
/// caller keeps the key columns until then and writes the keys after. A `NULL` is the tag, the same
/// as for any other key.
pub(crate) fn ranked_layout(types: &[LogicalType]) -> Option<Vec<(usize, bool)>> {
    let widths = fitted(types, |ty| if ranked(ty) { Some(RANK) } else { wide(ty) })?;
    Some(widths.into_iter().zip(types).map(|(wide, ty)| (wide, ranked(ty))).collect())
}

/// Whether a key of this type is written as its rank, which is a type ordered by its bytes alone.
pub(crate) fn ranked(ty: &LogicalType) -> bool {
    matches!(ty, LogicalType::Varchar | LogicalType::Blob)
}

/// How many bytes a rank takes, the tag included.
const RANK: usize = 1 + size_of::<u32>();

/// Each type's width under `wide`, or `None` when one has none or they do not fit in [`WIDTH`].
fn fitted(
    types: &[LogicalType],
    wide: impl Fn(&LogicalType) -> Option<usize>,
) -> Option<Vec<usize>> {
    let mut widths = Vec::with_capacity(types.len());
    let mut total = 0;
    for ty in types {
        let wide = wide(ty)?;
        total += wide;
        if total > WIDTH {
            return None;
        }
        widths.push(wide);
    }
    (!widths.is_empty()).then_some(widths)
}

/// How many bytes one value of this type takes, the tag included.
fn wide(ty: &LogicalType) -> Option<usize> {
    let ordered = matches!(
        ty,
        LogicalType::Boolean
            | LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::HugeInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt
            | LogicalType::UHugeInt
            | LogicalType::Date
            | LogicalType::Time
            | LogicalType::TimeTz
            | LogicalType::Timestamp
            | LogicalType::TimestampTz
            | LogicalType::Decimal { .. }
    );
    if !ordered {
        return None;
    }
    let payload = match ty.physical() {
        PhysicalType::Bool | PhysicalType::Int8 | PhysicalType::UInt8 => 1,
        PhysicalType::Int16 | PhysicalType::UInt16 => 2,
        PhysicalType::Int32 | PhysicalType::UInt32 => 4,
        PhysicalType::Int64 | PhysicalType::UInt64 => 8,
        PhysicalType::Int128 | PhysicalType::UInt128 => 16,
        _ => return None,
    };
    Some(payload + 1)
}

/// Writes one value into `into` at `at`, taking `wide` bytes, under `key`.
///
/// The encoding written out for one value at a time, which is what the sort did before
/// `write_column` and is kept as the reference the tests hold that one to.
///
/// # Errors
///
/// If the value is not one [`layout`] said this key would hold. That is an internal error and not a
/// user's mistake, because the width came off the same type the vector was built from, and it is
/// reported rather than encoded wrong because a key that silently took the wrong number of bytes
/// would push every key after it along and put the rows in an order nobody asked for.
#[cfg(test)]
pub(crate) fn write(
    into: &mut Normal,
    at: usize,
    wide: usize,
    value: &Value,
    key: SortKey,
) -> Result<()> {
    let Some(slot) = into.get_mut(at..at + wide) else {
        return Err(Error::internal("a normalized sort key wider than the buffer holding it"));
    };
    let (tag, payload) = slot.split_at_mut(1);
    if value.is_null() {
        tag[0] = u8::from(!key.nulls_first);
        return Ok(());
    }
    tag[0] = u8::from(key.nulls_first);
    let ordered = ordered(value, payload.len())?.to_be_bytes();
    let Some(bytes) = ordered.get(16 - payload.len()..) else {
        return Err(Error::internal("a normalized sort key narrower than the value in it"));
    };
    payload.copy_from_slice(bytes);
    if key.descending {
        for byte in payload {
            *byte = !*byte;
        }
    }
    Ok(())
}

/// Writes one key column into the keys of the rows it belongs to, a row a key, at `at`.
///
/// The encoding the top of this file describes, done for a whole column with a typed loop per
/// layout. The sort used to build a `Value` a key a row and encode that, and on SF1 `lineitem` sorted on
/// three keys that was 260ms of a 1.6s query on one thread, for eighteen million values that were
/// already sitting in three flat runs. The encoding is the same one, so a key written either way
/// is the same bytes.
///
/// # Errors
///
/// If the column does not have as many rows as there are keys, or is not a layout [`layout`] said
/// this key would hold. That is an internal error and not a user's mistake, because the width came
/// off the same type the vector was built from, and it is reported rather than encoded wrong
/// because a key that silently took the wrong number of bytes would push every key after it along
/// and put the rows in an order nobody asked for.
pub(crate) fn write_column<'a>(
    into: impl ExactSizeIterator<Item = &'a mut Normal>,
    at: usize,
    wide: usize,
    column: &Vector,
    key: SortKey,
) -> Result<()> {
    if into.len() != column.len() {
        return Err(Error::internal("a sort key column of a different length than its rows"));
    }
    // flatten: the keys are read as one run, and a key can arrive constant, dictionary encoded or
    // bit packed. A flat column is not copied by this, and a key column is one of the few per chunk.
    let flat = column.flatten()?;
    let Some(data) = flat.data() else {
        return Err(Error::internal("a flattened sort key with no run of data"));
    };
    let place = Place { at, wide, key, validity: flat.validity() };
    match data {
        Data::Bool(values) => place.put(into, values.as_slice(), u128::from),
        Data::Int8(values) => {
            place.put(into, values.as_slice(), |value| place.signed(value.into()))
        }
        Data::Int16(values) => {
            place.put(into, values.as_slice(), |value| place.signed(value.into()))
        }
        Data::Int32(values) => {
            place.put(into, values.as_slice(), |value| place.signed(value.into()))
        }
        Data::Int64(values) => {
            place.put(into, values.as_slice(), |value| place.signed(value.into()))
        }
        Data::Int128(values) => place.put(into, values.as_slice(), |value| place.signed(value)),
        Data::UInt8(values) => place.put(into, values.as_slice(), u128::from),
        Data::UInt16(values) => place.put(into, values.as_slice(), u128::from),
        Data::UInt32(values) => place.put(into, values.as_slice(), u128::from),
        Data::UInt64(values) => place.put(into, values.as_slice(), u128::from),
        Data::UInt128(values) => place.put(into, values.as_slice(), |value| value),
        _ => Err(Error::internal(format!(
            "a {} column reached the normalized sort key path",
            column.logical_type()
        ))),
    }
}

/// Where one key column goes in the rows' keys, and what it needs to encode a value there.
struct Place<'v> {
    at: usize,
    wide: usize,
    key: SortKey,
    validity: &'v Validity,
}

impl Place<'_> {
    /// A signed value with the sign bit of the width it is written at flipped.
    ///
    /// At that width and not at a hundred and twenty eight bits, because only the low bytes are
    /// written and a flip of the top bit of the widened value would be a flip of a bit that is
    /// thrown away, leaving the negatives above the positives.
    fn signed(&self, value: i128) -> u128 {
        let sign = (self.wide - 1).checked_mul(8).and_then(|bits| bits.checked_sub(1)).unwrap_or(0);
        (value as u128) ^ (1u128 << sign)
    }

    /// Writes `values` into `into`, one a row, each turned into its unsigned order by `raw`.
    fn put<'a, T: Copy>(
        &self,
        into: impl Iterator<Item = &'a mut Normal>,
        values: &[T],
        raw: impl Fn(T) -> u128,
    ) -> Result<()> {
        let payload = self.wide - 1;
        if payload != size_of::<T>() {
            return Err(Error::internal("a sort key column wider or narrower than its key"));
        }
        let all = matches!(self.validity, Validity::AllValid);
        let (present, absent) = (u8::from(self.key.nulls_first), u8::from(!self.key.nulls_first));
        for (row, (normal, &value)) in into.zip(values).enumerate() {
            let Some(slot) = normal.get_mut(self.at..self.at + self.wide) else {
                return Err(Error::internal(
                    "a normalized sort key wider than the buffer holding it",
                ));
            };
            let (tag, bytes) = slot.split_at_mut(1);
            if !all && !self.validity.is_valid(row) {
                tag[0] = absent;
                bytes.fill(0);
                continue;
            }
            tag[0] = present;
            bytes.copy_from_slice(&raw(value).to_be_bytes()[16 - payload..]);
            if self.key.descending {
                for byte in bytes {
                    *byte = !*byte;
                }
            }
        }
        Ok(())
    }
}

/// The value as an unsigned number whose low `bytes` bytes compare the way the value compares.
///
/// Only the low `bytes` of the answer mean anything, and the caller takes exactly those off the big
/// endian form. That is why the bias is applied at the width the key was given rather than at a
/// hundred and twenty eight bits: flipping the top bit of a widened value moves a bit the caller
/// throws away, and the four bytes it keeps would come out with the negatives still above the
/// positives.
///
/// Truncating the widened value is the right thing and not a risk, because the value fits the width
/// [`wide`] chose for its own type. A `DATE` widened to a hundred and twenty eight bits is a sign
/// extended `i32`, and its low four bytes are the `i32` back.
#[cfg(test)]
fn ordered(value: &Value, bytes: usize) -> Result<u128> {
    let (raw, signed) = match value {
        Value::Boolean(held) => (u128::from(*held), false),
        Value::TinyInt(held) => (i128::from(*held) as u128, true),
        Value::SmallInt(held) => (i128::from(*held) as u128, true),
        Value::Integer(held) | Value::Date(held) => (i128::from(*held) as u128, true),
        Value::BigInt(held)
        | Value::Time(held)
        | Value::TimeTz(held)
        | Value::Timestamp(held)
        | Value::TimestampTz(held) => (i128::from(*held) as u128, true),
        Value::HugeInt(held) => (*held as u128, true),
        Value::UTinyInt(held) => (u128::from(*held), false),
        Value::USmallInt(held) => (u128::from(*held), false),
        Value::UInteger(held) => (u128::from(*held), false),
        Value::UBigInt(held) => (u128::from(*held), false),
        Value::UHugeInt(held) => (*held, false),
        // The scale is the column's and every value in the column carries the same one, so the
        // unscaled integers order the way the numbers do. That is the same rule the value path
        // uses, which compares unscaled and refuses two decimals whose scales differ.
        Value::Decimal { unscaled, .. } => (*unscaled as u128, true),
        other => {
            return Err(Error::internal(format!(
                "a {} reached the normalized sort key path",
                other.logical_type()
            )));
        }
    };
    // The sign bit of the field the caller keeps, which for a boolean of one byte is bit seven and
    // for a `HUGEINT` is bit a hundred and twenty seven.
    let sign = bytes.checked_mul(8).and_then(|bits| bits.checked_sub(1)).unwrap_or(0);
    Ok(if signed { raw ^ (1u128 << sign) } else { raw })
}

#[cfg(test)]
mod tests {
    use rudb_common::Value;

    use rudb_plan::SortKey;

    use rudb_common::LogicalType;
    use rudb_vector::Vector;

    use super::{Normal, WIDTH, layout, write, write_column};

    /// A key with nothing but the direction and the null placement set, since `write` reads no more.
    fn key(descending: bool, nulls_first: bool) -> SortKey {
        SortKey { expr: 0, descending, nulls_first }
    }

    /// One value on its own, encoded into a fresh buffer.
    fn one(value: &Value, wide: usize, key: SortKey) -> Normal {
        let mut into: Normal = [0; WIDTH];
        write(&mut into, 0, wide, value, key).expect("a type the layout accepted");
        into
    }

    /// The encoded order of a column of values is the order of the values.
    ///
    /// The whole contract in one test, over the boundaries that catch a sign or an endianness
    /// mistake: either side of zero, the ends of the range, and a value whose high byte is smaller
    /// than another's while its low byte is larger, which is the case a little endian copy gets
    /// wrong and nothing else does.
    #[test]
    fn a_signed_column_encodes_into_the_order_it_compares_in() {
        let ty = LogicalType::Integer;
        let widths = layout(&[ty]).expect("an integer normalizes");
        let ascending = key(false, false);
        let mut held: Vec<i32> = vec![i32::MIN, -70000, -1, 0, 1, 255, 256, 70000, i32::MAX];
        held.sort_unstable();
        let encoded: Vec<Normal> =
            held.iter().map(|&v| one(&Value::Integer(v), widths[0], ascending)).collect();
        for pair in encoded.windows(2) {
            assert!(pair[0] < pair[1], "the bytes should rise with the values");
        }
    }

    /// Descending inverts the payload and leaves the tag where the null placement put it.
    ///
    /// The mistake this is here for is reversing the whole comparison, which moves the nulls too.
    /// `DESC NULLS LAST` puts the largest value first and the nulls after everything, and those are
    /// two independent decisions that a single reversal cannot express.
    #[test]
    fn a_descending_key_reverses_the_values_and_not_the_nulls() {
        let widths = layout(&[LogicalType::BigInt]).expect("a bigint normalizes");
        let falling = key(true, false);
        let low = one(&Value::BigInt(1), widths[0], falling);
        let high = one(&Value::BigInt(9), widths[0], falling);
        let none = one(&Value::Null, widths[0], falling);
        assert!(high < low, "descending puts the larger value first");
        assert!(low < none, "nulls last puts a null after every value, direction or not");

        let rising_first = key(false, true);
        let none = one(&Value::Null, widths[0], rising_first);
        let low = one(&Value::BigInt(1), widths[0], rising_first);
        assert!(none < low, "nulls first puts a null before every value");
    }

    /// Two nulls are equal whichever way the key faces, because a null payload is never inverted.
    #[test]
    fn two_nulls_encode_the_same_bytes() {
        let widths = layout(&[LogicalType::Date]).expect("a date normalizes");
        for falling in [false, true] {
            for first in [false, true] {
                let at = key(falling, first);
                assert_eq!(one(&Value::Null, widths[0], at), one(&Value::Null, widths[0], at));
            }
        }
    }

    /// Several keys pack in priority order, so an earlier key decides before a later one is read.
    #[test]
    fn the_first_key_decides_before_the_second_is_looked_at() {
        let types = [LogicalType::Date, LogicalType::BigInt, LogicalType::Integer];
        let widths = layout(&types).expect("the clustered layout normalizes");
        assert_eq!(widths, vec![5, 9, 5], "a tag and the payload, per key");
        let rising = key(false, false);
        let pack = |day: i32, order: i64, line: i32| {
            let mut into: Normal = [0; WIDTH];
            let mut at = 0;
            for (wide, value) in
                widths.iter().zip([Value::Date(day), Value::BigInt(order), Value::Integer(line)])
            {
                write(&mut into, at, *wide, &value, rising).expect("encodes");
                at += wide;
            }
            into
        };
        assert!(pack(1, 9, 9) < pack(2, 0, 0), "the date decides first");
        assert!(pack(1, 1, 9) < pack(1, 2, 0), "then the order key");
        assert!(pack(1, 1, 1) < pack(1, 1, 2), "then the line number");
        assert_eq!(pack(1, 1, 1), pack(1, 1, 1), "and equal rows encode equal");
    }

    /// A type with no fixed width order, and a list too wide to hold, both refuse the whole list.
    #[test]
    fn a_key_list_this_cannot_hold_takes_the_other_path() {
        use LogicalType as T;
        assert!(layout(&[T::Varchar]).is_none(), "a string has no fixed width");
        assert!(layout(&[T::Double]).is_none(), "a double does not order like its bytes");
        assert!(layout(&[T::Interval]).is_none(), "an interval orders over its fields folded");
        assert!(layout(&[T::BigInt, T::Varchar]).is_none(), "one bad key refuses the list");
        assert!(layout(&[T::HugeInt]).is_some(), "seventeen bytes fits");
        assert!(layout(&[T::HugeInt, T::HugeInt]).is_none(), "thirty four does not");
        assert!(layout(&[]).is_none(), "and a sort with no keys is not a sort");
        // A decimal takes the width its digits need, which is the whole point of storing it that
        // way, so a narrow one leaves room for keys a wide one would not.
        assert_eq!(layout(&[T::Decimal { width: 9, scale: 2 }]), Some(vec![5]));
        assert_eq!(layout(&[T::Decimal { width: 38, scale: 2 }]), Some(vec![17]));
    }

    /// The unsigned types are not biased, since they are already in the order their bytes are.
    #[test]
    fn an_unsigned_column_encodes_without_the_bias() {
        let widths = layout(&[LogicalType::UBigInt]).expect("normalizes");
        let rising = key(false, false);
        let held = [0u64, 1, u64::from(u32::MAX), u64::MAX];
        let encoded: Vec<Normal> =
            held.iter().map(|&v| one(&Value::UBigInt(v), widths[0], rising)).collect();
        for pair in encoded.windows(2) {
            assert!(pair[0] < pair[1], "the bytes should rise with the values");
        }
    }

    /// A column written at once is the same bytes as its values written one at a time.
    ///
    /// Over every width a key can be, both directions and both null placements, with a null in the
    /// column and a second key after it, so a slot that spilled into its neighbour would show.
    #[test]
    fn a_column_writes_the_bytes_its_values_write_one_at_a_time() {
        let columns = [
            (LogicalType::Boolean, vec![Value::Boolean(true), Value::Null, Value::Boolean(false)]),
            (LogicalType::SmallInt, vec![Value::SmallInt(-2), Value::SmallInt(7), Value::Null]),
            (LogicalType::Date, vec![Value::Date(-1), Value::Null, Value::Date(19000)]),
            (LogicalType::BigInt, vec![Value::Null, Value::BigInt(i64::MIN), Value::BigInt(3)]),
            (
                LogicalType::UInteger,
                vec![Value::UInteger(0), Value::UInteger(u32::MAX), Value::Null],
            ),
            (
                LogicalType::Decimal { width: 38, scale: 2 },
                vec![
                    Value::Decimal { unscaled: -5, width: 38, scale: 2 },
                    Value::Null,
                    Value::Decimal { unscaled: 12, width: 38, scale: 2 },
                ],
            ),
        ];
        for (ty, values) in columns {
            let widths = layout(&[ty.clone(), LogicalType::Integer]).expect("normalizes");
            let column = Vector::from_values(ty.clone(), &values).expect("a column of them");
            let after = Vector::from_values(LogicalType::Integer, &vec![Value::Integer(-9); 3])
                .expect("a second key");
            for (descending, nulls_first) in
                [(false, false), (false, true), (true, false), (true, true)]
            {
                let first = key(descending, nulls_first);
                let second = key(false, false);
                let mut together: Vec<Normal> = vec![[0; WIDTH]; values.len()];
                write_column(together.iter_mut(), 0, widths[0], &column, first).expect("writes");
                write_column(together.iter_mut(), widths[0], widths[1], &after, second)
                    .expect("writes");
                for (row, value) in values.iter().enumerate() {
                    let mut alone: Normal = [0; WIDTH];
                    write(&mut alone, 0, widths[0], value, first).expect("writes");
                    write(&mut alone, widths[0], widths[1], &Value::Integer(-9), second)
                        .expect("writes");
                    assert_eq!(together[row], alone, "{ty} row {row} desc {descending}");
                }
            }
        }
    }
}

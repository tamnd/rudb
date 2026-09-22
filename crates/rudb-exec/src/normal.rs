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
//! A type with no fixed width, which is `VARCHAR` and `BLOB`. A key of those is a prefix and a
//! tiebreak against the real value, and that is a second design rather than a longer buffer. So a
//! sort with a string key takes the old path, whole, rather than normalizing the keys around it.
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

use rudb_common::{Error, LogicalType, PhysicalType, Result, Value};
use rudb_plan::SortKey;

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
/// # Errors
///
/// If the value is not one [`layout`] said this key would hold. That is an internal error and not a
/// user's mistake, because the width came off the same type the vector was built from, and it is
/// reported rather than encoded wrong because a key that silently took the wrong number of bytes
/// would push every key after it along and put the rows in an order nobody asked for.
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

    use super::{Normal, WIDTH, layout, write};

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
        let ty = rudb_common::LogicalType::Integer;
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
        let widths = layout(&[rudb_common::LogicalType::BigInt]).expect("a bigint normalizes");
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
        let widths = layout(&[rudb_common::LogicalType::Date]).expect("a date normalizes");
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
        let types = [
            rudb_common::LogicalType::Date,
            rudb_common::LogicalType::BigInt,
            rudb_common::LogicalType::Integer,
        ];
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
        use rudb_common::LogicalType as T;
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
        let widths = layout(&[rudb_common::LogicalType::UBigInt]).expect("normalizes");
        let rising = key(false, false);
        let held = [0u64, 1, u64::from(u32::MAX), u64::MAX];
        let encoded: Vec<Normal> =
            held.iter().map(|&v| one(&Value::UBigInt(v), widths[0], rising)).collect();
        for pair in encoded.windows(2) {
            assert!(pair[0] < pair[1], "the bytes should rise with the values");
        }
    }
}

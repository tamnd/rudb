//! The bitwise functions: `&`, `|`, `xor`, `~`, the two shifts and `bit_count`.
//!
//! The binder has already cast both sides of a binary one to the type they meet at, so everything
//! here works on the bit pattern of one integer type at a time. A value is read as the raw bits of
//! its own width and written back masked to that width, which is what makes `~5::UTINYINT` 250 and
//! `~5` minus 6 without a separate path per type.

use rudb_common::{Error, Result, Value};

use crate::number::integral;

/// The answer to one of the bitwise functions, or `None` if `name` is not one of them.
pub(crate) fn value(name: &str, args: &[Value]) -> Option<Result<Value>> {
    let answer = match (name, args) {
        ("&", [left, right]) => both(left, right, |a, b| a & b),
        ("|", [left, right]) => both(left, right, |a, b| a | b),
        ("xor", [left, right]) => both(left, right, |a, b| a ^ b),
        ("~", [only]) => raw(only)
            .map(|(bits, width)| back(!bits, width))
            .ok_or_else(|| Error::internal(format!("~ of {only}"))),
        ("<<", [input, shift]) => shift_left(input, shift),
        (">>", [input, shift]) => Ok(shift_right(input, shift)),
        ("bit_count", [only]) => raw(only)
            .map(|(bits, _)| {
                // The pin counts into a TINYINT, so the 128 bits of a HUGEINT minus one wrap.
                #[expect(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                let count = bits.count_ones() as u8 as i8;
                Value::TinyInt(count)
            })
            .ok_or_else(|| Error::internal(format!("bit_count of {only}"))),
        _ => return None,
    };
    Some(answer)
}

/// How wide an integer type is and whether it has a sign.
#[derive(Clone, Copy)]
struct Width {
    bits: u32,
    signed: bool,
}

/// The value's bit pattern, zero extended from its own width, and that width.
#[expect(clippy::cast_sign_loss)]
fn raw(value: &Value) -> Option<(u128, Width)> {
    let signed = |bits| Width { bits, signed: true };
    let unsigned = |bits| Width { bits, signed: false };
    Some(match *value {
        Value::TinyInt(v) => (u128::from(v as u8), signed(8)),
        Value::SmallInt(v) => (u128::from(v as u16), signed(16)),
        Value::Integer(v) => (u128::from(v as u32), signed(32)),
        Value::BigInt(v) => (u128::from(v as u64), signed(64)),
        Value::HugeInt(v) => (v as u128, signed(128)),
        Value::UTinyInt(v) => (u128::from(v), unsigned(8)),
        Value::USmallInt(v) => (u128::from(v), unsigned(16)),
        Value::UInteger(v) => (u128::from(v), unsigned(32)),
        Value::UBigInt(v) => (u128::from(v), unsigned(64)),
        Value::UHugeInt(v) => (v, unsigned(128)),
        _ => return None,
    })
}

/// A bit pattern as a value of the type `width` describes, keeping only the low `width` bits.
#[expect(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn back(bits: u128, width: Width) -> Value {
    match (width.bits, width.signed) {
        (8, true) => Value::TinyInt(bits as u8 as i8),
        (16, true) => Value::SmallInt(bits as u16 as i16),
        (32, true) => Value::Integer(bits as u32 as i32),
        (64, true) => Value::BigInt(bits as u64 as i64),
        (_, true) => Value::HugeInt(bits as i128),
        (8, false) => Value::UTinyInt(bits as u8),
        (16, false) => Value::USmallInt(bits as u16),
        (32, false) => Value::UInteger(bits as u32),
        (64, false) => Value::UBigInt(bits as u64),
        (_, false) => Value::UHugeInt(bits),
    }
}

fn both(left: &Value, right: &Value, body: fn(u128, u128) -> u128) -> Result<Value> {
    match (raw(left), raw(right)) {
        (Some((a, width)), Some((b, _))) => Ok(back(body(a, b), width)),
        _ => Err(Error::internal(format!("a bitwise function of {left} and {right}"))),
    }
}

/// How far a shift goes, with anything past what an `i128` holds read as far enough to empty the
/// value, which is all a `UHUGEINT` count that large can mean.
fn count(shift: &Value) -> i128 {
    integral(shift).unwrap_or(i128::MAX)
}

/// `<<`, with the pin's four refusals in the order it checks them.
///
/// An unsigned type may shift a one into its top bit and a signed one may not, which is the pin's
/// rule: `1::UTINYINT << 7` is 128 and `1::TINYINT << 7` is an overflow.
fn shift_left(input: &Value, shift: &Value) -> Result<Value> {
    let Some((bits, width)) = raw(input) else {
        return Err(Error::internal(format!("{input} << {shift}")));
    };
    let negative = width.signed && (bits >> (width.bits - 1)) & 1 == 1;
    if negative {
        return Err(Error::out_of_range(format!("Cannot left-shift negative number {input}")));
    }
    let by = count(shift);
    if by < 0 {
        return Err(Error::out_of_range(format!("Cannot left-shift by negative number {shift}")));
    }
    let most = i128::from(width.bits) + i128::from(!width.signed);
    if by >= most {
        if bits == 0 {
            return Ok(back(0, width));
        }
        return Err(Error::out_of_range(format!("Left-shift value {shift} is out of range")));
    }
    if by == 0 || bits == 0 {
        return Ok(input.clone());
    }
    // Both are below 129 here, and `most - by - 1` is at most 127.
    #[expect(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let (by, room) = (by as u32, (most - by - 1) as u32);
    if bits >= 1u128 << room {
        return Err(Error::out_of_range(format!("Overflow in left shift ({input} << {shift})")));
    }
    Ok(back(bits << by, width))
}

/// `>>`, which is arithmetic on a signed type and answers zero for a count outside the width, a
/// negative one included, whatever the sign of the value.
fn shift_right(input: &Value, shift: &Value) -> Value {
    let Some((bits, width)) = raw(input) else {
        return input.clone();
    };
    let by = count(shift);
    if by < 0 || by >= i128::from(width.bits) {
        return back(0, width);
    }
    #[expect(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let by = by as u32;
    if width.signed {
        // Move the sign up to the top of the `i128` so the shift carries it down again.
        let spare = 128 - width.bits;
        #[expect(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
        let shifted = (((bits << spare) as i128) >> spare >> by) as u128;
        back(shifted, width)
    } else {
        back(bits >> by, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &[Value]) -> Result<Value> {
        value(name, args).expect("a bitwise function")
    }

    #[test]
    fn the_binary_ones_keep_the_type_they_were_given() {
        assert_eq!(call("&", &[Value::Integer(5), Value::Integer(3)]).unwrap(), Value::Integer(1));
        assert_eq!(call("|", &[Value::Integer(5), Value::Integer(3)]).unwrap(), Value::Integer(7));
        assert_eq!(
            call("xor", &[Value::TinyInt(5), Value::TinyInt(3)]).unwrap(),
            Value::TinyInt(6)
        );
        assert_eq!(call("~", &[Value::Integer(5)]).unwrap(), Value::Integer(-6));
        assert_eq!(call("~", &[Value::UTinyInt(5)]).unwrap(), Value::UTinyInt(250));
    }

    #[test]
    fn a_left_shift_refuses_what_the_pin_refuses() {
        let shl = |a: Value, b: Value| call("<<", &[a, b]).map_err(|e| e.to_string());
        assert_eq!(shl(Value::Integer(1), Value::Integer(4)), Ok(Value::Integer(16)));
        assert_eq!(shl(Value::UTinyInt(1), Value::UTinyInt(7)), Ok(Value::UTinyInt(128)));
        assert_eq!(shl(Value::Integer(0), Value::Integer(99)), Ok(Value::Integer(0)));
        assert!(shl(Value::Integer(1), Value::Integer(31)).unwrap_err().contains("(1 << 31)"));
        assert!(shl(Value::UTinyInt(1), Value::UTinyInt(8)).unwrap_err().contains("(1 << 8)"));
        assert!(shl(Value::Integer(1), Value::Integer(70)).unwrap_err().contains("value 70"));
        assert!(shl(Value::Integer(-1), Value::Integer(1)).unwrap_err().contains("number -1"));
        assert!(shl(Value::Integer(1), Value::Integer(-1)).unwrap_err().contains("by negative"));
    }

    #[test]
    fn a_right_shift_carries_the_sign_and_empties_past_the_width() {
        let shr = |a: Value, b: Value| call(">>", &[a, b]).unwrap();
        assert_eq!(shr(Value::Integer(256), Value::Integer(4)), Value::Integer(16));
        assert_eq!(shr(Value::Integer(-8), Value::Integer(1)), Value::Integer(-4));
        assert_eq!(shr(Value::Integer(-8), Value::Integer(70)), Value::Integer(0));
        assert_eq!(shr(Value::Integer(8), Value::Integer(-1)), Value::Integer(0));
    }

    #[test]
    fn bit_count_counts_the_twos_complement_and_wraps_at_128() {
        assert_eq!(call("bit_count", &[Value::BigInt(-1)]).unwrap(), Value::TinyInt(64));
        assert_eq!(call("bit_count", &[Value::HugeInt(-1)]).unwrap(), Value::TinyInt(-128));
        assert_eq!(call("bit_count", &[Value::TinyInt(7)]).unwrap(), Value::TinyInt(3));
    }
}

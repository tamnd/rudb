//! The functions over bit strings, and `bitstring_agg`, which makes one.
//!
//! A bit string is a [`Value::Bit`] in the layout [`rudb_common::bit`] describes. The operators
//! work a byte at a time and set the padding bits again after, so an answer is laid out the way
//! any other bit string is. The errors are the pin's, word for word.

use rudb_common::{Error, Result, Value, bit};

/// The most bits `bitstring_agg` makes, which is the pin's cap.
const MAX_BITS: u128 = 1_000_000_000;

/// The answer to a function over bit strings, or `None` when the call is not one.
pub(crate) fn value(name: &str, args: &[Value]) -> Option<Result<Value>> {
    let answer = match (name, args) {
        ("&", [Value::Bit(left), Value::Bit(right)]) => both(left, right, "AND", |a, b| a & b),
        ("|", [Value::Bit(left), Value::Bit(right)]) => both(left, right, "OR", |a, b| a | b),
        ("xor", [Value::Bit(left), Value::Bit(right)]) => both(left, right, "XOR", |a, b| a ^ b),
        ("~", [Value::Bit(bits)]) => {
            let mut out = bits.clone();
            out[1..].iter_mut().for_each(|byte| *byte = !*byte);
            bit::finalize(&mut out);
            Ok(Value::Bit(out))
        }
        ("<<", [Value::Bit(bits), shift]) => shifted(bits, shift, true),
        (">>", [Value::Bit(bits), shift]) => shifted(bits, shift, false),
        ("bit_count", [Value::Bit(bits)]) => Ok(Value::BigInt(bit::count(bits) as i64)),
        ("length" | "bit_length", [Value::Bit(bits)]) => Ok(Value::BigInt(bit::len(bits) as i64)),
        ("octet_length", [Value::Bit(bits)]) => Ok(Value::BigInt(bits.len() as i64 - 1)),
        ("octet_length", [Value::Blob(bytes)]) => Ok(Value::BigInt(bytes.len() as i64)),
        ("bit_length", [Value::Varchar(text)]) => Ok(Value::BigInt(text.len() as i64 * 8)),
        ("get_bit", [Value::Bit(bits), at]) => {
            index(bits, at).map(|at| Value::Integer(i32::from(bit::get(bits, at))))
        }
        ("set_bit", [Value::Bit(bits), at, to]) => set_bit(bits, at, to),
        ("bit_position", [Value::Bit(needle), Value::Bit(bits)]) => {
            Ok(Value::Integer(position(needle, bits)))
        }
        ("bitstring", [Value::Varchar(text), len]) => {
            if text.is_empty() {
                return Some(Err(Error::conversion("Cannot cast empty string to BIT")));
            }
            bit::from_text(text).and_then(|bits| widened(&bits, len))
        }
        ("bitstring", [Value::Bit(bits), len]) => widened(bits, len),
        _ => return None,
    };
    Some(answer)
}

/// Two bit strings of the same length put together a byte at a time.
fn both(left: &[u8], right: &[u8], what: &str, op: fn(u8, u8) -> u8) -> Result<Value> {
    if bit::len(left) != bit::len(right) {
        return Err(Error::invalid_input(format!("Cannot {what} bit strings of different sizes")));
    }
    let mut out = left.to_vec();
    for (byte, theirs) in out[1..].iter_mut().zip(&right[1..]) {
        *byte = op(*byte, *theirs);
    }
    bit::finalize(&mut out);
    Ok(Value::Bit(out))
}

/// A whole number argument as an `i64`.
fn whole(value: &Value) -> Result<i64> {
    crate::number::integral(value)
        .and_then(|n| i64::try_from(n).ok())
        .ok_or_else(|| Error::internal(format!("{value} is not a whole number")))
}

/// A bit string shifted left or right by some bits, keeping its length and filling with zeros. A
/// shift past the end, or one that is negative, leaves no bit standing.
fn shifted(bits: &[u8], shift: &Value, left: bool) -> Result<Value> {
    let len = bit::len(bits);
    let shift = usize::try_from(whole(shift)?).unwrap_or(usize::MAX);
    let mut out = bits.to_vec();
    for n in 0..len {
        let from = if left {
            n.checked_add(shift).filter(|&from| from < len)
        } else {
            n.checked_sub(shift)
        };
        bit::set(&mut out, n, from.is_some_and(|from| bit::get(bits, from)));
    }
    Ok(Value::Bit(out))
}

/// A bit index that is in range, or the pin's error.
fn index(bits: &[u8], at: &Value) -> Result<usize> {
    let at = whole(at)?;
    let len = bit::len(bits);
    usize::try_from(at).ok().filter(|&at| at < len).ok_or_else(|| {
        Error::out_of_range(format!("bit index {at} out of valid range (0..{})", len - 1))
    })
}

fn set_bit(bits: &[u8], at: &Value, to: &Value) -> Result<Value> {
    let to = whole(to)?;
    if to != 0 && to != 1 {
        return Err(Error::invalid_input("The new bit must be 1 or 0"));
    }
    let at = index(bits, at)?;
    let mut out = bits.to_vec();
    bit::set(&mut out, at, to == 1);
    Ok(Value::Bit(out))
}

/// Where `needle` first starts in `bits`, counting from 1, or 0 when it does not.
///
/// The pin's scan starts the needle over at a bit that does not match without looking at that bit
/// again, so it misses a match that starts inside a false start, and this scans the same way.
fn position(needle: &[u8], bits: &[u8]) -> i32 {
    let wanted = bit::len(needle);
    let mut matched = 0;
    for n in 0..bit::len(bits) {
        if bit::get(bits, n) == bit::get(needle, matched) {
            matched += 1;
            if matched == wanted {
                return i32::try_from(n + 2 - wanted).unwrap_or(i32::MAX);
            }
        } else {
            matched = 0;
        }
    }
    0
}

/// A bit string made `len` bits long by putting zeros in front of it.
fn widened(bits: &[u8], len: &Value) -> Result<Value> {
    let had = bit::len(bits);
    let len = usize::try_from(whole(len)?)
        .ok()
        .filter(|&len| len >= had)
        .ok_or_else(|| Error::invalid_input("Length must be equal or larger than input string"))?;
    let mut out = bit::zeros(len);
    for n in 0..had {
        bit::set(&mut out, len - had + n, bit::get(bits, n));
    }
    Ok(Value::Bit(out))
}

/// A group of `bitstring_agg(x, min, max)`: a bit for each whole number from `min` to `max`, set
/// for the ones that came up.
#[derive(Debug, Clone, Default)]
pub(crate) struct Gathered {
    /// The bits so far and the smallest number they stand for, once a row has come.
    bits: Option<(Vec<u8>, i128)>,
}

impl Gathered {
    /// Sets the bit of a value that is not null, taking the range from the first row's `min` and
    /// `max`.
    pub(crate) fn update(&mut self, value: &Value, bounds: &[Value]) -> Result<()> {
        let n = number(value)?;
        if self.bits.is_none() {
            let [min, max] = bounds else {
                return Err(Error::binder(
                    "Could not retrieve required statistics. Alternatively, try by providing the \
                     statistics explicitly: BITSTRING_AGG(col, min, max) ",
                ));
            };
            if min.is_null() || max.is_null() {
                return Err(Error::binder(
                    "Could not retrieve required statistics. Alternatively, try by providing the \
                     statistics explicitly: BITSTRING_AGG(col, min, max) ",
                ));
            }
            let (low, high) = (number(min)?, number(max)?);
            if low > high {
                return Err(Error::invalid_input(format!(
                    "Invalid explicit bitstring range: Minimum ({min}) > maximum ({max})"
                )));
            }
            let range = high.abs_diff(low).saturating_add(1);
            if range > MAX_BITS {
                return Err(Error::out_of_range(format!(
                    "The range between min and max value ({min} <-> {max}) is too large for \
                     bitstring aggregation"
                )));
            }
            self.bits = Some((bit::zeros(range as usize), low));
        }
        let Some((bits, low)) = &mut self.bits else { unreachable!("set above") };
        let at = n.checked_sub(*low).and_then(|at| usize::try_from(at).ok());
        match at.filter(|&at| at < bit::len(bits)) {
            Some(at) => {
                bit::set(bits, at, true);
                Ok(())
            }
            None => {
                let high = *low + bit::len(bits) as i128 - 1;
                Err(Error::out_of_range(format!(
                    "Value {value} is outside of provided min and max range ({low} <-> {high})"
                )))
            }
        }
    }

    /// Takes in the bits of another group of the same call.
    pub(crate) fn combine(&mut self, other: &Self) {
        match (&mut self.bits, &other.bits) {
            (_, None) => {}
            (None, Some(_)) => self.bits.clone_from(&other.bits),
            (Some((mine, _)), Some((theirs, _))) => {
                for (byte, their) in mine[1..].iter_mut().zip(&theirs[1..]) {
                    *byte |= their;
                }
            }
        }
    }

    /// The bits, or null for a group that saw no rows.
    pub(crate) fn finish(&self) -> Value {
        self.bits.as_ref().map_or(Value::Null, |(bits, _)| Value::Bit(bits.clone()))
    }
}

/// A whole number of any width as an `i128`, which every `bitstring_agg` input fits except the
/// top half of `UHUGEINT`.
fn number(value: &Value) -> Result<i128> {
    crate::number::integral(value).ok_or_else(|| {
        Error::out_of_range(format!("Value {value} is too large for bitstring aggregation"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(text: &str) -> Value {
        Value::Bit(bit::from_text(text).unwrap())
    }

    fn call(name: &str, args: &[Value]) -> Result<String> {
        value(name, args).expect("a bit function").map(|answer| answer.to_string())
    }

    #[test]
    fn the_bit_operators_answer_what_the_pin_does() {
        let (a, b) = (bits("1100"), bits("1010"));
        assert_eq!(call("&", &[a.clone(), b.clone()]).unwrap(), "1000");
        assert_eq!(call("|", &[a.clone(), b.clone()]).unwrap(), "1110");
        assert_eq!(call("xor", &[a.clone(), b]).unwrap(), "0110");
        assert_eq!(call("~", std::slice::from_ref(&a)).unwrap(), "0011");
        assert_eq!(call("<<", &[a.clone(), Value::Integer(1)]).unwrap(), "1000");
        assert_eq!(call(">>", &[a.clone(), Value::Integer(1)]).unwrap(), "0110");
        assert_eq!(call(">>", &[a.clone(), Value::Integer(9)]).unwrap(), "0000");
        let error = call("&", &[a, bits("10")]).unwrap_err();
        assert_eq!(error.message(), "Cannot AND bit strings of different sizes");
    }

    #[test]
    fn the_bit_functions_count_read_and_write_single_bits() {
        assert_eq!(call("bit_count", &[bits("1101")]).unwrap(), "3");
        assert_eq!(call("octet_length", &[bits("111100001")]).unwrap(), "2");
        assert_eq!(call("get_bit", &[bits("0110"), Value::Integer(1)]).unwrap(), "1");
        assert_eq!(
            call("set_bit", &[bits("0110"), Value::Integer(0), Value::Integer(1)]).unwrap(),
            "1110"
        );
        assert_eq!(call("bit_position", &[bits("11"), bits("0011")]).unwrap(), "3");
        assert_eq!(
            call("bitstring", &[Value::Varchar("101".into()), Value::Integer(6)]).unwrap(),
            "000101"
        );
        let error = call("get_bit", &[bits("0110"), Value::Integer(4)]).unwrap_err();
        assert_eq!(error.message(), "bit index 4 out of valid range (0..3)");
    }

    #[test]
    fn bitstring_agg_sets_a_bit_per_value_between_the_bounds() {
        let mut group = Gathered::default();
        let bounds = [Value::Integer(1), Value::Integer(10)];
        for n in [1, 3, 10, 3] {
            group.update(&Value::Integer(n), &bounds).unwrap();
        }
        assert_eq!(group.finish().to_string(), "1010000001");
        let error = group.update(&Value::Integer(11), &bounds).unwrap_err();
        assert_eq!(error.message(), "Value 11 is outside of provided min and max range (1 <-> 10)");
        assert_eq!(Gathered::default().finish(), Value::Null);
    }
}

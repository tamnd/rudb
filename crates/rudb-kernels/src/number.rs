//! The numeric plumbing every kernel shares.
//!
//! Casting, arithmetic and comparison all need the same four things: the exact integer a value
//! holds, its approximate value as a double, the power of ten a decimal scale stands for, and the
//! rule for putting a wide result back into a narrow type. Writing those four once is not tidiness.
//! Two copies of the narrowing rule that disagree by one is a query that returns 2147483648 in one
//! operator and raises in another, and there is no test that would catch that except one that
//! happens to exercise both.

use rudb_common::{LogicalType, Value};

/// The exact integer a value holds, for the values that hold one.
///
/// A `UHUGEINT` above the `HUGEINT` range gives `None`, which the callers turn into a range error.
/// The alternative is a second signed and unsigned path through every kernel for a type nothing in
/// any benchmark uses.
pub(crate) fn integral(value: &Value) -> Option<i128> {
    match *value {
        Value::Boolean(v) => Some(i128::from(v)),
        Value::TinyInt(v) => Some(i128::from(v)),
        Value::SmallInt(v) => Some(i128::from(v)),
        Value::Integer(v) => Some(i128::from(v)),
        Value::BigInt(v) => Some(i128::from(v)),
        Value::HugeInt(v) => Some(v),
        Value::UTinyInt(v) => Some(i128::from(v)),
        Value::USmallInt(v) => Some(i128::from(v)),
        Value::UInteger(v) => Some(i128::from(v)),
        Value::UBigInt(v) => Some(i128::from(v)),
        Value::UHugeInt(v) => i128::try_from(v).ok(),
        _ => None,
    }
}

/// The value as a `f64`, for every numeric type.
///
/// A `HUGEINT` past 2^53 loses digits here, which is what a double is and is why nothing but a
/// float path calls this.
#[expect(
    clippy::cast_precision_loss,
    reason = "the exact paths are tried first and this is the fallback across representations"
)]
pub(crate) fn approximate(value: &Value) -> Option<f64> {
    match *value {
        Value::Float(v) => Some(f64::from(v)),
        Value::Double(v) => Some(v),
        Value::Decimal { unscaled, scale, .. } => Some(unscaled as f64 / pow10(scale) as f64),
        Value::UHugeInt(v) => Some(v as f64),
        _ => integral(value).map(|whole| whole as f64),
    }
}

/// Ten to the power of a decimal scale.
///
/// The scale is capped at 38 by [`LogicalType::decimal`], so this cannot overflow an `i128`.
pub(crate) fn pow10(scale: u8) -> i128 {
    let mut result: i128 = 1;
    for _ in 0..scale {
        result *= 10;
    }
    result
}

/// How many decimal digits a number is written with, which is what a decimal's width has to hold.
pub(crate) fn digits(unscaled: i128) -> u8 {
    let mut count: u8 = 1;
    let mut left = unscaled.unsigned_abs() / 10;
    while left > 0 {
        count += 1;
        left /= 10;
    }
    count
}

/// The same number written at a different scale, rounding half away from zero on the way down.
pub(crate) fn rescale(unscaled: i128, from: u8, to: u8) -> Option<i128> {
    if to >= from {
        unscaled.checked_mul(pow10(to - from))
    } else {
        let factor = pow10(from - to);
        let half = factor / 2;
        let shifted = if unscaled >= 0 { unscaled + half } else { unscaled - half };
        Some(shifted / factor)
    }
}

/// A wide result as a value of `target`, or `None` if it does not fit.
pub(crate) fn fit(whole: i128, target: &LogicalType) -> Option<Value> {
    match target {
        LogicalType::TinyInt => i8::try_from(whole).ok().map(Value::TinyInt),
        LogicalType::SmallInt => i16::try_from(whole).ok().map(Value::SmallInt),
        LogicalType::Integer => i32::try_from(whole).ok().map(Value::Integer),
        LogicalType::BigInt => i64::try_from(whole).ok().map(Value::BigInt),
        LogicalType::HugeInt => Some(Value::HugeInt(whole)),
        LogicalType::UTinyInt => u8::try_from(whole).ok().map(Value::UTinyInt),
        LogicalType::USmallInt => u16::try_from(whole).ok().map(Value::USmallInt),
        LogicalType::UInteger => u32::try_from(whole).ok().map(Value::UInteger),
        LogicalType::UBigInt => u64::try_from(whole).ok().map(Value::UBigInt),
        LogicalType::UHugeInt => u128::try_from(whole).ok().map(Value::UHugeInt),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_counts_what_a_decimal_width_has_to_hold() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(-1234), 4);
    }

    #[test]
    fn rescaling_up_is_exact_and_rescaling_down_rounds_away_from_zero() {
        assert_eq!(rescale(5, 1, 3), Some(500));
        assert_eq!(rescale(15, 1, 0), Some(2));
        assert_eq!(rescale(-15, 1, 0), Some(-2));
    }

    #[test]
    fn a_result_that_does_not_fit_the_target_says_so_rather_than_wrapping() {
        assert_eq!(fit(300, &LogicalType::TinyInt), None);
        assert_eq!(fit(-1, &LogicalType::UInteger), None);
        assert_eq!(fit(127, &LogicalType::TinyInt), Some(Value::TinyInt(127)));
    }
}

//! The math functions: the ones over a double, rounding in its five spellings, and the few that
//! work on whole numbers.
//!
//! Everything over a double follows the pin's default, which is IEEE 754 rather than SQL. A value
//! outside a function's domain is a NaN or an infinity and not an error, so `sqrt(-1)` is `-nan`
//! and `ln(0)` is `-inf` on both sides.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Data, Form, Validity, Vector};

use crate::number::{approximate, fit, integral, pow10};
use crate::scalar::{finish, over_valid};
use crate::shape::nulls_of;

/// Whether `name` is one of the functions from a double to a double this file has a loop for.
fn over_double(name: &str) -> Option<fn(f64) -> f64> {
    Some(match name {
        "sqrt" => f64::sqrt,
        "cbrt" => f64::cbrt,
        "exp" => f64::exp,
        "ln" => f64::ln,
        // One argument is base ten on the pin, and not the natural logarithm it is in C.
        "log" | "log10" => f64::log10,
        "log2" => f64::log2,
        "sin" => f64::sin,
        "cos" => f64::cos,
        "tan" => f64::tan,
        "cot" => |x: f64| 1.0 / x.tan(),
        "asin" => f64::asin,
        "acos" => f64::acos,
        "atan" => f64::atan,
        "sinh" => f64::sinh,
        "cosh" => f64::cosh,
        "tanh" => f64::tanh,
        "asinh" => f64::asinh,
        "acosh" => f64::acosh,
        "atanh" => f64::atanh,
        "degrees" => f64::to_degrees,
        "radians" => f64::to_radians,
        "even" => even,
        "gamma" => libm::tgamma,
        "lgamma" => libm::lgamma,
        _ => return None,
    })
}

/// `even`, which is away from zero to the next even whole number.
fn even(x: f64) -> f64 {
    let away = if x >= 0.0 { x.ceil() } else { -(-x).ceil() };
    if (away / 2.0).floor() * 2.0 == away {
        away
    } else if x >= 0.0 {
        away + 1.0
    } else {
        away - 1.0
    }
}

/// A loop over a column of doubles, for the one argument functions from a double to a double.
///
/// These are the functions a query runs over every row of a table, `sqrt(x)` or `ln(price)`, and
/// the row at a time path pays for a `Value` per row for what is a single instruction.
pub(crate) fn vectorized<A: Fn(usize) -> usize>(
    name: &str,
    data: &Data,
    at: A,
    base: Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    let body = over_double(name).or(match name {
        "floor" => Some(f64::floor as fn(f64) -> f64),
        "ceil" => Some(f64::ceil),
        "round" => Some(f64::round),
        "trunc" => Some(f64::trunc),
        _ => None,
    });
    let (Some(body), Data::Float64(held), LogicalType::Double) = (body, data, returns) else {
        return Ok(None);
    };
    let mut out = vec![0.0f64; rows];
    let validity = over_valid(rows, base, |index| {
        out[index] = body(held[at(index)]);
        Ok(())
    })?;
    finish(returns, Data::Float64(out.into()), validity)
}

/// A column of doubles rounded to a count of digits that is the same for every row, which is how
/// `round(x, 2)` is always written.
pub(crate) fn rounded_column(
    name: &str,
    value: &Vector,
    digits: &Vector,
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    if !matches!(name, "round" | "trunc" | "round_even")
        || *returns != LogicalType::Double
        || value.form() != Form::Flat
        || digits.form() != Form::Constant
    {
        return Ok(None);
    }
    let (Some(Data::Float64(held)), Some(count)) = (value.data(), digits.value_at(0).as_i64())
    else {
        return Ok(None);
    };
    let rule = Rule::of(name);
    let mut out = vec![0.0f64; rows];
    let validity = over_valid(rows, nulls_of(value), |index| {
        out[index] = rounded_double(rule, held[index], count);
        Ok(())
    })?;
    finish(returns, Data::Float64(out.into()), validity)
}

/// The math calls on one row, once no argument is null, or `None` for any other name.
pub(crate) fn value(name: &str, args: &[Value], returns: &LogicalType) -> Option<Result<Value>> {
    if let Some(name) = name.strip_prefix("__rudb_strict_") {
        return Some(checked(name, args).and_then(|()| {
            value(name, args, returns)
                .unwrap_or_else(|| Err(Error::internal(format!("a strict {name}"))))
        }));
    }
    if let (Some(body), [only]) = (over_double(name), args) {
        return Some(double(only).map(|x| Value::Double(body(x))));
    }
    let answer = match (name, args) {
        ("pi", []) => Ok(Value::Double(std::f64::consts::PI)),
        ("setseed", [seed]) => crate::random::setseed(seed),
        ("log", [base, x]) => two(base, x, |base, x| x.log10() / base.log10()),
        ("pow", [x, y]) => two(x, y, f64::powf),
        ("atan2", [y, x]) => two(y, x, f64::atan2),
        ("nextafter", [from, to]) => two(from, to, next_after),
        ("signbit", [only]) => double(only).map(|x| Value::Boolean(x.is_sign_negative())),
        ("isnan", [only]) => double(only).map(|x| Value::Boolean(x.is_nan())),
        ("isinf", [only]) => double(only).map(|x| Value::Boolean(x.is_infinite())),
        ("isfinite", [only]) => double(only).map(|x| Value::Boolean(x.is_finite())),
        ("sign", [only]) => Ok(Value::TinyInt(sign(only))),
        ("floor" | "ceil", [only]) => floored(name == "ceil", only, returns),
        ("round" | "trunc" | "round_even", [only]) => rounded(name, only, 0, returns),
        ("round" | "trunc" | "round_even", [only, digits]) => match digits.as_i64() {
            Some(digits) => rounded(name, only, digits, returns),
            None => Err(Error::internal(format!("{name} to {digits} digits"))),
        },
        ("gcd", [left, right]) => whole(left, right, returns, false),
        ("lcm", [left, right]) => whole(left, right, returns, true),
        ("factorial", [only]) => factorial(only),
        ("binom", [n, k]) => binom(n, k),
        _ => return None,
    };
    Some(answer)
}

/// The domain checks `SET ieee_floating_point_ops = false` asks for, in the pin's words.
fn checked(name: &str, args: &[Value]) -> Result<()> {
    let doubles = args.iter().map(double).collect::<Result<Vec<_>>>()?;
    let logarithm = |x: f64| {
        if x < 0.0 {
            Err(Error::out_of_range("cannot take logarithm of a negative number"))
        } else if x == 0.0 {
            Err(Error::out_of_range("cannot take logarithm of zero"))
        } else {
            Ok(())
        }
    };
    let finite = |x: f64| {
        if x.is_infinite() {
            Err(Error::out_of_range(format!(
                "input value {} is out of range for numeric function",
                if x > 0.0 { "inf" } else { "-inf" }
            )))
        } else {
            Ok(())
        }
    };
    let unit = |x: f64, spelled: &str| {
        if (-1.0..=1.0).contains(&x) || x.is_nan() {
            Ok(())
        } else {
            Err(Error::invalid_input(format!("{spelled} is undefined outside [-1,1]")))
        }
    };
    match (name, doubles.as_slice()) {
        ("sqrt", [x]) if *x < 0.0 => {
            Err(Error::out_of_range("cannot take square root of a negative number"))
        }
        ("ln" | "log" | "log10" | "log2", [x]) => logarithm(*x),
        ("log", [base, x]) => {
            logarithm(*base)?;
            if base.log10() == 0.0 {
                return Err(Error::out_of_range("division by zero in based logarithm"));
            }
            logarithm(*x)
        }
        ("sin" | "cos" | "tan", [x]) => finite(*x),
        ("asin", [x]) => finite(*x).and_then(|()| unit(*x, "ASIN")),
        ("acos", [x]) => finite(*x).and_then(|()| unit(*x, "ACOS")),
        ("atanh", [x]) => unit(*x, "ATANH"),
        ("cot", [x]) => {
            finite(*x)?;
            if *x == 0.0 {
                return Err(Error::out_of_range(
                    "input value 0.000000 is out of range for numeric function cotangent",
                ));
            }
            Ok(())
        }
        ("gamma", [x]) if *x == 0.0 => Err(Error::out_of_range("cannot take gamma of zero")),
        ("lgamma", [x]) if *x == 0.0 => Err(Error::out_of_range("cannot take log gamma of zero")),
        ("pow", [base, exponent]) if *base == 0.0 && *exponent < 0.0 => {
            Err(Error::out_of_range("zero raised to a negative power is undefined"))
        }
        _ => Ok(()),
    }
}

/// The argument as a double, which the signature cast it to.
fn double(value: &Value) -> Result<f64> {
    approximate(value).ok_or_else(|| Error::internal(format!("a double of {value}")))
}

fn two(left: &Value, right: &Value, body: impl Fn(f64, f64) -> f64) -> Result<Value> {
    Ok(Value::Double(body(double(left)?, double(right)?)))
}

/// C's `nextafter`, which the standard library does not have.
fn next_after(from: f64, to: f64) -> f64 {
    if from.is_nan() || to.is_nan() {
        return f64::NAN;
    }
    if from == to {
        return to;
    }
    if from == 0.0 {
        let least = f64::from_bits(1);
        return if to > 0.0 { least } else { -least };
    }
    let bits = from.to_bits();
    let up = (to > from) == (from > 0.0);
    f64::from_bits(if up { bits + 1 } else { bits - 1 })
}

/// `sign`, which is -1, 0 or 1 whatever the type, and 0 for a NaN.
fn sign(value: &Value) -> i8 {
    if let Some(whole) = integral(value) {
        return whole.signum() as i8;
    }
    if let Value::Decimal { unscaled, .. } = value {
        return unscaled.signum() as i8;
    }
    match approximate(value) {
        Some(x) if x > 0.0 => 1,
        Some(x) if x < 0.0 => -1,
        _ => 0,
    }
}

/// `floor` and `ceil`. A decimal keeps its width and has no scale left, and the signature cast
/// every whole number to a double on the way in.
fn floored(up: bool, value: &Value, returns: &LogicalType) -> Result<Value> {
    match *value {
        Value::Float(x) => Ok(Value::Float(if up { x.ceil() } else { x.floor() })),
        Value::Double(x) => Ok(Value::Double(if up { x.ceil() } else { x.floor() })),
        Value::Decimal { unscaled, scale, .. } => {
            let step = pow10(scale);
            let mut whole = unscaled / step;
            let rest = unscaled % step;
            if up && rest > 0 {
                whole += 1;
            } else if !up && rest < 0 {
                whole -= 1;
            }
            decimal(whole, returns)
        }
        _ => Err(Error::internal(format!("floor of a {}", value.logical_type()))),
    }
}

/// A decimal of `returns`, holding `unscaled` at the scale `returns` has.
fn decimal(unscaled: i128, returns: &LogicalType) -> Result<Value> {
    let LogicalType::Decimal { width, scale } = *returns else {
        return Err(Error::internal(format!("a decimal answer typed {returns}")));
    };
    Ok(Value::Decimal { unscaled, width, scale })
}

/// Which way a rounding goes at the halfway point, or whether it rounds at all.
#[derive(Clone, Copy)]
enum Rule {
    /// Half away from zero, which is `round`.
    Away,
    /// Half to the even neighbour, which is `round_even` and `roundbankers`.
    Even,
    /// Toward zero, which is `trunc`.
    Toward,
}

impl Rule {
    fn of(name: &str) -> Self {
        match name {
            "trunc" => Self::Toward,
            "round_even" => Self::Even,
            _ => Self::Away,
        }
    }

    fn double(self, x: f64) -> f64 {
        match self {
            Self::Away => x.round(),
            Self::Even => x.round_ties_even(),
            Self::Toward => x.trunc(),
        }
    }

    /// `whole` divided by `step`, rounded this way.
    fn quotient(self, whole: i128, step: i128) -> i128 {
        let quotient = whole / step;
        let rest = (whole % step).abs() * 2;
        let bump = match self {
            Self::Toward => false,
            Self::Away => rest >= step,
            Self::Even => rest > step || (rest == step && quotient % 2 != 0),
        };
        if bump { quotient + whole.signum() } else { quotient }
    }
}

/// `round`, `round_even` and `trunc` to `digits` places after the point, or before it when the
/// count is negative.
///
/// A decimal comes back at the scale the binder picked for it, which is the count when that was a
/// literal below the decimal's own scale, so the rounded value is expressed there.
fn rounded(name: &str, value: &Value, digits: i64, returns: &LogicalType) -> Result<Value> {
    let rule = Rule::of(name);
    match *value {
        Value::Double(x) => Ok(Value::Double(rounded_double(rule, x, digits))),
        Value::Float(x) => Ok(Value::Float(rounded_double(rule, f64::from(x), digits) as f32)),
        Value::Decimal { unscaled, scale, .. } => {
            let kept = returns.decimal_shape().map_or(scale, |(_, kept)| kept);
            let places = if digits < 0 {
                i64::from(scale) - digits
            } else {
                i64::from(scale) - digits.min(i64::from(scale))
            };
            let Some(step) = u8::try_from(places).ok().filter(|places| *places <= 38) else {
                return decimal(0, returns);
            };
            let step = pow10(step);
            let at_scale = rule.quotient(unscaled, step) * step;
            decimal(at_scale / pow10(scale - kept.min(scale)), returns)
        }
        _ => {
            let Some(whole) = integral(value) else {
                return Err(Error::internal(format!("{name} of a {}", value.logical_type())));
            };
            if digits >= 0 {
                return Ok(value.clone());
            }
            let Some(step) = u8::try_from(-digits).ok().filter(|places| *places <= 38) else {
                return fit(0, returns).ok_or_else(|| Error::internal("a zero that does not fit"));
            };
            let step = pow10(step);
            fit(rule.quotient(whole, step) * step, returns).ok_or_else(|| {
                let spelled = if name == "round_even" { "ROUND_EVEN" } else { "ROUND" };
                Error::out_of_range(format!("Overflow in {spelled} of integer"))
            })
        }
    }
}

fn rounded_double(rule: Rule, x: f64, digits: i64) -> f64 {
    if digits == 0 {
        return rule.double(x);
    }
    let answer = if digits < 0 {
        let modifier = 10f64.powi(i32::try_from(-digits).unwrap_or(i32::MAX));
        rule.double(x / modifier) * modifier
    } else {
        let modifier = 10f64.powi(i32::try_from(digits).unwrap_or(i32::MAX));
        rule.double(x * modifier) / modifier
    };
    // What the pin does with a count so large the arithmetic overflows: the value comes back as it
    // went in, which is the right answer for every count that big anyway.
    if answer.is_finite() { answer } else { x }
}

/// `gcd` and `lcm`, which are declared over `BIGINT` and `HUGEINT` and land on one of the two.
fn whole(left: &Value, right: &Value, returns: &LogicalType, least: bool) -> Result<Value> {
    let (Some(a), Some(b)) = (integral(left), integral(right)) else {
        return Err(Error::internal(format!("gcd of {left} and {right}")));
    };
    let (Some(a), Some(b)) = (a.checked_abs(), b.checked_abs()) else {
        return Err(Error::out_of_range("Overflow on abs"));
    };
    let answer = if least { lcm_of(a, b)? } else { gcd_of(a, b) };
    // A `gcd` that does not fit is the smallest `BIGINT` against a zero, which the pin reaches by
    // taking the absolute value of the answer in the argument's own type.
    fit(answer, returns).ok_or_else(|| {
        if least {
            Error::out_of_range("lcm value is out of range")
        } else {
            Error::out_of_range(format!("Overflow on abs({})", -answer))
        }
    })
}

fn gcd_of(mut a: i128, mut b: i128) -> i128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn lcm_of(a: i128, b: i128) -> Result<i128> {
    if a == 0 || b == 0 {
        return Ok(0);
    }
    (a / gcd_of(a, b))
        .checked_mul(b)
        .ok_or_else(|| Error::out_of_range("lcm value is out of range"))
}

/// `factorial`, which is exact and as wide as a `HUGEINT` goes.
fn factorial(value: &Value) -> Result<Value> {
    let Some(count) = integral(value) else {
        return Err(Error::internal(format!("factorial of a {}", value.logical_type())));
    };
    if count < 0 {
        return Err(Error::out_of_range("factorial of a negative number is undefined"));
    }
    let mut product: i128 = 1;
    for step in 2..=count {
        product =
            product.checked_mul(step).ok_or_else(|| Error::out_of_range("Value out of range"))?;
    }
    Ok(Value::HugeInt(product))
}

/// `binom`, the number of ways to pick `k` things out of `n`, exact and as wide as a `HUGEINT`.
///
/// This is the pin's loop step for step, cancelling each factor against the running product before
/// multiplying it in, because where that loop overflows is where the pin says the value is out of
/// range: `binom(130, 65)` fits and `binom(131, 65)` does not.
fn binom(n: &Value, k: &Value) -> Result<Value> {
    let (Some(n), Some(k)) = (integral(n), integral(k)) else {
        return Err(Error::internal(format!("binom of {n} and {k}")));
    };
    if n < 0 || k < 0 {
        return Err(Error::out_of_range("binom with negative input is undefined"));
    }
    if n < k {
        return Ok(Value::HugeInt(0));
    }
    let k = k.min(n - k);
    let mut answer: i128 = 1;
    for step in 1..=k {
        let mut numerator = n - k + step;
        let mut denominator = step;
        let common = gcd_of(numerator, denominator);
        numerator /= common;
        denominator /= common;
        let common = gcd_of(answer, denominator);
        answer /= common;
        answer = answer
            .checked_mul(numerator)
            .ok_or_else(|| Error::out_of_range("Value out of range"))?;
    }
    Ok(Value::HugeInt(answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &[Value], returns: &LogicalType) -> Value {
        value(name, args, returns).expect("a math function").expect("answers")
    }

    fn dec(unscaled: i128, width: u8, scale: u8) -> Value {
        Value::Decimal { unscaled, width, scale }
    }

    #[test]
    fn binom_is_exact_until_the_pins_loop_overflows() {
        let binom =
            |n, k| value("binom", &[Value::Integer(n), Value::Integer(k)], &LogicalType::HugeInt);
        assert_eq!(binom(5, 2).unwrap().unwrap(), Value::HugeInt(10));
        assert_eq!(binom(6, 8).unwrap().unwrap(), Value::HugeInt(0));
        assert_eq!(binom(0, 0).unwrap().unwrap(), Value::HugeInt(1));
        assert_eq!(
            binom(130, 65).unwrap().unwrap(),
            Value::HugeInt(95_067_625_827_960_698_145_584_333_020_095_113_100)
        );
        assert!(binom(131, 65).unwrap().is_err());
        assert!(binom(-6, 3).unwrap().is_err());
    }

    #[test]
    fn a_decimal_rounds_to_the_scale_the_binder_picked() {
        let two = LogicalType::Decimal { width: 5, scale: 2 };
        assert_eq!(call("round", &[dec(12345, 5, 4), Value::Integer(2)], &two), dec(123, 5, 2));
        let none = LogicalType::Decimal { width: 5, scale: 0 };
        assert_eq!(call("round", &[dec(12345, 5, 3), Value::Integer(-1)], &none), dec(10, 5, 0));
        assert_eq!(call("round", &[dec(-5, 2, 1)], &none), dec(-1, 5, 0));
        assert_eq!(call("round_even", &[dec(125, 4, 3), Value::Integer(2)], &two), dec(12, 5, 2));
        assert_eq!(call("trunc", &[dec(-1789, 4, 3), Value::Integer(1)], &two), dec(-170, 5, 2));
    }

    #[test]
    fn floor_and_ceil_of_a_decimal_go_the_right_way_on_both_sides_of_zero() {
        let none = LogicalType::Decimal { width: 3, scale: 0 };
        assert_eq!(call("floor", &[dec(-5, 2, 1)], &none), dec(-1, 3, 0));
        assert_eq!(call("ceil", &[dec(-5, 2, 1)], &none), dec(0, 3, 0));
        assert_eq!(call("ceil", &[dec(999, 3, 2)], &none), dec(10, 3, 0));
    }

    #[test]
    fn a_whole_number_rounds_before_the_point_and_keeps_its_type() {
        let int = LogicalType::Integer;
        assert_eq!(
            call("round", &[Value::Integer(15), Value::Integer(-1)], &int),
            Value::Integer(20)
        );
        assert_eq!(
            call("trunc", &[Value::Integer(1789), Value::Integer(-2)], &int),
            Value::Integer(1700)
        );
        assert_eq!(call("round", &[Value::Integer(5), Value::Integer(1)], &int), Value::Integer(5));
    }

    #[test]
    fn a_double_rounds_half_away_from_zero_the_way_the_pin_does() {
        let double = LogicalType::Double;
        assert_eq!(call("round", &[Value::Double(-2.5)], &double), Value::Double(-3.0));
        assert_eq!(
            call("round", &[Value::Double(1.005), Value::Integer(2)], &double),
            Value::Double(1.0)
        );
        assert_eq!(
            call("round", &[Value::Double(1234.5678), Value::Integer(-2)], &double),
            Value::Double(1200.0)
        );
        assert_eq!(
            call("round_even", &[Value::Double(2.5), Value::Integer(0)], &double),
            Value::Double(2.0)
        );
        assert_eq!(call("even", &[Value::Double(-1.5)], &double), Value::Double(-2.0));
    }

    #[test]
    fn gcd_and_lcm_answer_positive_and_say_when_they_do_not_fit() {
        let big = LogicalType::BigInt;
        assert_eq!(call("lcm", &[Value::BigInt(-4), Value::BigInt(6)], &big), Value::BigInt(12));
        assert_eq!(call("gcd", &[Value::BigInt(-4), Value::BigInt(-6)], &big), Value::BigInt(2));
        let error = value("gcd", &[Value::BigInt(i64::MIN), Value::BigInt(0)], &big)
            .expect("a math function")
            .expect_err("overflows");
        assert_eq!(error.message(), "Overflow on abs(-9223372036854775808)");
        let error = value("lcm", &[Value::BigInt(i64::MAX), Value::BigInt(i64::MAX - 1)], &big)
            .expect("a math function")
            .expect_err("overflows");
        assert_eq!(error.message(), "lcm value is out of range");
    }

    #[test]
    fn factorial_is_exact_until_a_hugeint_runs_out() {
        let huge = LogicalType::HugeInt;
        assert_eq!(
            call("factorial", &[Value::Integer(21)], &huge),
            Value::HugeInt(51_090_942_171_709_440_000)
        );
        let error = value("factorial", &[Value::Integer(34)], &huge)
            .expect("a math function")
            .expect_err("overflows");
        assert_eq!(error.message(), "Value out of range");
    }

    #[test]
    fn next_after_steps_one_unit_in_the_last_place() {
        assert_eq!(next_after(1.0, 2.0), 1.000_000_000_000_000_2);
        assert_eq!(next_after(1.0, 0.0), 1.0 - f64::EPSILON / 2.0);
        assert_eq!(next_after(0.0, -1.0), -f64::from_bits(1));
    }
}

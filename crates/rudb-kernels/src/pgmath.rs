//! The math functions of `pg_proc`, by the name of the C function in `prosrc`, as
//! `src/backend/utils/adt/float.c`, `int.c`, `int8.c`, `numeric.c` and `varlena.c` write them.
//!
//! Every function here is strict, so the caller gives a null for a null argument and the
//! functions see only values.

use rudb_common::{Error, Result, SqlState, Value};
use rudb_pgtypes::Numeric;

/// The C functions of this module, sorted.
pub(crate) const SOURCES: &[&str] = &[
    "dacos",
    "dacosd",
    "dacosh",
    "dasin",
    "dasind",
    "dasinh",
    "datan",
    "datan2",
    "datan2d",
    "datand",
    "datanh",
    "dcbrt",
    "dceil",
    "dcos",
    "dcosd",
    "dcosh",
    "dcot",
    "dcotd",
    "degrees",
    "derf",
    "derfc",
    "dexp",
    "dfloor",
    "dgamma",
    "dlgamma",
    "dlog1",
    "dlog10",
    "dpi",
    "dpow",
    "dround",
    "dsign",
    "dsin",
    "dsind",
    "dsinh",
    "dsqrt",
    "dtan",
    "dtand",
    "dtanh",
    "dtrunc",
    "float4abs",
    "float8abs",
    "int2abs",
    "int2mod",
    "int2shl",
    "int2shr",
    "int4abs",
    "int4gcd",
    "int4lcm",
    "int4mod",
    "int4shl",
    "int4shr",
    "int8abs",
    "int8gcd",
    "int8lcm",
    "int8mod",
    "int8shl",
    "int8shr",
    "numeric_abs",
    "numeric_ceil",
    "numeric_div_trunc",
    "numeric_exp",
    "numeric_fac",
    "numeric_floor",
    "numeric_gcd",
    "numeric_lcm",
    "numeric_ln",
    "numeric_log",
    "numeric_min_scale",
    "numeric_mod",
    "numeric_power",
    "numeric_round",
    "numeric_scale",
    "numeric_sign",
    "numeric_sqrt",
    "numeric_trim_scale",
    "numeric_trunc",
    "radians",
    "to_bin32",
    "to_bin64",
    "to_hex32",
    "to_hex64",
    "to_oct32",
    "to_oct64",
    "width_bucket_float8",
    "width_bucket_numeric",
];

/// The C functions of this module whose operators the engine answers in another way, so that the
/// operator is the kernel too. The shifts of `int.c` and `int8.c` shift as C does: the count is
/// taken modulo the width of the type that C shifts, which is `int` for an `int2`, and the bits
/// that go past the top are lost.
pub(crate) const OPERATORS: &[&str] =
    &["int2shl", "int2shr", "int4shl", "int4shr", "int8shl", "int8shr"];

/// The value of the C function `src` for the arguments, or `None` when the arguments are not of
/// the types of the function.
pub(crate) fn call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    if let Some(body) = float_unary(src) {
        return match args {
            [Value::Double(x)] => body(*x).map(|x| Some(Value::Double(x))),
            _ => Ok(None),
        };
    }
    let value = match (src, args) {
        ("dpi", []) => Value::Double(std::f64::consts::PI),
        ("float4abs", [Value::Float(x)]) => Value::Float(x.abs()),
        ("datan2", [Value::Double(y), Value::Double(x)]) => Value::Double(datan2(*y, *x)?),
        ("datan2d", [Value::Double(y), Value::Double(x)]) => Value::Double(datan2d(*y, *x)?),
        ("dpow", [Value::Double(x), Value::Double(y)]) => Value::Double(dpow(*x, *y)?),
        (
            "width_bucket_float8",
            [
                Value::Double(operand),
                Value::Double(low),
                Value::Double(high),
                Value::Integer(count),
            ],
        ) => Value::Integer(width_bucket_float8(*operand, *low, *high, *count)?),
        ("int2abs", [Value::SmallInt(x)]) => {
            Value::SmallInt(x.checked_abs().ok_or_else(|| out_of_range("smallint"))?)
        }
        ("int4abs", [Value::Integer(x)]) => {
            Value::Integer(x.checked_abs().ok_or_else(|| out_of_range("integer"))?)
        }
        ("int8abs", [Value::BigInt(x)]) => {
            Value::BigInt(x.checked_abs().ok_or_else(|| out_of_range("bigint"))?)
        }
        ("int2mod", [Value::SmallInt(x), Value::SmallInt(y)]) => {
            Value::SmallInt(int_mod(i64::from(*x), i64::from(*y))? as i16)
        }
        ("int4mod", [Value::Integer(x), Value::Integer(y)]) => {
            Value::Integer(int_mod(i64::from(*x), i64::from(*y))? as i32)
        }
        ("int8mod", [Value::BigInt(x), Value::BigInt(y)]) => Value::BigInt(int_mod(*x, *y)?),
        // `int2shl` and `int2shr` shift the value as an `int` and keep the low 16 bits.
        ("int2shl", [Value::SmallInt(x), Value::Integer(y)]) => {
            Value::SmallInt(i32::from(*x).wrapping_shl(y.cast_unsigned()) as i16)
        }
        ("int2shr", [Value::SmallInt(x), Value::Integer(y)]) => {
            Value::SmallInt(i32::from(*x).wrapping_shr(y.cast_unsigned()) as i16)
        }
        ("int4shl", [Value::Integer(x), Value::Integer(y)]) => {
            Value::Integer(x.wrapping_shl(y.cast_unsigned()))
        }
        ("int4shr", [Value::Integer(x), Value::Integer(y)]) => {
            Value::Integer(x.wrapping_shr(y.cast_unsigned()))
        }
        ("int8shl", [Value::BigInt(x), Value::Integer(y)]) => {
            Value::BigInt(x.wrapping_shl(y.cast_unsigned()))
        }
        ("int8shr", [Value::BigInt(x), Value::Integer(y)]) => {
            Value::BigInt(x.wrapping_shr(y.cast_unsigned()))
        }
        ("int4gcd", [Value::Integer(x), Value::Integer(y)]) => {
            let gcd = int_gcd(i64::from(*x), i64::from(*y));
            Value::Integer(i32::try_from(gcd).map_err(|_| out_of_range("integer"))?)
        }
        ("int8gcd", [Value::BigInt(x), Value::BigInt(y)]) => {
            Value::BigInt(i64::try_from(int_gcd(*x, *y)).map_err(|_| out_of_range("bigint"))?)
        }
        ("int4lcm", [Value::Integer(x), Value::Integer(y)]) => {
            let lcm = int_lcm(i64::from(*x), i64::from(*y));
            Value::Integer(
                lcm.and_then(|lcm| i32::try_from(lcm).ok())
                    .ok_or_else(|| out_of_range("integer"))?,
            )
        }
        ("int8lcm", [Value::BigInt(x), Value::BigInt(y)]) => {
            let lcm = int_lcm(*x, *y);
            Value::BigInt(
                lcm.and_then(|lcm| i64::try_from(lcm).ok())
                    .ok_or_else(|| out_of_range("bigint"))?,
            )
        }
        ("to_bin32", [Value::Integer(x)]) => Value::Varchar(format!("{:b}", *x as u32)),
        ("to_bin64", [Value::BigInt(x)]) => Value::Varchar(format!("{:b}", *x as u64)),
        ("to_oct32", [Value::Integer(x)]) => Value::Varchar(format!("{:o}", *x as u32)),
        ("to_oct64", [Value::BigInt(x)]) => Value::Varchar(format!("{:o}", *x as u64)),
        ("to_hex32", [Value::Integer(x)]) => Value::Varchar(format!("{:x}", *x as u32)),
        ("to_hex64", [Value::BigInt(x)]) => Value::Varchar(format!("{:x}", *x as u64)),
        ("numeric_fac", [Value::BigInt(n)]) => numeric(Numeric::factorial(*n))?,
        _ => return numeric_call(src, args),
    };
    Ok(Some(value))
}

/// The functions of `numeric`.
fn numeric_call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    let read = |value: &Value| match value {
        Value::Numeric(bytes) => Some(Numeric::from_bytes(bytes)),
        _ => None,
    };
    let value = match (src, args) {
        (_, [x]) => {
            let Some(x) = read(x) else { return Ok(None) };
            match src {
                "numeric_abs" => numeric(Ok(x.abs()))?,
                "numeric_ceil" => numeric(x.whole(true))?,
                "numeric_floor" => numeric(x.whole(false))?,
                "numeric_sign" => numeric(Ok(x.signum()))?,
                "numeric_sqrt" => numeric(x.sqrt())?,
                "numeric_exp" => numeric(x.exp())?,
                "numeric_ln" => numeric(x.ln())?,
                "numeric_trim_scale" => numeric(Ok(x.trim_scale()))?,
                "numeric_scale" => x.scale().map_or(Value::Null, Value::Integer),
                "numeric_min_scale" => x.min_scale().map_or(Value::Null, Value::Integer),
                _ => return Ok(None),
            }
        }
        ("numeric_round" | "numeric_trunc", [x, Value::Integer(scale)]) => {
            let Some(x) = read(x) else { return Ok(None) };
            numeric(if src == "numeric_round" { x.round(*scale) } else { x.trunc(*scale) })?
        }
        (_, [x, y]) => {
            let (Some(x), Some(y)) = (read(x), read(y)) else { return Ok(None) };
            numeric(match src {
                "numeric_mod" => x.modulo(&y),
                "numeric_div_trunc" => x.div_trunc(&y),
                "numeric_gcd" => x.gcd(&y),
                "numeric_lcm" => x.lcm(&y),
                "numeric_log" => Numeric::log(&x, &y),
                "numeric_power" => x.power(&y),
                _ => return Ok(None),
            })?
        }
        ("width_bucket_numeric", [operand, low, high, Value::Integer(count)]) => {
            let (Some(operand), Some(low), Some(high)) = (read(operand), read(low), read(high))
            else {
                return Ok(None);
            };
            let bucket = operand.width_bucket(&low, &high, *count);
            Value::Integer(bucket.map_err(|error| Error::from(error).unplaced())?)
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// A `numeric` that a function of `rudb_pgtypes` gave, or its error.
fn numeric(answer: std::result::Result<Numeric, rudb_pgtypes::TypeError>) -> Result<Value> {
    answer
        .map(|held| Value::Numeric(held.to_bytes()))
        .map_err(|error| Error::from(error).unplaced())
}

/// The function of one `float8` that `src` names, or `None`.
pub(crate) fn float_unary(src: &str) -> Option<fn(f64) -> Result<f64>> {
    Some(match src {
        "float8abs" => |x: f64| Ok(x.abs()),
        "dceil" => |x: f64| Ok(x.ceil()),
        "dfloor" => |x: f64| Ok(x.floor()),
        // `rint` in the default mode of rounding, which is half to even.
        "dround" => |x: f64| Ok(x.round_ties_even()),
        "dtrunc" => |x: f64| Ok(x.trunc()),
        "dsign" => |x: f64| {
            Ok(if x > 0.0 {
                1.0
            } else if x < 0.0 {
                -1.0
            } else {
                0.0
            })
        },
        "dsqrt" => dsqrt,
        "dcbrt" => |x: f64| checked(x, x.cbrt(), 0.0),
        "dexp" => dexp,
        "dlog1" => |x: f64| logarithm(x, x.ln()),
        "dlog10" => |x: f64| logarithm(x, x.log10()),
        "dacos" => |x: f64| unit(x, f64::acos),
        "dasin" => |x: f64| unit(x, f64::asin),
        "datan" => |x: f64| no_infinity(x.atan()),
        "dcos" => |x: f64| periodic(x, f64::cos).and_then(no_infinity),
        "dsin" => |x: f64| periodic(x, f64::sin).and_then(no_infinity),
        "dtan" => |x: f64| periodic(x, f64::tan),
        "dcot" => |x: f64| periodic(x, |x| 1.0 / x.tan()),
        "dacosd" => dacosd,
        "dasind" => dasind,
        "datand" => |x: f64| match x.is_nan() {
            true => Ok(f64::NAN),
            false => no_infinity(x.atan() / degree().atan_1_0 * 45.0),
        },
        "dcosd" => dcosd,
        "dsind" => dsind,
        "dtand" => |x: f64| tangent(x, false),
        "dcotd" => |x: f64| tangent(x, true),
        "dsinh" => |x: f64| Ok(x.sinh()),
        "dcosh" => |x: f64| {
            let result = x.cosh();
            if result == 0.0 { Err(underflow()) } else { Ok(result) }
        },
        "dtanh" => |x: f64| no_infinity(x.tanh()),
        "dasinh" => |x: f64| Ok(x.asinh()),
        "dacosh" => |x: f64| if x < 1.0 { Err(input_out_of_range()) } else { Ok(x.acosh()) },
        "datanh" => datanh,
        "derf" => |x: f64| no_infinity(libm::erf(x)),
        "derfc" => |x: f64| no_infinity(libm::erfc(x)),
        "dgamma" => dgamma,
        "dlgamma" => |x: f64| {
            let result = libm::lgamma(x);
            if result.is_infinite() && !x.is_infinite() { Err(overflow()) } else { Ok(result) }
        },
        "degrees" => |x: f64| float8_div(x, RADIANS_PER_DEGREE),
        "radians" => |x: f64| float8_mul(x, RADIANS_PER_DEGREE),
        _ => return None,
    })
}

/// `RADIANS_PER_DEGREE` of `float.h`, which is this `f64` with more digits written.
const RADIANS_PER_DEGREE: f64 = 0.017_453_292_519_943_295;

/// `float_overflow_error`.
pub(crate) fn overflow() -> Error {
    Error::out_of_range("value out of range: overflow")
        .state(SqlState::NUMERIC_VALUE_OUT_OF_RANGE)
        .unplaced()
}

/// `float_underflow_error`.
fn underflow() -> Error {
    Error::out_of_range("value out of range: underflow")
        .state(SqlState::NUMERIC_VALUE_OUT_OF_RANGE)
        .unplaced()
}

/// The overflow of an integer type, `integer out of range` and the others.
fn out_of_range(type_name: &str) -> Error {
    Error::out_of_range(format!("{type_name} out of range"))
        .state(SqlState::NUMERIC_VALUE_OUT_OF_RANGE)
        .unplaced()
}

fn input_out_of_range() -> Error {
    Error::out_of_range("input is out of range")
        .state(SqlState::NUMERIC_VALUE_OUT_OF_RANGE)
        .unplaced()
}

fn division_by_zero() -> Error {
    Error::invalid_input("division by zero").state(SqlState::DIVISION_BY_ZERO).unplaced()
}

fn invalid(state: SqlState, message: &str) -> Error {
    Error::invalid_input(message).state(state).unplaced()
}

/// The result of a function that gives an infinity only for an infinity and a zero only for
/// `zero`, as most of the functions check it.
fn checked(x: f64, result: f64, zero: f64) -> Result<f64> {
    if result.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    if result == 0.0 && x != zero {
        return Err(underflow());
    }
    Ok(result)
}

fn no_infinity(result: f64) -> Result<f64> {
    if result.is_infinite() { Err(overflow()) } else { Ok(result) }
}

fn dsqrt(x: f64) -> Result<f64> {
    if x < 0.0 {
        let message = "cannot take square root of a negative number";
        return Err(invalid(SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION, message));
    }
    checked(x, x.sqrt(), 0.0)
}

fn dexp(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(x);
    }
    if x.is_infinite() {
        return Ok(if x > 0.0 { x } else { 0.0 });
    }
    let result = x.exp();
    if result.is_infinite() {
        return Err(overflow());
    }
    if result == 0.0 {
        return Err(underflow());
    }
    Ok(result)
}

/// `dlog1` and `dlog10`.
fn logarithm(x: f64, result: f64) -> Result<f64> {
    if x == 0.0 {
        return Err(invalid(SqlState::INVALID_ARGUMENT_FOR_LOG, "cannot take logarithm of zero"));
    }
    if x < 0.0 {
        let message = "cannot take logarithm of a negative number";
        return Err(invalid(SqlState::INVALID_ARGUMENT_FOR_LOG, message));
    }
    checked(x, result, 1.0)
}

/// `dacos` and `dasin`, which take a value from -1 to 1.
fn unit(x: f64, body: fn(f64) -> f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(input_out_of_range());
    }
    no_infinity(body(x))
}

/// `dcos`, `dsin`, `dtan` and `dcot`, which take no infinity.
fn periodic(x: f64, body: fn(f64) -> f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(input_out_of_range());
    }
    Ok(body(x))
}

fn datan2(y: f64, x: f64) -> Result<f64> {
    if y.is_nan() || x.is_nan() {
        return Ok(f64::NAN);
    }
    no_infinity(y.atan2(x))
}

fn datanh(x: f64) -> Result<f64> {
    if !(-1.0..=1.0).contains(&x) {
        return Err(input_out_of_range());
    }
    Ok(if x == -1.0 {
        f64::NEG_INFINITY
    } else if x == 1.0 {
        f64::INFINITY
    } else {
        x.atanh()
    })
}

fn dgamma(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(x);
    }
    if x.is_infinite() {
        return if x < 0.0 { Err(overflow()) } else { Ok(x) };
    }
    let result = libm::tgamma(x);
    if result.is_infinite() || result.is_nan() {
        return Err(overflow());
    }
    if result == 0.0 {
        return Err(underflow());
    }
    Ok(result)
}

fn dpow(x: f64, y: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(if y.is_nan() || y != 0.0 { f64::NAN } else { 1.0 });
    }
    if y.is_nan() {
        return Ok(if x != 1.0 { f64::NAN } else { 1.0 });
    }
    if x == 0.0 && y < 0.0 {
        let message = "zero raised to a negative power is undefined";
        return Err(invalid(SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION, message));
    }
    if x < 0.0 && y.floor() != y {
        let message = "a negative number raised to a non-integer power yields a complex result";
        return Err(invalid(SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION, message));
    }
    if y.is_infinite() {
        let abs = x.abs();
        return Ok(if abs == 1.0 {
            1.0
        } else if (y > 0.0) == (abs > 1.0) {
            f64::INFINITY
        } else {
            0.0
        });
    }
    if x.is_infinite() {
        if y == 0.0 {
            return Ok(1.0);
        }
        if x > 0.0 {
            return Ok(if y > 0.0 { x } else { 0.0 });
        }
        let half = y / 2.0;
        let odd = half.floor() != half;
        return Ok(match (y > 0.0, odd) {
            (true, true) => x,
            (true, false) => -x,
            (false, true) => -0.0,
            (false, false) => 0.0,
        });
    }
    let result = x.powf(y);
    if result.is_infinite() {
        return Err(overflow());
    }
    if result == 0.0 && x != 0.0 {
        return Err(underflow());
    }
    Ok(result)
}

/// `float8_mul` of `float.h`.
fn float8_mul(x: f64, y: f64) -> Result<f64> {
    let result = x * y;
    if result.is_infinite() && !x.is_infinite() && !y.is_infinite() {
        return Err(overflow());
    }
    if result == 0.0 && x != 0.0 && y != 0.0 {
        return Err(underflow());
    }
    Ok(result)
}

/// `float8_div` of `float.h`.
fn float8_div(x: f64, y: f64) -> Result<f64> {
    if y == 0.0 && !x.is_nan() {
        return Err(division_by_zero());
    }
    let result = x / y;
    if result.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    if result == 0.0 && x != 0.0 && !y.is_infinite() {
        return Err(underflow());
    }
    Ok(result)
}

/// The constants of `init_degree_constants`, which make the functions of degrees exact at the
/// usual angles.
struct Degree {
    sin_30: f64,
    one_minus_cos_60: f64,
    asin_0_5: f64,
    acos_0_5: f64,
    atan_1_0: f64,
    tan_45: f64,
    cot_45: f64,
}

fn degree() -> &'static Degree {
    static DEGREE: std::sync::OnceLock<Degree> = std::sync::OnceLock::new();
    DEGREE.get_or_init(|| {
        let mut held = Degree {
            sin_30: (30.0 * RADIANS_PER_DEGREE).sin(),
            one_minus_cos_60: 1.0 - (60.0 * RADIANS_PER_DEGREE).cos(),
            asin_0_5: 0.5f64.asin(),
            acos_0_5: 0.5f64.acos(),
            atan_1_0: 1.0f64.atan(),
            tan_45: 0.0,
            cot_45: 0.0,
        };
        held.tan_45 = sind_q1(&held, 45.0) / cosd_q1(&held, 45.0);
        held.cot_45 = cosd_q1(&held, 45.0) / sind_q1(&held, 45.0);
        held
    })
}

fn sind_0_to_30(degree: &Degree, x: f64) -> f64 {
    ((x * RADIANS_PER_DEGREE).sin() / degree.sin_30) / 2.0
}

fn cosd_0_to_60(degree: &Degree, x: f64) -> f64 {
    let one_minus_cos = 1.0 - (x * RADIANS_PER_DEGREE).cos();
    1.0 - (one_minus_cos / degree.one_minus_cos_60) / 2.0
}

fn sind_q1(degree: &Degree, x: f64) -> f64 {
    if x <= 30.0 { sind_0_to_30(degree, x) } else { cosd_0_to_60(degree, 90.0 - x) }
}

fn cosd_q1(degree: &Degree, x: f64) -> f64 {
    if x <= 60.0 { cosd_0_to_60(degree, x) } else { sind_0_to_30(degree, 90.0 - x) }
}

fn asind_q1(degree: &Degree, x: f64) -> f64 {
    if x <= 0.5 {
        (x.asin() / degree.asin_0_5) * 30.0
    } else {
        90.0 - (x.acos() / degree.acos_0_5) * 60.0
    }
}

fn acosd_q1(degree: &Degree, x: f64) -> f64 {
    if x <= 0.5 {
        90.0 - (x.asin() / degree.asin_0_5) * 30.0
    } else {
        (x.acos() / degree.acos_0_5) * 60.0
    }
}

fn dacosd(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(input_out_of_range());
    }
    let degree = degree();
    no_infinity(if x >= 0.0 { acosd_q1(degree, x) } else { 90.0 + asind_q1(degree, -x) })
}

fn dasind(x: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(input_out_of_range());
    }
    let degree = degree();
    no_infinity(if x >= 0.0 { asind_q1(degree, x) } else { -asind_q1(degree, -x) })
}

fn datan2d(y: f64, x: f64) -> Result<f64> {
    if y.is_nan() || x.is_nan() {
        return Ok(f64::NAN);
    }
    no_infinity(y.atan2(x) / degree().atan_1_0 * 45.0)
}

/// The angle in degrees brought to the range from 0 to 90, with the signs that the cosine and
/// the sine take for that. `None` for `NaN`.
fn quadrant(x: f64) -> Result<Option<(f64, f64, f64)>> {
    if x.is_nan() {
        return Ok(None);
    }
    if x.is_infinite() {
        return Err(input_out_of_range());
    }
    let (mut x, mut sin, mut cos) = (x % 360.0, 1.0, 1.0);
    if x < 0.0 {
        x = -x;
        sin = -sin;
    }
    if x > 180.0 {
        x = 360.0 - x;
        sin = -sin;
    }
    if x > 90.0 {
        x = 180.0 - x;
        cos = -cos;
    }
    Ok(Some((x, sin, cos)))
}

fn dsind(x: f64) -> Result<f64> {
    let Some((x, sin, _)) = quadrant(x)? else { return Ok(f64::NAN) };
    no_infinity(sin * sind_q1(degree(), x))
}

fn dcosd(x: f64) -> Result<f64> {
    let Some((x, _, cos)) = quadrant(x)? else { return Ok(f64::NAN) };
    no_infinity(cos * cosd_q1(degree(), x))
}

/// `dtand`, or `dcotd` when `cot` is set. A zero is never negative.
fn tangent(x: f64, cot: bool) -> Result<f64> {
    let Some((x, sin, cos)) = quadrant(x)? else { return Ok(f64::NAN) };
    let degree = degree();
    let result = match cot {
        true => sin * cos * (cosd_q1(degree, x) / sind_q1(degree, x)) / degree.cot_45,
        false => sin * cos * (sind_q1(degree, x) / cosd_q1(degree, x)) / degree.tan_45,
    };
    Ok(if result == 0.0 { 0.0 } else { result })
}

fn width_bucket_float8(operand: f64, low: f64, high: f64, count: i32) -> Result<i32> {
    let state = SqlState::INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION;
    if count <= 0 {
        return Err(invalid(state, "count must be greater than zero"));
    }
    if low.is_nan() || high.is_nan() {
        return Err(invalid(state, "lower and upper bounds cannot be NaN"));
    }
    if low.is_infinite() || high.is_infinite() {
        return Err(invalid(state, "lower and upper bounds must be finite"));
    }
    let above = || count.checked_add(1).ok_or_else(|| out_of_range("integer"));
    // The bucket of an operand inside the bounds, which is `distance` over `width` of the way
    // from the bound of bucket 1. The width of two finite bounds can overflow, and their halves
    // cannot. The fraction can round to 1, which is not a bucket.
    let inside = |distance: f64, width: f64, halves: f64| {
        let fraction = if width.is_infinite() { halves } else { distance / width };
        let bucket = (f64::from(count) * fraction) as i32;
        bucket.min(count - 1) + 1
    };
    if low < high {
        if operand.is_nan() || operand >= high {
            return above();
        }
        if operand < low {
            return Ok(0);
        }
        let halves = (operand / 2.0 - low / 2.0) / (high / 2.0 - low / 2.0);
        return Ok(inside(operand - low, high - low, halves));
    }
    if low > high {
        if operand.is_nan() || operand > low {
            return Ok(0);
        }
        if operand <= high {
            return above();
        }
        let halves = (low / 2.0 - operand / 2.0) / (low / 2.0 - high / 2.0);
        return Ok(inside(low - operand, low - high, halves));
    }
    Err(invalid(state, "lower bound cannot equal upper bound"))
}

/// `int2mod`, `int4mod` and `int8mod`: the sign of the result is the sign of `x`.
fn int_mod(x: i64, y: i64) -> Result<i64> {
    match y {
        0 => Err(division_by_zero()),
        -1 => Ok(0),
        y => Ok(x % y),
    }
}

/// `int4gcd` and `int8gcd` with no overflow: the caller checks that the result fits.
fn int_gcd(x: i64, y: i64) -> u64 {
    let (mut a, mut b) = (x.unsigned_abs(), y.unsigned_abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// `int4lcm` and `int8lcm`, or `None` when the result does not fit `i64`.
fn int_lcm(x: i64, y: i64) -> Option<u64> {
    if x == 0 || y == 0 {
        return Some(0);
    }
    (x.unsigned_abs() / int_gcd(x, y)).checked_mul(y.unsigned_abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(src: &str, args: &[Value]) -> String {
        match call(src, args) {
            Ok(Some(Value::Double(x))) => x.to_string(),
            Ok(Some(Value::SmallInt(x))) => x.to_string(),
            Ok(Some(Value::Integer(x))) => x.to_string(),
            Ok(Some(Value::BigInt(x))) => x.to_string(),
            Ok(Some(Value::Varchar(x))) => x,
            Ok(other) => format!("{other:?}"),
            Err(error) => format!("{} {}", error.reported_state(), error.message()),
        }
    }

    #[test]
    fn the_functions_give_the_values_and_the_errors_of_postgres() {
        let x = |x: f64| [Value::Double(x)];
        // The values and the errors are the ones that PostgreSQL 19 gives.
        for (src, args, expected) in [
            ("dsind", x(30.0).to_vec(), "0.5"),
            ("dcosd", x(60.0).to_vec(), "0.5"),
            ("dtand", x(45.0).to_vec(), "1"),
            ("dcotd", x(45.0).to_vec(), "1"),
            ("dasind", x(0.5).to_vec(), "30"),
            ("dacosd", x(0.5).to_vec(), "60"),
            ("datand", x(1.0).to_vec(), "45"),
            ("dsind", x(-90.0).to_vec(), "-1"),
            ("dtand", x(90.0).to_vec(), "inf"),
            ("dround", x(2.5).to_vec(), "2"),
            ("dround", x(3.5).to_vec(), "4"),
            ("dgamma", x(5.0).to_vec(), "24"),
            ("dsqrt", x(-1.0).to_vec(), "2201F cannot take square root of a negative number"),
            ("dexp", x(800.0).to_vec(), "22003 value out of range: overflow"),
            ("dexp", x(-1000.0).to_vec(), "22003 value out of range: underflow"),
            ("dlog1", x(0.0).to_vec(), "2201E cannot take logarithm of zero"),
            ("dlog10", x(-1.0).to_vec(), "2201E cannot take logarithm of a negative number"),
            ("dacos", x(2.0).to_vec(), "22003 input is out of range"),
            ("datan2d", vec![Value::Double(1.0), Value::Double(1.0)], "45"),
            (
                "dpow",
                vec![Value::Double(-8.0), Value::Double(0.5)],
                "2201F a negative number raised to a non-integer power yields a complex result",
            ),
            (
                "dpow",
                vec![Value::Double(0.0), Value::Double(-1.0)],
                "2201F zero raised to a negative power is undefined",
            ),
            (
                "width_bucket_float8",
                vec![
                    Value::Double(5.35),
                    Value::Double(0.024),
                    Value::Double(10.06),
                    Value::Integer(5),
                ],
                "3",
            ),
            (
                "width_bucket_float8",
                vec![
                    Value::Double(1e308),
                    Value::Double(-1e308),
                    Value::Double(1e308),
                    Value::Integer(10),
                ],
                "11",
            ),
            (
                "width_bucket_float8",
                vec![
                    Value::Double(0.0),
                    Value::Double(10.0),
                    Value::Double(0.0),
                    Value::Integer(4),
                ],
                "5",
            ),
            (
                "width_bucket_float8",
                vec![
                    Value::Double(5.0),
                    Value::Double(0.0),
                    Value::Double(10.0),
                    Value::Integer(0),
                ],
                "2201G count must be greater than zero",
            ),
            ("int4mod", vec![Value::Integer(i32::MIN), Value::Integer(-1)], "0"),
            ("int4mod", vec![Value::Integer(1), Value::Integer(0)], "22012 division by zero"),
            ("int4gcd", vec![Value::Integer(1071), Value::Integer(462)], "21"),
            (
                "int4gcd",
                vec![Value::Integer(i32::MIN), Value::Integer(0)],
                "22003 integer out of range",
            ),
            ("int4lcm", vec![Value::Integer(1071), Value::Integer(462)], "23562"),
            (
                "int4lcm",
                vec![Value::Integer(i32::MAX), Value::Integer(i32::MAX - 1)],
                "22003 integer out of range",
            ),
            ("int4abs", vec![Value::Integer(i32::MIN)], "22003 integer out of range"),
            ("int4shl", vec![Value::Integer(1), Value::Integer(31)], "-2147483648"),
            ("int4shl", vec![Value::Integer(1), Value::Integer(33)], "2"),
            ("int4shl", vec![Value::Integer(1), Value::Integer(-1)], "-2147483648"),
            ("int4shr", vec![Value::Integer(-8), Value::Integer(33)], "-4"),
            ("int2shl", vec![Value::SmallInt(1), Value::Integer(15)], "-32768"),
            ("int2shl", vec![Value::SmallInt(1), Value::Integer(16)], "0"),
            ("int2shl", vec![Value::SmallInt(1), Value::Integer(-1)], "0"),
            ("int2shr", vec![Value::SmallInt(-8), Value::Integer(2)], "-2"),
            ("int8shl", vec![Value::BigInt(1), Value::Integer(63)], "-9223372036854775808"),
            ("int8shl", vec![Value::BigInt(1), Value::Integer(64)], "1"),
            ("to_bin32", vec![Value::Integer(-1)], "11111111111111111111111111111111"),
            ("to_oct64", vec![Value::BigInt(-8)], "1777777777777777777770"),
            ("to_hex64", vec![Value::BigInt(-1)], "ffffffffffffffff"),
        ] {
            assert_eq!(shown(src, &args), expected, "{src}({args:?})");
        }
    }
}

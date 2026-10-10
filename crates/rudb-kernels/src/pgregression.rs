//! `corr`, the covariances and the `regr_*` family as PostgreSQL computes them.
//!
//! PostgreSQL keeps the count, the two sums, the sums of squared differences from the means, the
//! sum of the products of the differences, and the common value of each side when every row so
//! far had the same one. `float8_regr_accum` updates them in the steps of Youngs and Cramer, and it
//! leaves a sum of squares at an exact zero while its side stays the same, so that `corr` of a
//! constant column is null rather than a quotient of rounding errors. The pin keeps the states of
//! [`crate::statistics`] and answers NaN there. The binder gives a PostgreSQL session the names of
//! these states, which are the names of PostgreSQL's final functions.

use rudb_common::{Error, Result, Value};

use crate::pgmath::overflow;

/// Which answer a [`FloatPairs`] gives, one for each final function of PostgreSQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Final {
    Corr,
    CovarPop,
    CovarSamp,
    AvgX,
    AvgY,
    Intercept,
    R2,
    Slope,
    Sxx,
    Sxy,
    Syy,
}

impl Final {
    /// The answer `name` asks for, or `None` when it is not one of these.
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "float8_corr" => Self::Corr,
            "float8_covar_pop" => Self::CovarPop,
            "float8_covar_samp" => Self::CovarSamp,
            "float8_regr_avgx" => Self::AvgX,
            "float8_regr_avgy" => Self::AvgY,
            "float8_regr_intercept" => Self::Intercept,
            "float8_regr_r2" => Self::R2,
            "float8_regr_slope" => Self::Slope,
            "float8_regr_sxx" => Self::Sxx,
            "float8_regr_sxy" => Self::Sxy,
            "float8_regr_syy" => Self::Syy,
            _ => return None,
        })
    }
}

/// The state of `float8_regr_accum`. The count is a double, as it is in PostgreSQL's state, and a
/// common value is NaN once the rows on its side differ.
#[derive(Debug, Clone)]
pub(crate) struct FloatPairs {
    count: f64,
    sum_x: f64,
    squared_x: f64,
    sum_y: f64,
    squared_y: f64,
    product: f64,
    common_x: f64,
    common_y: f64,
    /// Whether finite values made a sum overflow, which is an error at the answer since
    /// [`crate::general::General::push_reals`] has no way to give one.
    overflowed: bool,
    answer: Final,
}

impl FloatPairs {
    /// A fresh state for `name`, or `None` when the name is not one of these.
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(Self {
            count: 0.0,
            sum_x: 0.0,
            squared_x: 0.0,
            sum_y: 0.0,
            squared_y: 0.0,
            product: 0.0,
            common_x: 0.0,
            common_y: 0.0,
            overflowed: false,
            answer: Final::named(name)?,
        })
    }

    /// Adds one row, with `y` first. A row with a null in it changes nothing.
    pub(crate) fn update(&mut self, args: &[Value]) -> Result<()> {
        match args {
            [Value::Double(y), Value::Double(x)] => self.add(*y, *x),
            [Value::Null, _] | [_, Value::Null] => {}
            _ => return Err(Error::internal(format!("{args:?} handed to a float pair statistic"))),
        }
        Ok(())
    }

    /// Adds the pair `(y, x)`, in the steps of `float8_regr_accum`.
    pub(crate) fn add(&mut self, y: f64, x: f64) {
        let (before, sum_x_before, sum_y_before) = (self.count, self.sum_x, self.sum_y);
        self.count += 1.0;
        self.sum_x += x;
        self.sum_y += y;
        if before == 0.0 {
            // A first value that is not finite makes its sums NaN, so that a single row does not
            // read as a variance of zero.
            if !x.is_finite() {
                (self.squared_x, self.product) = (f64::NAN, f64::NAN);
            }
            if !y.is_finite() {
                (self.squared_y, self.product) = (f64::NAN, f64::NAN);
            }
            (self.common_x, self.common_y) = (x, y);
            return;
        }
        // A NaN is never equal to itself, so a NaN row ends a common value too.
        if x != self.common_x {
            self.common_x = f64::NAN;
        }
        if y != self.common_y {
            self.common_y = f64::NAN;
        }
        let step_x = x * self.count - self.sum_x;
        let step_y = y * self.count - self.sum_y;
        let scale = 1.0 / (self.count * before);
        // A side that has stayed the same keeps its sums at an exact zero.
        let (varied_x, varied_y) = (self.common_x.is_nan(), self.common_y.is_nan());
        if varied_x {
            self.squared_x += step_x * step_x * scale;
        }
        if varied_y {
            self.squared_y += step_y * step_y * scale;
        }
        if varied_x && varied_y {
            self.product += step_x * step_y * scale;
        } else if !x.is_finite() || !y.is_finite() {
            self.product = f64::NAN;
        }
        let infinite = f64::is_infinite;
        if infinite(self.sum_x)
            || infinite(self.squared_x)
            || infinite(self.sum_y)
            || infinite(self.squared_y)
            || infinite(self.product)
        {
            let finite_x = !infinite(sum_x_before) && !infinite(x);
            let finite_y = !infinite(sum_y_before) && !infinite(y);
            self.overflowed |= ((infinite(self.sum_x) || infinite(self.squared_x)) && finite_x)
                || ((infinite(self.sum_y) || infinite(self.squared_y)) && finite_y)
                || (infinite(self.product) && finite_x && finite_y);
            for sum in [&mut self.squared_x, &mut self.squared_y, &mut self.product] {
                if sum.is_infinite() {
                    *sum = f64::NAN;
                }
            }
        }
    }

    /// Folds another state for the same call into this one, as `float8_regr_combine` does.
    pub(crate) fn combine(&mut self, other: &Self) -> Result<()> {
        self.overflowed |= other.overflowed;
        if other.count == 0.0 {
            return Ok(());
        }
        if self.count == 0.0 {
            let overflowed = self.overflowed;
            *self = Self { overflowed, ..other.clone() };
            return Ok(());
        }
        let (here, there) = (self.count, other.count);
        let count = here + there;
        let sum_x = self.sum_x + other.sum_x;
        let sum_y = self.sum_y + other.sum_y;
        if (sum_x.is_infinite() && !self.sum_x.is_infinite() && !other.sum_x.is_infinite())
            || (sum_y.is_infinite() && !self.sum_y.is_infinite() && !other.sum_y.is_infinite())
        {
            return Err(overflow());
        }
        let apart_x = self.sum_x / here - other.sum_x / there;
        let apart_y = self.sum_y / here - other.sum_y / there;
        let joined = |mine: f64, theirs: f64, step: f64| {
            let joined = mine + theirs + here * there * step / count;
            match joined.is_infinite() && !mine.is_infinite() && !theirs.is_infinite() {
                true => Err(overflow()),
                false => Ok(joined),
            }
        };
        self.squared_x = joined(self.squared_x, other.squared_x, apart_x * apart_x)?;
        self.squared_y = joined(self.squared_y, other.squared_y, apart_y * apart_y)?;
        self.product = joined(self.product, other.product, apart_x * apart_y)?;
        // `float8_eq` holds two NaNs equal, and a NaN here already means the values differ.
        if self.common_x != other.common_x {
            self.common_x = f64::NAN;
        }
        if self.common_y != other.common_y {
            self.common_y = f64::NAN;
        }
        (self.count, self.sum_x, self.sum_y) = (count, sum_x, sum_y);
        Ok(())
    }

    /// The answer, a `double precision` or null, as PostgreSQL's final function gives it.
    pub(crate) fn finish(&self) -> Result<Value> {
        if self.overflowed {
            return Err(overflow());
        }
        let (count, sxx, syy, sxy) = (self.count, self.squared_x, self.squared_y, self.product);
        if count < 1.0 {
            return Ok(Value::Null);
        }
        let answer = match self.answer {
            Final::Sxx => sxx,
            Final::Syy => syy,
            Final::Sxy => sxy,
            Final::AvgX if !self.common_x.is_nan() => self.common_x,
            Final::AvgX => self.sum_x / count,
            Final::AvgY if !self.common_y.is_nan() => self.common_y,
            Final::AvgY => self.sum_y / count,
            Final::CovarPop => sxy / count,
            Final::CovarSamp if count < 2.0 => return Ok(Value::Null),
            Final::CovarSamp => sxy / (count - 1.0),
            // A horizontal or a vertical line has no correlation.
            Final::Corr if sxx == 0.0 || syy == 0.0 => return Ok(Value::Null),
            Final::Corr => {
                let product = sxx * syy;
                let root = match product == 0.0 || product.is_infinite() {
                    true => sxx.sqrt() * syy.sqrt(),
                    false => product.sqrt(),
                };
                (sxy / root).clamp(-1.0, 1.0)
            }
            // A vertical line has no slope, and a horizontal one fits all of its points.
            Final::R2 | Final::Slope | Final::Intercept if sxx == 0.0 => return Ok(Value::Null),
            Final::R2 if syy == 0.0 => 1.0,
            Final::R2 => r2(sxx, syy, sxy),
            Final::Slope => sxy / sxx,
            Final::Intercept => (self.sum_y - rise(self.sum_x, sxx, sxy)) / count,
        };
        Ok(Value::Double(answer))
    }
}

/// `regr_r2` of sums that are not zero, through the roots when a product does not fit.
fn r2(sxx: f64, syy: f64, sxy: f64) -> f64 {
    let (numerator, denominator) = (sxy * sxy, sxx * syy);
    let unfit = |value: f64| value == 0.0 || value.is_infinite();
    let answer = match unfit(numerator) || unfit(denominator) {
        true => {
            let root = sxy / (sxx.sqrt() * syy.sqrt());
            root * root
        }
        false => numerator / denominator,
    };
    answer.min(1.0)
}

/// The `Sx * Sxy / Sxx` that `regr_intercept` takes from the sum of the `y` values, through the
/// mantissas and the exponents when the product does not fit, as `float8_regr_intercept` does.
fn rise(sx: f64, sxx: f64, sxy: f64) -> f64 {
    let dy = sx * sxy / sxx;
    if (dy != 0.0 && dy.is_finite()) || [sx, sxy, sxx].iter().any(|value| !value.is_finite()) {
        return dy;
    }
    let ((m_sx, n_sx), (m_sxy, n_sxy), (m_sxx, n_sxx)) = (frexp(sx), frexp(sxy), frexp(sxx));
    ldexp(m_sx * m_sxy / m_sxx, n_sx + n_sxy - n_sxx)
}

/// The mantissa in `[0.5, 1)` and the exponent of a finite double, as C's `frexp` gives them.
fn frexp(value: f64) -> (f64, i32) {
    if value == 0.0 {
        return (value, 0);
    }
    let bits = value.to_bits();
    let exponent = i32::try_from((bits >> 52) & 0x7ff).unwrap_or_default();
    if exponent == 0 {
        // A subnormal: scale it into the normal range first.
        let (mantissa, shift) = frexp(value * f64::from_bits(0x4350_0000_0000_0000));
        return (mantissa, shift - 54);
    }
    let mantissa = f64::from_bits((bits & !(0x7ff << 52)) | (1022 << 52));
    (mantissa, exponent - 1022)
}

/// `value * 2^exponent`, as C's `ldexp` gives it, in steps that do not overflow on the way.
fn ldexp(mut value: f64, mut exponent: i32) -> f64 {
    while exponent > 1000 {
        value *= 2f64.powi(1000);
        exponent -= 1000;
    }
    while exponent < -1000 {
        value *= 2f64.powi(-1000);
        exponent += 1000;
    }
    value * 2f64.powi(exponent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(name: &str, rows: &[(f64, f64)]) -> Value {
        let mut state = FloatPairs::named(name).unwrap();
        for &(y, x) in rows {
            state.add(y, x);
        }
        state.finish().unwrap()
    }

    #[test]
    fn a_constant_side_answers_as_postgres() {
        let rows: Vec<(f64, f64)> = (1..=10).map(|g| (0.09, f64::from(g))).collect();
        assert_eq!(answer("float8_corr", &rows), Value::Null);
        assert_eq!(answer("float8_regr_r2", &rows), Value::Double(1.0));
        assert_eq!(answer("float8_regr_avgy", &rows), Value::Double(0.09));
        let flipped: Vec<(f64, f64)> = rows.iter().map(|&(y, x)| (x, y)).collect();
        assert_eq!(answer("float8_regr_slope", &flipped), Value::Null);
        assert_eq!(answer("float8_regr_intercept", &flipped), Value::Null);
        let same: Vec<(f64, f64)> = (1..=10).map(|g| (f64::from(g), f64::from(g))).collect();
        assert_eq!(answer("float8_corr", &same), Value::Double(1.0));
        assert_eq!(answer("float8_regr_slope", &same), Value::Double(1.0));
        assert_eq!(answer("float8_covar_samp", &same[..1]), Value::Null);
        assert_eq!(answer("float8_covar_pop", &[]), Value::Null);
    }

    #[test]
    fn a_split_state_combines_as_one() {
        let rows: Vec<(f64, f64)> =
            (1..=9).map(|g| (f64::from(g * g), f64::from(g) / 3.0)).collect();
        let whole = answer("float8_regr_intercept", &rows);
        let (mut left, mut right) = (
            FloatPairs::named("float8_regr_intercept").unwrap(),
            FloatPairs::named("float8_regr_intercept").unwrap(),
        );
        rows[..4].iter().for_each(|&(y, x)| left.add(y, x));
        rows[4..].iter().for_each(|&(y, x)| right.add(y, x));
        left.combine(&right).unwrap();
        let (Value::Double(whole), Value::Double(split)) = (whole, left.finish().unwrap()) else {
            panic!("both are doubles")
        };
        assert!((whole - split).abs() < 1e-9, "{whole} and {split}");
    }

    #[test]
    fn finite_values_that_overflow_are_an_error() {
        let mut state = FloatPairs::named("float8_regr_sxx").unwrap();
        state.add(1.0, 1e308);
        state.add(1.0, 1e308);
        assert!(state.finish().is_err());
        let mut infinite = FloatPairs::named("float8_corr").unwrap();
        infinite.add(1.0, f64::INFINITY);
        infinite.add(2.0, 3.0);
        let Value::Double(nan) = infinite.finish().unwrap() else { panic!("a double") };
        assert!(nan.is_nan());
    }

    #[test]
    fn frexp_and_ldexp_agree_with_c() {
        assert_eq!(frexp(8.0), (0.5, 4));
        assert_eq!(frexp(-0.75), (-0.75, 0));
        assert_eq!(frexp(f64::MIN_POSITIVE / 4.0), (0.5, -1023));
        assert_eq!(ldexp(0.5, 4), 8.0);
        assert_eq!(ldexp(0.5, 1024), 2f64.powi(1023));
    }
}

//! The variance family as PostgreSQL computes it, in its two kinds of state.
//!
//! Over an exact type PostgreSQL keeps `var_pop`, `var_samp`, `stddev_pop` and `stddev_samp` in
//! `numeric`: the count, the exact sum and the exact sum of squares, which
//! `numeric_stddev_internal` finishes into a `numeric`. Over `real` and `double precision` it keeps
//! the count, the sum and the sum of squared differences from the mean, which `float8_accum`
//! updates in the steps of Youngs and Cramer, finished into a `double precision`. The pin keeps a
//! Welford state for both in [`crate::aggregate`]. The binder gives a PostgreSQL session the names
//! of these states, which are the names of PostgreSQL's final functions.

use rudb_common::{Error, Result, Value};
use rudb_pgtypes::Numeric;

use crate::general::Measure;
use crate::number::{approximate, integral};
use crate::pgmath::overflow;

impl Measure {
    /// Whether the answer is the variance rather than the standard deviation.
    const fn variance(self) -> bool {
        matches!(self, Self::VarSamp | Self::VarPop)
    }

    /// Whether the answer is of a sample rather than of the population.
    const fn sample(self) -> bool {
        matches!(self, Self::VarSamp | Self::StddevSamp)
    }
}

/// The measure of a PostgreSQL name: `prefix` and then one of the four.
fn measure(name: &str, prefix: &str) -> Option<Measure> {
    Measure::named(name.strip_prefix(prefix)?).filter(|measure| *measure != Measure::Sem)
}

/// The two sums of an exact state.
#[derive(Debug, Clone)]
enum Sums {
    /// Counts of `10^-scale`, while every value has the same scale and the sums fit.
    Scaled { sum: i128, squares: i128, scale: u32 },
    /// The sums as `numeric`, from the first value that does not go in [`Sums::Scaled`].
    Numeric { sum: Box<Numeric>, squares: Box<Numeric> },
}

/// `numeric_var_pop` and the others over an exact type, kept as `do_numeric_accum` keeps them.
#[derive(Debug, Clone)]
pub(crate) struct ExactSpread {
    count: i64,
    sums: Option<Sums>,
    measure: Measure,
}

impl ExactSpread {
    /// A fresh state for `name`, or `None` when the name is not one of these.
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(Self { count: 0, sums: None, measure: measure(name, "numeric_")? })
    }

    /// Adds one value, which is an integer, a decimal or a `numeric`.
    pub(crate) fn update(&mut self, value: &Value) -> Result<()> {
        let number = match value {
            Value::Null => return Ok(()),
            Value::Numeric(bytes) => Numeric::from_bytes(bytes),
            Value::Decimal { unscaled, scale, .. } => {
                let scale = u32::from(*scale);
                if self.add_scaled(*unscaled, scale) {
                    return Ok(());
                }
                Numeric::from_decimal(*unscaled, scale)
            }
            _ => {
                let whole = integral(value).ok_or_else(|| {
                    Error::internal(format!("{value:?} handed to an exact variance"))
                })?;
                if self.add_scaled(whole, 0) {
                    return Ok(());
                }
                Numeric::from_integer(whole)
            }
        };
        let square = number.mul(&number)?;
        let (sum, squares) = self.numeric_sums();
        self.sums = Some(Sums::Numeric {
            sum: Box::new(sum.add(&number)?),
            squares: Box::new(squares.add(&square)?),
        });
        self.count += 1;
        Ok(())
    }

    /// Adds `unscaled / 10^scale` to sums that are still integers at that scale, and says whether
    /// it could.
    fn add_scaled(&mut self, unscaled: i128, scale: u32) -> bool {
        let Some(square) = unscaled.checked_mul(unscaled) else { return false };
        match &mut self.sums {
            None => self.sums = Some(Sums::Scaled { sum: unscaled, squares: square, scale }),
            Some(Sums::Scaled { sum, squares, scale: held }) if *held == scale => {
                let (Some(more), Some(more_squares)) =
                    (sum.checked_add(unscaled), squares.checked_add(square))
                else {
                    return false;
                };
                (*sum, *squares) = (more, more_squares);
            }
            Some(_) => return false,
        }
        self.count += 1;
        true
    }

    /// The two sums as `numeric`, with the display scales `do_numeric_accum` gives them.
    fn numeric_sums(&self) -> (Numeric, Numeric) {
        match &self.sums {
            None => (Numeric::from_integer(0), Numeric::from_integer(0)),
            Some(Sums::Scaled { sum, squares, scale }) => {
                (Numeric::from_decimal(*sum, *scale), Numeric::from_decimal(*squares, scale * 2))
            }
            Some(Sums::Numeric { sum, squares }) => ((**sum).clone(), (**squares).clone()),
        }
    }

    /// Folds another state for the same call into this one.
    pub(crate) fn combine(&mut self, other: &Self) -> Result<()> {
        if let (
            Some(Sums::Scaled { sum, squares, scale }),
            Some(Sums::Scaled { sum: more, squares: more_squares, scale: theirs }),
        ) = (&mut self.sums, &other.sums)
            && scale == theirs
            && let (Some(total), Some(total_squares)) =
                (sum.checked_add(*more), squares.checked_add(*more_squares))
        {
            (*sum, *squares) = (total, total_squares);
            self.count += other.count;
            return Ok(());
        }
        if other.sums.is_none() {
            return Ok(());
        }
        if self.sums.is_none() {
            self.sums.clone_from(&other.sums);
            self.count = other.count;
            return Ok(());
        }
        let ((sum, squares), (more, more_squares)) = (self.numeric_sums(), other.numeric_sums());
        self.sums = Some(Sums::Numeric {
            sum: Box::new(sum.add(&more)?),
            squares: Box::new(squares.add(&more_squares)?),
        });
        self.count += other.count;
        Ok(())
    }

    /// The answer, a `numeric` or null, as `numeric_stddev_internal` gives it.
    pub(crate) fn finish(&self) -> Result<Value> {
        let (sum, squares) = self.numeric_sums();
        let answer = Numeric::stddev(
            self.count,
            &sum,
            &squares,
            self.measure.variance(),
            self.measure.sample(),
        )?;
        Ok(answer.map_or(Value::Null, |answer| Value::Numeric(answer.to_bytes())))
    }
}

/// `float8_var_pop` and the others over `real` and `double precision`, kept as `float8_accum`
/// keeps them. The count is a double, as it is in PostgreSQL's state.
#[derive(Debug, Clone)]
pub(crate) struct FloatSpread {
    count: f64,
    sum: f64,
    squared: f64,
    /// Whether a value made a finite sum overflow, which is an error at the answer since
    /// [`crate::general::General::push_reals`] has no way to give one.
    overflowed: bool,
    measure: Measure,
}

impl FloatSpread {
    /// A fresh state for `name`, or `None` when the name is not one of these.
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(Self {
            count: 0.0,
            sum: 0.0,
            squared: 0.0,
            overflowed: false,
            measure: measure(name, "float8_")?,
        })
    }

    /// Adds one value, which is a `real` or a `double precision`.
    pub(crate) fn update(&mut self, value: &Value) -> Result<()> {
        match value {
            Value::Null => {}
            Value::Float(_) | Value::Double(_) => self.add(approximate(value).unwrap_or(f64::NAN)),
            _ => return Err(Error::internal(format!("{value:?} handed to a float variance"))),
        }
        Ok(())
    }

    /// Adds one double, in the steps of `float8_accum`.
    pub(crate) fn add(&mut self, input: f64) {
        let (before, sum_before) = (self.count, self.sum);
        self.count += 1.0;
        self.sum += input;
        if before > 0.0 {
            // Two roundings, as PostgreSQL writes it, rather than one fused step.
            let step = input * self.count - self.sum;
            self.squared += step * step / (self.count * before);
            if self.sum.is_infinite() || self.squared.is_infinite() {
                self.overflowed |= !sum_before.is_infinite() && !input.is_infinite();
                self.squared = f64::NAN;
            }
        } else if !input.is_finite() {
            self.squared = f64::NAN;
        }
    }

    /// Folds another state for the same call into this one, as `float8_combine` does.
    pub(crate) fn combine(&mut self, other: &Self) -> Result<()> {
        self.overflowed |= other.overflowed;
        if other.count == 0.0 {
            return Ok(());
        }
        if self.count == 0.0 {
            (self.count, self.sum, self.squared) = (other.count, other.sum, other.squared);
            return Ok(());
        }
        let count = self.count + other.count;
        let sum = self.sum + other.sum;
        if sum.is_infinite() && !self.sum.is_infinite() && !other.sum.is_infinite() {
            return Err(overflow());
        }
        let step = self.sum / self.count - other.sum / other.count;
        let squared = self.squared + other.squared + self.count * other.count * step * step / count;
        if squared.is_infinite() && !self.squared.is_infinite() && !other.squared.is_infinite() {
            return Err(overflow());
        }
        (self.count, self.sum, self.squared) = (count, sum, squared);
        Ok(())
    }

    /// The answer, a `double precision` or null, as `float8_var_pop` and the others give it.
    pub(crate) fn finish(&self) -> Result<Value> {
        if self.overflowed {
            return Err(overflow());
        }
        let divisor = if self.measure.sample() { self.count - 1.0 } else { self.count };
        if divisor <= 0.0 {
            return Ok(Value::Null);
        }
        let variance = self.squared / divisor;
        Ok(Value::Double(if self.measure.variance() { variance } else { variance.sqrt() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(name: &str, values: &[Value]) -> Value {
        let mut state = ExactSpread::named(name).unwrap();
        for value in values {
            state.update(value).unwrap();
        }
        state.finish().unwrap()
    }

    fn text(value: &Value) -> String {
        match value {
            Value::Numeric(bytes) => {
                let mut out = Vec::new();
                rudb_pgtypes::numeric_out(&Numeric::from_bytes(bytes), &mut out);
                String::from_utf8(out).unwrap()
            }
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn exact_matches_postgres() {
        let values: Vec<Value> = [1, 2, 4].map(Value::Integer).to_vec();
        assert_eq!(text(&exact("numeric_var_pop", &values)), "1.5555555555555556");
        assert_eq!(text(&exact("numeric_var_samp", &values)), "2.3333333333333333");
        assert_eq!(text(&exact("numeric_stddev_pop", &values)), "1.2472191289246471");
        assert_eq!(text(&exact("numeric_stddev_samp", &values)), "1.5275252316519467");
        assert_eq!(exact("numeric_var_samp", &values[..1]), Value::Null);
        assert_eq!(text(&exact("numeric_var_pop", &values[..1])), "0");
        assert_eq!(exact("numeric_var_pop", &[]), Value::Null);
    }

    #[test]
    fn exact_goes_over_to_numeric() {
        let big = Value::BigInt(i64::MAX);
        let small = Value::Decimal { width: 3, scale: 1, unscaled: 15 };
        let mut wide = ExactSpread::named("numeric_var_samp").unwrap();
        for value in [&big, &small, &Value::Integer(2)] {
            wide.update(value).unwrap();
        }
        let mut split = ExactSpread::named("numeric_var_samp").unwrap();
        split.update(&big).unwrap();
        let mut rest = ExactSpread::named("numeric_var_samp").unwrap();
        rest.update(&small).unwrap();
        rest.update(&Value::Integer(2)).unwrap();
        split.combine(&rest).unwrap();
        assert_eq!(text(&wide.finish().unwrap()), text(&split.finish().unwrap()));
    }

    #[test]
    fn float_matches_postgres() {
        let mut state = FloatSpread::named("float8_var_samp").unwrap();
        for input in [1.0, 2.0, 4.0] {
            state.add(input);
        }
        let Value::Double(variance) = state.finish().unwrap() else { panic!() };
        assert!((variance - 7.0 / 3.0).abs() < 1e-15);
        let mut infinite = FloatSpread::named("float8_var_pop").unwrap();
        infinite.add(f64::INFINITY);
        let Value::Double(nan) = infinite.finish().unwrap() else { panic!() };
        assert!(nan.is_nan());
        let mut huge = FloatSpread::named("float8_var_pop").unwrap();
        huge.add(1e308);
        huge.add(1e308);
        assert!(huge.finish().is_err());
    }
}

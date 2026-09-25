//! The statistics over pairs, `corr`, `covar_pop`, `covar_samp` and the nine `regr_*`, and the
//! higher moments of one column, `skewness`, `kurtosis` and `kurtosis_pop`.
//!
//! Every formula here is the pin's, in the pin's order, because the order the arithmetic happens in
//! is the last digit of the answer. The pin is built with GCC, which fuses a multiply and an add
//! into one rounding wherever it can, so a `mul_add` here marks each place the pin's build fused
//! and a plain product marks each place it did not. A pair is read as `(y, x)`, the order the SQL standard writes
//! them in, and a row where either one is null is skipped.
//!
//! The pin keeps one small state per function and builds the bigger ones out of the smaller: the
//! slope is a covariance and a variance, the intercept is a slope and two sums, and `regr_r2` is a
//! correlation and two more variances. Those copies see the same rows and do the same arithmetic,
//! so here there is one of each piece, which is the same numbers in less room.

use rudb_common::{Error, Result, Value};

/// Which answer a [`Paired`] gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pairing {
    Corr,
    CovarPop,
    CovarSamp,
    AvgX,
    AvgY,
    Count,
    Intercept,
    R2,
    Slope,
    Sxx,
    Sxy,
    Syy,
}

impl Pairing {
    /// The measure `name` asks for, or `None` when it is not one of these.
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "corr" => Self::Corr,
            "covar_pop" => Self::CovarPop,
            "covar_samp" => Self::CovarSamp,
            "regr_avgx" => Self::AvgX,
            "regr_avgy" => Self::AvgY,
            "regr_count" => Self::Count,
            "regr_intercept" => Self::Intercept,
            "regr_r2" => Self::R2,
            "regr_slope" => Self::Slope,
            "regr_sxx" => Self::Sxx,
            "regr_sxy" => Self::Sxy,
            "regr_syy" => Self::Syy,
            _ => return None,
        })
    }
}

/// A running mean and sum of squared differences, Welford's update and the pin's combine.
#[derive(Debug, Clone, Copy, Default)]
struct Spread {
    mean: f64,
    squared: f64,
}

impl Spread {
    /// Adds `input` as row number `count`, counted from one.
    fn add(&mut self, input: f64, count: f64) {
        let next = self.mean + (input - self.mean) / count;
        self.squared += (input - next) * (input - self.mean);
        self.mean = next;
    }

    /// Adds `source`, which saw `there` rows, to this one, which saw `here`.
    fn combine(&mut self, source: Self, here: f64, there: f64) {
        let total = here + there;
        let delta = source.mean - self.mean;
        self.squared = source.squared + self.squared + delta * delta * there * here / total;
        self.mean = (there / total).mul_add(delta, self.mean);
    }

    /// The population variance over `count` rows, which the pin takes as zero for one row.
    fn variance(self, count: u64) -> f64 {
        if count > 1 { self.squared / as_float(count) } else { 0.0 }
    }
}

/// The state of one pair statistic.
#[derive(Debug, Clone)]
pub(crate) struct Paired {
    count: u64,
    /// The co-moment's running means, which a combine moves differently from a [`Spread`]'s.
    mean_x: f64,
    mean_y: f64,
    co_moment: f64,
    x: Spread,
    y: Spread,
    sum_x: f64,
    sum_y: f64,
    measure: Pairing,
}

impl Paired {
    pub(crate) fn new(measure: Pairing) -> Self {
        Self {
            count: 0,
            mean_x: 0.0,
            mean_y: 0.0,
            co_moment: 0.0,
            x: Spread::default(),
            y: Spread::default(),
            sum_x: 0.0,
            sum_y: 0.0,
            measure,
        }
    }

    /// Adds one row, with `y` first. A row with a null in it changes nothing.
    pub(crate) fn update(&mut self, args: &[Value]) -> Result<()> {
        let (Some(y), Some(x)) = (args.first(), args.get(1)) else {
            return Err(Error::internal("a pair statistic over fewer than two arguments"));
        };
        match (y, x) {
            (Value::Double(y), Value::Double(x)) => self.add(*y, *x),
            (Value::Null, _) | (_, Value::Null) => {}
            _ => return Err(Error::internal(format!("a pair statistic over {y} and {x}"))),
        }
        Ok(())
    }

    /// Adds the pair `(y, x)`.
    pub(crate) fn add(&mut self, y: f64, x: f64) {
        self.count += 1;
        let n = as_float(self.count);
        // Schubert and Gertz, SSDBM 2018, section 4.3, which is what the pin runs.
        let dx = x - self.mean_x;
        self.mean_x += dx / n;
        let dy = y - self.mean_y;
        self.mean_y += dy / n;
        self.co_moment = dx.mul_add(y - self.mean_y, self.co_moment);
        self.x.add(x, n);
        self.y.add(y, n);
        self.sum_x += x;
        self.sum_y += y;
    }

    pub(crate) fn combine(&mut self, source: &Self) {
        if self.count == 0 {
            *self = source.clone();
            return;
        }
        if source.count == 0 {
            return;
        }
        let (here, there) = (as_float(self.count), as_float(source.count));
        let total = as_float(self.count + source.count);
        let mean_x = (there * source.mean_x + here * self.mean_x) / total;
        let mean_y = (there * source.mean_y + here * self.mean_y) / total;
        let delta_x = self.mean_x - source.mean_x;
        let delta_y = self.mean_y - source.mean_y;
        self.co_moment =
            source.co_moment + self.co_moment + delta_x * delta_y * there * here / total;
        self.mean_x = mean_x;
        self.mean_y = mean_y;
        self.x.combine(source.x, here, there);
        self.y.combine(source.y, here, there);
        self.sum_x += source.sum_x;
        self.sum_y += source.sum_y;
        self.count += source.count;
    }

    pub(crate) fn finish(&self) -> Value {
        let count = self.count;
        if self.measure == Pairing::Count {
            // The pin's count is a `UINTEGER` cut down from the 64 bit one it keeps.
            #[expect(clippy::cast_possible_truncation, reason = "the pin truncates the same way")]
            return Value::UInteger(count as u32);
        }
        if count == 0 {
            return Value::Null;
        }
        let n = as_float(count);
        let covariance = self.co_moment / n;
        let slope = || {
            let variance = self.x.variance(count);
            if variance == 0.0 { f64::NAN } else { covariance / variance }
        };
        let corr = || {
            let spread_x = self.x.variance(count).sqrt();
            let spread_y = self.y.variance(count).sqrt();
            let product = spread_x * spread_y;
            if product == 0.0 { f64::NAN } else { covariance / product }
        };
        Value::Double(match self.measure {
            Pairing::Count => unreachable!("answered above"),
            Pairing::CovarPop => covariance,
            Pairing::CovarSamp if count < 2 => return Value::Null,
            Pairing::CovarSamp => self.co_moment / as_float(count - 1),
            Pairing::Corr => corr(),
            Pairing::AvgX => self.sum_x / n,
            Pairing::AvgY => self.sum_y / n,
            Pairing::Slope => slope(),
            Pairing::Intercept => {
                let slope = slope();
                if slope.is_nan() {
                    return Value::Null;
                }
                (-(self.sum_x / n)).mul_add(slope, self.sum_y / n)
            }
            Pairing::R2 => {
                if self.x.variance(count) == 0.0 {
                    return Value::Null;
                }
                if self.y.variance(count) == 0.0 {
                    1.0
                } else {
                    let corr = corr();
                    corr * corr
                }
            }
            Pairing::Sxx => n * self.x.variance(count),
            Pairing::Syy => n * self.y.variance(count),
            Pairing::Sxy => n * covariance,
        })
    }
}

/// Which answer a [`Powers`] gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Moment {
    Skewness,
    Kurtosis,
    KurtosisPop,
}

impl Moment {
    /// The measure `name` asks for, or `None` when it is not one of these.
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "skewness" => Self::Skewness,
            "kurtosis" => Self::Kurtosis,
            "kurtosis_pop" => Self::KurtosisPop,
            _ => return None,
        })
    }
}

/// The sums of the first four powers of a column, which is how the pin keeps skewness and kurtosis.
#[derive(Debug, Clone)]
pub(crate) struct Powers {
    count: u64,
    sum: f64,
    squares: f64,
    cubes: f64,
    fourths: f64,
    measure: Moment,
}

impl Powers {
    pub(crate) fn new(measure: Moment) -> Self {
        Self { count: 0, sum: 0.0, squares: 0.0, cubes: 0.0, fourths: 0.0, measure }
    }

    /// Adds one value. The cubes and fourths go through `pow` rather than a product, because a cube
    /// made of two multiplications rounds twice and the pin's rounds once. The square is one fused
    /// multiply and add, the same as the pin's.
    pub(crate) fn add(&mut self, input: f64) {
        self.count += 1;
        self.sum += input;
        self.squares = input.mul_add(input, self.squares);
        self.cubes += input.powf(3.0);
        if self.measure != Moment::Skewness {
            self.fourths += input.powf(4.0);
        }
    }

    pub(crate) fn combine(&mut self, source: &Self) {
        if source.count == 0 {
            return;
        }
        self.count += source.count;
        self.sum += source.sum;
        self.squares += source.squares;
        self.cubes += source.cubes;
        self.fourths += source.fourths;
    }

    /// The answer, or null where the pin answers null.
    ///
    /// # Errors
    ///
    /// An out of range error when the answer is not a finite number, in the pin's words.
    pub(crate) fn finish(&self) -> Result<Value> {
        let n = as_float(self.count);
        let (sum, squares) = (self.sum, self.squares);
        let temp = 1.0 / n;
        let answer = match self.measure {
            Moment::Skewness => {
                if self.count <= 2 {
                    return Ok(Value::Null);
                }
                let raw = (-(sum * sum)).mul_add(temp, squares);
                // A second moment that is only noise counts as zero, scaled to the squares.
                if raw.is_finite()
                    && squares.is_finite()
                    && raw.abs() <= f64::EPSILON * 1.0_f64.max(squares.abs())
                {
                    return Ok(Value::Null);
                }
                let variance = temp * raw;
                if variance <= 0.0 {
                    return Ok(Value::Null);
                }
                let div = variance.powf(3.0).sqrt();
                if div == 0.0 {
                    return Ok(Value::Null);
                }
                let temp1 = ((n - 1.0) * n).sqrt() / (n - 2.0);
                let third = (-(3.0 * squares * sum)).mul_add(temp, self.cubes);
                let third = (2.0 * sum.powf(3.0) * temp).mul_add(temp, third);
                let answer = temp1 * temp * third / div;
                if !answer.is_finite() {
                    return Err(Error::out_of_range("SKEW is out of range!".to_string()));
                }
                answer
            }
            Moment::Kurtosis | Moment::KurtosisPop => {
                if self.count <= 1 || (self.measure == Moment::Kurtosis && self.count <= 3) {
                    return Ok(Value::Null);
                }
                // The pin checks this once more in `long double`, which only differs from the
                // check in `double` when the two sides are within a rounding of each other.
                let raw = (-(sum * sum)).mul_add(temp, squares);
                if raw == 0.0 {
                    return Ok(Value::Null);
                }
                let fourth = (-(4.0 * self.cubes * sum)).mul_add(temp, self.fourths);
                let fourth = (6.0 * squares * sum * sum * temp).mul_add(temp, fourth);
                let fourth = (-(3.0 * sum.powf(4.0))).mul_add(temp.powf(3.0), fourth);
                let m4 = temp * fourth;
                let m2 = temp * raw;
                if m2 <= 0.0 {
                    return Ok(Value::Null);
                }
                let answer = if self.measure == Moment::KurtosisPop {
                    m4 / (m2 * m2) - 3.0
                } else {
                    (n - 1.0) * (-(n - 1.0)).mul_add(3.0, (n + 1.0) * m4 / (m2 * m2))
                        / ((n - 2.0) * (n - 3.0))
                };
                if !answer.is_finite() {
                    return Err(Error::out_of_range("Kurtosis is out of range!".to_string()));
                }
                answer
            }
        };
        Ok(Value::Double(answer))
    }
}

/// A count as the double the pin divides by.
#[expect(clippy::cast_precision_loss, reason = "the pin converts a 64 bit count the same way")]
fn as_float(count: u64) -> f64 {
    count as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paired(measure: Pairing, rows: &[(f64, f64)]) -> Value {
        let mut state = Paired::new(measure);
        for &(y, x) in rows {
            state.add(y, x);
        }
        state.finish()
    }

    /// The pin's answers over `(1, 2), (5, 7), (2, 1), (9, 3)`, probed on the pinned binary.
    #[test]
    fn the_pair_statistics_give_the_pins_digits() {
        let rows = [(1.0, 2.0), (5.0, 7.0), (2.0, 1.0), (9.0, 3.0)];
        let expected = [
            (Pairing::Corr, Value::Double(0.379_108_533_559_513_57)),
            (Pairing::CovarPop, Value::Double(2.6875)),
            (Pairing::CovarSamp, Value::Double(3.583_333_333_333_333_5)),
            (Pairing::AvgX, Value::Double(3.25)),
            (Pairing::AvgY, Value::Double(4.25)),
            (Pairing::Count, Value::UInteger(4)),
            (Pairing::Intercept, Value::Double(2.566_265_060_240_963_3)),
            (Pairing::R2, Value::Double(0.143_723_280_217_644_83)),
            (Pairing::Slope, Value::Double(0.518_072_289_156_626_6)),
            (Pairing::Sxx, Value::Double(20.749_999_999_999_996)),
            (Pairing::Sxy, Value::Double(10.75)),
            (Pairing::Syy, Value::Double(38.75)),
        ];
        for (measure, answer) in expected {
            assert_eq!(paired(measure, &rows), answer, "{measure:?}");
        }
    }

    /// A combine of two halves is the pin's formula, and a half with nothing in it is ignored.
    #[test]
    fn a_combine_takes_either_side_when_the_other_is_empty() {
        let mut empty = Paired::new(Pairing::Slope);
        let mut full = Paired::new(Pairing::Slope);
        full.add(1.0, 2.0);
        full.add(5.0, 7.0);
        let before = full.finish();
        empty.combine(&full);
        assert_eq!(empty.finish(), before);
        full.combine(&Paired::new(Pairing::Slope));
        assert_eq!(full.finish(), before);
    }

    #[test]
    fn the_higher_moments_give_the_pins_digits() {
        let answer = |measure, values: &[f64]| {
            let mut state = Powers::new(measure);
            values.iter().for_each(|&value| state.add(value));
            state.finish().unwrap()
        };
        let values = [1.0, 2.0, 4.0, 8.0, 9.0];
        assert_eq!(answer(Moment::Skewness, &values), Value::Double(0.271_768_758_235_262_6));
        assert_eq!(answer(Moment::Kurtosis, &values), Value::Double(-2.680_265_360_530_715));
        assert_eq!(answer(Moment::KurtosisPop, &values), Value::Double(-1.670_066_340_132_678_7));
        assert_eq!(answer(Moment::KurtosisPop, &[1.0, 2.0]), Value::Double(-2.0));
        assert_eq!(answer(Moment::Skewness, &[1.0, 2.0]), Value::Null);
        assert_eq!(answer(Moment::Skewness, &[3.0, 3.0, 3.0]), Value::Null);
        let mut state = Powers::new(Moment::Skewness);
        [1e200, 2e200, -5e200].iter().for_each(|&value| state.add(value));
        assert!(state.finish().unwrap_err().to_string().contains("SKEW is out of range!"));
    }
}

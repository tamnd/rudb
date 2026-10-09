//! `equi_width_bins(min, max, bin_count, nice_rounding)`, the upper boundaries of `bin_count`
//! bins of equal width between `min` and `max`.
//!
//! The binder casts both ends to one of three types and the boundaries are worked out in it: a
//! BIGINT, a DOUBLE or a TIMESTAMP. The answer is then cast to the list type the binder asked for,
//! which is the type of `max`. The arithmetic is the pin's step for step, because the boundaries
//! are what a histogram is cut at and a boundary off by one unit puts a row in another bin.
//!
//! With `nice_rounding` the step is moved to a number a person would pick, a multiple of one, two
//! or five times a power of ten for numbers and a whole unit of time for timestamps, and the last
//! boundary is moved up to a multiple of it. The bin count is then only a hint, and a number may
//! come back with up to twice as many bins.

use rudb_common::{Error, LogicalType, Result, Value, civil_from_days, days_from_civil};

use crate::cast;
use crate::datetime::{
    MICROS_PER_DAY, MICROS_PER_HOUR, MICROS_PER_MINUTE, MICROS_PER_SECOND, infinite_stamp,
    shifted_stamp,
};

/// The most bins a call may ask for, which is the pin's limit.
const MOST_BINS: i64 = 1_000_000;

/// The answer of `equi_width_bins` when `name` is it, or `None` for any other function.
pub(crate) fn value(name: &str, args: &[Value], returns: &LogicalType) -> Option<Result<Value>> {
    if name != "equi_width_bins" {
        return None;
    }
    Some(bins(args, returns))
}

fn bins(args: &[Value], returns: &LogicalType) -> Result<Value> {
    let [min, max, Value::BigInt(count), Value::Boolean(nice)] = args else {
        return Err(Error::internal("equi_width_bins takes two ends, a count and a flag"));
    };
    let element = match returns {
        LogicalType::List(element) => (**element).clone(),
        other => return Err(Error::internal(format!("equi_width_bins answers {other}"))),
    };
    let ordered = match (min, max) {
        (Value::BigInt(min), Value::BigInt(max)) => max.cmp(min),
        (Value::Double(min), Value::Double(max)) => max.partial_cmp(min).unwrap_or_default(),
        (Value::Timestamp(min), Value::Timestamp(max)) => max.cmp(min),
        _ => return Err(Error::internal("equi_width_bins takes two ends of one type")),
    };
    if ordered.is_lt() {
        return Err(Error::invalid_input(
            "Invalid input for bin function - max value is smaller than min value",
        ));
    }
    if *count <= 0 {
        return Err(Error::invalid_input(
            "Invalid input for bin function - there must be > 0 bins",
        ));
    }
    if *count > MOST_BINS {
        return Err(Error::invalid_input(format!(
            "Invalid input for bin function - max bin count of {MOST_BINS} exceeded"
        )));
    }
    let count = count.unsigned_abs();
    let mut boundaries = if same(min, max) {
        vec![max.clone()]
    } else {
        let mut boundaries = match (min, max) {
            (Value::BigInt(min), Value::BigInt(max)) => {
                whole(*min, *max, count, *nice).into_iter().map(Value::BigInt).collect()
            }
            (Value::Double(min), Value::Double(max)) => {
                real(*min, *max, count, *nice)?.into_iter().map(Value::Double).collect()
            }
            (Value::Timestamp(min), Value::Timestamp(max)) => {
                stamps(*min, *max, count, *nice)?.into_iter().map(Value::Timestamp).collect()
            }
            _ => Vec::new(),
        };
        // The last boundary is never below the input's max, and the pin moves it up when the
        // steps fell short. Every boundary was worked out from the top down.
        match boundaries.first_mut() {
            Some(first) if below(first, max) => *first = max.clone(),
            Some(_) => {}
            None => boundaries.push(max.clone()),
        }
        boundaries.reverse();
        boundaries
    };
    for boundary in &mut boundaries {
        *boundary = cast::cast_value(boundary, &element, false)?;
    }
    Ok(Value::List { element, values: boundaries })
}

/// Whether the two ends are equal, a NaN being equal to nothing as it is in the pin's comparison.
#[allow(clippy::float_cmp)]
fn same(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Double(left), Value::Double(right)) => left == right,
        _ => left == right,
    }
}

fn below(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::BigInt(left), Value::BigInt(right)) => left < right,
        (Value::Double(left), Value::Double(right)) => left < right,
        (Value::Timestamp(left), Value::Timestamp(right)) => left < right,
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum Rounding {
    Ceiling,
    Nearest,
}

/// The power of ten one below `input`, so 10 for 67 and 0 for 1.
fn whole_power_below(input: i128) -> i128 {
    let mut power = 1;
    while power < input {
        power *= 10;
    }
    power / 10
}

fn whole_rounded(input: i128, to: i128, rounding: Rounding) -> i128 {
    if to == 0 {
        return input;
    }
    match rounding {
        Rounding::Nearest => (input + to / 2) / to * to,
        Rounding::Ceiling => (input + (to - 1)) / to * to,
    }
}

/// `input` moved to the nearer multiple of two or five times the power of ten below `step`, so
/// 122 becomes 120 and 153 becomes 150 for a step of 100.
fn whole_nice(input: i128, step: i128, rounding: Rounding) -> i128 {
    let power = whole_power_below(step);
    let mut two = power * 2;
    let mut five = power;
    if power * 3 <= step {
        two *= 5;
    }
    if power * 2 <= step {
        five *= 5;
    }
    let by_two = whole_rounded(input, two, rounding);
    let by_five = whole_rounded(input, five, rounding);
    if (input - by_two).abs() < (input - by_five).abs() { by_two } else { by_five }
}

/// The boundaries over whole numbers, from the top down. They are worked out a thousand times
/// larger so that truncating the step does not move the boundaries.
fn whole(least: i64, most: i64, count: u64, nice: bool) -> Vec<i64> {
    const FACTOR: i128 = 1000;
    let min = i128::from(least) * FACTOR;
    let mut max = i128::from(most) * FACTOR;
    let mut count = count;
    // More bins than there are numbers in the range step by the smallest step there is.
    let mut step = ((max - min) / i128::from(count)).max(1);
    if nice {
        let nicer = whole_nice(step, step, Rounding::Nearest);
        let top = whole_rounded(max, nicer, Rounding::Ceiling);
        if top != min && nicer != 0 {
            max = top;
            step = nicer;
        }
        count *= 2;
    }
    let mut boundaries: Vec<i64> = Vec::new();
    let mut boundary = max;
    while boundary > min {
        // A boundary past the end of a BIGINT is read as zero, which is what the pin's cast does,
        // and it is only ever the top one, which the caller moves down to the max.
        let held = i64::try_from(boundary / FACTOR).unwrap_or(0);
        boundary -= step;
        if let Some(&last) = boundaries.last() {
            if held < least || boundaries.len() as u64 >= count {
                break;
            }
            if held == last {
                continue;
            }
        }
        boundaries.push(held);
    }
    boundaries
}

fn real_power_below(input: f64) -> f64 {
    let mut power = 1.0;
    if input < 1.0 {
        while power > input {
            power /= 10.0;
        }
        return power;
    }
    while power < input {
        power *= 10.0;
    }
    power / 10.0
}

fn real_rounded(input: f64, to: f64, rounding: Rounding) -> f64 {
    let rounded = match rounding {
        Rounding::Nearest => (input / to).round() * to,
        Rounding::Ceiling => (input / to).ceil() * to,
    };
    if rounded.is_finite() { rounded } else { input }
}

fn real_nice(input: f64, step: f64, rounding: Rounding) -> f64 {
    if input == 0.0 {
        return 0.0;
    }
    let power = real_power_below(step);
    let mut two = power * 2.0;
    let mut five = power;
    if power * 3.0 <= step {
        two *= 5.0;
    }
    if power * 2.0 <= step {
        five *= 5.0;
    }
    let by_two = real_rounded(input, two, rounding);
    let by_five = real_rounded(input, five, rounding);
    if (input - by_two).abs() < (input - by_five).abs() { by_two } else { by_five }
}

fn unbounded() -> Error {
    Error::invalid_input("equi_width_bucket does not support infinite or nan as min/max value")
}

/// The boundaries over doubles, from the top down.
#[allow(clippy::cast_precision_loss, clippy::float_cmp)]
fn real(min: f64, most: f64, count: u64, nice: bool) -> Result<Vec<f64>> {
    if !min.is_finite() || !most.is_finite() {
        return Err(unbounded());
    }
    let mut max = most;
    let mut count = count;
    let span = max - min;
    let mut step = if span.is_finite() {
        span / count as f64
    } else {
        // The two ends are too far apart for a double to hold the span.
        max / count as f64 - min / count as f64
    };
    let power = real_power_below(step);
    if nice {
        step = real_nice(step, step, Rounding::Nearest);
        max = real_rounded(most, step, Rounding::Ceiling);
        count *= 2;
    }
    let scale = 10.0 / power;
    if max - step >= max || (nice && !scale.is_finite()) {
        // The span is too small for a step to move anything.
        return Ok(vec![max]);
    }
    let mut boundaries: Vec<f64> = Vec::new();
    let mut boundary = max;
    while boundary > min {
        // Each step is rounded again, since subtracting a step over and over drifts.
        let held = if nice { (boundary * scale).round() / scale } else { boundary };
        boundary -= step;
        if boundaries.last() == Some(&held) {
            continue;
        }
        if held <= min || boundaries.len() as u64 >= count {
            break;
        }
        boundaries.push(held);
    }
    Ok(boundaries)
}

/// A timestamp's calendar date and clock, the clock in microseconds.
struct Parts {
    year: i32,
    month: u32,
    day: u32,
    hour: i64,
    minute: i64,
    second: i64,
    micros: i64,
}

impl Parts {
    fn of(stamp: i64) -> Self {
        let days = i32::try_from(stamp.div_euclid(MICROS_PER_DAY)).unwrap_or_default();
        let clock = stamp.rem_euclid(MICROS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: clock / MICROS_PER_HOUR,
            minute: clock % MICROS_PER_HOUR / MICROS_PER_MINUTE,
            second: clock % MICROS_PER_MINUTE / MICROS_PER_SECOND,
            micros: clock % MICROS_PER_SECOND,
        }
    }

    fn stamp(&self) -> i64 {
        let days = i64::from(days_from_civil(self.year, self.month, self.day));
        days * MICROS_PER_DAY
            + self.hour * MICROS_PER_HOUR
            + self.minute * MICROS_PER_MINUTE
            + self.second * MICROS_PER_SECOND
            + self.micros
    }

    fn next_month(&mut self) {
        self.month += 1;
        if self.month == 13 {
            self.year += 1;
            self.month = 1;
        }
    }

    fn next_day(&mut self) {
        self.day += 1;
        let (_, month, _) = civil_from_days(days_from_civil(self.year, self.month, self.day));
        if month != self.month {
            self.next_month();
            self.day = 1;
        }
    }

    fn next_hour(&mut self) {
        self.hour += 1;
        if self.hour >= 24 {
            self.next_day();
            self.hour = 0;
        }
    }

    fn next_minute(&mut self) {
        self.minute += 1;
        if self.minute >= 60 {
            self.next_hour();
            self.minute = 0;
        }
    }

    fn next_second(&mut self) {
        self.second += 1;
        if self.second >= 60 {
            self.next_minute();
            self.second = 0;
        }
    }

    /// Moved up to the next whole unit of the step: a month for a step of a year or more, a day
    /// for a step with months or days in it, then an hour, a minute or a second.
    fn nice(mut self, step: &Step) -> Self {
        let clock = self.hour > 0 || self.minute > 0 || self.second > 0 || self.micros > 0;
        if step.months >= 12 {
            if self.day > 1 || clock {
                self.next_month();
                self.day = 1;
                (self.hour, self.minute, self.second, self.micros) = (0, 0, 0, 0);
            }
        } else if step.months > 0 || step.days >= 1 {
            if clock {
                self.next_day();
                (self.hour, self.minute, self.second, self.micros) = (0, 0, 0, 0);
            }
        } else if step.micros >= MICROS_PER_HOUR {
            if self.minute > 0 || self.second > 0 || self.micros > 0 {
                self.next_hour();
                (self.minute, self.second, self.micros) = (0, 0, 0);
            }
        } else if step.micros >= MICROS_PER_MINUTE {
            if self.second > 0 || self.micros > 0 {
                self.next_minute();
                (self.second, self.micros) = (0, 0);
            }
        } else if step.micros >= MICROS_PER_SECOND && self.micros > 0 {
            self.next_second();
            self.micros = 0;
        }
        self
    }
}

/// An interval kept the way the pin keeps one, so that the step rounds as it does there.
struct Step {
    months: i32,
    days: i32,
    micros: i64,
}

fn to_multiple(number: i64, divisor: i64) -> i64 {
    (number + divisor / 2) / divisor * divisor
}

impl Step {
    /// The step with its smaller units dropped or rounded, the more so the longer it is.
    fn nice(mut self) -> Self {
        if self.months >= 6 {
            self.days = 0;
            self.micros = 0;
        } else if self.months > 0 || self.days >= 5 {
            self.micros = 0;
        } else if self.days > 0 || self.micros >= 6 * MICROS_PER_HOUR {
            self.micros = to_multiple(self.micros, MICROS_PER_HOUR);
        } else if self.micros >= MICROS_PER_HOUR {
            self.micros = to_multiple(self.micros, MICROS_PER_MINUTE * 15);
        } else if self.micros >= MICROS_PER_MINUTE * 10 {
            self.micros = to_multiple(self.micros, MICROS_PER_MINUTE);
        } else if self.micros >= MICROS_PER_MINUTE {
            self.micros = to_multiple(self.micros, MICROS_PER_SECOND * 15);
        } else if self.micros >= MICROS_PER_SECOND * 10 {
            self.micros = to_multiple(self.micros, MICROS_PER_SECOND);
        }
        self
    }
}

/// The boundaries over timestamps, from the top down. Without rounding they are the whole number
/// boundaries over the microseconds.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn stamps(min: i64, max: i64, count: u64, nice: bool) -> Result<Vec<i64>> {
    if infinite_stamp(min) || infinite_stamp(max) {
        return Err(unbounded());
    }
    if !nice {
        return Ok(whole(min, max, count, false));
    }
    let (low, high) = (Parts::of(min), Parts::of(max));
    // Each unit is a difference of its own and any of them can be negative, except the largest
    // one that is not zero.
    let months = (high.year - low.year) * 12 + (high.month.cast_signed() - low.month.cast_signed());
    let days = high.day.cast_signed() - low.day.cast_signed();
    let micros = (high.hour - low.hour) * MICROS_PER_HOUR
        + (high.minute - low.minute) * MICROS_PER_MINUTE
        + (high.second - low.second) * MICROS_PER_SECOND
        + (high.micros - low.micros);
    let bins = count as f64;
    let step_months = f64::from(months) / bins;
    let mut step_days = f64::from(days) / bins;
    let mut step_micros = micros as f64 / bins;
    // A month or a day cut short hands what was cut to the unit below it, a month as thirty days.
    if step_months > 0.0 {
        step_days += (step_months - step_months.floor()) * 30.0;
    }
    if step_days > 0.0 {
        step_micros += (step_days - step_days.floor()) * MICROS_PER_DAY as f64;
    }
    let mut step =
        Step { months: step_months as i32, days: step_days as i32, micros: step_micros as i64 }
            .nice();
    let mut stamp = high.nice(&step).stamp();
    if step.months <= 0 && step.days <= 0 && step.micros <= 0 {
        step = Step { months: 0, days: 0, micros: 1 };
    }
    let mut boundaries = Vec::new();
    while stamp >= min && (boundaries.len() as u64) < count {
        boundaries.push(stamp);
        stamp = shifted_stamp(
            stamp,
            -i64::from(step.months),
            -i64::from(step.days),
            -i128::from(step.micros),
        )?;
    }
    Ok(boundaries)
}

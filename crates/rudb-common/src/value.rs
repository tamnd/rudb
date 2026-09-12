//! Single values.
//!
//! A `Value` is one SQL value, boxed up on its own. It is what a literal parses into, what a
//! constant folds to, and what a result set is read out as one cell at a time. It is deliberately
//! not what execution runs on: `spec/07-execution.md` says the unit of data is a vector of 1024,
//! and an operator that touches a `Value` per row is an operator that has already lost.
//!
//! The formatting here is DuckDB's, because a shell that prints `2024-01-15` where DuckDB prints
//! `2024-01-15` is a shell whose output can be diffed against DuckDB's in `tamnd/rudb-compat`.

use std::fmt;

use crate::types::LogicalType;

/// A single SQL value.
///
/// `PartialEq` here is Rust equality and not SQL equality. Two nulls compare equal and two NaNs
/// compare equal, both of which SQL disagrees with. That is the right behaviour for a test
/// assertion and the wrong behaviour for a `WHERE` clause, and the `WHERE` clause gets its
/// comparison from the kernels rather than from here.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// `NULL`, of no particular type.
    Null,
    /// `BOOLEAN`.
    Boolean(bool),
    /// `TINYINT`.
    TinyInt(i8),
    /// `SMALLINT`.
    SmallInt(i16),
    /// `INTEGER`.
    Integer(i32),
    /// `BIGINT`.
    BigInt(i64),
    /// `HUGEINT`.
    HugeInt(i128),
    /// `UTINYINT`.
    UTinyInt(u8),
    /// `USMALLINT`.
    USmallInt(u16),
    /// `UINTEGER`.
    UInteger(u32),
    /// `UBIGINT`.
    UBigInt(u64),
    /// `UHUGEINT`.
    UHugeInt(u128),
    /// `FLOAT`.
    Float(f32),
    /// `DOUBLE`.
    Double(f64),
    /// `DECIMAL(width, scale)`, carrying the unscaled integer.
    Decimal {
        /// The unscaled value, so 12.34 at scale 2 is 1234.
        unscaled: i128,
        /// Total digits.
        width: u8,
        /// Digits right of the point.
        scale: u8,
    },
    /// `VARCHAR`.
    Varchar(String),
    /// `BLOB`.
    Blob(Vec<u8>),
    /// `DATE`, days since 1970-01-01.
    Date(i32),
    /// `TIME`, microseconds since midnight.
    Time(i64),
    /// `TIMESTAMP`, microseconds since 1970-01-01 00:00:00.
    Timestamp(i64),
    /// `INTERVAL`, the months, days and microseconds triple.
    ///
    /// Three fields rather than one duration because interval arithmetic with months is not
    /// associative with days, and DuckDB's specific behaviour is what tests assert on. A month is
    /// not 30 days and this representation is what refuses to pretend otherwise.
    Interval {
        /// Whole months.
        months: i32,
        /// Whole days.
        days: i32,
        /// Microseconds.
        micros: i64,
    },
    /// A list, carrying its element type so that an empty list still knows what it is empty of.
    List {
        /// The element type.
        element: LogicalType,
        /// The elements.
        values: Vec<Value>,
    },
    /// A struct, in field order.
    Struct(Vec<(String, Value)>),
}

impl Value {
    /// How many bytes this value takes, counting what it owns on the heap.
    ///
    /// What the memory limit charges for a value held in a buffer. It is the enum itself plus the
    /// string, the blob, the list or the struct behind it, and it counts capacity rather than
    /// length, because capacity is what was taken from the allocator and a string built by pushing
    /// bytes usually has more of it than it needs.
    ///
    /// The enum is as wide as its widest arm whatever is in it, so a `BOOLEAN` costs the same as a
    /// `HUGEINT` here. That is not a rounding error, it is the layout: a row of booleans held as
    /// values really does cost that.
    #[must_use]
    pub fn footprint(&self) -> usize {
        size_of::<Self>() + self.heap()
    }

    /// What this value owns beyond its own bytes.
    fn heap(&self) -> usize {
        match self {
            Self::Varchar(text) => text.capacity(),
            Self::Blob(bytes) => bytes.capacity(),
            Self::List { values, .. } => {
                values.capacity() * size_of::<Self>() + values.iter().map(Self::heap).sum::<usize>()
            }
            Self::Struct(fields) => {
                fields.capacity() * size_of::<(String, Self)>()
                    + fields
                        .iter()
                        .map(|(name, value)| name.capacity() + value.heap())
                        .sum::<usize>()
            }
            _ => 0,
        }
    }

    /// Whether this is `NULL`.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// The type of this value.
    #[must_use]
    pub fn logical_type(&self) -> LogicalType {
        match self {
            Self::Null => LogicalType::Null,
            Self::Boolean(_) => LogicalType::Boolean,
            Self::TinyInt(_) => LogicalType::TinyInt,
            Self::SmallInt(_) => LogicalType::SmallInt,
            Self::Integer(_) => LogicalType::Integer,
            Self::BigInt(_) => LogicalType::BigInt,
            Self::HugeInt(_) => LogicalType::HugeInt,
            Self::UTinyInt(_) => LogicalType::UTinyInt,
            Self::USmallInt(_) => LogicalType::USmallInt,
            Self::UInteger(_) => LogicalType::UInteger,
            Self::UBigInt(_) => LogicalType::UBigInt,
            Self::UHugeInt(_) => LogicalType::UHugeInt,
            Self::Float(_) => LogicalType::Float,
            Self::Double(_) => LogicalType::Double,
            Self::Decimal { width, scale, .. } => {
                LogicalType::Decimal { width: *width, scale: *scale }
            }
            Self::Varchar(_) => LogicalType::Varchar,
            Self::Blob(_) => LogicalType::Blob,
            Self::Date(_) => LogicalType::Date,
            Self::Time(_) => LogicalType::Time,
            Self::Timestamp(_) => LogicalType::Timestamp,
            Self::Interval { .. } => LogicalType::Interval,
            Self::List { element, .. } => LogicalType::list(element.clone()),
            Self::Struct(fields) => LogicalType::Struct(
                fields
                    .iter()
                    .map(|(name, value)| crate::types::Field::new(name, value.logical_type()))
                    .collect(),
            ),
        }
    }

    /// The value as an `i64`, for the integer types that fit in one.
    ///
    /// Used by the planner for the places where a literal has to be a small integer, `LIMIT` and
    /// `OFFSET` being the obvious ones. Returns `None` rather than saturating, because a `LIMIT`
    /// that silently became `i64::MAX` is worse than an error.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::TinyInt(v) => Some(i64::from(v)),
            Self::SmallInt(v) => Some(i64::from(v)),
            Self::Integer(v) => Some(i64::from(v)),
            Self::BigInt(v) => Some(v),
            Self::UTinyInt(v) => Some(i64::from(v)),
            Self::USmallInt(v) => Some(i64::from(v)),
            Self::UInteger(v) => Some(i64::from(v)),
            Self::UBigInt(v) => i64::try_from(v).ok(),
            Self::HugeInt(v) => i64::try_from(v).ok(),
            Self::UHugeInt(v) => i64::try_from(v).ok(),
            _ => None,
        }
    }

    /// The value as a `bool`, for a `BOOLEAN` and nothing else.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Self::Boolean(v) => Some(v),
            _ => None,
        }
    }

    /// The value as a string slice, for a `VARCHAR` and nothing else.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Varchar(v) => Some(v),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Boolean(v) => f.write_str(if *v { "true" } else { "false" }),
            Self::TinyInt(v) => write!(f, "{v}"),
            Self::SmallInt(v) => write!(f, "{v}"),
            Self::Integer(v) => write!(f, "{v}"),
            Self::BigInt(v) => write!(f, "{v}"),
            Self::HugeInt(v) => write!(f, "{v}"),
            Self::UTinyInt(v) => write!(f, "{v}"),
            Self::USmallInt(v) => write!(f, "{v}"),
            Self::UInteger(v) => write!(f, "{v}"),
            Self::UBigInt(v) => write!(f, "{v}"),
            Self::UHugeInt(v) => write!(f, "{v}"),
            Self::Float(v) => write_float(f, *v),
            Self::Double(v) => write_float(f, *v),
            Self::Decimal { unscaled, scale, .. } => write_decimal(f, *unscaled, *scale),
            Self::Varchar(v) => f.write_str(v),
            Self::Blob(v) => write_blob(f, v),
            Self::Date(v) => write_date(f, *v),
            Self::Time(v) => write_time(f, *v),
            Self::Timestamp(v) => write_timestamp(f, *v),
            Self::Interval { months, days, micros } => write_interval(f, *months, *days, *micros),
            Self::List { values, .. } => {
                f.write_str("[")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{value}")?;
                }
                f.write_str("]")
            }
            Self::Struct(fields) => {
                f.write_str("{")?;
                for (index, (name, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "'{name}': {value}")?;
                }
                f.write_str("}")
            }
        }
    }
}

/// The two float types, so that one printer can serve both without going through `f64`.
///
/// Widening an `f32` to print it is wrong and quietly so: `0.1f32` as an `f64` is
/// `0.10000000149011612`, and the shortest text that reads back as the same `f32` is `0.1`. DuckDB
/// prints `0.1`, and it prints it because it formats the `float` rather than a `double` made out of
/// one.
trait Real: Copy + fmt::Display + fmt::LowerExp {
    fn is_nan(self) -> bool;
    fn is_infinite(self) -> bool;
    fn is_sign_negative(self) -> bool;
}

impl Real for f32 {
    fn is_nan(self) -> bool {
        Self::is_nan(self)
    }

    fn is_infinite(self) -> bool {
        Self::is_infinite(self)
    }

    fn is_sign_negative(self) -> bool {
        Self::is_sign_negative(self)
    }
}

impl Real for f64 {
    fn is_nan(self) -> bool {
        Self::is_nan(self)
    }

    fn is_infinite(self) -> bool {
        Self::is_infinite(self)
    }

    fn is_sign_negative(self) -> bool {
        Self::is_sign_negative(self)
    }
}

/// Floats print the shortest text that reads back as the same value, laid out the way DuckDB lays
/// it out.
///
/// Rust and DuckDB agree on the digits and disagree on everything around them. A float with nothing
/// after the point keeps its `.0`, so a `DOUBLE` never looks like an integer. Anything with a
/// decimal exponent outside `-4..16` is written in exponent form with a signed two digit exponent,
/// so `1e16` is `1e+16` and `0.00001` is `1e-05`, while `1e15` is still written out in full. That
/// is C's `%g` rule and it is what DuckDB's formatter implements, checked against the binary rather
/// than read out of its source.
fn write_float<T: Real>(f: &mut fmt::Formatter<'_>, value: T) -> fmt::Result {
    // The sign bit and nothing else, because no comparison against a nan says anything about it.
    // An invalid operation on x86 produces a nan with the bit set and DuckDB prints that as `-nan`,
    // where the nan a string parses to has the bit clear and prints as `nan`. Rust prints `NaN` for
    // both.
    if value.is_nan() {
        return f.write_str(if value.is_sign_negative() { "-nan" } else { "nan" });
    }
    if value.is_infinite() {
        return f.write_str(if value.is_sign_negative() { "-inf" } else { "inf" });
    }
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((scientific.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-4..16).contains(&exponent) {
        let text = format!("{value}");
        if text.contains('.') {
            return f.write_str(&text);
        }
        return write!(f, "{text}.0");
    }
    let sign = if exponent < 0 { '-' } else { '+' };
    write!(f, "{mantissa}e{sign}{:02}", exponent.abs())
}

fn write_decimal(f: &mut fmt::Formatter<'_>, unscaled: i128, scale: u8) -> fmt::Result {
    if scale == 0 {
        return write!(f, "{unscaled}");
    }
    let negative = unscaled < 0;
    // Widened before the negation so that i128::MIN does not overflow on the way to its digits.
    let digits = unscaled.unsigned_abs().to_string();
    let scale = usize::from(scale);
    let (whole, fraction) = if digits.len() > scale {
        let split = digits.len() - scale;
        (digits[..split].to_string(), digits[split..].to_string())
    } else {
        ("0".to_string(), format!("{:0>scale$}", digits))
    };
    if negative {
        f.write_str("-")?;
    }
    write!(f, "{whole}.{fraction}")
}

/// A blob prints as printable ASCII with everything else hex escaped, which is DuckDB's rule.
///
/// Three printable characters are escaped anyway, and they are the three that would otherwise make
/// the printed form ambiguous: a backslash because it starts an escape, and the two quotes because
/// the text this prints into is a string literal often enough. Every byte of all 256 was compared
/// against DuckDB and these three were the only disagreement.
fn write_blob(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for &byte in bytes {
        if (byte.is_ascii_graphic() || byte == b' ') && !matches!(byte, b'\\' | b'\'' | b'"') {
            write!(f, "{}", byte as char)?;
        } else {
            write!(f, "\\x{byte:02X}")?;
        }
    }
    Ok(())
}

/// Days since the epoch to the civil date, by Howard Hinnant's algorithm.
///
/// Written out rather than pulled in from a date library because it is twenty lines, because the
/// dependency table in `spec/18-package-layout.md` is short on purpose, and because a date library
/// that disagrees with DuckDB about a date before 1582 is a compatibility bug we would then own
/// without being able to fix it.
#[must_use]
pub fn civil_from_days(days: i32) -> (i32, u32, u32) {
    let z = i64::from(days) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    #[expect(clippy::cast_possible_truncation, reason = "the ranges are 1 to 12 and 1 to 31")]
    (year as i32, month as u32, day as u32)
}

/// The civil date to days since the epoch, the inverse of [`civil_from_days`].
#[must_use]
pub fn days_from_civil(year: i32, month: u32, day: u32) -> i32 {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    #[expect(clippy::cast_possible_truncation, reason = "a date in i32 range stays in i32 range")]
    ((era * 146_097 + day_of_era - 719_468) as i32)
}

fn write_date(f: &mut fmt::Formatter<'_>, days: i32) -> fmt::Result {
    let (year, month, day) = civil_from_days(days);
    if year < 0 {
        write!(f, "{:04}-{month:02}-{day:02} (BC)", -year + 1)
    } else {
        write!(f, "{year:04}-{month:02}-{day:02}")
    }
}

fn write_time(f: &mut fmt::Formatter<'_>, micros: i64) -> fmt::Result {
    let seconds = micros.div_euclid(1_000_000);
    let fraction = micros.rem_euclid(1_000_000);
    let (hours, minutes, seconds) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    write!(f, "{hours:02}:{minutes:02}:{seconds:02}")?;
    if fraction != 0 {
        // Trailing zeros are trimmed, so a value on a millisecond boundary prints three digits.
        let text = format!("{fraction:06}");
        write!(f, ".{}", text.trim_end_matches('0'))?;
    }
    Ok(())
}

fn write_timestamp(f: &mut fmt::Formatter<'_>, micros: i64) -> fmt::Result {
    const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;
    let days = micros.div_euclid(MICROS_PER_DAY);
    let within_day = micros.rem_euclid(MICROS_PER_DAY);
    let Ok(days) = i32::try_from(days) else {
        return f.write_str("timestamp out of range");
    };
    write_date(f, days)?;
    f.write_str(" ")?;
    write_time(f, within_day)
}

fn write_interval(f: &mut fmt::Formatter<'_>, months: i32, days: i32, micros: i64) -> fmt::Result {
    let mut wrote = false;
    let space = |f: &mut fmt::Formatter<'_>, wrote: &mut bool| -> fmt::Result {
        if *wrote {
            f.write_str(" ")?;
        }
        *wrote = true;
        Ok(())
    };
    let (years, rest_months) = (months / 12, months % 12);
    if years != 0 {
        space(f, &mut wrote)?;
        write!(f, "{years} year{}", plural(years))?;
    }
    if rest_months != 0 {
        space(f, &mut wrote)?;
        write!(f, "{rest_months} month{}", plural(rest_months))?;
    }
    if days != 0 {
        space(f, &mut wrote)?;
        write!(f, "{days} day{}", plural(days))?;
    }
    if micros != 0 || !wrote {
        space(f, &mut wrote)?;
        if micros < 0 {
            f.write_str("-")?;
        }
        write_time(f, micros.abs())?;
    }
    Ok(())
}

fn plural(n: i32) -> &'static str {
    if n == 1 || n == -1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::{Value, civil_from_days, days_from_civil};
    use crate::types::LogicalType;

    #[test]
    fn a_value_knows_its_own_type() {
        assert_eq!(Value::Integer(1).logical_type(), LogicalType::Integer);
        assert_eq!(Value::Null.logical_type(), LogicalType::Null);
        let list = Value::List { element: LogicalType::Varchar, values: Vec::new() };
        // The element type is carried rather than inferred, which is why an empty list still
        // knows what it is empty of.
        assert_eq!(list.logical_type(), LogicalType::list(LogicalType::Varchar));
    }

    #[test]
    fn the_date_conversion_is_its_own_inverse() {
        // Every day from 1600 to 2400, which covers the Gregorian corrections and both signs of
        // the era arithmetic. Cheap enough to be exhaustive, so it is exhaustive.
        for days in days_from_civil(1600, 1, 1)..days_from_civil(2400, 1, 1) {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days, "{year}-{month}-{day}");
        }
    }

    #[test]
    fn the_epoch_is_where_it_should_be() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(Value::Date(0).to_string(), "1970-01-01");
        assert_eq!(Value::Date(19_723).to_string(), "2024-01-01");
        assert_eq!(Value::Date(19_737).to_string(), "2024-01-15");
    }

    #[test]
    fn a_leap_day_is_a_day() {
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        // 1900 was not a leap year and 2000 was, which is the pair every naive implementation
        // gets wrong in one direction or the other.
        assert_eq!(days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 28), 1);
        assert_eq!(days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28), 2);
    }

    #[test]
    fn times_print_with_the_trailing_zeros_trimmed() {
        assert_eq!(Value::Time(0).to_string(), "00:00:00");
        assert_eq!(Value::Time(3_723_000_000).to_string(), "01:02:03");
        assert_eq!(Value::Time(3_723_500_000).to_string(), "01:02:03.5");
        assert_eq!(Value::Time(3_723_000_001).to_string(), "01:02:03.000001");
    }

    #[test]
    fn a_timestamp_before_the_epoch_borrows_from_the_day() {
        // The whole reason this uses div_euclid rather than a plain divide. A negative microsecond
        // count is the previous day at a positive time, not the next day at a negative one.
        assert_eq!(Value::Timestamp(-1).to_string(), "1969-12-31 23:59:59.999999");
        assert_eq!(Value::Timestamp(0).to_string(), "1970-01-01 00:00:00");
    }

    #[test]
    fn a_decimal_prints_at_its_scale() {
        let d = |unscaled, scale| Value::Decimal { unscaled, width: 18, scale }.to_string();
        assert_eq!(d(1234, 2), "12.34");
        assert_eq!(d(-1234, 2), "-12.34");
        assert_eq!(d(5, 3), "0.005");
        assert_eq!(d(-5, 3), "-0.005");
        assert_eq!(d(1234, 0), "1234");
        assert_eq!(d(1_000_000, 6), "1.000000");
    }

    #[test]
    fn a_float_keeps_the_point_that_says_it_is_one() {
        assert_eq!(Value::Double(1.0).to_string(), "1.0");
        assert_eq!(Value::Double(-3.0).to_string(), "-3.0");
        assert_eq!(Value::Double(1.5).to_string(), "1.5");
        assert_eq!(Value::Double(-0.0).to_string(), "-0.0");
        assert_eq!(Value::Float(0.5).to_string(), "0.5");
    }

    #[test]
    fn a_float_is_printed_from_its_own_width_rather_than_widened_first() {
        // 0.1f32 as an f64 is 0.10000000149011612, and printing that would be a real bug rather
        // than a rounding difference, so this is the test that pins it.
        assert_eq!(Value::Float(0.1).to_string(), "0.1");
        assert_eq!(Value::Float(1.0).to_string(), "1.0");
    }

    #[test]
    fn a_float_switches_to_an_exponent_where_duckdb_switches() {
        assert_eq!(Value::Double(1e15).to_string(), "1000000000000000.0");
        assert_eq!(Value::Double(1e16).to_string(), "1e+16");
        assert_eq!(Value::Double(1e20).to_string(), "1e+20");
        assert_eq!(Value::Double(1e-4).to_string(), "0.0001");
        assert_eq!(Value::Double(1e-5).to_string(), "1e-05");
        assert_eq!(Value::Double(1.234_567_890_123_456_8e17).to_string(), "1.2345678901234568e+17");
    }

    #[test]
    fn a_float_that_is_not_a_number_says_so_the_way_duckdb_says_it() {
        assert_eq!(Value::Double(f64::INFINITY).to_string(), "inf");
        assert_eq!(Value::Double(f64::NEG_INFINITY).to_string(), "-inf");
        assert_eq!(Value::Double(f64::NAN).to_string(), "nan");
        // A nan carries a sign bit and DuckDB prints it, per #266. Written as a negation of a nan
        // rather than as the nan an invalid operation produces, because which one of those the
        // hardware hands back is the hardware's business: x86 sets the bit on `0.0 / 0.0` and
        // aarch64 does not, and this is about the printing.
        assert_eq!(Value::Double(-f64::NAN).to_string(), "-nan");
        assert_eq!(Value::Float(-f32::NAN).to_string(), "-nan");
    }

    #[test]
    fn an_interval_keeps_months_days_and_micros_apart() {
        let i = |months, days, micros| Value::Interval { months, days, micros }.to_string();
        assert_eq!(i(14, 3, 3_723_000_000), "1 year 2 months 3 days 01:02:03");
        assert_eq!(i(1, 0, 0), "1 month");
        assert_eq!(i(0, 0, 0), "00:00:00");
        assert_eq!(i(0, 0, -1_000_000), "-00:00:01");
    }

    #[test]
    fn a_blob_escapes_what_is_not_printable() {
        assert_eq!(Value::Blob(b"ok".to_vec()).to_string(), "ok");
        assert_eq!(Value::Blob(vec![0, 1, b'a']).to_string(), "\\x00\\x01a");
        assert_eq!(Value::Blob(vec![0x7f, 0xff]).to_string(), "\\x7F\\xFF");
        // The three printable ones DuckDB escapes anyway, and the neighbours that it does not.
        assert_eq!(Value::Blob(br#"'"\"#.to_vec()).to_string(), "\\x27\\x22\\x5C");
        assert_eq!(Value::Blob(b" &`~".to_vec()).to_string(), " &`~");
    }

    #[test]
    fn a_limit_that_does_not_fit_is_none_rather_than_clamped() {
        assert_eq!(Value::Integer(5).as_i64(), Some(5));
        assert_eq!(Value::UBigInt(u64::MAX).as_i64(), None);
        assert_eq!(Value::Varchar("5".into()).as_i64(), None);
    }

    #[test]
    fn a_footprint_is_the_value_plus_what_it_owns() {
        let bare = Value::Integer(1).footprint();
        assert_eq!(bare, size_of::<Value>(), "a number owns nothing");
        assert_eq!(
            Value::Boolean(true).footprint(),
            bare,
            "the enum is one width whatever is in it"
        );
        let text = "a string long enough to be on the heap in any implementation".to_string();
        assert_eq!(Value::Varchar(text.clone()).footprint(), bare + text.capacity());
        let list = Value::List {
            element: LogicalType::Varchar,
            values: vec![Value::Varchar(text.clone())],
        };
        // The list itself, the one slot in its vector, and the bytes the string in that slot owns.
        // The slot is counted once: an element does not carry its own enum on top of the slot it
        // sits in.
        assert_eq!(list.footprint(), bare + size_of::<Value>() + text.capacity());
    }
}

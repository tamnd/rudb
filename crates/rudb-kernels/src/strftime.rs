//! `strftime`, which writes a date or a timestamp out in a format of the caller's choosing.
//!
//! The specifiers are upstream's, which are Python's with three of its own added: `%g` for the
//! milliseconds, `%n` for the nanoseconds and `%-` in front of a number to drop its padding. A
//! format is taken apart once into the literal text between the specifiers and the specifiers
//! themselves, which is what upstream does at bind time, and then written out once per row.
//!
//! The four locale ones are not locales at all upstream. `%c` is the ISO timestamp, `%x` the ISO
//! date and `%X` and `%T` the ISO time, and they are spliced in as the specifiers they stand for.

use rudb_common::{Error, LogicalType, Result, Value, civil_from_days, days_from_civil};

use rudb_vector::{Form, Vector};

/// One thing a format writes that is not literal text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Spec {
    WeekdayShort,
    WeekdayLong,
    /// Sunday is zero.
    Weekday,
    /// Monday is one and Sunday is seven.
    IsoWeekday,
    DayPadded,
    Day,
    MonthShort,
    MonthLong,
    MonthPadded,
    Month,
    ShortYearPadded,
    ShortYear,
    Year,
    IsoYear,
    HourPadded,
    Hour,
    Hour12Padded,
    Hour12,
    AmPm,
    MinutePadded,
    Minute,
    SecondPadded,
    Second,
    Micros,
    Millis,
    Nanos,
    Offset,
    ZoneName,
    DayOfYearPadded,
    DayOfYear,
    /// The week of the year with Sunday as its first day, where the days before the first Sunday
    /// are week zero.
    SundayWeek,
    /// The same with Monday as the first day.
    MondayWeek,
    IsoWeek,
}

/// A format taken apart, with one more literal than there are specifiers.
#[derive(Clone, Debug)]
pub struct Format {
    pub(crate) literals: Vec<String>,
    pub(crate) specs: Vec<Spec>,
}

/// The pieces of a moment every specifier reads from.
struct Moment {
    days: i32,
    year: i32,
    month: u32,
    day: u32,
    hour: i64,
    minute: i64,
    second: i64,
    nanos: i64,
    /// The offset and the name of the zone a `TIMESTAMPTZ` is written in, which `%z` and `%Z`
    /// write, or `None` for a moment with no zone.
    zone: Option<(i32, &'static str)>,
}

pub(crate) const WEEKDAYS: [&str; 7] =
    ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

pub(crate) const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

impl Format {
    /// Takes a format apart.
    ///
    /// # Errors
    ///
    /// The pin's invalid input error for an empty format, a `%` at the end and a letter that is not
    /// a specifier, which quotes the whole format and then the reason.
    pub fn parse(format: &str) -> Result<Self> {
        Self::parsed(format).map_err(|why| {
            Error::invalid_input(format!("Failed to parse format specifier {format}: {why}"))
        })
    }

    fn parsed(format: &str) -> std::result::Result<Self, String> {
        if format.is_empty() {
            return Err("Empty format string".to_string());
        }
        let mut literals = Vec::new();
        let mut specs = Vec::new();
        let mut literal = String::new();
        let mut chars = format.chars().peekable();
        while let Some(next) = chars.next() {
            if next != '%' {
                literal.push(next);
                continue;
            }
            let Some(letter) = chars.next() else {
                return Err("Trailing format character %".to_string());
            };
            let found = match letter {
                '%' => {
                    literal.push('%');
                    continue;
                }
                '-' if chars.peek().is_some() => {
                    let letter = chars.next().unwrap_or('-');
                    vec![match letter {
                        'd' => Spec::Day,
                        'm' => Spec::Month,
                        'y' => Spec::ShortYear,
                        'H' => Spec::Hour,
                        'I' => Spec::Hour12,
                        'M' => Spec::Minute,
                        'S' => Spec::Second,
                        'j' => Spec::DayOfYear,
                        _ => {
                            return Err(format!(
                                "Unrecognized format for strftime/strptime: %-{letter}"
                            ));
                        }
                    }]
                }
                'c' => vec![
                    Spec::Year,
                    Spec::MonthPadded,
                    Spec::DayPadded,
                    Spec::HourPadded,
                    Spec::MinutePadded,
                    Spec::SecondPadded,
                ],
                'x' => vec![Spec::Year, Spec::MonthPadded, Spec::DayPadded],
                'X' | 'T' => vec![Spec::HourPadded, Spec::MinutePadded, Spec::SecondPadded],
                _ => vec![match letter {
                    'a' => Spec::WeekdayShort,
                    'A' => Spec::WeekdayLong,
                    'w' => Spec::Weekday,
                    'u' => Spec::IsoWeekday,
                    'd' => Spec::DayPadded,
                    'h' | 'b' => Spec::MonthShort,
                    'B' => Spec::MonthLong,
                    'm' => Spec::MonthPadded,
                    'y' => Spec::ShortYearPadded,
                    'Y' => Spec::Year,
                    'G' => Spec::IsoYear,
                    'H' => Spec::HourPadded,
                    'I' => Spec::Hour12Padded,
                    'p' => Spec::AmPm,
                    'M' => Spec::MinutePadded,
                    'S' => Spec::SecondPadded,
                    'n' => Spec::Nanos,
                    'f' => Spec::Micros,
                    'g' => Spec::Millis,
                    'z' => Spec::Offset,
                    'Z' => Spec::ZoneName,
                    'j' => Spec::DayOfYearPadded,
                    'U' => Spec::SundayWeek,
                    'W' => Spec::MondayWeek,
                    'V' => Spec::IsoWeek,
                    _ => {
                        return Err(format!(
                            "Unrecognized format for strftime/strptime: %{letter}"
                        ));
                    }
                }],
            };
            // The spliced ones keep their own punctuation between the pieces.
            let between: &[&str] = match letter {
                'c' => &["-", "-", " ", ":", ":"],
                'x' => &["-", "-"],
                'X' | 'T' => &[":", ":"],
                _ => &[],
            };
            for (at, spec) in found.into_iter().enumerate() {
                if at > 0 {
                    literal.push_str(between[at - 1]);
                }
                literals.push(std::mem::take(&mut literal));
                specs.push(spec);
            }
        }
        literals.push(literal);
        Ok(Self { literals, specs })
    }

    /// Whether `%n` is in the format, which makes `strptime` answer a nanosecond timestamp.
    #[must_use]
    pub fn has_nanos(&self) -> bool {
        self.specs.contains(&Spec::Nanos)
    }

    /// Whether `%z` is in the format, which makes `strptime` answer a timestamp with a time zone.
    #[must_use]
    pub fn has_offset(&self) -> bool {
        self.specs.contains(&Spec::Offset)
    }

    /// Writes a date, a timestamp or a nanosecond timestamp out.
    ///
    /// An infinity is written the way it prints and not through the format, which is the pin's
    /// answer, so `strftime('infinity'::DATE, '%Y')` is `infinity`.
    ///
    /// # Errors
    ///
    /// If the value is not one of those, which the binder does not let through.
    pub fn write(&self, when: &Value) -> Result<Value> {
        self.write_in(when, None)
    }

    /// Writes a moment out the way [`Format::write`] does, with `zone` the offset and the zone
    /// name that a `TIMESTAMPTZ` already moved to its wall clock was read in.
    ///
    /// # Errors
    ///
    /// The same ones as [`Format::write`].
    pub fn write_in(&self, when: &Value, zone: Option<(i32, &'static str)>) -> Result<Value> {
        let (days, nanos_of_day) = match when {
            Value::Null => return Ok(Value::Null),
            Value::Date(day) if crate::datetime::infinite_day(*day) => {
                return Ok(Value::Varchar(when.to_string()));
            }
            Value::Timestamp(stamp) | Value::TimestampTz(stamp) | Value::TimestampNs(stamp)
                if crate::datetime::infinite_stamp(*stamp) =>
            {
                return Ok(Value::Varchar(
                    if *stamp > 0 { "infinity" } else { "-infinity" }.into(),
                ));
            }
            Value::Date(day) => (*day, 0),
            Value::Timestamp(micros) | Value::TimestampTz(micros) => {
                let per_day = NANOS_PER_DAY / 1_000;
                (day_count(micros.div_euclid(per_day))?, micros.rem_euclid(per_day) * 1_000)
            }
            Value::TimestampNs(nanos) => {
                (day_count(nanos.div_euclid(NANOS_PER_DAY))?, nanos.rem_euclid(NANOS_PER_DAY))
            }
            other => {
                return Err(Error::internal(format!("strftime cannot write {other:?}")));
            }
        };
        let (year, month, day) = civil_from_days(days);
        let seconds = nanos_of_day / 1_000_000_000;
        let moment = Moment {
            days,
            year,
            month,
            day,
            hour: seconds / 3_600,
            minute: seconds / 60 % 60,
            second: seconds % 60,
            nanos: nanos_of_day % 1_000_000_000,
            zone,
        };
        let mut out = String::with_capacity(32);
        for (literal, spec) in self.literals.iter().zip(&self.specs) {
            out.push_str(literal);
            moment.write(*spec, &mut out);
        }
        if let Some(last) = self.literals.last() {
            out.push_str(last);
        }
        Ok(Value::Varchar(out))
    }
}

fn day_count(days: i64) -> Result<i32> {
    i32::try_from(days).map_err(|_| Error::conversion("timestamp is outside the date range"))
}

impl Moment {
    fn write(&self, spec: Spec, out: &mut String) {
        use std::fmt::Write as _;
        // Day zero is a Thursday, so the shift that puts Sunday at zero is four.
        let weekday = (self.days + 4).rem_euclid(7);
        let day_of_year = self.days - days_from_civil(self.year, 1, 1);
        let twelve = match self.hour % 12 {
            0 => 12,
            hour => hour,
        };
        let short_year = self.year.unsigned_abs() % 100;
        let month_name = MONTHS[(self.month as usize).saturating_sub(1).min(11)];
        let weekday_name = WEEKDAYS[weekday as usize];
        let _ = match spec {
            Spec::WeekdayShort => write!(out, "{}", &weekday_name[..3]),
            Spec::WeekdayLong => write!(out, "{weekday_name}"),
            Spec::Weekday => write!(out, "{weekday}"),
            Spec::IsoWeekday => write!(out, "{}", if weekday == 0 { 7 } else { weekday }),
            Spec::DayPadded => write!(out, "{:02}", self.day),
            Spec::Day => write!(out, "{}", self.day),
            Spec::MonthShort => write!(out, "{}", &month_name[..3]),
            Spec::MonthLong => write!(out, "{month_name}"),
            Spec::MonthPadded => write!(out, "{:02}", self.month),
            Spec::Month => write!(out, "{}", self.month),
            Spec::ShortYearPadded => write!(out, "{short_year:02}"),
            Spec::ShortYear => write!(out, "{short_year}"),
            // Four digits inside the years people write and the plain number outside them, so the
            // year 5 is 0005 and the year 12345 is 12345.
            Spec::Year if (0..=9999).contains(&self.year) => write!(out, "{:04}", self.year),
            Spec::Year => write!(out, "{}", self.year),
            Spec::IsoYear => write!(out, "{:04}", crate::datetime::iso_year_of(self.days)),
            Spec::HourPadded => write!(out, "{:02}", self.hour),
            Spec::Hour => write!(out, "{}", self.hour),
            Spec::Hour12Padded => write!(out, "{twelve:02}"),
            Spec::Hour12 => write!(out, "{twelve}"),
            Spec::AmPm => write!(out, "{}", if self.hour >= 12 { "PM" } else { "AM" }),
            Spec::MinutePadded => write!(out, "{:02}", self.minute),
            Spec::Minute => write!(out, "{}", self.minute),
            Spec::SecondPadded => write!(out, "{:02}", self.second),
            Spec::Second => write!(out, "{}", self.second),
            Spec::Micros => write!(out, "{:06}", self.nanos / 1_000),
            Spec::Millis => write!(out, "{:03}", self.nanos / 1_000_000),
            Spec::Nanos => write!(out, "{:09}", self.nanos),
            // A plain timestamp is at no offset, and the pin writes that as the hours alone.
            Spec::Offset => match self.zone {
                Some((offset, _)) => write!(out, "{}", rudb_common::offset_text(offset)),
                None => write!(out, "+00"),
            },
            Spec::ZoneName => match self.zone {
                Some((_, name)) => write!(out, "{name}"),
                None => Ok(()),
            },
            Spec::DayOfYearPadded => write!(out, "{:03}", day_of_year + 1),
            Spec::DayOfYear => write!(out, "{}", day_of_year + 1),
            Spec::SundayWeek => write!(out, "{:02}", (day_of_year + 7 - weekday) / 7),
            Spec::MondayWeek => {
                write!(out, "{:02}", (day_of_year + 7 - (weekday + 6) % 7) / 7)
            }
            Spec::IsoWeek => write!(out, "{:02}", crate::datetime::iso_week_of(self.days)),
        };
    }
}

/// Which of the two arguments is the format. Upstream takes them in either order, and the format is
/// the one that is text.
fn format_at(args: &[&LogicalType]) -> usize {
    usize::from(!matches!(args.first(), Some(LogicalType::Varchar)))
}

/// `strftime` on one row.
///
/// # Errors
///
/// If the format does not parse.
pub(crate) fn value(left: &Value, right: &Value) -> Result<Value> {
    let (when, format) =
        if matches!(left, Value::Varchar(_)) { (right, left) } else { (left, right) };
    match (when, format) {
        (_, Value::Null) | (Value::Null, _) => Ok(Value::Null),
        (when, Value::Varchar(format)) => Format::parse(format)?.write(when),
        (_, other) => Err(Error::internal(format!("strftime has no format in {other:?}"))),
    }
}

/// `strftime` over a batch whose format is the same on every row, which is every one the binder
/// lets through, since upstream wants the format to be a constant. The format is taken apart once
/// for the batch and not once per row.
///
/// # Errors
///
/// If the format does not parse, or if a timestamp is outside the range of a date.
pub(crate) fn vectorized(args: &[&Vector], rows: usize) -> Result<Option<Vector>> {
    let [left, right] = args else {
        return Ok(None);
    };
    let at = format_at(&[left.logical_type(), right.logical_type()]);
    let (format, when) = if at == 0 { (left, right) } else { (right, left) };
    if format.form() != Form::Constant {
        return Ok(None);
    }
    let format = match format.try_value_at(0)? {
        Value::Varchar(format) => Format::parse(&format)?,
        _ => return Ok(Some(Vector::constant(LogicalType::Varchar, Value::Null, rows))),
    };
    let written: Vec<Value> =
        (0..rows).map(|row| format.write(&when.value_at(row))).collect::<Result<_>>()?;
    Vector::from_values(LogicalType::Varchar, &written).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(when: Value, format: &str) -> String {
        Format::parse(format).expect("a format").write(&when).expect("written").to_string()
    }

    #[test]
    fn a_date_is_written_with_every_specifier_the_pin_has() {
        let day = days_from_civil(1992, 1, 1);
        assert_eq!(
            written(
                Value::Date(day),
                "%a %A %w %u %d %-d %b %h %B %m %-m %y %-y %Y %G %V %U %W %j %-j"
            ),
            "Wed Wednesday 3 3 01 1 Jan Jan January 01 1 92 92 1992 1992 01 00 00 001 1"
        );
    }

    #[test]
    fn a_bad_format_is_refused_in_the_pins_words() {
        let error = |format: &str| Format::parse(format).unwrap_err().to_string();
        assert!(error("%Q").contains(
            "Failed to parse format specifier %Q: Unrecognized format for strftime/strptime: %Q"
        ));
        assert!(error("").contains("Failed to parse format specifier : Empty format string"));
        assert!(error("%").contains("Trailing format character %"));
        assert!(error("%-").contains("Unrecognized format for strftime/strptime: %-"));
    }
}

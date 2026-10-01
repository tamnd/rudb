//! The calls on a `TIMESTAMPTZ` that read it in the session time zone.
//!
//! The pin's ICU extension answers `hour`, `date_trunc`, `strftime`, adding an interval and the rest
//! from the wall clock an instant shows in the session zone, not from the instant in UTC. The
//! kernels in this crate read a `TIMESTAMPTZ` as the UTC wall clock, so these calls move every
//! instant to the wall clock it shows in the zone, run the same kernel on that, and move an answer
//! that is an instant again back to the instant it names.
//!
//! A session in UTC has every wall clock equal to its instant and comes here only for `strftime`,
//! which names the zone, and for `time_bucket`, `date_diff` and `date_sub`, whose kernels know only
//! a plain timestamp. That keeps the common case on the vectorized kernels.

use rudb_common::{Error, LogicalType, Result, SessionTimeZone, Value, time_tz};
use rudb_vector::Vector;

use crate::datetime::{
    MICROS_PER_DAY, MICROS_PER_SECOND, NEWEST_TIMESTAMP, OLDEST_TIMESTAMP, Part, infinite_stamp,
    shifted_stamp,
};
use crate::lists::{MAX_SERIES, Stepping, moment_steps};

/// The answer to a call that depends on the session zone, or `None` for a call that does not and
/// goes to the ordinary kernels.
///
/// # Errors
///
/// The ones the ordinary kernel reports, and a wall clock or an instant past the end of the range.
pub fn call_in_time_zone<V: AsRef<Vector>>(
    name: &str,
    args: &[V],
    returns: &LogicalType,
    zone: SessionTimeZone,
) -> Option<Result<Vector>> {
    let args: Vec<&Vector> = args.iter().map(AsRef::as_ref).collect();
    // A zone named in the call is read whatever the session zone is, and from a plain timestamp too.
    if let ("timezone", [named, when]) = (name, args.as_slice()) {
        return Some(converted(named, when, returns));
    }
    if !args.iter().any(|arg| zoned(arg)) {
        return None;
    }
    let types: Vec<&LogicalType> = args.iter().map(|arg| arg.logical_type()).collect();
    // These two have kernels that only know a plain timestamp, so they come here in any zone.
    match (name, types.as_slice()) {
        ("time_bucket", [_, _, LogicalType::Varchar]) => {
            return Some(rows(&args, returns, bucket_in));
        }
        // The other forms bucket in UTC on the pin whatever the session zone is.
        ("time_bucket", _) => {
            return Some(
                on_stamps(name, &args, &LogicalType::Timestamp, None)
                    .and_then(|stamps| to_zoned(&stamps, None)),
            );
        }
        ("date_diff" | "datediff" | "date_sub" | "datesub", [_, _, _]) => {
            return Some(counted(name, &args, returns, zone));
        }
        _ => {}
    }
    // `%Z` names the zone even in UTC, so `strftime` is the one call a UTC session still makes.
    if zone.is_utc() && name != "strftime" {
        return None;
    }
    Some(match (name, types.as_slice()) {
        ("date_part", [_, _]) => parts(&args, returns, zone),
        (
            "range" | "generate_series",
            [LogicalType::TimestampTz, LogicalType::TimestampTz, LogicalType::Interval],
        ) => {
            let inclusive = name == "generate_series";
            rows(&args, returns, |row| listed(inclusive, row, zone))
        }
        ("dayname" | "monthname" | "last_day" | "nanosecond" | "age", _) => {
            on_wall(name, &args, returns, zone)
        }
        // The format first is the core overload, which the pin answers with no zone at all.
        ("strftime", [LogicalType::TimestampTz, LogicalType::Varchar]) => {
            rows(&args, returns, |row| strftime(&row[0], &row[1], zone))
        }
        ("date_trunc", [_, LogicalType::TimestampTz]) => {
            on_wall(name, &args, returns, zone).and_then(|wall| to_instants(&wall, zone))
        }
        ("+" | "-", [LogicalType::TimestampTz, LogicalType::Interval])
        | ("+", [LogicalType::Interval, LogicalType::TimestampTz]) => {
            let subtract = name == "-";
            rows(&args, returns, |row| match (&row[0], &row[1]) {
                (Value::TimestampTz(when), Value::Interval { months, days, micros })
                | (Value::Interval { months, days, micros }, Value::TimestampTz(when)) => {
                    let sign = if subtract { -1 } else { 1 };
                    moved(*when, *months * sign, *days * sign, *micros * i64::from(sign), zone)
                        .map(Value::TimestampTz)
                }
                _ => Ok(Value::Null),
            })
        }
        ("-", [LogicalType::TimestampTz, LogicalType::TimestampTz]) => {
            rows(&args, returns, |row| match (&row[0], &row[1]) {
                (Value::TimestampTz(end), Value::TimestampTz(start)) => apart(*end, *start, zone),
                _ => Ok(Value::Null),
            })
        }
        _ => return None,
    })
}

fn zoned(vector: &Vector) -> bool {
    *vector.logical_type() == LogicalType::TimestampTz
}

/// The wall clock an instant shows in `zone`, with the two infinities kept infinite.
fn wall_of(micros: i64, zone: SessionTimeZone) -> Result<i64> {
    if infinite_stamp(micros) {
        return Ok(micros);
    }
    zone.local_of_instant(micros)
        .filter(|local| (OLDEST_TIMESTAMP..=NEWEST_TIMESTAMP).contains(local))
        .ok_or_else(|| Error::conversion("Unable to convert TIMESTAMPTZ to local TIMESTAMP"))
}

/// The instant a wall clock in `zone` names, with the two infinities kept infinite.
fn instant_of(micros: i64, zone: SessionTimeZone) -> Result<i64> {
    if infinite_stamp(micros) {
        return Ok(micros);
    }
    zone.instant_of_local(micros)
        .filter(|instant| (OLDEST_TIMESTAMP..=NEWEST_TIMESTAMP).contains(instant))
        .ok_or_else(|| Error::conversion("ICU date overflows timestamp range"))
}

/// Every `TIMESTAMPTZ` in a vector put through `move_one`, and every other vector as it is.
fn mapped(
    vector: &Vector,
    move_one: impl Fn(i64, SessionTimeZone) -> Result<i64>,
    zone: SessionTimeZone,
) -> Result<Vector> {
    let one = |value: Value| match value {
        Value::TimestampTz(micros) => move_one(micros, zone).map(Value::TimestampTz),
        other => Ok(other),
    };
    if let Some(value) = vector.constant_value() {
        return Ok(Vector::constant(
            vector.logical_type().clone(),
            one(value.clone())?,
            vector.len(),
        ));
    }
    let values = (0..vector.len())
        .map(|row| vector.try_value_at(row).and_then(one))
        .collect::<Result<Vec<_>>>()?;
    Vector::from_values(vector.logical_type().clone(), &values)
}

fn to_walls(vector: &Vector, zone: SessionTimeZone) -> Result<Vector> {
    if zoned(vector) { mapped(vector, wall_of, zone) } else { Ok(vector.clone()) }
}

fn to_instants(vector: &Vector, zone: SessionTimeZone) -> Result<Vector> {
    mapped(vector, instant_of, zone)
}

/// The ordinary kernel run on the wall clocks instead of the instants.
fn on_wall(
    name: &str,
    args: &[&Vector],
    returns: &LogicalType,
    zone: SessionTimeZone,
) -> Result<Vector> {
    let walls = args.iter().map(|arg| to_walls(arg, zone)).collect::<Result<Vec<_>>>()?;
    crate::scalar::call(name, &walls, returns, None)
}

/// A row at a time, for the calls that have no kernel of their own to hand the wall clocks to.
fn rows(
    args: &[&Vector],
    returns: &LogicalType,
    one: impl Fn(&[Value]) -> Result<Value>,
) -> Result<Vector> {
    let len = args.first().map_or(0, |arg| arg.len());
    let mut answers = Vec::with_capacity(len);
    let mut row = Vec::with_capacity(args.len());
    for at in 0..len {
        row.clear();
        for arg in args {
            row.push(arg.try_value_at(at)?);
        }
        answers.push(if row.iter().any(Value::is_null) { Value::Null } else { one(&row)? });
    }
    Vector::from_values(returns.clone(), &answers)
}

/// Every `TIMESTAMPTZ` in a vector as a plain `TIMESTAMP`, holding the wall clock in `zone` or the
/// UTC one for no zone.
fn to_stamps(vector: &Vector, zone: Option<SessionTimeZone>) -> Result<Vector> {
    if !zoned(vector) {
        return Ok(vector.clone());
    }
    let walls = match zone {
        Some(zone) => mapped(vector, wall_of, zone)?,
        None => vector.clone(),
    };
    let values = (0..walls.len())
        .map(|row| {
            walls.try_value_at(row).map(|value| match value {
                Value::TimestampTz(micros) => Value::Timestamp(micros),
                other => other,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Vector::from_values(LogicalType::Timestamp, &values)
}

/// A vector of plain timestamps back as `TIMESTAMPTZ`, each wall clock read in `zone` or as UTC.
fn to_zoned(vector: &Vector, zone: Option<SessionTimeZone>) -> Result<Vector> {
    let values = (0..vector.len())
        .map(|row| match vector.try_value_at(row)? {
            Value::Timestamp(micros) => match zone {
                Some(zone) => instant_of(micros, zone).map(Value::TimestampTz),
                None => Ok(Value::TimestampTz(micros)),
            },
            other => Ok(other),
        })
        .collect::<Result<Vec<_>>>()?;
    Vector::from_values(LogicalType::TimestampTz, &values)
}

/// The kernel for plain timestamps run on the `TIMESTAMPTZ` arguments as plain timestamps.
fn on_stamps(
    name: &str,
    args: &[&Vector],
    returns: &LogicalType,
    zone: Option<SessionTimeZone>,
) -> Result<Vector> {
    let stamps = args.iter().map(|arg| to_stamps(arg, zone)).collect::<Result<Vec<_>>>()?;
    crate::scalar::call(name, &stamps, returns, None)
}

/// Whether a part is shorter than a day, which ICU counts in elapsed time rather than on the
/// calendar.
fn elapsed_part(spelling: &str) -> bool {
    matches!(
        Part::parse(spelling),
        Ok(Part::Hour
            | Part::Minute
            | Part::Second
            | Part::Millisecond
            | Part::Microsecond
            | Part::Epoch)
    )
}

/// `date_diff` and `date_sub` of two `TIMESTAMPTZ`.
///
/// A part of a day or longer is counted on the wall clocks in the session zone. A shorter one is
/// elapsed time: `date_sub` counts it between the two instants, and `date_diff` between the
/// instants of the two wall clocks cut down to the part, which is why the day New York moves
/// forward has 23 hours in it.
fn counted(
    name: &str,
    args: &[&Vector],
    returns: &LogicalType,
    zone: SessionTimeZone,
) -> Result<Vector> {
    let Some(Value::Varchar(spelling)) = args[0].constant_value() else {
        return rows(args, returns, |row| {
            let row: Vec<Vector> = row
                .iter()
                .map(|value| Vector::constant(value.logical_type(), value.clone(), 1))
                .collect();
            let row: Vec<&Vector> = row.iter().collect();
            counted(name, &row, returns, zone)?.try_value_at(0)
        });
    };
    if !elapsed_part(spelling) {
        return on_stamps(name, args, returns, Some(zone));
    }
    if matches!(name, "date_sub" | "datesub") {
        return on_stamps(name, args, returns, None);
    }
    let cut = |when: &Vector| -> Result<Vector> {
        let truncated = on_wall("date_trunc", &[args[0], when], &LogicalType::TimestampTz, zone)?;
        to_instants(&truncated, zone)
    };
    let (start, end) = (cut(args[1])?, cut(args[2])?);
    on_stamps(name, &[args[0], &start, &end], returns, None)
}

/// `time_bucket` with a zone named in the call, which buckets in that zone from midnight on 3
/// January 2000 there, or the first of January for a width in months.
fn bucket_in(row: &[Value]) -> Result<Value> {
    let [Value::Interval { months, days, micros }, Value::TimestampTz(when), Value::Varchar(name)] =
        row
    else {
        return Ok(Value::Null);
    };
    let zone = zone_named(name)?;
    if infinite_stamp(*when) {
        return Ok(Value::TimestampTz(*when));
    }
    if *months == 0 && *days == 0 && *micros != 0 {
        let origin = instant_of(10_959 * MICROS_PER_DAY, zone)?;
        let since = when - origin;
        let buckets = since.div_euclid(*micros);
        return Ok(Value::TimestampTz(origin + buckets * micros));
    }
    let width = Vector::constant(LogicalType::Interval, row[0].clone(), 1);
    let wall = Vector::constant(LogicalType::Timestamp, Value::Timestamp(wall_of(*when, zone)?), 1);
    let bucket =
        crate::scalar::call("time_bucket", &[&width, &wall], &LogicalType::Timestamp, None)?;
    match bucket.try_value_at(0)? {
        Value::Timestamp(micros) => instant_of(micros, zone).map(Value::TimestampTz),
        other => Ok(other),
    }
}

/// `timezone(name, when)`: the wall clock an instant shows in the named zone, or the instant a wall
/// clock there names.
fn converted(named: &Vector, when: &Vector, returns: &LogicalType) -> Result<Vector> {
    let convert = |when: &Vector, zone: SessionTimeZone| {
        if zoned(when) { to_stamps(when, Some(zone)) } else { to_zoned(when, Some(zone)) }
    };
    if named.constant_value().is_some_and(Value::is_null) {
        return Ok(Vector::constant(returns.clone(), Value::Null, when.len()));
    }
    if *returns == LogicalType::TimeTz {
        return rows(&[named, when], returns, |row| moved_time(&row[0], &row[1]));
    }
    match named.constant_value() {
        Some(Value::Varchar(name)) => convert(when, zone_named(name)?),
        _ => rows(&[named, when], returns, |row| {
            let Value::Varchar(name) = &row[0] else {
                return Err(Error::internal("timezone without a zone name"));
            };
            let when = Vector::constant(row[1].logical_type(), row[1].clone(), 1);
            convert(&when, zone_named(name)?)?.try_value_at(0)
        }),
    }
}

/// A zoned time read at another offset: the zone's offset now for a zone name, and the interval's
/// microseconds for an interval, which is PostgreSQL's `timezone(INTERVAL, TIMETZ)`. The instant is
/// taken to UTC first and moved by the offset inside one day, so `12:00:00+05` in Tokyo is
/// `16:00:00+09`.
fn moved_time(named: &Value, when: &Value) -> Result<Value> {
    let Value::TimeTz(key) = when else {
        return Ok(Value::Null);
    };
    let (offset, shift) = match named {
        Value::Varchar(name) => {
            let offset = zone_named(name)?.offset_seconds_now();
            (offset, i64::from(offset) * MICROS_PER_SECOND)
        }
        // The pin moves the time by the whole interval and takes the offset from its microseconds,
        // and an offset past sixteen hours is bits it cannot hold, which it prints as garbage.
        Value::Interval { micros, .. } => {
            let offset = i32::try_from(micros / MICROS_PER_SECOND)
                .ok()
                .filter(|offset| time_tz::holds(*offset))
                .ok_or_else(|| {
                    Error::out_of_range(format!(
                        "Time zone offset of {micros} microseconds is out of range"
                    ))
                })?;
            (offset, *micros)
        }
        _ => return Ok(Value::Null),
    };
    let moved = (time_tz::at_utc(*key) + shift).rem_euclid(MICROS_PER_DAY);
    Ok(Value::TimeTz(time_tz::pack(moved, offset)))
}

/// A zone named in a call, refused the way `SET TimeZone` refuses one it does not know.
fn zone_named(name: &str) -> Result<SessionTimeZone> {
    SessionTimeZone::named(name)
        .ok_or_else(|| Error::not_implemented(format!("Unknown TimeZone '{name}'!")))
}

/// How one part of a `TIMESTAMPTZ` is read.
#[derive(Clone, Copy)]
enum Reading {
    /// From the wall clock, which is every part but these two kinds.
    Wall,
    /// From the instant, which is only `epoch`, since seconds since 1970 are the same everywhere.
    Instant,
    /// From the offset of the zone at the instant.
    Offset(Part),
}

fn reading(spelling: &str) -> Reading {
    match Part::parse(spelling) {
        Ok(Part::Epoch) => Reading::Instant,
        Ok(part @ (Part::Timezone | Part::TimezoneHour | Part::TimezoneMinute)) => {
            Reading::Offset(part)
        }
        _ => Reading::Wall,
    }
}

/// `date_part` with one part or a list of them.
fn parts(args: &[&Vector], returns: &LogicalType, zone: SessionTimeZone) -> Result<Vector> {
    let (spec, when) = (args[0], args[1]);
    match spec.constant_value() {
        Some(Value::Varchar(spelling)) => one_part(reading(spelling), args, returns, zone),
        Some(Value::List { values, .. }) => {
            let readings: Vec<Reading> = values
                .iter()
                .map(|spelling| match spelling {
                    Value::Varchar(spelling) => reading(spelling),
                    _ => Reading::Wall,
                })
                .collect();
            let wall = on_wall("date_part", args, returns, zone)?;
            let instant = crate::scalar::call("date_part", args, returns, None)?;
            let mut answers = Vec::with_capacity(when.len());
            for row in 0..when.len() {
                let (Value::Struct(mut fields), Value::Struct(instants)) =
                    (wall.try_value_at(row)?, instant.try_value_at(row)?)
                else {
                    answers.push(Value::Null);
                    continue;
                };
                let when = when.try_value_at(row)?;
                for (at, reading) in readings.iter().enumerate() {
                    match reading {
                        Reading::Wall => {}
                        Reading::Instant => fields[at].1 = instants[at].1.clone(),
                        Reading::Offset(part) => fields[at].1 = offset_of(*part, &when, zone),
                    }
                }
                answers.push(Value::Struct(fields));
            }
            Vector::from_values(returns.clone(), &answers)
        }
        // A part that changes from row to row is read one row at a time.
        _ => rows(args, returns, |row| {
            let Value::Varchar(spelling) = &row[0] else {
                return Err(Error::internal("date_part has no part"));
            };
            let row: Vec<Vector> = row
                .iter()
                .map(|value| Vector::constant(value.logical_type(), value.clone(), 1))
                .collect();
            let row: Vec<&Vector> = row.iter().collect();
            let answer = one_part(reading(spelling), &row, returns, zone)?;
            answer.try_value_at(0)
        }),
    }
}

fn one_part(
    reading: Reading,
    args: &[&Vector],
    returns: &LogicalType,
    zone: SessionTimeZone,
) -> Result<Vector> {
    match reading {
        Reading::Wall => on_wall("date_part", args, returns, zone),
        Reading::Instant => crate::scalar::call("date_part", args, returns, None),
        Reading::Offset(part) => offsets(part, args[1], returns, zone),
    }
}

/// The offset of the zone at each instant, in the unit `part` asks for.
///
/// The hours and the minutes keep the sign of the offset, so St John's at three and a half hours
/// behind is -3 hours and -30 minutes.
fn offsets(
    part: Part,
    when: &Vector,
    returns: &LogicalType,
    zone: SessionTimeZone,
) -> Result<Vector> {
    // A part that changes from row to row makes every part a double.
    rows(&[when], returns, |row| match offset_of(part, &row[0], zone) {
        Value::BigInt(offset) if *returns == LogicalType::Double => {
            Ok(Value::Double(offset as f64))
        }
        offset => Ok(offset),
    })
}

fn offset_of(part: Part, when: &Value, zone: SessionTimeZone) -> Value {
    let Value::TimestampTz(micros) = *when else { return Value::Null };
    let offset = i64::from(zone.offset_seconds_at(micros));
    Value::BigInt(match part {
        Part::TimezoneHour => offset / 3_600,
        Part::TimezoneMinute => offset / 60 % 60,
        _ => offset,
    })
}

fn strftime(when: &Value, format: &Value, zone: SessionTimeZone) -> Result<Value> {
    let (Value::TimestampTz(micros), Value::Varchar(format)) = (when, format) else {
        return Ok(Value::Null);
    };
    let wall = Value::TimestampTz(wall_of(*micros, zone)?);
    let offset = zone.offset_seconds_at(*micros);
    crate::strftime::Format::parse(format)?.write_in(&wall, Some((offset, zone.name())))
}

/// The series of instants `range` and `generate_series` walk from `start` toward `end`, each one the
/// last moved by the interval in `zone`, so a step of a day keeps the wall clock across the spring
/// change and a step of an hour does not.
///
/// A step of neither sign is an empty series and one of both signs is refused, which are the pin's
/// answers for the list form. The table form refuses both in its own words before it gets here.
///
/// # Errors
///
/// An infinite bound, a step of mixed signs, and a series longer than a list can hold.
pub fn zoned_steps(
    inclusive: bool,
    start: i64,
    end: i64,
    (months, days, micros): (i32, i32, i64),
    zone: SessionTimeZone,
) -> Result<Stepping> {
    let forward = months > 0 || days > 0 || micros > 0;
    let backward = months < 0 || days < 0 || micros < 0;
    if forward && backward {
        return Err(Error::invalid_input(
            "Interval with mix of negative/positive entries not supported",
        ));
    }
    if infinite_stamp(start) || infinite_stamp(end) {
        return Err(Error::invalid_input("Interval infinite bounds not supported"));
    }
    if zone.is_utc() || (months == 0 && days == 0) {
        return moment_steps(inclusive, start, end, (months, days, micros));
    }
    let mut stamps = Vec::new();
    let mut at = start;
    // A step that is all months and days is never zero here, so the walk always moves.
    loop {
        let past = if forward { at > end } else { at < end };
        if past || (at == end && !inclusive) {
            return Ok(Stepping::Listed(stamps));
        }
        if stamps.len() == MAX_SERIES {
            return Err(Error::invalid_input("Lists larger than 2^32 elements are not supported"));
        }
        let next = moved(at, months, days, micros, zone)?;
        stamps.push(at);
        at = next;
    }
}

/// `range` or `generate_series` of two `TIMESTAMPTZ` and an interval as a list, stepped in `zone`.
fn listed(inclusive: bool, row: &[Value], zone: SessionTimeZone) -> Result<Value> {
    let [
        Value::TimestampTz(start),
        Value::TimestampTz(end),
        Value::Interval { months, days, micros },
    ] = row
    else {
        return Err(Error::internal("a zoned range without two instants and an interval"));
    };
    let stamps = match zoned_steps(inclusive, *start, *end, (*months, *days, *micros), zone)? {
        Stepping::Listed(stamps) => stamps,
        Stepping::Even { start, step, count } => {
            let steps = i64::try_from(count).unwrap_or(i64::MAX);
            (0..steps).map(|at| start.wrapping_add(at.wrapping_mul(step))).collect()
        }
    };
    Ok(Value::List {
        element: LogicalType::TimestampTz,
        values: stamps.into_iter().map(Value::TimestampTz).collect(),
    })
}

/// An instant moved by an interval the way an ICU calendar adds one.
///
/// The months and the days are calendar fields, so they move the wall clock and a day across the
/// spring change is 23 hours. The rest is elapsed time and moves the instant. A forward move takes
/// the calendar fields first and a backward one takes them last, which is the order PostgreSQL adds
/// them in and the pin copies.
fn moved(when: i64, months: i32, days: i32, micros: i64, zone: SessionTimeZone) -> Result<i64> {
    if infinite_stamp(when) {
        return Ok(when);
    }
    let elapsed = |instant: i64| {
        instant
            .checked_add(micros)
            .filter(|moved| (OLDEST_TIMESTAMP..=NEWEST_TIMESTAMP).contains(moved))
            .ok_or_else(|| Error::conversion("ICU date overflows timestamp range"))
    };
    let calendar = |instant: i64| -> Result<i64> {
        if months == 0 && days == 0 {
            return Ok(instant);
        }
        let wall = wall_of(instant, zone)?;
        instant_of(shifted_stamp(wall, i64::from(months), i64::from(days), 0)?, zone)
    };
    if months < 0 || days < 0 || micros < 0 {
        calendar(elapsed(when)?)
    } else {
        elapsed(calendar(when)?)
    }
}

/// `end - start` the way an ICU calendar measures it: the whole calendar days that fit from the
/// start, and then the elapsed time that is left, with no months.
fn apart(end: i64, start: i64, zone: SessionTimeZone) -> Result<Value> {
    if infinite_stamp(end) || infinite_stamp(start) {
        return Err(Error::invalid_input("Cannot subtract infinite timestamps"));
    }
    if start > end {
        let Value::Interval { months, days, micros } = apart(start, end, zone)? else {
            unreachable!("apart answers an interval");
        };
        return Ok(Value::Interval { months: -months, days: -days, micros: -micros });
    }
    let wall = wall_of(start, zone)?;
    let after = |days: i64| instant_of(shifted_stamp(wall, 0, days, 0)?, zone);
    let mut days = (wall_of(end, zone)? - wall).div_euclid(MICROS_PER_DAY).max(0);
    while days > 0 && after(days)? > end {
        days -= 1;
    }
    while after(days + 1).is_ok_and(|next| next <= end) {
        days += 1;
    }
    let micros = end - after(days)?;
    let days = i32::try_from(days).map_err(|_| Error::conversion("Interval value out of range"))?;
    Ok(Value::Interval { months: 0, days, micros })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_york() -> SessionTimeZone {
        SessionTimeZone::named("America/New_York").expect("a zone")
    }

    /// 2020-03-07 12:00 in New York, the day before the clocks go forward.
    const SATURDAY_NOON: i64 = 1_583_600_400_000_000;
    const HOUR: i64 = 3_600_000_000;

    #[test]
    fn a_day_across_the_spring_change_is_twenty_three_hours() {
        let zone = new_york();
        assert_eq!(moved(SATURDAY_NOON, 0, 1, 0, zone).unwrap(), SATURDAY_NOON + 23 * HOUR);
        assert_eq!(moved(SATURDAY_NOON, 0, 0, 24 * HOUR, zone).unwrap(), SATURDAY_NOON + 24 * HOUR);
        assert_eq!(moved(SATURDAY_NOON, 0, 1, HOUR, zone).unwrap(), SATURDAY_NOON + 24 * HOUR);
    }

    #[test]
    fn a_gap_counts_calendar_days_and_then_elapsed_time() {
        let zone = new_york();
        let monday_noon = SATURDAY_NOON + 47 * HOUR;
        assert_eq!(
            apart(monday_noon, SATURDAY_NOON, zone).unwrap(),
            Value::Interval { months: 0, days: 2, micros: 0 }
        );
        assert_eq!(
            apart(SATURDAY_NOON, monday_noon, zone).unwrap(),
            Value::Interval { months: 0, days: -2, micros: 0 }
        );
        assert!(apart(i64::MAX, SATURDAY_NOON, zone).is_err());
    }
}

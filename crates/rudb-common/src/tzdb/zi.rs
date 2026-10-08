//! The reading of `tzdata.zi`, as `infile` and the functions it calls in `zic.c` read it.
//!
//! The file is the source that the pin compiles into its zone files. A line is a rule, a zone, a
//! continuation of the zone above it, or a link. The pin's data has no leap second lines, so they
//! are not read.

/// `ZIC_MIN` and `ZIC_MAX`: the years `minimum` and `maximum`, and the times before and after all
/// others.
pub(super) const MIN: i64 = i64::MIN;
pub(super) const MAX: i64 = i64::MAX;

pub(super) const SECSPERDAY: i64 = 86_400;
pub(super) const YEARSPERREPEAT: i64 = 400;
/// `SECSPERREPEAT`: 400 years of 365.2425 days.
pub(super) const SECSPERREPEAT: i64 = YEARSPERREPEAT * 31_556_952;
pub(super) const EPOCH_YEAR: i64 = 1970;
/// `EPOCH_WDAY`: 1970-01-01 is a Thursday.
const EPOCH_WDAY: i64 = 4;

/// The kind of the day of a rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DayCode {
    /// `DC_DOM`: a day of the month, such as `5`.
    Dom,
    /// `DC_DOWGEQ`: the first weekday on or after a day, such as `Sun>=8`.
    DowGeq,
    /// `DC_DOWLEQ`: the last weekday on or before a day, such as `lastSun` or `Sun<=25`.
    DowLeq,
}

/// `struct rule`: a line `R NAME FROM TO - IN ON AT SAVE LETTER/S`, or the time an era of a zone
/// ends.
#[derive(Clone, Debug)]
pub(super) struct Rule {
    pub name: &'static str,
    pub loyear: i64,
    pub hiyear: i64,
    pub lowasnum: bool,
    pub hiwasnum: bool,
    /// The month, from 0 for January.
    pub month: usize,
    pub dycode: DayCode,
    pub dayofmonth: i64,
    /// The weekday, from 0 for Sunday.
    pub wday: i64,
    pub tod: i64,
    pub todisstd: bool,
    pub todisut: bool,
    pub save: i64,
    pub isdst: bool,
    pub abbrvar: &'static str,
}

/// `struct zone`: one line of a zone, which is one era of its history.
#[derive(Clone, Debug)]
pub(super) struct Era {
    pub stdoff: i64,
    /// The rule field: the name of the rules, empty for `-`, or a saved time.
    pub rule: &'static str,
    /// The format of the abbreviation, with a `%z` written as `%s`.
    pub format: String,
    /// The letter after the `%` of the format, if it has one.
    pub specifier: Option<u8>,
    /// The time the era ends and how it is written, for an era that is not the last.
    pub until: Option<(Rule, i64)>,
    /// The rules the era follows, a range of [`Data::rules`].
    pub rules: std::ops::Range<usize>,
    /// The saved time and the daylight saving flag of an era that names no rules.
    pub save: i64,
    pub isdst: bool,
}

/// A zone and the eras of its history.
#[derive(Debug)]
pub(super) struct Zone {
    pub name: &'static str,
    pub eras: Vec<Era>,
}

/// What `tzdata.zi` holds.
#[derive(Debug)]
pub(super) struct Data {
    /// The rules, sorted by name as `associate` sorts them.
    pub rules: Vec<Rule>,
    pub zones: Vec<Zone>,
    /// The links: the target and the name of the link.
    pub links: Vec<(&'static str, &'static str)>,
}

const MONTHS: [&str; 12] = [
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

const WEEKDAYS: [&str; 7] =
    ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/// `len_months`: the days of each month in a common and in a leap year.
pub(super) const LEN_MONTHS: [[i64; 12]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];

pub(super) fn isleap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn len_year(year: i64) -> i64 {
    if isleap(year) { 366 } else { 365 }
}

/// `ciprefix`: whether `abbr` starts `word`, without regard to case.
fn ciprefix(abbr: &str, word: &str) -> bool {
    word.len() >= abbr.len() && word.as_bytes()[..abbr.len()].eq_ignore_ascii_case(abbr.as_bytes())
}

/// `byword`: the index of the word of the table that `word` spells in full or starts, without
/// regard to case. A start that two words share is no match.
fn byword(word: &str, table: &[&str]) -> Option<usize> {
    if let Some(found) = table.iter().position(|entry| entry.eq_ignore_ascii_case(word)) {
        return Some(found);
    }
    let mut found = None;
    for (at, entry) in table.iter().enumerate() {
        if ciprefix(word, entry) {
            if found.is_some() {
                return None;
            }
            found = Some(at);
        }
    }
    found
}

/// `sscanf("%d%c")` that must read the whole field: a number with an optional sign.
fn integer(text: &str) -> Option<i64> {
    let text = text.trim_start();
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// `gethms`: seconds from `[-]h[:mm[:ss[.fraction]]]`, with the fraction rounded to the nearest
/// second and a half rounded to even. An empty field is 0.
fn gethms(text: &str) -> Option<i64> {
    if text.is_empty() {
        return Some(0);
    }
    let (sign, text) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text),
    };
    let (clock, fraction) = match text.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (text, None),
    };
    let mut parts = clock.split(':');
    let hh = integer(parts.next()?)?;
    let mm = parts.next().map_or(Some(0), integer)?;
    let mut ss = parts.next().map_or(Some(0), integer)?;
    if parts.next().is_some() || hh < 0 || !(0..60).contains(&mm) || !(0..=60).contains(&ss) {
        return None;
    }
    if let Some(fraction) = fraction {
        if clock.matches(':').count() != 2 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let tenths = fraction.bytes().next().map_or(0, |b| i64::from(b - b'0'));
        let exact = fraction.bytes().skip(1).all(|b| b == b'0');
        ss += i64::from(5 + ((ss ^ 1) & i64::from(exact)) <= tenths);
    }
    Some(sign * (hh * 3600 + mm * 60 + ss))
}

/// `getsave`: the saved time of a rule or an era, and whether it is daylight saving time. A
/// trailing `d` or `s` says which, and otherwise a time that is not zero is daylight saving time.
fn getsave(field: &str) -> Option<(i64, bool)> {
    let (field, dst) = match field.as_bytes().last() {
        Some(b'd') => (&field[..field.len() - 1], Some(true)),
        Some(b's') => (&field[..field.len() - 1], Some(false)),
        _ => (field, None),
    };
    let save = gethms(field)?;
    Some((save, dst.unwrap_or(save != 0)))
}

/// `rulesub`: the years, the day and the time of day of a rule.
#[allow(clippy::too_many_arguments)]
fn rulesub(
    name: &'static str,
    loyear: &str,
    hiyear: &str,
    month: &str,
    day: &str,
    time: &str,
    save: (i64, bool),
    abbrvar: &'static str,
) -> Option<Rule> {
    let month = byword(month, &MONTHS)?;
    let (time, todisstd, todisut) = match time.as_bytes().last().map(u8::to_ascii_lowercase) {
        Some(b's') => (&time[..time.len() - 1], true, false),
        Some(b'w') => (&time[..time.len() - 1], false, false),
        Some(b'g' | b'u' | b'z') => (&time[..time.len() - 1], true, true),
        _ => (time, false, false),
    };
    let tod = gethms(time)?;
    let (lo, lowasnum) = match byword(loyear, &["minimum", "maximum"]) {
        Some(0) => (MIN, false),
        Some(_) => (MAX, false),
        None => (integer(loyear)?, true),
    };
    let (hi, hiwasnum) = match byword(hiyear, &["minimum", "maximum", "only"]) {
        Some(0) => (MIN, false),
        Some(1) => (MAX, false),
        Some(_) => (lo, false),
        None => (integer(hiyear)?, true),
    };
    if lo > hi {
        return None;
    }
    let longest = LEN_MONTHS[1][month];
    let (dycode, wday, dayofmonth) = if let Some(rest) =
        day.get(..4).filter(|start| start.eq_ignore_ascii_case("last")).map(|_| &day[4..])
        && !rest.is_empty()
        && !rest.starts_with('-')
    {
        (DayCode::DowLeq, byword(rest, &WEEKDAYS)? as i64, longest)
    } else {
        let (dycode, weekday, number) = if let Some((weekday, number)) = day.split_once('<') {
            (DayCode::DowLeq, Some(weekday), number.strip_prefix('=')?)
        } else if let Some((weekday, number)) = day.split_once('>') {
            (DayCode::DowGeq, Some(weekday), number.strip_prefix('=')?)
        } else {
            (DayCode::Dom, None, day)
        };
        let wday = match weekday {
            Some(weekday) => byword(weekday, &WEEKDAYS)? as i64,
            None => 0,
        };
        let number = integer(number)?;
        if number <= 0 || number > longest {
            return None;
        }
        (dycode, wday, number)
    };
    Some(Rule {
        name,
        loyear: lo,
        hiyear: hi,
        lowasnum,
        hiwasnum,
        month,
        dycode,
        dayofmonth,
        wday,
        tod,
        todisstd,
        todisut,
        save: save.0,
        isdst: save.1,
        abbrvar,
    })
}

/// `oadd`, which stops at the ends of the `i64` where `zic` reports an overflow.
pub(super) fn oadd(t1: i64, t2: i64) -> i64 {
    t1.saturating_add(t2)
}

/// `tadd`: a time plus seconds, where the time before or after all others stays where it is.
pub(super) fn tadd(t1: i64, t2: i64) -> i64 {
    if (t1 == MIN && t2 < 0) || (t1 == MAX && t2 > 0) {
        return t1;
    }
    t1.saturating_add(t2)
}

/// `rpytime`: the time the rule names in a year, in seconds since 1970-01-01 00:00 of the clock
/// the rule is written in.
pub(super) fn rpytime(rule: &Rule, wanted: i64) -> i64 {
    if wanted == MIN {
        return MIN;
    }
    if wanted == MAX {
        return MAX;
    }
    let mut wanted = wanted;
    let mut dayoff: i64 = 0;
    let mut y = EPOCH_YEAR;
    if y < wanted {
        wanted -= y;
        dayoff = (wanted / YEARSPERREPEAT) * (SECSPERREPEAT / SECSPERDAY);
        wanted %= YEARSPERREPEAT;
        wanted += y;
    } else if wanted < 0 {
        dayoff = (wanted / YEARSPERREPEAT) * (SECSPERREPEAT / SECSPERDAY);
        wanted %= YEARSPERREPEAT;
    }
    while wanted != y {
        if wanted > y {
            dayoff = oadd(dayoff, len_year(y));
            y += 1;
        } else {
            y -= 1;
            dayoff = oadd(dayoff, -len_year(y));
        }
    }
    let leap = usize::from(isleap(y));
    for &len in &LEN_MONTHS[leap][..rule.month] {
        dayoff = oadd(dayoff, len);
    }
    let mut i = rule.dayofmonth;
    if rule.month == 1 && i == 29 && leap == 0 && rule.dycode == DayCode::DowLeq {
        i -= 1;
    }
    dayoff = oadd(dayoff, i - 1);
    if rule.dycode != DayCode::Dom {
        let mut wday = (EPOCH_WDAY + dayoff).rem_euclid(7);
        while wday != rule.wday {
            if rule.dycode == DayCode::DowGeq {
                dayoff = oadd(dayoff, 1);
                wday = (wday + 1) % 7;
            } else {
                dayoff = oadd(dayoff, -1);
                wday = (wday + 6) % 7;
            }
        }
    }
    if dayoff < MIN / SECSPERDAY {
        return MIN;
    }
    if dayoff > MAX / SECSPERDAY {
        return MAX;
    }
    tadd(dayoff * SECSPERDAY, rule.tod)
}

/// `getfields`: the fields of a line, split at white space and ended by a `#`, where a field can
/// quote white space in double quotes. A `-` field is empty.
fn getfields(line: &'static str) -> Vec<&'static str> {
    let mut fields = Vec::new();
    for field in line.split('#').next().unwrap_or_default().split_ascii_whitespace() {
        fields.push(if field == "-" { "" } else { field.trim_matches('"') });
    }
    fields
}

/// `infile` and `associate`: the rules, the zones and the links of the file.
pub(super) fn read(text: &'static str) -> Data {
    let mut rules = Vec::new();
    let mut zones: Vec<Zone> = Vec::new();
    let mut links = Vec::new();
    let mut wantcont = false;
    for line in text.lines() {
        let fields = getfields(line);
        if fields.is_empty() {
            continue;
        }
        if wantcont {
            if let Some(zone) = zones.last_mut() {
                wantcont = era(&fields, zone);
            }
            continue;
        }
        match byword(fields[0], &["Rule", "Zone", "Link"]) {
            Some(0) if fields.len() == 10 => {
                let Some(save) = getsave(fields[8]) else { continue };
                if let Some(rule) = rulesub(
                    fields[1], fields[2], fields[3], fields[5], fields[6], fields[7], save,
                    fields[9],
                ) {
                    rules.push(rule);
                }
            }
            Some(1) if fields.len() >= 5 => {
                let mut zone = Zone { name: fields[1], eras: Vec::new() };
                wantcont = era(&fields[2..], &mut zone);
                zones.push(zone);
            }
            Some(2) if fields.len() == 3 => links.push((fields[1], fields[2])),
            _ => {}
        }
    }
    // `qsort` with `rcomp` keeps no order among the rules of one name, and the order of the
    // file is the one the pin's build gets from the sort of glibc, which is stable.
    rules.sort_by_key(|rule: &Rule| rule.name);
    for zone in &mut zones {
        for era in &mut zone.eras {
            let start = rules.partition_point(|rule| rule.name < era.rule);
            let end = rules.partition_point(|rule| rule.name <= era.rule);
            era.rules = start..end;
            if start == end {
                (era.save, era.isdst) = getsave(era.rule).unwrap_or((0, false));
            }
        }
    }
    Data { rules, zones, links }
}

/// `inzsub`: an era of a zone, from the fields after the name. Whether a continuation line
/// follows, which is when the era has an end.
fn era(fields: &[&'static str], zone: &mut Zone) -> bool {
    if fields.len() < 3 || fields.len() > 7 {
        return false;
    }
    let Some(stdoff) = gethms(fields[0]) else { return false };
    let raw = fields[2];
    let specifier = raw.find('%').and_then(|at| raw.as_bytes().get(at + 1).copied());
    let format = match specifier {
        Some(b'z') => raw.replacen("%z", "%s", 1),
        _ => raw.to_owned(),
    };
    let until = (fields.len() > 3)
        .then(|| {
            let month = fields.get(4).copied().unwrap_or("Jan");
            let day = fields.get(5).copied().unwrap_or("1");
            let time = fields.get(6).copied().unwrap_or("0");
            rulesub("", fields[3], "only", month, day, time, (0, false), "")
        })
        .flatten()
        .map(|rule| {
            let time = rpytime(&rule, rule.loyear);
            (rule, time)
        });
    let more = until.is_some();
    zone.eras.push(Era {
        stdoff,
        rule: fields[1],
        format,
        specifier,
        until,
        rules: 0..0,
        save: 0,
        isdst: false,
    });
    more
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_saved_times_read_as_zic_reads_them() {
        assert_eq!(gethms("2:00"), Some(7200));
        assert_eq!(gethms("-0:25:21"), Some(-1521));
        assert_eq!(gethms("24"), Some(86_400));
        assert_eq!(gethms("0:00:30.5"), Some(30));
        assert_eq!(gethms("0:00:31.5"), Some(32));
        assert_eq!(gethms("0:00:30.51"), Some(31));
        assert_eq!(gethms(""), Some(0));
        assert_eq!(getsave("1"), Some((3600, true)));
        assert_eq!(getsave("-1"), Some((-3600, true)));
        assert_eq!(getsave("0:30s"), Some((1800, false)));
        assert_eq!(getsave("0d"), Some((0, true)));
    }

    #[test]
    fn words_match_a_unique_start() {
        assert_eq!(byword("Ja", &MONTHS), Some(0));
        assert_eq!(byword("Ma", &MONTHS), None);
        assert_eq!(byword("mar", &MONTHS), Some(2));
        assert_eq!(byword("o", &["minimum", "maximum", "only"]), Some(2));
        assert_eq!(byword("R", &["Rule", "Zone", "Link"]), Some(0));
    }

    #[test]
    fn a_rule_names_its_day_in_each_year() {
        let rule = rulesub("US", "2007", "ma", "Mar", "Sun>=8", "2", (3600, true), "D").unwrap();
        // 2026-03-08 02:00.
        assert_eq!(rpytime(&rule, 2026), 1_772_935_200);
        let last = rulesub("EU", "1996", "ma", "O", "lastSu", "1u", (0, false), "").unwrap();
        assert!(last.todisut && last.todisstd);
        // 2026-10-25 01:00.
        assert_eq!(rpytime(&last, 2026), 1_792_890_000);
        assert_eq!(rpytime(&last, 1800), -5_338_911_600);
    }
}

//! The reading of `tzdata.zi`, as `infile` and the functions it calls in `zic.c` read it.
//!
//! The file is the source that the pin compiles into its zone files. A line is a rule, a zone, a
//! continuation of the zone above it, or a link. The pin's data has no leap second lines, so they
//! are not read.
//!
//! The file is read in two steps. [`find`] finds the line a name's zone starts on, and [`zone`]
//! reads the eras of the zone and the rules they follow when the zone is compiled. A session
//! compiles one or two of the six hundred zones of a release, and reading them all took 20 million
//! instructions a release, more than answering `SELECT COUNT(*)` over ten million rows. Keeping
//! where every zone and rule starts took a fifth of a megabyte of every process, so a lookup reads
//! the lines again instead, which is a few hundred microseconds once for each name. Only a listing
//! of every zone, which [`read`] gives, keeps them all.

use std::ops::Range;

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
    /// The rules the era follows, a range of the rules [`zone`] reads with the eras.
    pub rules: Range<usize>,
    /// The saved time and the daylight saving flag of an era that names no rules.
    pub save: i64,
    pub isdst: bool,
}

/// What `tzdata.zi` holds, with the zones and the rules still as their lines.
#[derive(Debug)]
pub(super) struct Data {
    /// The name of each zone and the text of the file from the zone's line on.
    pub zones: Vec<(&'static str, &'static str)>,
    /// The links: the target and the name of the link.
    pub links: Vec<(&'static str, &'static str)>,
    /// The name of each rule and its line, in the order of the file.
    pub rules: Vec<(&'static str, &'static str)>,
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
fn rulesub(
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
    Fields(line).collect()
}

/// The fields of a line as `getfields` splits it, found one at a time, so that a scan reads no
/// further into a line than the fields it needs.
struct Fields(&'static str);

impl Iterator for Fields {
    type Item = &'static str;

    fn next(&mut self) -> Option<&'static str> {
        let bytes = self.0.as_bytes();
        let start = bytes.iter().position(|b| !b.is_ascii_whitespace())?;
        if bytes[start] == b'#' {
            self.0 = "";
            return None;
        }
        let end = bytes[start..]
            .iter()
            .position(|&b| b.is_ascii_whitespace() || b == b'#')
            .map_or(bytes.len(), |length| start + length);
        let field = &self.0[start..end];
        self.0 = &self.0[end..];
        Some(match field.as_bytes() {
            b"-" => "",
            [b'"', ..] | [.., b'"'] => field.trim_matches('"'),
            _ => field,
        })
    }
}

/// A line of the file that is a rule, a zone or a link.
enum Line {
    /// The fields of a rule after its word, which start with its name, and its line. A scan that
    /// wants no rules splits no more of the line.
    Rule(Fields, &'static str),
    /// The name of a zone and the text of the file from the zone's line on.
    Zone(&'static str, &'static str),
    /// The target and the name of a link.
    Link(&'static str, &'static str),
}

/// `infile`, as far as telling the lines apart: the rules, the zones and the links of the file, in
/// its order.
///
/// A line is told by its first word alone. `infile` reads the line after an era with an end as
/// the era's continuation whatever its first word is, but `zic` refuses one that is a rule, a zone
/// or a link, so in data that compiles the word says the same. The fields of a rule are counted
/// when a zone reads the rule.
fn lines(text: &'static str) -> impl Iterator<Item = Line> {
    text.lines().filter_map(move |line| {
        // Only a word that starts one of the three, or is quoted, can be one of them, and most of
        // the file is the continuation lines of zones. Telling those by their first byte, before
        // any fields are split, is what makes reading the file again for a lookup cheap.
        let first = line.bytes().find(|byte| !byte.is_ascii_whitespace())?;
        if !matches!(first.to_ascii_lowercase(), b'r' | b'z' | b'l' | b'"') {
            return None;
        }
        let mut fields = Fields(line);
        // The file writes the words as their first letters, which `byword` takes as well.
        let word = match fields.next()? {
            "R" => 0,
            "Z" => 1,
            "L" => 2,
            word => byword(word, &["Rule", "Zone", "Link"])?,
        };
        match word {
            0 => Some(Line::Rule(fields, line)),
            1 => {
                let name = fields.next().unwrap_or_default();
                (fields.count() >= 3).then(|| {
                    let at = line.as_ptr() as usize - text.as_ptr() as usize;
                    Line::Zone(name, &text[at..])
                })
            }
            _ => match (fields.next(), fields.next(), fields.next()) {
                (Some(target), Some(name), None) => Some(Line::Link(target, name)),
                _ => None,
            },
        }
    })
}

/// The name of a rule and its line. `inrule` refuses a rule without a name, which no zone could
/// follow.
fn rule(mut fields: Fields, line: &'static str) -> Option<(&'static str, &'static str)> {
    fields.next().filter(|name| !name.is_empty()).map(|name| (name, line))
}

/// Every zone, link and rule of the file, for a listing of every zone.
pub(super) fn read(text: &'static str) -> Data {
    let mut zones = Vec::new();
    let mut links = Vec::new();
    let mut rules = Vec::new();
    for line in lines(text) {
        match line {
            Line::Zone(name, at) => zones.push((name, at)),
            Line::Link(target, name) => links.push((target, name)),
            Line::Rule(fields, line) => rules.extend(rule(fields, line)),
        }
    }
    Data { zones, links, rules }
}

/// The zone file a name finds without regard to case, as `pg_open_tzfile` finds it among a file
/// for each zone and each link: the name of the file and the text of its zone from the zone's line
/// on.
///
/// The files are listed in the order of their names, a zone before a link of the same name, and
/// the last that matches is the one found. That is the name last in byte order, and of the zones
/// and links that share it the last link of the file and then the last zone. A link is to the last
/// zone of its target, and a link to no zone is no file.
pub(super) fn find(text: &'static str, name: &str) -> Option<(&'static str, &'static str)> {
    // Every zone, so that a link finds its target without reading the file again.
    let mut zones = Vec::new();
    let mut matches = Vec::new();
    for (at, line) in lines(text).enumerate() {
        match line {
            Line::Zone(zone, from) => {
                zones.push((zone, from));
                if zone.eq_ignore_ascii_case(name) {
                    matches.push((zone, false, at, from));
                }
            }
            Line::Link(target, link) if link.eq_ignore_ascii_case(name) => {
                matches.push((link, true, at, target));
            }
            _ => {}
        }
    }
    matches.sort_unstable_by_key(|&(name, link, at, _)| (name, link, at));
    matches.into_iter().rev().find_map(|(name, link, _, found)| {
        if !link {
            return Some((name, found));
        }
        let &(_, zone) = zones.iter().rev().find(|&&(zone, _)| zone == found)?;
        Some((name, zone))
    })
}

/// `inzone`, `inzcont` and `associate` for the zone whose line starts `at`, a part of `text`: its
/// eras, and the rules they follow, sorted by name as `associate` sorts them.
///
/// The rules are read from `rules_of` when a listing of every zone has them, and from the lines of
/// `text` otherwise. A listing compiles every zone, and each one reading the file again took a
/// second.
pub(super) fn zone(
    text: &'static str,
    at: &'static str,
    rules_of: Option<&[(&'static str, &'static str)]>,
) -> (Vec<Era>, Vec<Rule>) {
    let mut lines_at = at.lines();
    let mut eras = Vec::new();
    let first = getfields(lines_at.next().unwrap_or_default());
    let mut wantcont = era(&first[2..], &mut eras);
    for line in lines_at {
        if !wantcont {
            break;
        }
        let fields = getfields(line);
        if !fields.is_empty() {
            wantcont = era(&fields, &mut eras);
        }
    }
    // The lines of each name of rules the eras follow, read in one pass over the file when no
    // listing has them. `qsort` with `rcomp` keeps no order among the rules of one name, and the
    // order of the file is the one the pin's build gets from the sort of glibc, which is stable.
    let mut named: Vec<(&str, Vec<&'static str>)> = Vec::new();
    for era in &eras {
        if !named.iter().any(|(name, _)| *name == era.rule) {
            named.push((era.rule, Vec::new()));
        }
    }
    let listed = rules_of.map(|listed| listed.iter().copied());
    // A rule has a name, so an era that follows none reads no lines.
    let wanted = named.iter().any(|(name, _)| !name.is_empty());
    let scanned = (rules_of.is_none() && wanted).then(|| {
        lines(text).filter_map(|line| match line {
            Line::Rule(fields, line) => rule(fields, line),
            _ => None,
        })
    });
    for (name, line) in listed.into_iter().flatten().chain(scanned.into_iter().flatten()) {
        if let Some((_, lines)) = named.iter_mut().find(|(rule, _)| *rule == name) {
            lines.push(line);
        }
    }
    let mut rules = Vec::new();
    let mut read = Vec::with_capacity(named.len());
    for (name, lines) in named {
        let start = rules.len();
        for line in lines {
            let fields = getfields(line);
            if fields.len() != 10 {
                continue;
            }
            let Some(save) = getsave(fields[8]) else { continue };
            rules.extend(rulesub(
                fields[2], fields[3], fields[5], fields[6], fields[7], save, fields[9],
            ));
        }
        read.push((name, start..rules.len()));
    }
    for era in &mut eras {
        let (_, range) = read.iter().find(|(name, _)| *name == era.rule).unwrap_or(&("", 0..0));
        if range.is_empty() {
            (era.save, era.isdst) = getsave(era.rule).unwrap_or((0, false));
        }
        era.rules = range.clone();
    }
    (eras, rules)
}

/// `inzsub`: an era of a zone, from the fields after the name. Whether a continuation line
/// follows, which is when the era has an end.
fn era(fields: &[&'static str], eras: &mut Vec<Era>) -> bool {
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
            rulesub(fields[3], "only", month, day, time, (0, false), "")
        })
        .flatten()
        .map(|rule| {
            let time = rpytime(&rule, rule.loyear);
            (rule, time)
        });
    let more = until.is_some();
    eras.push(Era {
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
        let rule = rulesub("2007", "ma", "Mar", "Sun>=8", "2", (3600, true), "D").unwrap();
        // 2026-03-08 02:00.
        assert_eq!(rpytime(&rule, 2026), 1_772_935_200);
        let last = rulesub("1996", "ma", "O", "lastSu", "1u", (0, false), "").unwrap();
        assert!(last.todisut && last.todisstd);
        // 2026-10-25 01:00.
        assert_eq!(rpytime(&last, 2026), 1_792_890_000);
        assert_eq!(rpytime(&last, 1800), -5_338_911_600);
    }
}

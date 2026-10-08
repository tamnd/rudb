//! The rules of a zone as `localtime.c` of the pin loads and reads them: `tzload` with the POSIX
//! string at the end of a zone file, `tzparse` for a POSIX zone such as `EST5EDT,M3.2.0,M11.1.0`,
//! and the lookups `localsub`, `pg_next_dst_boundary`, `pg_interpret_timezone_abbrev` and
//! `pg_timezone_abbrev_is_known`.

use super::zi::{SECSPERDAY, SECSPERREPEAT, YEARSPERREPEAT, isleap};
use super::zic::{cstr, find_chars};

/// `TZ_MAX_TIMES`, `TZ_MAX_TYPES` and `TZ_MAX_CHARS` of `tzfile.h`.
const TZ_MAX_TIMES: usize = 2000;
const TZ_MAX_TYPES: usize = 256;
const TZ_MAX_CHARS: usize = 50;
/// The size of `chars` of `struct state`, which a POSIX zone fills.
const STATE_CHARS: usize = 512;
/// `TZDEFRULESTRING`: the rules of a POSIX zone that names daylight saving time and no rules.
const TZDEFRULESTRING: &str = ",M3.2.0,M11.1.0";
/// `AVGSECSPERYEAR`.
const AVGSECSPERYEAR: i64 = SECSPERREPEAT / YEARSPERREPEAT;

/// `struct ttinfo`: a local time type. The flags of standard and universal time are always false
/// in the pin's slim zone files and in a POSIX zone, so they are not kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TtInfo {
    /// Seconds east of UTC.
    pub utoff: i32,
    pub isdst: bool,
    /// The offset of the abbreviation in the buffer of abbreviations.
    pub desig: usize,
}

/// The content of a zone file: the transitions, the types, the abbreviations and the POSIX string.
pub(super) struct Tzif {
    pub ats: Vec<i64>,
    pub types: Vec<u8>,
    pub ttis: Vec<TtInfo>,
    pub chars: Vec<u8>,
    pub footer: String,
}

/// `struct state`: the transitions of a zone, the local time types they move to, and the
/// abbreviations.
#[derive(Debug)]
pub struct State {
    ats: Vec<i64>,
    types: Vec<u8>,
    ttis: Vec<TtInfo>,
    chars: Vec<u8>,
    defaulttype: usize,
    goback: bool,
    goahead: bool,
}

/// The local time type of a zone at an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Period<'a> {
    /// Seconds east of UTC.
    pub offset: i32,
    pub dst: bool,
    pub abbrev: &'a str,
}

/// What `pg_next_dst_boundary` finds after an instant: the offset east of UTC and the daylight
/// saving flag in effect, and the next transition with the offset and the flag after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boundary {
    pub before: (i32, bool),
    pub next: Option<(i64, i32, bool)>,
}

/// The rule of a POSIX string, as `getrule` reads it.
#[derive(Clone, Copy)]
enum PosixRule {
    /// `Jn`: the day of the year from 1, where February 29 is never counted.
    Julian(i64, i64),
    /// `n`: the day of the year from 0.
    DayOfYear(i64, i64),
    /// `Mm.n.d`: the day `d` of the week `n` of the month `m`.
    MonthWeekDay(i64, i64, i64, i64),
}

impl PosixRule {
    fn time(self) -> i64 {
        match self {
            PosixRule::Julian(_, time)
            | PosixRule::DayOfYear(_, time)
            | PosixRule::MonthWeekDay(.., time) => time,
        }
    }
}

/// A reader of a POSIX string, which holds what is left of it.
struct Posix<'a>(&'a [u8]);

impl Posix<'_> {
    fn peek(&self) -> u8 {
        self.0.first().copied().unwrap_or(0)
    }

    fn bump(&mut self) {
        self.0 = &self.0[1.min(self.0.len())..];
    }

    /// `getzname`: an abbreviation up to a digit, a comma, a sign or the end.
    fn zname(&mut self) -> Vec<u8> {
        let end = self
            .0
            .iter()
            .position(|&c| c.is_ascii_digit() || matches!(c, b',' | b'-' | b'+'))
            .unwrap_or(self.0.len());
        let name = self.0[..end].to_vec();
        self.0 = &self.0[end..];
        name
    }

    /// `getqzname` after the `<`: an abbreviation up to the `>`, which is passed.
    fn qzname(&mut self) -> Option<Vec<u8>> {
        let end = self.0.iter().position(|&c| c == b'>')?;
        let name = self.0[..end].to_vec();
        self.0 = &self.0[end + 1..];
        Some(name)
    }

    /// An abbreviation in angle brackets or without them.
    fn name(&mut self) -> Option<Vec<u8>> {
        if self.peek() == b'<' {
            self.bump();
            self.qzname()
        } else {
            Some(self.zname())
        }
    }

    /// `getnum`.
    fn num(&mut self, min: i64, max: i64) -> Option<i64> {
        if !self.peek().is_ascii_digit() {
            return None;
        }
        let mut num = 0;
        while self.peek().is_ascii_digit() {
            num = num * 10 + i64::from(self.peek() - b'0');
            if num > max {
                return None;
            }
            self.bump();
        }
        (num >= min).then_some(num)
    }

    /// `getsecs`: `hh[:mm[:ss]]`, where the hours go up to a week.
    fn secs(&mut self) -> Option<i64> {
        let mut secs = self.num(0, 24 * 7 - 1)? * 3600;
        if self.peek() == b':' {
            self.bump();
            secs += self.num(0, 59)? * 60;
            if self.peek() == b':' {
                self.bump();
                secs += self.num(0, 60)?;
            }
        }
        Some(secs)
    }

    /// `getoffset`: `[+-]hh[:mm[:ss]]`.
    fn offset(&mut self) -> Option<i64> {
        let negative = self.peek() == b'-';
        if matches!(self.peek(), b'-' | b'+') {
            self.bump();
        }
        let secs = self.secs()?;
        Some(if negative { -secs } else { secs })
    }

    /// `getrule`: a date and an optional `/time`, where the time is 02:00 by default.
    fn rule(&mut self) -> Option<PosixRule> {
        let kind = self.peek();
        let date = match kind {
            b'J' => {
                self.bump();
                (self.num(1, 365)?, 0, 0)
            }
            b'M' => {
                self.bump();
                let month = self.num(1, 12)?;
                (self.peek() == b'.').then_some(())?;
                self.bump();
                let week = self.num(1, 5)?;
                (self.peek() == b'.').then_some(())?;
                self.bump();
                (month, week, self.num(0, 6)?)
            }
            c if c.is_ascii_digit() => (self.num(0, 365)?, 0, 0),
            _ => return None,
        };
        let time = if self.peek() == b'/' {
            self.bump();
            self.offset()?
        } else {
            2 * 3600
        };
        Some(match kind {
            b'J' => PosixRule::Julian(date.0, time),
            b'M' => PosixRule::MonthWeekDay(date.0, date.1, date.2, time),
            _ => PosixRule::DayOfYear(date.0, time),
        })
    }
}

/// `mon_lengths` and `year_lengths`.
const MON_LENGTHS: [[i64; 12]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];

fn year_secs(year: i64) -> i64 {
    if isleap(year) { 366 * SECSPERDAY } else { 365 * SECSPERDAY }
}

/// `transtime`: the seconds from the start of the year in UTC to the time a rule takes effect,
/// where `offset` is the offset west of UTC in effect when it does.
fn transtime(year: i64, rule: PosixRule, offset: i64) -> i64 {
    let leap = usize::from(isleap(year));
    let value = match rule {
        PosixRule::Julian(day, _) => {
            (day - 1) * SECSPERDAY + if leap == 1 && day >= 60 { SECSPERDAY } else { 0 }
        }
        PosixRule::DayOfYear(day, _) => day * SECSPERDAY,
        PosixRule::MonthWeekDay(month, week, day, _) => {
            // Zeller's congruence for the weekday of the first day of the month.
            let m1 = (month + 9) % 12 + 1;
            let yy0 = if month <= 2 { year - 1 } else { year };
            let (yy1, yy2) = (yy0 / 100, yy0 % 100);
            let mut dow = ((26 * m1 - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
            if dow < 0 {
                dow += 7;
            }
            let mut d = day - dow;
            if d < 0 {
                d += 7;
            }
            let length = MON_LENGTHS[leap][(month - 1) as usize];
            for _ in 1..week {
                if d + 7 >= length {
                    break;
                }
                d += 7;
            }
            d * SECSPERDAY
                + MON_LENGTHS[leap][..(month - 1) as usize].iter().sum::<i64>() * SECSPERDAY
        }
    };
    value + rule.time() + offset
}

impl State {
    /// `tzparse`: the state of a POSIX string. `lastditch` reads the whole string as the name of
    /// a zone at UTC, as the pin reads `GMT`.
    pub(super) fn parse(text: &str, lastditch: bool) -> Option<State> {
        let mut p = Posix(text.as_bytes());
        let (stdname, stdoffset) = if lastditch {
            (text.as_bytes().to_vec(), 0)
        } else {
            let name = p.name()?;
            // An empty name of standard time is allowed, unlike IANA.
            if p.0.is_empty() {
                return None;
            }
            (name, p.offset()?)
        };
        if lastditch {
            p.0 = &[];
        }
        let mut charcnt = stdname.len() + 1;
        if charcnt > STATE_CHARS {
            return None;
        }
        let mut chars = stdname.clone();
        chars.push(0);
        let mut state = State {
            ats: Vec::new(),
            types: Vec::new(),
            ttis: vec![TtInfo { utoff: -stdoffset as i32, isdst: false, desig: 0 }],
            chars: Vec::new(),
            defaulttype: 0,
            goback: false,
            goahead: false,
        };
        if !p.0.is_empty() {
            let dstname = p.name()?;
            if dstname.is_empty() {
                return None;
            }
            charcnt += dstname.len() + 1;
            if charcnt > STATE_CHARS {
                return None;
            }
            let dstoffset =
                if !matches!(p.peek(), 0 | b',' | b';') { p.offset()? } else { stdoffset - 3600 };
            if p.0.is_empty() {
                p.0 = TZDEFRULESTRING.as_bytes();
            }
            if !matches!(p.peek(), b',' | b';') {
                return None;
            }
            p.bump();
            let start = p.rule()?;
            (p.peek() == b',').then_some(())?;
            p.bump();
            let end = p.rule()?;
            if !p.0.is_empty() {
                return None;
            }
            state.ttis = vec![
                TtInfo { utoff: -stdoffset as i32, isdst: false, desig: 0 },
                TtInfo { utoff: -dstoffset as i32, isdst: true, desig: stdname.len() + 1 },
            ];
            // Two transitions a year from 1770, 200 years before 1970, for at least 401 years.
            let mut janfirst: i64 = 0;
            let mut yearbeg: i64 = 1970;
            loop {
                let secs = year_secs(yearbeg - 1);
                yearbeg -= 1;
                janfirst -= secs;
                if 1970 - YEARSPERREPEAT / 2 >= yearbeg {
                    break;
                }
            }
            let mut yearlim = yearbeg + YEARSPERREPEAT + 1;
            let mut year = yearbeg;
            while year < yearlim {
                let mut starttime = transtime(year, start, stdoffset);
                let mut endtime = transtime(year, end, dstoffset);
                let secs = year_secs(year);
                let reversed = endtime < starttime;
                if reversed {
                    std::mem::swap(&mut starttime, &mut endtime);
                }
                if reversed
                    || (starttime < endtime && endtime - starttime < secs + (stdoffset - dstoffset))
                {
                    if TZ_MAX_TIMES - 2 < state.ats.len() {
                        break;
                    }
                    state.ats.push(janfirst + starttime);
                    state.types.push(u8::from(!reversed));
                    state.ats.push(janfirst + endtime);
                    state.types.push(u8::from(reversed));
                    yearlim = year + YEARSPERREPEAT + 1;
                }
                janfirst += secs;
                year += 1;
            }
            if state.ats.is_empty() {
                // Daylight saving time for good.
                state.ttis = vec![state.ttis[1]];
            } else if YEARSPERREPEAT < year - yearbeg {
                state.goback = true;
                state.goahead = true;
            }
            chars.extend_from_slice(&dstname);
            chars.push(0);
        }
        state.chars = chars;
        Some(state)
    }

    /// `tzloadbody` after the read of the file: the POSIX string adds its transitions after the
    /// last one of the file, then the default type is inferred. `None` for a file that the pin
    /// cannot load.
    pub(super) fn load(file: Tzif) -> Option<State> {
        let Tzif { mut ats, mut types, mut ttis, mut chars, footer } = file;
        if ats.len() > TZ_MAX_TIMES
            || ttis.is_empty()
            || ttis.len() > TZ_MAX_TYPES
            || chars.len() > TZ_MAX_CHARS
        {
            return None;
        }
        if !footer.is_empty()
            && ttis.len() + 2 <= TZ_MAX_TYPES
            && let Some(mut ts) = State::parse(&footer, false)
        {
            // Reuse the abbreviations the file has, and add the others while they fit.
            let mut gotabbr = 0;
            let mut grown = chars.clone();
            for tti in &mut ts.ttis {
                let abbr = cstr(&ts.chars, tti.desig).to_vec();
                if let Some(j) = find_chars(&grown, &abbr) {
                    tti.desig = j;
                    gotabbr += 1;
                } else if grown.len() + abbr.len() < TZ_MAX_CHARS {
                    tti.desig = grown.len();
                    grown.extend_from_slice(&abbr);
                    grown.push(0);
                    gotabbr += 1;
                }
            }
            if gotabbr == ts.ttis.len() {
                chars = grown;
                // Drop the trailing transitions that change nothing.
                while 1 < types.len() && types[types.len() - 1] == types[types.len() - 2] {
                    types.pop();
                    ats.pop();
                }
                let first = ts
                    .ats
                    .iter()
                    .position(|&at| ats.last().is_none_or(|&last| last < at))
                    .unwrap_or(ts.ats.len());
                let base = ttis.len() as u8;
                for (at, kind) in ts.ats.iter().zip(&ts.types).skip(first) {
                    if ats.len() >= TZ_MAX_TIMES {
                        break;
                    }
                    ats.push(*at);
                    types.push(base + kind);
                }
                ttis.extend(ts.ttis);
            }
        }
        let mut state =
            State { ats, types, ttis, chars, defaulttype: 0, goback: false, goahead: false };
        let n = state.ats.len();
        if n > 1 {
            state.goback = (1..n).any(|i| {
                state.typesequiv(state.types[i], state.types[0])
                    && state.ats[i] - state.ats[0] == SECSPERREPEAT
            });
            state.goahead = (0..n - 1).rev().any(|i| {
                state.typesequiv(state.types[n - 1], state.types[i])
                    && state.ats[n - 1] - state.ats[i] == SECSPERREPEAT
            });
        }
        state.defaulttype = state.infer_defaulttype();
        Some(state)
    }

    /// `typesequiv`.
    fn typesequiv(&self, a: u8, b: u8) -> bool {
        let (a, b) = (&self.ttis[usize::from(a)], &self.ttis[usize::from(b)]);
        a.utoff == b.utoff
            && a.isdst == b.isdst
            && cstr(&self.chars, a.desig) == cstr(&self.chars, b.desig)
    }

    /// The default type of `tzloadbody`: type 0 when no transition uses it, which is so for the
    /// data of recent releases, and otherwise the heuristics for older data.
    fn infer_defaulttype(&self) -> usize {
        if !self.types.contains(&0) {
            return 0;
        }
        if let Some(&first) = self.types.first()
            && self.ttis[usize::from(first)].isdst
            && let Some(found) = (0..usize::from(first)).rev().find(|&i| !self.ttis[i].isdst)
        {
            return found;
        }
        (0..self.ttis.len()).find(|&i| !self.ttis[i].isdst).unwrap_or(0)
    }

    fn period(&self, kind: usize) -> Period<'_> {
        let tti = &self.ttis[kind];
        let abbrev = std::str::from_utf8(cstr(&self.chars, tti.desig)).unwrap_or_default();
        Period { offset: tti.utoff, dst: tti.isdst, abbrev }
    }

    /// The instant moved by whole cycles of 400 years into the transitions, for an instant
    /// before them in a zone that goes back or after them in a zone that goes ahead, as
    /// `localsub` and `pg_next_dst_boundary` move it. The moved instant and the seconds it moved.
    fn cycled(&self, t: i64) -> Option<(i64, i64)> {
        let (first, last) = (*self.ats.first()?, *self.ats.last()?);
        if !((self.goback && t < first) || (self.goahead && t > last)) {
            return None;
        }
        let seconds = if t < first { first.checked_sub(t)? } else { t.checked_sub(last)? } - 1;
        let moved = (seconds / SECSPERREPEAT + 1).checked_mul(YEARSPERREPEAT * AVGSECSPERYEAR)?;
        let newt = if t < first { t.checked_add(moved)? } else { t.checked_sub(moved)? };
        (first..=last).contains(&newt).then_some((newt, if t < first { -moved } else { moved }))
    }

    /// `localsub`: the local time type at an instant in seconds since 1970-01-01 UTC.
    #[must_use]
    pub fn at(&self, t: i64) -> Period<'_> {
        let t = self.cycled(t).map_or(t, |(newt, _)| newt);
        if self.ats.is_empty() || t < self.ats[0] {
            return self.period(self.defaulttype);
        }
        let lo = self.ats[1..].partition_point(|&at| at <= t) + 1;
        self.period(usize::from(self.types[lo - 1]))
    }

    /// `pg_next_dst_boundary`: the offset and the flag at an instant, and the next transition
    /// after it.
    #[must_use]
    pub fn next_boundary(&self, t: i64) -> Boundary {
        let info = |kind: usize| (self.ttis[kind].utoff, self.ttis[kind].isdst);
        let n = self.ats.len();
        if n == 0 {
            return Boundary { before: info(self.defaulttype), next: None };
        }
        if let Some((newt, moved)) = self.cycled(t) {
            let mut found = self.next_boundary(newt);
            // A change past the end of the range of an instant is no change.
            found.next = found.next.and_then(|(boundary, offset, isdst)| {
                Some((boundary.checked_add(moved)?, offset, isdst))
            });
            return found;
        }
        if t >= self.ats[n - 1] {
            return Boundary { before: info(usize::from(self.types[n - 1])), next: None };
        }
        if t < self.ats[0] {
            let (utoff, isdst) = info(usize::from(self.types[0]));
            return Boundary {
                before: info(self.defaulttype),
                next: Some((self.ats[0], utoff, isdst)),
            };
        }
        let i = self.ats[1..n - 1].partition_point(|&at| at <= t) + 1;
        let (utoff, isdst) = info(usize::from(self.types[i]));
        Boundary {
            before: info(usize::from(self.types[i - 1])),
            next: Some((self.ats[i], utoff, isdst)),
        }
    }

    /// The offset east of UTC and the next change after an instant, in the form that
    /// [`determine_offset`] reads.
    #[must_use]
    pub fn next_change(&self, t: i64) -> (i32, Option<(i64, i32)>) {
        let found = self.next_boundary(t);
        (found.before.0, found.next.map(|(boundary, offset, _)| (boundary, offset)))
    }

    /// The offset east of UTC of a wall clock in seconds since 1970-01-01, as
    /// [`determine_offset`] finds it.
    #[must_use]
    pub fn local_offset(&self, local: i64) -> i32 {
        determine_offset(local, |t| self.next_change(t))
    }

    /// The offset of an abbreviation as the walk over the whole strings of the buffer finds it,
    /// so an abbreviation that only ends another one is not found.
    fn abbrind(&self, abbrev: &str) -> Option<usize> {
        let mut at = 0;
        while at < self.chars.len() {
            let here = cstr(&self.chars, at);
            if here == abbrev.as_bytes() {
                return Some(at);
            }
            at += here.len() + 1;
        }
        None
    }

    /// `pg_interpret_timezone_abbrev`: the offset and the flag of the last use of an abbreviation
    /// at or before an instant, else of the default type, else of the first use after it.
    #[must_use]
    pub fn interpret_abbrev(&self, abbrev: &str, t: i64) -> Option<(i32, bool)> {
        let abbrind = self.abbrind(abbrev)?;
        let uses = |i: &usize| self.ttis[usize::from(self.types[*i])].desig == abbrind;
        let meaning = |kind: usize| (self.ttis[kind].utoff, self.ttis[kind].isdst);
        let cutoff = self.ats.partition_point(|&at| at <= t);
        if let Some(i) = (0..cutoff).rev().find(uses) {
            return Some(meaning(usize::from(self.types[i])));
        }
        if self.ttis[self.defaulttype].desig == abbrind {
            return Some(meaning(self.defaulttype));
        }
        (cutoff..self.ats.len()).find(uses).map(|i| meaning(usize::from(self.types[i])))
    }

    /// `pg_timezone_abbrev_is_known`: for an abbreviation of the zone, its offset and flag when
    /// all its types agree on them, or `Some(None)` when they do not.
    #[must_use]
    pub fn abbrev_is_known(&self, abbrev: &str) -> Option<Option<(i32, bool)>> {
        let abbrind = self.abbrind(abbrev)?;
        let mut found: Option<(i32, bool)> = None;
        for tti in self.ttis.iter().filter(|tti| tti.desig == abbrind) {
            match found {
                None => found = Some((tti.utoff, tti.isdst)),
                Some(first) if first != (tti.utoff, tti.isdst) => return Some(None),
                Some(_) => {}
            }
        }
        found.map(Some)
    }

    /// `pg_get_next_timezone_abbrev`: the abbreviations of the zone in the order of the buffer.
    pub fn abbrevs(&self) -> impl Iterator<Item = &str> {
        let mut at = 0;
        std::iter::from_fn(move || {
            if at >= self.chars.len() {
                return None;
            }
            let here = cstr(&self.chars, at);
            at += here.len() + 1;
            Some(std::str::from_utf8(here).unwrap_or_default())
        })
    }

    /// `pg_get_timezone_offset`: the offset of a zone whose types all have one offset.
    #[must_use]
    pub fn fixed_offset(&self) -> Option<i32> {
        let first = self.ttis[0].utoff;
        self.ttis.iter().all(|tti| tti.utoff == first).then_some(first)
    }
}

/// `DetermineTimeZoneOffsetInternal`: the offset east of UTC of a wall clock in seconds since
/// 1970-01-01, from the offset a day before it and the first change after that, which
/// `next_change` gives as `pg_next_dst_boundary` does. A wall clock that the clocks skipped over
/// has the offset from before the change, and a wall clock that the clocks passed twice has the
/// offset from after the change.
pub fn determine_offset(
    local: i64,
    next_change: impl FnOnce(i64) -> (i32, Option<(i64, i32)>),
) -> i32 {
    let (before, change) = next_change(local.saturating_sub(SECSPERDAY));
    let Some((boundary, after)) = change else {
        return before;
    };
    let before_time = local.saturating_sub(i64::from(before));
    let after_time = local.saturating_sub(i64::from(after));
    if before_time < boundary && after_time < boundary {
        return before;
    }
    if before_time > boundary && after_time >= boundary {
        return after;
    }
    if before_time > after_time { before } else { after }
}

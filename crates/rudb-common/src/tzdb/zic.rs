//! The compilation of a zone into the data of its zone file, as `outzone`, `stringzone` and
//! `writezone` in `zic.c` make it. The pin runs `zic` in its default slim mode, which lists the
//! transitions up to the point where the POSIX string at the end of the file takes over.

use super::zi::{
    DayCode, EPOCH_YEAR, Era, LEN_MONTHS, MAX, MIN, Rule, SECSPERDAY, YEARSPERREPEAT, oadd,
    rpytime, tadd,
};
use super::{TtInfo, Tzif};

/// `struct attype`: a transition while the zone is compiled.
#[derive(Clone, Copy)]
struct AtType {
    at: i64,
    dontmerge: bool,
    kind: usize,
}

/// The tables that `zic` fills for one zone.
#[derive(Default)]
struct Out {
    attypes: Vec<AtType>,
    utoffs: Vec<i64>,
    isdsts: Vec<bool>,
    desigidx: Vec<usize>,
    /// The abbreviations, each ended by a NUL.
    chars: Vec<u8>,
}

/// The string at an offset of a buffer of strings that each end in a NUL.
pub(super) fn cstr(chars: &[u8], at: usize) -> &[u8] {
    let rest = &chars[at.min(chars.len())..];
    &rest[..rest.iter().position(|&b| b == 0).unwrap_or(rest.len())]
}

/// The offset of the first string of the buffer that equals `abbr`, where a string can start
/// inside another one, as the `strcmp` loops of `zic` and `localtime.c` find it.
pub(super) fn find_chars(chars: &[u8], abbr: &[u8]) -> Option<usize> {
    (0..chars.len()).find(|&j| cstr(chars, j) == abbr)
}

impl Out {
    fn addtt(&mut self, at: i64, kind: usize) {
        self.attypes.push(AtType { at, dontmerge: false, kind });
    }

    /// `addtype`, where the slim mode drops the flags of standard and universal time.
    fn addtype(&mut self, utoff: i64, abbr: &str, isdst: bool) -> usize {
        let j = match find_chars(&self.chars, abbr.as_bytes()) {
            Some(j) => {
                let found = (0..self.utoffs.len()).find(|&i| {
                    self.utoffs[i] == utoff && self.isdsts[i] == isdst && self.desigidx[i] == j
                });
                if let Some(found) = found {
                    return found;
                }
                j
            }
            None => {
                let j = self.chars.len();
                self.chars.extend_from_slice(abbr.as_bytes());
                self.chars.push(0);
                j
            }
        };
        self.utoffs.push(utoff);
        self.isdsts.push(isdst);
        self.desigidx.push(j);
        self.utoffs.len() - 1
    }
}

/// `abbroffset`: the offset for `%z`, such as `+05`, `-0330` or `+054508`.
fn abbroffset(offset: i64) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let offset = offset.abs();
    let (hours, minutes, seconds) = (offset / 3600, offset / 60 % 60, offset % 60);
    if hours >= 100 {
        return "%z".to_owned();
    }
    let mut out = format!("{sign}{hours:02}");
    if minutes != 0 || seconds != 0 {
        out.push_str(&format!("{minutes:02}"));
        if seconds != 0 {
            out.push_str(&format!("{seconds:02}"));
        }
    }
    out
}

/// `doabbr`: the abbreviation of an era with the letters of a rule. `doquotes` puts one that is
/// not all letters in angle brackets, as a POSIX string needs it.
fn doabbr(era: &Era, letters: Option<&str>, isdst: bool, save: i64, doquotes: bool) -> String {
    let format = era.format.as_str();
    let abbr = match format.find('/') {
        None => {
            let offset;
            let letters = if era.specifier == Some(b'z') {
                offset = abbroffset(era.stdoff + save);
                offset.as_str()
            } else {
                letters.unwrap_or("%s")
            };
            format.replacen("%s", letters, 1)
        }
        Some(slash) if isdst => format[slash + 1..].to_owned(),
        Some(slash) => format[..slash].to_owned(),
    };
    if !doquotes || (!abbr.is_empty() && abbr.bytes().all(|b| b.is_ascii_alphabetic())) {
        return abbr;
    }
    format!("<{abbr}>")
}

/// `stringoffset`: an offset of a POSIX string, such as `5`, `-5:30` or `0:25:21`. `None` for
/// an offset of a week or more.
fn stringoffset(offset: i64) -> Option<String> {
    let negative = offset < 0;
    let offset = offset.abs();
    let (hours, minutes, seconds) = (offset / 3600, offset / 60 % 60, offset % 60);
    if hours >= 24 * 7 {
        return None;
    }
    let mut out = if negative { format!("-{hours}") } else { format!("{hours}") };
    if minutes != 0 || seconds != 0 {
        out.push_str(&format!(":{minutes:02}"));
        if seconds != 0 {
            out.push_str(&format!(":{seconds:02}"));
        }
    }
    Some(out)
}

/// `stringrule`: the date and the time of a rule in a POSIX string, and the year of the version
/// of the POSIX string that it needs. `None` for a rule that a POSIX string cannot say.
fn stringrule(rule: &Rule, save: i64, stdoff: i64) -> Option<(String, i32)> {
    let mut tod = rule.tod;
    let mut compat = 0;
    let mut out = if rule.dycode == DayCode::Dom {
        if rule.dayofmonth == 29 && rule.month == 1 {
            return None;
        }
        let total: i64 = LEN_MONTHS[0][..rule.month].iter().sum();
        if rule.month <= 1 {
            format!("{}", total + rule.dayofmonth - 1)
        } else {
            format!("J{}", total + rule.dayofmonth)
        }
    } else {
        let mut wday = rule.wday;
        let week = if rule.dycode == DayCode::DowGeq {
            let wdayoff = (rule.dayofmonth - 1) % 7;
            if wdayoff != 0 {
                compat = 2013;
            }
            wday -= wdayoff;
            tod += wdayoff * SECSPERDAY;
            1 + (rule.dayofmonth - 1) / 7
        } else if rule.dayofmonth == LEN_MONTHS[1][rule.month] {
            5
        } else {
            let wdayoff = rule.dayofmonth % 7;
            if wdayoff != 0 {
                compat = 2013;
            }
            wday -= wdayoff;
            tod += wdayoff * SECSPERDAY;
            rule.dayofmonth / 7
        };
        if wday < 0 {
            wday += 7;
        }
        format!("M{}.{week}.{wday}", rule.month + 1)
    };
    if rule.todisut {
        tod += stdoff;
    }
    if rule.todisstd && !rule.isdst {
        tod += save;
    }
    if tod != 2 * 3600 {
        out.push('/');
        out.push_str(&stringoffset(tod)?);
        if tod < 0 {
            compat = compat.max(2013);
        } else if tod >= SECSPERDAY {
            compat = compat.max(1994);
        }
    }
    Some((out, compat))
}

/// `rule_cmp`: rules ordered by their last year, then by their month and day.
fn rule_cmp(a: Option<&Rule>, b: Option<&Rule>) -> i64 {
    match (a, b) {
        (None, b) => -i64::from(b.is_some()),
        (Some(_), None) => 1,
        (Some(a), Some(b)) if a.hiyear != b.hiyear => {
            if a.hiyear < b.hiyear {
                -1
            } else {
                1
            }
        }
        (Some(a), Some(b)) if a.month != b.month => a.month as i64 - b.month as i64,
        (Some(a), Some(b)) => a.dayofmonth - b.dayofmonth,
    }
}

/// A rule that only the POSIX string of a zone in permanent daylight saving time uses.
fn made_rule(
    month: usize,
    dayofmonth: i64,
    tod: i64,
    save: i64,
    isdst: bool,
    abbrvar: &'static str,
) -> Rule {
    Rule {
        name: "",
        loyear: 0,
        hiyear: 0,
        lowasnum: false,
        hiwasnum: false,
        month,
        dycode: DayCode::Dom,
        dayofmonth,
        wday: 0,
        tod,
        todisstd: false,
        todisut: false,
        save,
        isdst,
        abbrvar,
    }
}

/// `stringzone`: the POSIX string of the times after the last transition, and the year of the
/// version of the POSIX string it needs, or `None` when no POSIX string says the zone's rules.
fn stringzone(eras: &[Era], rules: &[Rule]) -> Option<(String, i32)> {
    let era = eras.last()?;
    let own = &rules[era.rules.clone()];
    let mut stdrp: Option<&Rule> = None;
    let mut dstrp: Option<&Rule> = None;
    for rule in own {
        if rule.hiwasnum || rule.hiyear != MAX {
            continue;
        }
        let slot = if rule.isdst { &mut dstrp } else { &mut stdrp };
        if slot.is_some() {
            return None;
        }
        *slot = Some(rule);
    }
    let (made_std, made_dst);
    if stdrp.is_none() && dstrp.is_none() {
        // No rule runs to the maximum: the latest rule is the last one, and the latest rule of
        // standard time gives the letters.
        let mut stdabbrrp: Option<&Rule> = None;
        for rule in own {
            if !rule.isdst && rule_cmp(stdabbrrp, Some(rule)) < 0 {
                stdabbrrp = Some(rule);
            }
            if rule_cmp(stdrp, Some(rule)) < 0 {
                stdrp = Some(rule);
            }
        }
        if let Some(last) = stdrp
            && last.isdst
        {
            // Daylight saving time for good.
            made_dst = made_rule(0, 1, 0, last.save, last.isdst, last.abbrvar);
            let letters = stdabbrrp.map_or("", |rule| rule.abbrvar);
            made_std = made_rule(11, 31, SECSPERDAY + last.save, 0, false, letters);
            dstrp = Some(&made_dst);
            stdrp = Some(&made_std);
        }
    }
    if stdrp.is_none() && (!own.is_empty() || era.isdst) {
        return None;
    }
    let mut compat = 0;
    let mut out = doabbr(era, Some(stdrp.map_or("", |rule| rule.abbrvar)), false, 0, true);
    out.push_str(&stringoffset(-era.stdoff)?);
    let (Some(dstrp), Some(stdrp)) = (dstrp, stdrp) else {
        return Some((out, compat));
    };
    out.push_str(&doabbr(era, Some(dstrp.abbrvar), dstrp.isdst, dstrp.save, true));
    if dstrp.save != 3600 {
        out.push_str(&stringoffset(-(era.stdoff + dstrp.save))?);
    }
    for rule in [dstrp, stdrp] {
        let (text, needs) = stringrule(rule, dstrp.save, era.stdoff)?;
        out.push(',');
        out.push_str(&text);
        compat = compat.max(needs);
    }
    Some((out, compat))
}

/// `outzone` and `writezone`: the zone file of the eras of a zone.
pub(super) fn compile(eras: &[Era], rules: &[Rule]) -> Tzif {
    let mut out = Out::default();
    let zonecount = eras.len();
    let (mut min_year, mut max_year) = (EPOCH_YEAR, EPOCH_YEAR);
    let mut updateminmax = |x: i64| {
        min_year = min_year.min(x);
        max_year = max_year.max(x);
    };
    let mut prodstic = zonecount == 1;
    for (i, era) in eras.iter().enumerate() {
        if i < zonecount - 1
            && let Some((until, _)) = &era.until
        {
            updateminmax(until.loyear);
        }
        for rule in &rules[era.rules.clone()] {
            if rule.lowasnum {
                updateminmax(rule.loyear);
            }
            if rule.hiwasnum {
                updateminmax(rule.hiyear);
            }
            if rule.lowasnum || rule.hiwasnum {
                prodstic = false;
            }
        }
    }
    let footer = stringzone(eras, rules);
    let do_extend = footer.is_none();
    if do_extend {
        // Two years past the 400 of a cycle, for the edge cases `zic` describes.
        let years_of_observations = YEARSPERREPEAT + 2;
        min_year = min_year.checked_sub(years_of_observations).unwrap_or(MIN);
        max_year = max_year.checked_add(years_of_observations).unwrap_or(MAX);
        if prodstic {
            min_year = 1900;
            max_year = min_year + years_of_observations;
        }
    }
    let max_year0 = max_year;
    let y2038_boundary: i64 = 1 << 31;
    let mut defaulttype: Option<usize> = None;
    let mut lastatmax: Option<usize> = None;
    let mut starttime = 0;
    let mut todo: Vec<bool> = Vec::new();
    let mut temp: Vec<i64> = Vec::new();
    for (i, zp) in eras.iter().enumerate() {
        let own = &rules[zp.rules.clone()];
        let mut prevrp: Option<&Rule> = None;
        let mut save: i64 = 0;
        let untiltime_of = |era: &Era| era.until.as_ref().map_or(MAX, |(_, time)| *time);
        let mut usestart = i > 0 && untiltime_of(&eras[i - 1]) > MIN;
        let useuntil = i < zonecount - 1;
        if useuntil && untiltime_of(zp) == MIN {
            continue;
        }
        let stdoff = zp.stdoff;
        let mut startbuf = String::new();
        let mut startoff = zp.stdoff;
        if own.is_empty() {
            save = zp.save;
            startbuf = doabbr(zp, None, zp.isdst, save, false);
            let kind = out.addtype(oadd(zp.stdoff, save), &startbuf, zp.isdst);
            if usestart {
                out.addtt(starttime, kind);
                usestart = false;
            } else {
                defaulttype = Some(kind);
            }
        } else {
            todo.clear();
            todo.resize(own.len(), false);
            temp.clear();
            temp.resize(own.len(), 0);
            let mut year = min_year;
            while year <= max_year {
                if useuntil && zp.until.as_ref().is_some_and(|(until, _)| year > until.hiyear) {
                    break;
                }
                for (j, rule) in own.iter().enumerate() {
                    todo[j] = year >= rule.loyear && year <= rule.hiyear;
                    if todo[j] {
                        temp[j] = rpytime(rule, year);
                        todo[j] = temp[j] < y2038_boundary || year <= max_year0;
                    }
                }
                loop {
                    let mut untiltime = 0;
                    if let Some((until, time)) = zp.until.as_ref().filter(|_| useuntil) {
                        untiltime = *time;
                        if !until.todisut {
                            untiltime = tadd(untiltime, -stdoff);
                        }
                        if !until.todisstd {
                            untiltime = tadd(untiltime, -save);
                        }
                    }
                    // The rule to do that takes effect first in the year.
                    let mut k: Option<usize> = None;
                    let mut ktime = 0;
                    for (j, rule) in own.iter().enumerate() {
                        if !todo[j] {
                            continue;
                        }
                        let mut offset = if rule.todisut { 0 } else { stdoff };
                        if !rule.todisstd {
                            offset = oadd(offset, save);
                        }
                        let jtime = temp[j];
                        if jtime == MIN || jtime == MAX {
                            continue;
                        }
                        let jtime = tadd(jtime, -offset);
                        if k.is_none() || jtime < ktime {
                            k = Some(j);
                            ktime = jtime;
                        }
                    }
                    let Some(k) = k else { break };
                    let rp = &own[k];
                    todo[k] = false;
                    if useuntil && ktime >= untiltime {
                        break;
                    }
                    save = rp.save;
                    if usestart && ktime == starttime {
                        usestart = false;
                    }
                    if usestart {
                        if ktime < starttime {
                            startoff = oadd(zp.stdoff, save);
                            startbuf = doabbr(zp, Some(rp.abbrvar), rp.isdst, rp.save, false);
                            continue;
                        }
                        if startbuf.is_empty() && startoff == oadd(zp.stdoff, save) {
                            startbuf = doabbr(zp, Some(rp.abbrvar), rp.isdst, rp.save, false);
                        }
                    }
                    let ab = doabbr(zp, Some(rp.abbrvar), rp.isdst, rp.save, false);
                    let offset = oadd(zp.stdoff, rp.save);
                    if !useuntil
                        && !do_extend
                        && prevrp.is_some_and(|prev| prev.hiyear == MAX)
                        && rp.hiyear == MAX
                    {
                        break;
                    }
                    let kind = out.addtype(offset, &ab, rp.isdst);
                    if defaulttype.is_none() && !rp.isdst {
                        defaulttype = Some(kind);
                    }
                    if rp.hiyear == MAX
                        && !lastatmax.is_some_and(|last| ktime < out.attypes[last].at)
                    {
                        lastatmax = Some(out.attypes.len());
                    }
                    out.addtt(ktime, kind);
                    prevrp = Some(rp);
                }
                year += 1;
            }
        }
        if usestart {
            if startbuf.is_empty() && !zp.format.contains(['%', '/']) {
                startbuf.clone_from(&zp.format);
            }
            // An empty abbreviation is an error of `zic`, and the pin's data has none.
            if !startbuf.is_empty() {
                let isdst = startoff != zp.stdoff;
                let kind = out.addtype(startoff, &startbuf, isdst);
                if defaulttype.is_none() && !isdst {
                    defaulttype = Some(kind);
                }
                out.addtt(starttime, kind);
            }
        }
        if let Some((until, time)) = zp.until.as_ref().filter(|_| useuntil) {
            starttime = *time;
            if !until.todisstd {
                starttime = tadd(starttime, -save);
            }
            if !until.todisut {
                starttime = tadd(starttime, -stdoff);
            }
        }
    }
    let defaulttype = defaulttype.unwrap_or(0);
    if let Some(last) = lastatmax {
        out.attypes[last].dontmerge = true;
    }
    if do_extend {
        // Say that nothing changes up to the end of the years listed, with a transition that
        // changes nothing at the start of the year after them.
        let first_of = |year| rpytime(&made_rule(0, 1, 0, 0, false, ""), year);
        let lastat = (0..out.attypes.len())
            .reduce(|best, i| if out.attypes[i].at > out.attypes[best].at { i } else { best });
        if lastat.is_none_or(|last| out.attypes[last].at < first_of(max_year - 1)) {
            let kind = lastat.map_or(defaulttype, |last| out.attypes[last].kind);
            out.addtt(first_of(max_year + 1), kind);
            if let Some(added) = out.attypes.last_mut() {
                added.dontmerge = true;
            }
        }
    }
    writezone(out, defaulttype, footer.map(|(text, _)| text).unwrap_or_default())
}

/// `writezone`: the sorted and merged transitions, and the types and the abbreviations in the
/// order the file has them, with the default type first.
fn writezone(mut out: Out, defaulttype: usize, footer: String) -> Tzif {
    out.attypes.sort_by_key(|at| at.at);
    let utoffs = &out.utoffs;
    let mut kept: Vec<AtType> = Vec::with_capacity(out.attypes.len());
    for from in &out.attypes {
        let toi = kept.len();
        if toi != 0 {
            let before = if toi == 1 { utoffs[0] } else { utoffs[kept[toi - 2].kind] };
            if from.at + utoffs[kept[toi - 1].kind] <= kept[toi - 1].at + before {
                kept[toi - 1].kind = from.kind;
                continue;
            }
        }
        let same = |a: usize, b: usize| {
            utoffs[a] == utoffs[b]
                && out.isdsts[a] == out.isdsts[b]
                && out.desigidx[a] == out.desigidx[b]
        };
        if toi == 0 || from.dontmerge || !same(kept[toi - 1].kind, from.kind) {
            kept.push(*from);
        }
    }
    let typecnt = utoffs.len();
    let mut omittype = vec![true; typecnt];
    omittype[defaulttype] = false;
    for at in &kept {
        omittype[at.kind] = false;
    }
    let old0 = omittype.iter().position(|&omit| !omit).unwrap_or(0);
    let swap = |i: usize| {
        if i == old0 {
            defaulttype
        } else if i == defaulttype {
            old0
        } else {
            i
        }
    };
    let mut typemap = vec![0u8; typecnt];
    let mut order = Vec::new();
    for i in old0..typecnt {
        if !omittype[i] {
            typemap[swap(i)] = order.len() as u8;
            order.push(swap(i));
        }
    }
    let mut chars: Vec<u8> = Vec::new();
    let mut indmap: Vec<Option<usize>> = vec![None; out.chars.len()];
    for i in old0..typecnt {
        if omittype[i] || indmap[out.desigidx[i]].is_some() {
            continue;
        }
        let abbr = cstr(&out.chars, out.desigidx[i]);
        let j = find_chars(&chars, abbr).unwrap_or_else(|| {
            let j = chars.len();
            chars.extend_from_slice(abbr);
            chars.push(0);
            j
        });
        indmap[out.desigidx[i]] = Some(j);
    }
    let ttis = order
        .iter()
        .map(|&h| TtInfo {
            utoff: out.utoffs[h] as i32,
            isdst: out.isdsts[h],
            desig: indmap[out.desigidx[h]].unwrap_or(0),
        })
        .collect();
    Tzif {
        ats: kept.iter().map(|at| at.at).collect(),
        types: kept.iter().map(|at| typemap[at.kind]).collect(),
        ttis,
        chars,
        footer,
    }
}

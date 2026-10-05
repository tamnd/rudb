//! The zone of the `TimeZone` setting, for the text of `timestamptz` values on the wire.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Offset, TimeZone as _};
use chrono_tz::{OffsetName, Tz};
use rudb_common::guc::Zone as Spec;
use rudb_pgtypes::{AbbrevMeaning, FixedZone, TimeZone};

/// A zone of the time zone database or a zone with one offset.
pub(super) enum Zone {
    Named {
        tz: Tz,
        /// The abbreviations that this zone gave, which [`TimeZone::at`] lends out.
        seen: RefCell<Vec<&'static str>>,
    },
    Fixed(FixedZone),
}

impl Zone {
    /// The zone of a value of `TimeZone`, or UTC for a value that is not a zone, which the check
    /// of the setting does not let through.
    pub(super) fn of(name: &str) -> Zone {
        match rudb_common::guc::zone(name) {
            Ok((Spec::Named(tz), _)) => Zone::Named { tz, seen: RefCell::default() },
            Ok((Spec::Fixed { offset, abbrev }, _)) => Zone::Fixed(FixedZone { offset, abbrev }),
            Err(_) => Zone::Fixed(FixedZone::utc()),
        }
    }
}

/// The abbreviation as a string that lives as long as the program. There are a few hundred
/// different abbreviations in the database, and each one is kept once.
fn intern(abbrev: &str) -> &'static str {
    static ALL: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut all = ALL.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(kept) = all.get(abbrev) {
        return kept;
    }
    let kept: &'static str = Box::leak(abbrev.to_owned().into_boxed_str());
    all.insert(kept);
    kept
}

/// The abbreviation that PostgreSQL makes for an offset without a name, such as `+0530`.
fn numeric(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.unsigned_abs();
    let (hours, minutes) = (abs / 3600, abs % 3600 / 60);
    if minutes == 0 { format!("{sign}{hours:02}") } else { format!("{sign}{hours:02}{minutes:02}") }
}

impl TimeZone for Zone {
    fn at(&self, unix_seconds: i64) -> (i32, &str) {
        match self {
            Zone::Fixed(zone) => zone.at(unix_seconds),
            Zone::Named { tz, seen } => {
                let instant = DateTime::from_timestamp(unix_seconds, 0).unwrap_or_default();
                let offset = tz.offset_from_utc_datetime(&instant.naive_utc());
                let east = offset.fix().local_minus_utc();
                let numbered;
                let abbrev = match offset.abbreviation() {
                    Some(abbrev) => abbrev,
                    None => {
                        numbered = numeric(east);
                        &numbered
                    }
                };
                let mut seen = seen.borrow_mut();
                let kept = match seen.iter().find(|kept| **kept == abbrev) {
                    Some(kept) => *kept,
                    None => {
                        let kept = intern(abbrev);
                        seen.push(kept);
                        kept
                    }
                };
                (east, kept)
            }
        }
    }

    fn abbrev_meaning(&self, abbrev: &str) -> Option<AbbrevMeaning> {
        match self {
            Zone::Fixed(zone) => zone.abbrev_meaning(abbrev),
            Zone::Named { .. } => None,
        }
    }

    fn abbrev_at(&self, abbrev: &str, unix_seconds: i64) -> Option<(i32, bool)> {
        match self {
            Zone::Fixed(zone) => zone.abbrev_at(abbrev, unix_seconds),
            Zone::Named { .. } => None,
        }
    }

    fn fixed_offset(&self) -> Option<i32> {
        match self {
            Zone::Fixed(zone) => Some(zone.offset),
            Zone::Named { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_and_fixed_zones() {
        let tokyo = Zone::of("Asia/Tokyo");
        assert_eq!(tokyo.at(0), (9 * 3600, "JST"));
        let paris = Zone::of("Europe/Paris");
        assert_eq!(paris.at(1_751_328_000), (2 * 3600, "CEST"));
        assert_eq!(paris.at(1_735_689_600), (3600, "CET"));
        assert_eq!(Zone::of("<+05:30>-05:30").at(0), (5 * 3600 + 1800, "+05:30"));
        assert_eq!(Zone::of("UTC").at(0), (0, "UTC"));
        assert_eq!(numeric(-3 * 3600), "-03");
    }
}

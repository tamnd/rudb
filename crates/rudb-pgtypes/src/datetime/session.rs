//! The zones of the engine as a [`TimeZone`], and the zones of PostgreSQL as a [`ZoneLookup`].
//!
//! The kernels that run in a PostgreSQL session have the zone of the `TimeZone` setting as a
//! [`SessionTimeZone`]. These impls let the input and the formatting functions of this crate read
//! it as PostgreSQL reads `session_timezone`.

use std::sync::Arc;

use rudb_common::SessionTimeZone;
use rudb_common::tzdb::Zone;

use super::decode::ZoneLookup;
use super::{AbbrevMeaning, TimeZone};

impl TimeZone for Zone {
    fn at(&self, unix_seconds: i64) -> (i32, &str) {
        let period = self.state().at(unix_seconds);
        (period.offset, period.abbrev)
    }

    fn next_change(&self, unix_seconds: i64) -> (i32, Option<(i64, i32)>) {
        self.state().next_change(unix_seconds)
    }

    fn abbrev_meaning(&self, abbrev: &str) -> Option<AbbrevMeaning> {
        Some(match self.state().abbrev_is_known(abbrev)? {
            Some((offset, dst)) => AbbrevMeaning::Fixed { offset, dst },
            None => AbbrevMeaning::Varies,
        })
    }

    fn abbrev_at(&self, abbrev: &str, unix_seconds: i64) -> Option<(i32, bool)> {
        self.state().interpret_abbrev(abbrev, unix_seconds)
    }

    fn fixed_offset(&self) -> Option<i32> {
        self.state().fixed_offset()
    }
}

impl TimeZone for SessionTimeZone {
    fn at(&self, unix_seconds: i64) -> (i32, &str) {
        let period = self.zone().state().at(unix_seconds);
        (period.offset, period.abbrev)
    }

    fn next_change(&self, unix_seconds: i64) -> (i32, Option<(i64, i32)>) {
        self.zone().next_change(unix_seconds)
    }

    fn abbrev_meaning(&self, abbrev: &str) -> Option<AbbrevMeaning> {
        self.zone().abbrev_meaning(abbrev)
    }

    fn abbrev_at(&self, abbrev: &str, unix_seconds: i64) -> Option<(i32, bool)> {
        self.zone().abbrev_at(abbrev, unix_seconds)
    }

    fn fixed_offset(&self) -> Option<i32> {
        SessionTimeZone::fixed_offset(*self)
    }
}

/// The zones that `pg_tzset` finds by name, which a dynamic abbreviation of
/// `timezone_abbreviations` names.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionZones;

impl ZoneLookup for SessionZones {
    fn zone(&self, name: &str) -> Option<Arc<dyn TimeZone + Send + Sync>> {
        Zone::postgres(name).map(|zone| Arc::new(zone) as Arc<dyn TimeZone + Send + Sync>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_zone_reads_as_postgresql_reads_its_zone() {
        let paris = SessionTimeZone::of_postgres("Europe/Paris").unwrap();
        // 2024-07-10 12:34 UTC.
        assert_eq!(paris.at(1_720_614_840), (7200, "CEST"));
        // 2024-03-31 02:30 does not exist in Paris and reads as winter time.
        assert_eq!(paris.local_offset(1_711_852_200), 3600);
        // 2024-10-27 02:30 exists twice and reads as winter time.
        assert_eq!(paris.local_offset(1_729_996_200), 3600);
        let fixed = SessionTimeZone::of_postgres("XYZ+3").unwrap();
        assert_eq!(
            fixed.abbrev_meaning("XYZ"),
            Some(AbbrevMeaning::Fixed { offset: -10800, dst: false })
        );
        let new_york = SessionTimeZone::of_postgres("America/New_York").unwrap();
        assert_eq!(
            new_york.abbrev_meaning("EST"),
            Some(AbbrevMeaning::Fixed { offset: -18000, dst: false })
        );
        assert_eq!(
            new_york.abbrev_meaning("LMT"),
            Some(AbbrevMeaning::Fixed { offset: -17762, dst: false })
        );
        assert_eq!(new_york.abbrev_meaning("CET"), None);
        // EWT and EPT of the Second World War.
        assert_eq!(new_york.abbrev_at("EWT", 0), Some((-14400, true)));
        let moscow = SessionTimeZone::of_postgres("Europe/Moscow").unwrap();
        assert_eq!(moscow.abbrev_meaning("MSK"), Some(AbbrevMeaning::Varies));
        // 2012-07-01 12:00 UTC, when Moscow time was +04.
        assert_eq!(moscow.abbrev_at("MSK", 1_341_144_000), Some((14400, false)));
        assert!(SessionZones.zone("Europe/Moscow").is_some());
        assert!(SessionZones.zone("Mars/Olympus").is_none());
    }
}

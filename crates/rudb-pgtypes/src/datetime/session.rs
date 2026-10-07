//! The session time zone of the engine as a [`TimeZone`], and the zones of the time zone database
//! as a [`ZoneLookup`].
//!
//! The kernels that run in a PostgreSQL session have the zone of the `TimeZone` setting as a
//! [`SessionTimeZone`]. These impls let the input and the formatting functions of this crate read
//! it as PostgreSQL reads `session_timezone`.

use std::sync::Arc;

use rudb_common::SessionTimeZone;

use super::decode::ZoneLookup;
use super::{AbbrevMeaning, TimeZone, USECS_PER_SEC};

impl TimeZone for SessionTimeZone {
    fn at(&self, unix_seconds: i64) -> (i32, &str) {
        let micros = unix_seconds.saturating_mul(USECS_PER_SEC);
        (self.offset_seconds_at(micros), self.abbreviation_at(micros))
    }

    fn offset_at(&self, unix_seconds: i64) -> i32 {
        self.offset_seconds_at(unix_seconds.saturating_mul(USECS_PER_SEC))
    }

    fn local_offset(&self, local_seconds: i64) -> i32 {
        let local = local_seconds.saturating_mul(USECS_PER_SEC);
        match self.instant_of_local(local) {
            Some(instant) => ((local - instant) / USECS_PER_SEC) as i32,
            None => self.offset_seconds_at(local),
        }
    }

    // A zone of the database gives no meaning to an abbreviation here, so the abbreviation is
    // read from `timezone_abbreviations`, as the server's own zone does.
    fn abbrev_meaning(&self, abbrev: &str) -> Option<AbbrevMeaning> {
        let offset = self.fixed_offset()?;
        let known = self.abbreviation_at(0);
        (!known.is_empty() && abbrev == known)
            .then_some(AbbrevMeaning::Fixed { offset, dst: false })
    }

    fn abbrev_at(&self, abbrev: &str, _: i64) -> Option<(i32, bool)> {
        match self.abbrev_meaning(abbrev)? {
            AbbrevMeaning::Fixed { offset, dst } => Some((offset, dst)),
            AbbrevMeaning::Varies => None,
        }
    }

    fn fixed_offset(&self) -> Option<i32> {
        SessionTimeZone::fixed_offset(*self)
    }
}

/// The zones of the time zone database that the engine bundles, by name.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionZones;

impl ZoneLookup for SessionZones {
    fn zone(&self, name: &str) -> Option<Arc<dyn TimeZone + Send + Sync>> {
        SessionTimeZone::named(name).map(|zone| Arc::new(zone) as Arc<dyn TimeZone + Send + Sync>)
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
        assert!(SessionZones.zone("Europe/Moscow").is_some());
        assert!(SessionZones.zone("Mars/Olympus").is_none());
    }
}

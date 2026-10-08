//! The zone of the `TimeZone` setting, for the text of `timestamptz` values on the wire.

use rudb_common::tzdb::Release;
pub(super) use rudb_common::tzdb::Zone;

/// The zone of a value of `TimeZone`, or UTC for a value that is not a zone, which the check of
/// the setting does not let through.
pub(super) fn of(name: &str) -> Zone {
    rudb_common::guc::zone(name).unwrap_or_else(|_| Zone::utc(Release::Postgres))
}

#[cfg(test)]
mod tests {
    use rudb_pgtypes::TimeZone;

    use super::*;

    #[test]
    fn named_and_fixed_zones() {
        let tokyo = of("Asia/Tokyo");
        assert_eq!(tokyo.at(0), (9 * 3600, "JST"));
        let paris = of("Europe/Paris");
        assert_eq!(paris.at(1_751_328_000), (2 * 3600, "CEST"));
        assert_eq!(paris.at(1_735_689_600), (3600, "CET"));
        assert_eq!(of("<+05:30>-05:30").at(0), (5 * 3600 + 1800, "+05:30"));
        assert_eq!(of("UTC").at(0), (0, "UTC"));
        assert_eq!(of("-3").at(0), (-3 * 3600, "-03"));
        assert_eq!(of("Nowhere/X"), of("UTC"));
    }
}

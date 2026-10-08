//! The time zones of both dialects, compiled from IANA data as the pin's `zic` compiles it in
//! slim mode and read as the pin's `localtime.c` reads the result.
//!
//! The dialects differ in the data. PostgreSQL reads the `tzdata.zi` that its source tree
//! vendors, and DuckDB reads the release that its copy of ICU holds, in the rearguard form. A
//! [`Release`] is one of the two. PostgreSQL also accepts a POSIX zone such as
//! `EST5EDT,M3.2.0,M11.1.0`, which `pg_tzset` reads with `tzparse`.
//!
//! A zone compiles when it is first used. A handle is a `'static` reference, so it is `Copy` and
//! two handles are equal when they name the same zone of the same release.

mod localtime;
mod zi;
mod zic;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{LazyLock, Mutex, OnceLock};

pub use localtime::{Boundary, Period, State, determine_offset};
use localtime::{TtInfo, Tzif};

/// `TZ_STRLEN_MAX`: the longest name `pg_tzset` accepts.
const TZ_STRLEN_MAX: usize = 255;

/// The IANA data of a dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Release {
    /// `src/timezone/data/tzdata.zi` of the pin of PostgreSQL.
    Postgres,
    /// The release of the ICU data of the pin of DuckDB.
    Icu,
}

impl Release {
    fn text(self) -> &'static str {
        match self {
            Release::Postgres => include_str!("../../vendor/tzdata.zi"),
            Release::Icu => include_str!("../../vendor-icu/tzdata.zi"),
        }
    }

    fn registry(self) -> &'static Registry {
        static POSTGRES: LazyLock<Registry> = LazyLock::new(|| Registry::new(Release::Postgres));
        static ICU: LazyLock<Registry> = LazyLock::new(|| Registry::new(Release::Icu));
        match self {
            Release::Postgres => &POSTGRES,
            Release::Icu => &ICU,
        }
    }

    /// The IANA release of the data, such as `2026e`.
    #[must_use]
    pub fn version(self) -> &'static str {
        self.text()
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("# version "))
            .unwrap_or_default()
    }
}

/// Where the rules of a zone come from.
enum Source {
    /// A zone of the data of a release, by its index in [`zi::Data::zones`].
    File(Release, usize),
    /// A POSIX string, which `tzparse` reads.
    Posix { lastditch: bool },
}

/// A zone that a name has found: its canonical name, where its rules come from, and the rules
/// once they are compiled.
struct Entry {
    name: &'static str,
    source: Source,
    state: OnceLock<Option<State>>,
}

impl Entry {
    fn state(&self) -> Option<&State> {
        self.state
            .get_or_init(|| match self.source {
                Source::File(release, index) => {
                    let data = &release.registry().data;
                    State::load(zic::compile(&data.zones[index].eras, &data.rules))
                }
                Source::Posix { lastditch } => State::parse(self.name, lastditch),
            })
            .as_ref()
    }
}

/// The zones and links of the data of a release.
struct Registry {
    data: zi::Data,
    /// The zone files as the pin of PostgreSQL installs them: a file for each zone and each link,
    /// in the order of their names.
    files: Vec<Entry>,
    /// The index in `files` of each name in upper case.
    by_upper: HashMap<String, usize>,
}

impl Registry {
    fn new(release: Release) -> Registry {
        let data = zi::read(release.text());
        let mut files: Vec<Entry> = data
            .zones
            .iter()
            .enumerate()
            .map(|(index, zone)| Entry {
                name: zone.name,
                source: Source::File(release, index),
                state: OnceLock::new(),
            })
            .collect();
        let zone_of: HashMap<&str, usize> =
            data.zones.iter().enumerate().map(|(index, zone)| (zone.name, index)).collect();
        for &(target, name) in &data.links {
            if let Some(&index) = zone_of.get(target) {
                files.push(Entry {
                    name,
                    source: Source::File(release, index),
                    state: OnceLock::new(),
                });
            }
        }
        files.sort_by_key(|entry| entry.name);
        let by_upper = files
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.name.to_ascii_uppercase(), index))
            .collect();
        Registry { data, files, by_upper }
    }
}

/// A time zone.
#[derive(Clone, Copy)]
pub struct Zone(&'static Entry);

impl Zone {
    /// The zone file of a release with a name, found without regard to case, as `pg_open_tzfile`
    /// finds it.
    #[must_use]
    pub fn file(release: Release, name: &str) -> Option<Zone> {
        let registry = release.registry();
        let entry = &registry.files[*registry.by_upper.get(&name.to_ascii_uppercase())?];
        entry.state().is_some().then_some(Zone(entry))
    }

    /// `pg_tzset`: the zone a value of the setting `TimeZone` of PostgreSQL names. `GMT` is
    /// always the POSIX zone; another name is a zone file, and then a POSIX string unless it
    /// starts with `:`.
    #[must_use]
    pub fn postgres(name: &str) -> Option<Zone> {
        if name.len() > TZ_STRLEN_MAX {
            return None;
        }
        let upper = name.to_ascii_uppercase();
        if upper != "GMT"
            && let Some(zone) = Zone::file(Release::Postgres, &upper)
        {
            return Some(zone);
        }
        if upper.starts_with(':') {
            return None;
        }
        Zone::posix(upper)
    }

    /// The POSIX zone of a name in upper case, interned so that one name has one handle.
    fn posix(upper: String) -> Option<Zone> {
        static POSIX: LazyLock<Mutex<HashMap<String, &'static Entry>>> =
            LazyLock::new(Mutex::default);
        let mut posix = POSIX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(&entry) = posix.get(&upper) {
            return Some(Zone(entry));
        }
        let lastditch = upper == "GMT";
        let state = State::parse(&upper, lastditch)?;
        let name: &'static str = Box::leak(upper.clone().into_boxed_str());
        let entry: &'static Entry = Box::leak(Box::new(Entry {
            name,
            source: Source::Posix { lastditch },
            state: OnceLock::from(Some(state)),
        }));
        posix.insert(upper, entry);
        Some(Zone(entry))
    }

    /// The zone `UTC` of a release, the default zone of a session.
    #[must_use]
    pub fn utc(release: Release) -> Zone {
        static UTC: LazyLock<[Zone; 2]> = LazyLock::new(|| {
            [Release::Postgres, Release::Icu]
                .map(|release| Zone::file(release, "UTC").expect("the data has the zone UTC"))
        });
        UTC[usize::from(release == Release::Icu)]
    }

    /// The zone files of a release in the order of their names, as `pg_timezone_names` walks
    /// the directory of zones.
    pub fn files(release: Release) -> impl Iterator<Item = Zone> {
        release.registry().files.iter().map(Zone)
    }

    /// The canonical name: the name of the zone file, or the POSIX string in upper case.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.0.name
    }

    /// The rules of the zone.
    ///
    /// # Panics
    ///
    /// Never: a handle is made only for a zone whose rules load.
    #[must_use]
    pub fn state(self) -> &'static State {
        // A handle exists only for a zone whose rules load.
        self.0.state().expect("a zone handle has rules")
    }
}

impl PartialEq for Zone {
    fn eq(&self, other: &Zone) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl Eq for Zone {}

impl Hash for Zone {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::ptr::hash(self.0, state);
    }
}

impl std::fmt::Debug for Zone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Zone").field(&self.0.name).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(name: &str, t: i64) -> (i32, &'static str) {
        let period = Zone::postgres(name).unwrap().state().at(t);
        (period.offset, period.abbrev)
    }

    #[test]
    fn the_data_is_the_release_of_the_pin() {
        assert_eq!(Release::Postgres.version(), "2026e");
        assert_eq!(Release::Icu.version(), "2026c");
        for release in [Release::Postgres, Release::Icu] {
            let registry = release.registry();
            assert!(registry.files.iter().all(|entry| entry.state().is_some()));
        }
    }

    #[test]
    fn zones_read_as_the_pin_reads_them() {
        assert_eq!(at("Asia/Tokyo", 0), (32_400, "JST"));
        assert_eq!(at("europe/paris", 1_751_328_000), (7200, "CEST"));
        assert_eq!(at("Europe/Paris", 1_735_689_600), (3600, "CET"));
        // Before the first transition, local mean time.
        assert_eq!(at("America/New_York", -8_504_654_400), (-17_762, "LMT"));
        assert_eq!(at("Europe/Paris", -5_348_980_800), (561, "LMT"));
        assert_eq!(at("Europe/London", -5_348_980_800), (-75, "LMT"));
        // After the last transition, the rules of 400 years before.
        assert_eq!(at("America/New_York", 13_585_233_600), (-14_400, "EDT"));
        assert_eq!(at("Europe/London", 253_386_446_400), (3600, "BST"));
        assert_eq!(at("Africa/Casablanca", 3_676_363_200), (0, "+00"));
        assert_eq!(Zone::postgres("US/Eastern").unwrap().name(), "US/Eastern");
        assert_eq!(Zone::postgres("utc"), Some(Zone::utc(Release::Postgres)));
    }

    #[test]
    fn each_dialect_reads_its_own_release() {
        let offset = |release, name, t| Zone::file(release, name).unwrap().state().at(t).offset;
        // The Northwest Territories stop changing their clocks in 2026d.
        assert_eq!(offset(Release::Postgres, "America/Inuvik", 1_797_336_000), -6 * 3600);
        assert_eq!(offset(Release::Icu, "America/Inuvik", 1_797_336_000), -7 * 3600);
        // EST5EDT is a link to New York before 2026d.
        assert_eq!(offset(Release::Postgres, "EST5EDT", -1_570_374_000), -5 * 3600);
        assert_eq!(offset(Release::Icu, "est5edt", -1_570_374_000), -4 * 3600);
        assert_ne!(Zone::utc(Release::Postgres), Zone::utc(Release::Icu));
        assert!(Zone::file(Release::Icu, "EST5EDT,M3.2.0,M11.1.0").is_none());
    }

    #[test]
    fn posix_zones_read_as_the_pin_reads_them() {
        let gmt = Zone::postgres("gmt").unwrap();
        assert_eq!((gmt.name(), gmt.state().at(0).abbrev), ("GMT", "GMT"));
        let zone = Zone::postgres("abc5xyz").unwrap();
        assert_eq!(zone.name(), "ABC5XYZ");
        assert_eq!(zone.state().at(1_751_328_000).offset, -4 * 3600);
        assert_eq!(zone.state().at(1_735_689_600).offset, -5 * 3600);
        assert_eq!(zone.state().at(13_585_233_600).abbrev, "XYZ");
        assert_eq!(
            Zone::postgres("<+05:30>-05:30").unwrap().state().at(0),
            Period { offset: 19_800, dst: false, abbrev: "+05:30" }
        );
        assert_eq!(Zone::postgres("ABC5XYZ"), Some(zone));
        assert!(Zone::postgres(":UTC").is_none());
        assert!(Zone::postgres("nonsense").is_none());
    }

    #[test]
    fn abbreviations_read_as_the_pin_reads_them() {
        let state = Zone::postgres("America/New_York").unwrap().state();
        assert_eq!(state.interpret_abbrev("EST", 1_735_689_600), Some((-18_000, false)));
        assert_eq!(state.abbrev_is_known("EDT"), Some(Some((-14_400, true))));
        assert_eq!(state.abbrev_is_known("CET"), None);
        assert_eq!(state.fixed_offset(), None);
        assert_eq!(Zone::utc(Release::Postgres).state().fixed_offset(), Some(0));
        let boundary = state.next_boundary(1_735_689_600);
        assert_eq!(boundary.before, (-18_000, false));
        assert_eq!(boundary.next, Some((1_741_503_600, -14_400, true)));
        // 2025-03-09 02:30 does not exist and reads as winter time, and 2025-11-02 01:30 exists
        // twice and reads as winter time.
        assert_eq!(state.local_offset(1_741_487_400), -18_000);
        assert_eq!(state.local_offset(1_762_047_000), -18_000);
        assert_eq!(state.local_offset(1_751_371_200), -14_400);
        // The ends of the range of an instant do not overflow.
        assert!(["EST", "EDT"].contains(&state.at(i64::MAX).abbrev));
        assert_eq!(state.next_boundary(i64::MAX).next, None);
        assert_eq!(state.at(i64::MIN).abbrev, "LMT");
    }
}

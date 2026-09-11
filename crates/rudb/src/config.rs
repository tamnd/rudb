//! What a database was opened with.

use std::time::Duration;

use rudb_common::{Error, Result};

/// The settings a database is opened with.
///
/// Three of them today: how much memory the engine may use, how many threads it may run a query on,
/// and how long a query may take. They are set once, at open time, and read back afterwards. That
/// is deliberately narrower than DuckDB, where almost anything can be changed by `SET` in the
/// middle of a session, and the narrower version is the one worth having first: a setting that can
/// change under a running query is a setting every operator has to re-read, and there is no
/// operator yet that would honour a change.
///
/// # What reads these, and what does not yet
///
/// `--print-config` prints them, and a harness that opened the database is the thing that told it
/// what to print, so the two agree by construction rather than by both being kept up to date. That
/// is the whole reason this exists now: a benchmark result that does not say how many threads it
/// used is not a result, and a run that says eight while the engine used one is worse than one that
/// says nothing.
///
/// The query timeout and the memory limit are enforced. The thread count is not yet, because that
/// needs a parallel executor, which is E4, and the documentation on it says so rather than implying
/// otherwise. Recording the intent first is what lets the harnesses be written against the final
/// shape, and it is also what makes the gap visible: a setting that is stored and ignored is easier
/// to find than a setting that was never accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    memory_limit: Option<u64>,
    threads: usize,
    query_timeout: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        Self { memory_limit: rudb_io::default_memory_limit(), threads, query_timeout: None }
    }
}

impl Config {
    /// The defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many bytes the engine may use, or `None` for no limit.
    ///
    /// Enforced, by the operators that buffer without bound charging what they hold against one
    /// budget for the whole database. A query that passes the limit stops with an `Out of Memory
    /// Error` saying what it asked for and what was already held. See [`rudb_common::Memory`] for
    /// what is counted and what is not, which is a shorter list than it will be: nothing here hooks
    /// the allocator, so the number is what the operators said they were holding.
    ///
    /// The default is eighty percent of what the machine has, which is what DuckDB does, and `None`
    /// on a platform that will not say how much that is. See [`rudb_io::machine`] for how each
    /// platform is asked and why the answer on Linux is the smaller of the machine and the control
    /// group.
    ///
    /// It was `None` everywhere until #219, and the reason it is not any more is that a budget
    /// nobody set is a budget the operating system enforces. Eight of the forty three ClickBench
    /// queries ended in `memory allocation of 128 bytes failed`, which is the allocator's abort
    /// handler, so the process was gone and there was no error for a harness to report. The same
    /// queries under a limit say `Out of Memory Error` and name what they asked for. A database
    /// that has an out of memory error and does not use it unless asked is a database that aborts
    /// on every machine where nobody typed the `SET`.
    ///
    /// [`Config::with_no_memory_limit`] is still there and still means no limit, which is now a
    /// thing somebody asks for rather than a thing they get.
    #[must_use]
    pub fn memory_limit(&self) -> Option<u64> {
        self.memory_limit
    }

    /// How many threads a query may run on.
    ///
    /// Defaults to the number of cores the program can see, which is what DuckDB does, and one if
    /// the operating system will not say. The executor is single threaded today, so this is
    /// recorded and not yet obeyed.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// How long a query may run, or `None` for no limit.
    ///
    /// Enforced. The clock starts when the statement starts, so it is a limit on one statement
    /// rather than on a session, and a statement over the limit stops at its next chunk boundary
    /// with an `Interrupt Error` saying what limit it passed. See [`crate::Cancel`] for what a
    /// chunk boundary costs in response time and why it is the right place to check.
    #[must_use]
    pub fn query_timeout(&self) -> Option<Duration> {
        self.query_timeout
    }

    /// The same settings with this memory limit.
    #[must_use]
    pub fn with_memory_limit(mut self, bytes: u64) -> Self {
        self.memory_limit = Some(bytes);
        self
    }

    /// The same settings with no memory limit.
    #[must_use]
    pub fn with_no_memory_limit(mut self) -> Self {
        self.memory_limit = None;
        self
    }

    /// The same settings with this memory limit, written the way a person writes one.
    ///
    /// `1GB`, `512MiB`, `2048`. See [`parse_size`] for exactly what is accepted and why the two
    /// spellings of a gigabyte are two different numbers.
    ///
    /// # Errors
    ///
    /// When the text is not a size.
    pub fn with_memory_limit_text(mut self, text: &str) -> Result<Self> {
        self.memory_limit = Some(parse_size(text)?);
        Ok(self)
    }

    /// The same settings with this thread count.
    ///
    /// # Errors
    ///
    /// For zero, which would mean a query runs on nothing. DuckDB refuses it too.
    pub fn with_threads(mut self, threads: usize) -> Result<Self> {
        if threads == 0 {
            return Err(Error::invalid_input("threads must be at least 1"));
        }
        self.threads = threads;
        Ok(self)
    }

    /// The same settings with this query timeout.
    #[must_use]
    pub fn with_query_timeout(mut self, timeout: Duration) -> Self {
        self.query_timeout = Some(timeout);
        self
    }

    /// The same settings with no query timeout.
    #[must_use]
    pub fn with_no_query_timeout(mut self) -> Self {
        self.query_timeout = None;
        self
    }

    /// Every setting as a name and a value, in a fixed order.
    ///
    /// For `--print-config` and for a harness writing a run's settings into its report. A list
    /// rather than eight lines of formatting at each call site, so that a setting added here shows
    /// up in both places without either of them being edited.
    #[must_use]
    pub fn settings(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "memory-limit",
                self.memory_limit.map_or_else(|| "unlimited".to_string(), format_size),
            ),
            ("threads", self.threads.to_string()),
            (
                "query-timeout",
                self.query_timeout.map_or_else(
                    || "none".to_string(),
                    |timeout| format!("{}ms", timeout.as_millis()),
                ),
            ),
        ]
    }
}

/// A size written the way a person writes one, in bytes.
///
/// `KB`, `MB`, `GB` and `TB` are powers of a thousand, and `KiB`, `MiB`, `GiB` and `TiB` are powers
/// of 1024. That is what DuckDB does, measured on the pinned binary: `SET memory_limit='1GB'` reads
/// back as `953.6 MiB` and `SET memory_limit='1GiB'` reads back as `1.0 GiB`. A script that says
/// `10GB` has to get the same number from both engines, so the two spellings are two numbers here
/// even though treating them as one is tidier.
///
/// A number with no unit is bytes, which DuckDB refuses and this accepts, because this is also the
/// function a harness calls to turn a number it already has into a limit. `SET memory_limit` does
/// not take that path, so the statement still refuses a bare number the way the binary does.
///
/// Case does not matter, a space before the unit is allowed and the number may have a fraction,
/// because all three appear in the wild and DuckDB takes all three. The messages are word for word
/// the ones the binary prints, so a script that matches on them matches on both engines.
///
/// # Errors
///
/// For text that is not a number, a number below zero, a unit that is not one of the nine, and a
/// size that does not fit in a `u64`.
pub fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim();
    let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c.is_whitespace());
    let unit = text[digits.len()..].trim().to_ascii_uppercase();
    let number: f64 =
        digits.trim().parse().map_err(|_| Error::parser("Memory must have a number (e.g. 1GB)"))?;
    let scale: f64 = match unit.as_str() {
        "" | "B" => 1.0,
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KIB" => 1024.0,
        "MIB" => 1024f64.powi(2),
        "GIB" => 1024f64.powi(3),
        "TIB" => 1024f64.powi(4),
        other => {
            let other = other.to_ascii_lowercase();
            return Err(Error::parser(format!(
                "Unknown unit for memory: '{other}' (expected: KB, MB, GB, TB for 1000^i units or KiB, MiB, GiB, TiB for 1024^i units)"
            )));
        }
    };
    let bytes = number * scale;
    if !bytes.is_finite() || bytes < 0.0 {
        return Err(Error::parser(format!("\"{text}\" is not a size")));
    }
    if bytes >= SIZE_CEILING {
        return Err(Error::parser(format!("\"{text}\" is larger than a 64 bit size")));
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the range is checked on the line above and a size is a whole number of bytes"
    )]
    Ok(bytes as u64)
}

/// The first size that does not survive the trip through an `f64` and back.
///
/// A size is parsed as a float so that `1.5GB` is a size, and a float at or past this cannot be
/// turned into a `u64`. Two to the sixty fourth rather than `u64::MAX`, because `u64::MAX` is not a
/// float and comparing against the nearest one that is would let a value through that does not fit.
const SIZE_CEILING: f64 = 18_446_744_073_709_551_616.0;

/// A size in bytes, written the way a person reads one.
///
/// The largest unit that leaves a whole number, binary before decimal, so that a limit set as `1GiB`
/// prints as `1GiB` and one set as `2GB` prints as `2GB`. A size that is not a whole number of any
/// unit prints as bytes, which is exact, because a rounded number in a configuration dump is a
/// number somebody will later compare against what they set.
fn format_size(bytes: u64) -> String {
    for power in (1..=4).rev() {
        for (scale, unit) in [(1024u64, "iB"), (1000u64, "B")] {
            let Some(scale) = scale.checked_pow(power) else { continue };
            if bytes >= scale && bytes % scale == 0 {
                let prefix = ["K", "M", "G", "T"][power as usize - 1];
                return format!("{}{prefix}{unit}", bytes / scale);
            }
        }
    }
    format!("{bytes}B")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Config, format_size, parse_size};

    #[test]
    fn the_defaults_are_most_of_the_machine_and_every_core() {
        let config = Config::new();
        assert_eq!(config.memory_limit(), rudb_io::default_memory_limit());
        assert_eq!(config.query_timeout(), None);
        assert!(config.threads() >= 1);
    }

    #[test]
    fn the_default_limit_leaves_the_machine_something() {
        // The twenty percent that is left is for what the budget does not count, which is the
        // allocator's bookkeeping, the page cache the scan reads through, and every other process.
        // A machine that will not say how much memory it has gets no limit, the same as before.
        let Some(machine) = rudb_io::physical_memory() else {
            eprintln!("skipping, this platform does not say how much memory it has");
            return;
        };
        let limit = Config::new().memory_limit().expect("a machine that says its size has one");
        assert!(limit < machine, "{limit} is not under {machine}");
        assert!(limit > machine / 2, "{limit} is a smaller share of {machine} than intended");
    }

    #[test]
    fn a_setting_reads_back_as_what_it_was_set_to() {
        let config = Config::new()
            .with_memory_limit(1024)
            .with_threads(4)
            .expect("four is a thread count")
            .with_query_timeout(Duration::from_secs(30));
        assert_eq!(config.memory_limit(), Some(1024));
        assert_eq!(config.threads(), 4);
        assert_eq!(config.query_timeout(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn a_limit_can_be_taken_off_again() {
        let config = Config::new()
            .with_memory_limit(1024)
            .with_no_memory_limit()
            .with_query_timeout(Duration::from_secs(1))
            .with_no_query_timeout();
        assert_eq!(config.memory_limit(), None);
        assert_eq!(config.query_timeout(), None);
    }

    #[test]
    fn no_threads_at_all_is_refused_rather_than_silently_made_one() {
        let error = Config::new().with_threads(0).expect_err("zero threads runs nothing");
        assert!(error.to_string().contains("at least 1"), "{error}");
    }

    #[test]
    fn a_decimal_unit_is_a_power_of_a_thousand_and_a_binary_one_a_power_of_1024() {
        assert_eq!(parse_size("1024").expect("a number is bytes"), 1024);
        assert_eq!(parse_size("1KB").expect("a kilobyte"), 1000);
        assert_eq!(parse_size("1MB").expect("a megabyte"), 1_000_000);
        assert_eq!(parse_size("10GB").expect("ten gigabytes"), 10_000_000_000);
        assert_eq!(parse_size("1TB").expect("a terabyte"), 1_000_000_000_000);
        assert_eq!(parse_size("1KiB").expect("a kibibyte"), 1024);
        assert_eq!(parse_size("1MiB").expect("a mebibyte"), 1024 * 1024);
        assert_eq!(parse_size("1GiB").expect("a gibibyte"), 1024u64.pow(3));
        assert_eq!(parse_size("1TiB").expect("a tebibyte"), 1024u64.pow(4));
    }

    #[test]
    fn the_two_spellings_of_a_gigabyte_are_two_different_numbers() {
        // Measured on the pinned binary: SET memory_limit='1GB' reads back as 953.6 MiB, which is
        // 10^9 bytes printed in binary units, and '1GiB' reads back as 1.0 GiB. A script that says
        // 10GB has to get the same number from both engines, so the difference is the point.
        let decimal = parse_size("1GB").expect("a gigabyte");
        let binary = parse_size("1GiB").expect("a gibibyte");
        assert_eq!(decimal, 1_000_000_000);
        assert_eq!(binary, 1_073_741_824);
        assert_eq!(rudb_common::human(decimal), "953.7 MiB");
    }

    #[test]
    fn case_and_a_space_before_the_unit_are_both_allowed() {
        assert_eq!(parse_size("512mb").expect("lower case"), 512_000_000);
        assert_eq!(parse_size("512 MB").expect("a space"), 512_000_000);
        assert_eq!(parse_size("  512MiB  ").expect("surrounding space"), 512 * 1024 * 1024);
        assert_eq!(parse_size("512 gib").expect("both at once"), 512 * 1024u64.pow(3));
    }

    #[test]
    fn a_fraction_is_a_size_because_duckdb_takes_one() {
        assert_eq!(parse_size("1.5GB").expect("a gigabyte and a half"), 1_500_000_000);
        assert_eq!(parse_size("0.5MiB").expect("half a mebibyte"), 512 * 1024);
    }

    #[test]
    fn something_that_is_not_a_size_says_so() {
        assert!(parse_size("").is_err());
        assert!(parse_size("lots").is_err());
        assert!(parse_size("-1").is_err());
        let error = parse_size("5PB").expect_err("petabytes are not a unit here");
        assert!(error.to_string().contains("Unknown unit"), "{error}");
        let error = parse_size("16777216TiB").expect_err("that does not fit in a u64");
        assert!(error.to_string().contains("64 bit"), "{error}");
    }

    #[test]
    fn a_size_prints_back_as_the_unit_it_was_written_in() {
        assert_eq!(format_size(1024), "1KiB");
        assert_eq!(format_size(10 * 1024u64.pow(3)), "10GiB");
        assert_eq!(format_size(2_000_000_000), "2GB");
        assert_eq!(format_size(0), "0B");
        // Not a whole number of anything, so bytes, because a rounded number in a configuration
        // dump is one somebody will later compare against what they set.
        assert_eq!(format_size(1025), "1025B");
    }

    #[test]
    fn the_settings_list_is_what_print_config_prints() {
        let config = Config::new()
            .with_memory_limit_text("2GB")
            .expect("two gigabytes")
            .with_threads(8)
            .expect("eight is a thread count")
            .with_query_timeout(Duration::from_millis(1500));
        let settings = config.settings();
        assert_eq!(settings[0], ("memory-limit", "2GB".to_string()));
        assert_eq!(settings[1], ("threads", "8".to_string()));
        assert_eq!(settings[2], ("query-timeout", "1500ms".to_string()));
    }

    #[test]
    fn a_limit_taken_off_prints_as_the_absence_of_one_rather_than_as_a_number() {
        let settings = Config::new().with_no_memory_limit().settings();
        assert_eq!(settings[0].1, "unlimited");
        assert_eq!(settings[2].1, "none");
    }

    #[test]
    fn the_default_limit_prints_as_a_size() {
        // The point of printing it. A run that does not say what budget it had is a run nobody can
        // reproduce, and a default nobody typed is exactly the one left out of the report.
        if rudb_io::physical_memory().is_none() {
            eprintln!("skipping, this platform does not say how much memory it has");
            return;
        }
        let printed = Config::new().settings()[0].1.clone();
        assert_ne!(printed, "unlimited");
        assert!(printed.ends_with('B'), "{printed}");
    }
}

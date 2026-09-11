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
/// None of the three is enforced yet, and the documentation on each says so rather than implying
/// otherwise. The memory limit needs a buffer manager to be the thing that respects it, which is
/// E2. The timeout needs cancellation, which is the next item on #110. The thread count needs a
/// parallel executor, which is E4. Recording the intent first is what lets the harnesses be written
/// against the final shape, and it is also what makes the gap visible: a setting that is stored and
/// ignored is easier to find than a setting that was never accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    memory_limit: Option<u64>,
    threads: usize,
    query_timeout: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        Self { memory_limit: None, threads, query_timeout: None }
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
    /// No limit is the default, which is not what DuckDB does. DuckDB defaults to eighty percent of
    /// physical memory, and reading physical memory means asking the operating system in three
    /// different ways for three different platforms. This workspace has no dependencies, so that is
    /// code we would be writing and maintaining ourselves for a number that nothing consults yet.
    /// When the buffer manager arrives and the number starts mattering, it arrives with it.
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
    /// `1GB`, `512MiB`, `2048`. See [`parse_size`] for exactly what is accepted and why both
    /// spellings of a gigabyte mean the same thing here.
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
/// A number on its own is bytes. A number with `KB`, `MB`, `GB` or `TB` after it is that many
/// powers of 1024, and `KiB`, `MiB`, `GiB` and `TiB` mean the same thing. That is what DuckDB does
/// and it is wrong about what the SI prefixes mean, but a script that says `memory_limit='10GB'`
/// and gets 10 * 1024^3 from DuckDB has to get the same number here, and being right about the
/// prefix at the cost of a different answer to the same string is not a trade worth making.
///
/// Case does not matter and a space before the unit is allowed, because both appear in the wild.
///
/// # Errors
///
/// For text that is not a number, a unit that is not one of the eight, and a size that does not fit
/// in a `u64`.
pub fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim();
    let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c.is_whitespace());
    let unit = text[digits.len()..].trim().to_ascii_uppercase();
    let number: u64 = digits
        .trim()
        .parse()
        .map_err(|_| Error::invalid_input(format!("\"{text}\" is not a size")))?;
    let power = match unit.as_str() {
        "" | "B" => 0,
        "KB" | "KIB" => 1,
        "MB" | "MIB" => 2,
        "GB" | "GIB" => 3,
        "TB" | "TIB" => 4,
        other => {
            return Err(Error::invalid_input(format!(
                "\"{other}\" is not a unit, which is one of B, KB, MB, GB and TB"
            )));
        }
    };
    number
        .checked_mul(1024u64.pow(power))
        .ok_or_else(|| Error::invalid_input(format!("\"{text}\" is larger than a 64 bit size")))
}

/// A size in bytes, written the way a person reads one.
///
/// The largest unit that leaves a whole number, so that a limit set as `1GB` prints as `1GB` rather
/// than as a number nobody recognizes. A size that is not a whole number of any unit prints as
/// bytes, which is exact, because a rounded number in a configuration dump is a number somebody
/// will later compare against what they set.
fn format_size(bytes: u64) -> String {
    for (power, unit) in [(4, "TB"), (3, "GB"), (2, "MB"), (1, "KB")] {
        let scale = 1024u64.pow(power);
        if bytes >= scale && bytes % scale == 0 {
            return format!("{}{unit}", bytes / scale);
        }
    }
    format!("{bytes}B")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Config, format_size, parse_size};

    #[test]
    fn the_defaults_are_no_limits_and_every_core() {
        let config = Config::new();
        assert_eq!(config.memory_limit(), None);
        assert_eq!(config.query_timeout(), None);
        assert!(config.threads() >= 1);
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
    fn a_size_with_a_unit_is_that_many_powers_of_1024() {
        assert_eq!(parse_size("1024").expect("a number is bytes"), 1024);
        assert_eq!(parse_size("1KB").expect("a kilobyte"), 1024);
        assert_eq!(parse_size("1MB").expect("a megabyte"), 1024 * 1024);
        assert_eq!(parse_size("10GB").expect("ten gigabytes"), 10 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("1TB").expect("a terabyte"), 1024u64.pow(4));
    }

    #[test]
    fn the_two_spellings_of_a_gigabyte_are_the_same_number_here() {
        // DuckDB is wrong about what the SI prefix means and a script that says 10GB has to get the
        // same number from both engines, so this agreement is the point rather than an oversight.
        assert_eq!(parse_size("1GB").expect("a gigabyte"), parse_size("1GiB").expect("a gibibyte"));
    }

    #[test]
    fn case_and_a_space_before_the_unit_are_both_allowed() {
        let expected = 512 * 1024 * 1024;
        assert_eq!(parse_size("512mb").expect("lower case"), expected);
        assert_eq!(parse_size("512 MB").expect("a space"), expected);
        assert_eq!(parse_size("  512MiB  ").expect("surrounding space"), expected);
    }

    #[test]
    fn something_that_is_not_a_size_says_so() {
        assert!(parse_size("").is_err());
        assert!(parse_size("lots").is_err());
        assert!(parse_size("-1").is_err());
        assert!(parse_size("1.5GB").is_err());
        let error = parse_size("5PB").expect_err("petabytes are not a unit here");
        assert!(error.to_string().contains("not a unit"), "{error}");
        let error = parse_size("16777216TB").expect_err("that does not fit in a u64");
        assert!(error.to_string().contains("64 bit"), "{error}");
    }

    #[test]
    fn a_size_prints_back_as_the_unit_it_was_written_in() {
        assert_eq!(format_size(1024), "1KB");
        assert_eq!(format_size(10 * 1024 * 1024 * 1024), "10GB");
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
    fn the_defaults_print_as_the_absence_of_a_limit_rather_than_as_a_number() {
        let settings = Config::new().settings();
        assert_eq!(settings[0].1, "unlimited");
        assert_eq!(settings[2].1, "none");
    }
}

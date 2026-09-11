//! The measurement apparatus the in-repo benchmarks share.
//!
//! There are two of them now, the front end table in [`crate::bench`] and the kernel table in
//! [`crate::kernels`], and there will be more, because every layer of the engine that lands gets a
//! table next to it. What they must not have is two copies of the sampling rules, because the rules
//! are the reason the numbers can be compared at all and a second copy of them is a second set of
//! rules that drifts from the first one quietly.
//!
//! The rules come from `spec/15-rudb-bench.md`.
//!
//! Rule two says the median of at least five runs with the interquartile range, never a minimum. A
//! minimum is the run where the scheduler happened to leave you alone, and optimizing against it
//! optimizes for a machine nobody has. So every number is the median of [`SAMPLES`] samples, each
//! sample an inner loop calibrated to run for at least [`SAMPLE_NANOS`] nanoseconds, and the spread
//! is printed next to it rather than left out.
//!
//! Rule seven says one machine, so a table carries the line that says which one it was.
//!
//! Rule ten says a micro number never appears without the end to end number it explains. That one
//! is not enforceable from here, because what the end to end number is depends on what is being
//! measured, so each table says its own version of it in its own caveats and this file only
//! supplies the two lines that are the same everywhere.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

/// How many samples each number is the median of. Rule two says at least five. Nine is used
/// because it makes the two quartiles land on real samples with three samples on either side of
/// the median, which five does not.
pub(crate) const SAMPLES: usize = 9;

/// How long one sample's inner loop has to run before the sample counts.
///
/// `Instant` on the platforms this runs on resolves to somewhere between tens and hundreds of
/// nanoseconds, and the fastest thing in either table is a few hundred nanoseconds. Timing one call
/// would be measuring the clock. Ten milliseconds is four to five orders of magnitude above the
/// resolution, which puts the clock's contribution below the rounding in the last printed digit.
pub(crate) const SAMPLE_NANOS: f64 = 10_000_000.0;

/// A ceiling on the calibration so a pathological case cannot spin forever.
const MAX_ITERS: u32 = 1 << 26;

/// A median with the spread that says whether to believe it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Number {
    /// Nanoseconds per call, the median of [`SAMPLES`] samples.
    pub(crate) median: f64,
    /// The interquartile range, in nanoseconds.
    pub(crate) iqr: f64,
}

impl Number {
    /// The interquartile range as a fraction of the median, which is the form that can be compared
    /// between a row that takes 200 nanoseconds and a row that takes 20 microseconds.
    pub(crate) fn relative(self) -> f64 {
        if self.median == 0.0 { 0.0 } else { self.iqr / self.median }
    }

    /// The median divided by a row count, which is the unit every kernel number is quoted in.
    pub(crate) fn per(self, rows: usize) -> f64 {
        if rows == 0 { 0.0 } else { self.median / rows as f64 }
    }
}

/// Run one thing enough times to say how long it takes.
pub(crate) fn time(mut once: impl FnMut()) -> Number {
    let iters = calibrate(&mut once);
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..iters {
            once();
        }
        samples.push(start.elapsed().as_nanos() as f64 / f64::from(iters));
    }
    samples.sort_by(f64::total_cmp);
    Number {
        median: percentile(&samples, 50),
        iqr: percentile(&samples, 75) - percentile(&samples, 25),
    }
}

/// How many calls it takes to fill one sample.
///
/// Doubling from one rather than dividing an estimate, because the estimate would come from a
/// single timed call, which is the measurement this whole file exists to avoid making. The
/// discarded calibration runs are also the warmup, which is why there is not a separate one.
fn calibrate(once: &mut impl FnMut()) -> u32 {
    let mut iters: u32 = 1;
    loop {
        let start = Instant::now();
        for _ in 0..iters {
            once();
        }
        if start.elapsed().as_nanos() as f64 >= SAMPLE_NANOS || iters >= MAX_ITERS {
            return iters;
        }
        iters = iters.saturating_mul(2);
    }
}

/// The nearest rank percentile of a sorted slice.
///
/// Nearest rank rather than interpolated, so every quartile printed is a number that some sample
/// actually produced rather than a point between two that were. With nine samples the quartiles are
/// the third and the seventh, which leaves three real samples outside each of them.
pub(crate) fn percentile(sorted: &[f64], p: usize) -> f64 {
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// Nanoseconds in the unit a person reads.
pub(crate) fn show(nanos: f64) -> String {
    if nanos >= 1_000_000.0 {
        format!("{:.3}ms", nanos / 1e6)
    } else if nanos >= 1_000.0 {
        format!("{:.3}us", nanos / 1e3)
    } else {
        format!("{nanos:.1}ns")
    }
}

/// What produced these numbers, which rule one says has to travel with them.
pub(crate) fn build_line() -> String {
    let rustc = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map_or_else(
            || "an unknown rustc".to_string(),
            |out| String::from_utf8_lossy(&out.stdout).trim().to_string(),
        );
    format!(
        "{rustc}, bench profile, {} {}, rudb {}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        env!("CARGO_PKG_VERSION")
    )
}

/// Re-run one of these tasks under the `bench` profile.
///
/// A debug build of any of them would be measuring the borrow checker's leftovers rather than the
/// thing named at the top of the table, and a number from one would be wrong by a factor that
/// changes with every edit. `cargo xtask` is an alias for a debug `cargo run`, so rather than
/// documenting a longer command that people will get wrong, the task re-runs itself here. The
/// discriminator at the call site is `debug_assertions` rather than a flag, so there is no way to
/// ask for the number that does not mean anything.
pub(crate) fn rebuild(root: &Path, task: &str, rest: &[String]) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    println!("building xtask under the bench profile, because a debug number is not a number");
    let mut args: Vec<String> =
        ["run", "--quiet", "--profile", "bench", "--package", "xtask", "--", task]
            .iter()
            .map(|&arg| arg.to_string())
            .collect();
    args.extend(rest.iter().cloned());
    let status = Command::new(cargo)
        .current_dir(root)
        .args(&args)
        .status()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if status.success() { Ok(()) } else { Err("the bench build did not run".to_string()) }
}

/// The two caveats that are the same under every table here.
///
/// Written as lines that are printed every time rather than as a paragraph in a document, for the
/// same reason `rudb-bench` prints the reasons a result may not be published under every result: a
/// caveat that lives somewhere else is a caveat nobody reads.
pub(crate) fn shared_caveats() -> Vec<String> {
    vec![
        format!("  rule two: every number is the median of {SAMPLES} samples, each an inner loop"),
        format!(
            "    calibrated to run for at least {}ms. IQR is the interquartile range as a",
            (SAMPLE_NANOS / 1e6) as u64
        ),
        "    percentage of its median, and double figures means the machine was busy and the"
            .to_string(),
        "    run should be taken again.".to_string(),
        "  rule seven: this is one machine, so do not put it next to a number from another."
            .to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{Number, percentile, shared_caveats, show, time};

    #[test]
    fn quartiles_land_on_samples_that_really_happened() {
        let sorted = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0];
        assert!((percentile(&sorted, 25) - 3.0).abs() < f64::EPSILON);
        assert!((percentile(&sorted, 50) - 5.0).abs() < f64::EPSILON);
        assert!((percentile(&sorted, 75) - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_single_sample_still_has_a_percentile() {
        assert!((percentile(&[4.0], 25) - 4.0).abs() < f64::EPSILON);
        assert!((percentile(&[4.0], 75) - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_spread_is_relative_to_the_median_and_not_absolute() {
        let slow = Number { median: 20_000.0, iqr: 2_000.0 };
        let fast = Number { median: 200.0, iqr: 20.0 };
        assert!((slow.relative() - fast.relative()).abs() < f64::EPSILON);
    }

    #[test]
    fn a_median_of_zero_does_not_produce_a_spread_of_infinity() {
        assert!((Number { median: 0.0, iqr: 0.0 }.relative()).abs() < f64::EPSILON);
    }

    #[test]
    fn a_chunk_of_no_rows_does_not_produce_a_time_per_row_of_infinity() {
        assert!((Number { median: 500.0, iqr: 0.0 }.per(0)).abs() < f64::EPSILON);
    }

    #[test]
    fn the_units_change_where_the_numbers_do() {
        assert_eq!(show(999.4), "999.4ns");
        assert_eq!(show(1_000.0), "1.000us");
        assert_eq!(show(1_500_000.0), "1.500ms");
    }

    #[test]
    fn a_time_that_is_measured_is_positive_and_has_a_spread() {
        // Runs the real apparatus on the cheapest real thing there is, which is what catches a
        // calibration loop that never terminates or a percentile that indexes off the end.
        let mut total: u64 = 0;
        let number = time(|| {
            total = std::hint::black_box(total).wrapping_add(1);
        });
        assert!(number.median > 0.0);
        assert!(number.iqr >= 0.0);
    }

    #[test]
    fn the_rules_that_every_table_shares_are_printed_and_not_remembered() {
        let text = shared_caveats().join("\n");
        assert!(text.contains("rule two"));
        assert!(text.contains("rule seven"));
    }
}

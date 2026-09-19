//! How far the plan's row counts were from the rows the run produced, kept apart by class.
//!
//! The q-error is the ratio between an estimate and the truth, larger over smaller, so one is
//! perfect and it grows the same amount whether the estimate was high or low. On its own it is a
//! number about a query. Split by [`Class`] it is a number about the statistics layer, which is what
//! `spec/stats/09-measurement.md` section 9.5 asks for and what the P0 milestone's third exit
//! criterion is: the q-error histogram published per class.
//!
//! The split is the point. An estimate that is an order of magnitude out is an ordinary bad guess
//! and the work to fix it is to find a better source. An exact number that is an order of magnitude
//! out is a different thing entirely, because something claimed to have counted and was wrong, and
//! the work to fix that is to find the bug. Reporting both in one histogram averages the two
//! together and hides the second inside the first, which is the failure this module exists to
//! prevent.
//!
//! Nothing here reads a class back into a decision. It is a report, and the numbers in it are what a
//! milestone is measured by rather than what a plan turns on.

use std::fmt;

use rudb_common::{Class, Direction};

use crate::commas;
use crate::document::{Document, Operator};

/// The two sides of a q-error, larger first, ready to divide.
///
/// A zero on either side is an estimate of nothing or a result of nothing, and dividing by it says
/// infinity when what it means is that one of the two is a special case. One row is the floor, which
/// makes the ratio the size of the other side.
#[must_use]
pub fn q_error(estimated: u64, produced: u64) -> (u128, u128) {
    let (high, low) = if estimated > produced {
        (u128::from(estimated), u128::from(produced))
    } else {
        (u128::from(produced), u128::from(estimated))
    };
    (high, low.max(1))
}

/// The word for a class, without the certificate on the end of it.
///
/// [`Class`] prints its bound and its direction, which is what a plan line wants and what a bucket
/// label does not: `certified at most 100.00%` is a fine thing to read next to one number and a poor
/// thing to group forty of them under.
#[must_use]
pub const fn word(class: Option<Class>) -> &'static str {
    match class {
        Some(Class::Exact) => "exact",
        Some(Class::Certified { .. }) => "certified",
        Some(Class::Estimated) => "estimated",
        None => "unknown",
    }
}

/// One class's q-errors, in buckets.
///
/// Buckets rather than a mean, because the distribution is the finding. A class whose estimates are
/// all within a factor of two and a class where half are perfect and half are out by a thousand have
/// similar means and nothing else in common, and it is the second one that has a bug in it.
///
/// The boundaries are powers of ten with an exact bucket at the bottom. Exact is its own bucket
/// rather than the bottom of the first range because the difference between a q-error of one and a
/// q-error of one point one is the difference between counting and guessing well.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spread {
    exact: u64,
    two: u64,
    ten: u64,
    hundred: u64,
    thousand: u64,
    beyond: u64,
    /// The largest q-error seen, in tenths, so that the whole type compares by value and two
    /// machines that agree on the rows agree on the number.
    worst: u128,
}

impl Spread {
    /// An empty spread.
    #[must_use]
    pub const fn new() -> Self {
        Self { exact: 0, two: 0, ten: 0, hundred: 0, thousand: 0, beyond: 0, worst: 0 }
    }

    /// Counts one estimate against what the run produced.
    pub fn record(&mut self, estimated: u64, produced: u64) {
        let (high, low) = q_error(estimated, produced);
        match high {
            _ if high == low => self.exact += 1,
            _ if high <= low * 2 => self.two += 1,
            _ if high <= low * 10 => self.ten += 1,
            _ if high <= low * 100 => self.hundred += 1,
            _ if high <= low * 1_000 => self.thousand += 1,
            _ => self.beyond += 1,
        }
        self.worst = self.worst.max((high * 10 + low / 2) / low);
    }

    /// How many estimates are in it.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.exact + self.two + self.ten + self.hundred + self.thousand + self.beyond
    }

    /// How many of them the run agreed with exactly.
    #[must_use]
    pub const fn perfect(self) -> u64 {
        self.exact
    }

    /// The largest q-error in it, in tenths, and zero when it is empty.
    #[must_use]
    pub const fn worst(self) -> u128 {
        self.worst
    }

    /// Adds another spread into this one, for a report over a suite rather than a query.
    pub fn merge(&mut self, other: Self) {
        self.exact += other.exact;
        self.two += other.two;
        self.ten += other.ten;
        self.hundred += other.hundred;
        self.thousand += other.thousand;
        self.beyond += other.beyond;
        self.worst = self.worst.max(other.worst);
    }
}

impl fmt::Display for Spread {
    /// The non empty buckets and the worst of them, which is the line `EXPLAIN` prints.
    ///
    /// Empty buckets are left out rather than printed as zero, because a class with two estimates in
    /// it would otherwise be four zeroes and two numbers and a reader would have to find them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut written = false;
        for (count, label) in [
            (self.exact, "at 1"),
            (self.two, "up to 2"),
            (self.ten, "up to 10"),
            (self.hundred, "up to 100"),
            (self.thousand, "up to 1,000"),
            (self.beyond, "over 1,000"),
        ] {
            if count == 0 {
                continue;
            }
            if written {
                f.write_str(", ")?;
            }
            written = true;
            write!(f, "{} {label}", commas(count))?;
        }
        if !written {
            return f.write_str("nothing measured");
        }
        if self.worst > 10 {
            write!(f, ", worst {}", tenths(self.worst))?;
        }
        Ok(())
    }
}

/// A number of tenths as a decimal, which is how a q-error is printed everywhere.
#[must_use]
pub fn tenths(value: u128) -> String {
    format!("{}.{}", commas(u64::try_from(value / 10).unwrap_or(u64::MAX)), value % 10)
}

/// Every class's spread, which is the histogram the milestone is measured by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QErrors {
    exact: Spread,
    certified: Spread,
    estimated: Spread,
}

impl QErrors {
    /// An empty histogram.
    #[must_use]
    pub const fn new() -> Self {
        Self { exact: Spread::new(), certified: Spread::new(), estimated: Spread::new() }
    }

    /// Counts one estimate, and ignores one that had no class because there was no number.
    pub fn record(&mut self, class: Option<Class>, estimated: u64, produced: u64) {
        match class {
            Some(Class::Exact) => self.exact.record(estimated, produced),
            Some(Class::Certified { .. }) => self.certified.record(estimated, produced),
            Some(Class::Estimated) => self.estimated.record(estimated, produced),
            None => {}
        }
    }

    /// The three spreads with the word for each, in the order a report reads them.
    #[must_use]
    pub const fn named(self) -> [(&'static str, Spread); 3] {
        [("exact", self.exact), ("certified", self.certified), ("estimated", self.estimated)]
    }

    /// The exact estimates, whose q-error is one or there is a bug.
    #[must_use]
    pub const fn exact(self) -> Spread {
        self.exact
    }

    /// The certified estimates.
    #[must_use]
    pub const fn certified(self) -> Spread {
        self.certified
    }

    /// The guesses.
    #[must_use]
    pub const fn estimated(self) -> Spread {
        self.estimated
    }

    /// How many estimates the run measured.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.exact.total() + self.certified.total() + self.estimated.total()
    }

    /// Adds another histogram into this one.
    pub fn merge(&mut self, other: Self) {
        self.exact.merge(other.exact);
        self.certified.merge(other.certified);
        self.estimated.merge(other.estimated);
    }
}

impl Document {
    /// The q-error of every operator the optimizer had a number for, split by class.
    ///
    /// Operators the optimizer had nothing for are left out rather than counted as infinitely wrong.
    /// A missing estimate is already counted, as `unknown`, in the class histogram beside this one,
    /// and a q-error against a number nobody produced is not a measurement of anything.
    #[must_use]
    pub fn q_errors(&self) -> QErrors {
        let mut errors = QErrors::new();
        for operator in &self.operators {
            let Some(estimated) = operator.estimated_rows else { continue };
            errors.record(operator.estimate_class, estimated, operator.rows_out);
        }
        errors
    }
}

/// Whether the run produced more rows than an operator's estimate said could exist.
///
/// Only the classes that claim something. An estimate claims nothing, so nothing can contradict it
/// and the ordinary q-error warning is all there is to say. `Exact` claims the number is the number.
/// `Certified` claims the truth is no further from it than the bound says.
///
/// # Why only one direction
///
/// A plan's row count is a ceiling on what execution produces and not a prediction of it, because
/// this engine has three ways to produce fewer rows than the plan says and no way at all to produce
/// more. A limit stops a pipeline part way. A filter the scan applies itself means the scan's rows
/// are what came out of the filter. A hash join hands the scan under its driving side the range and
/// the key filter of the side that finished first, and `crates/rudb-exec/src/sideways.rs` is a whole
/// module about how many rows that is allowed to drop: on TPC-H q12 it takes the scan of `orders`
/// from the one and a half million rows the catalog counted down to the forty four thousand that can
/// match, and the catalog was right about both.
///
/// So under production is execution working and over production is arithmetic that does not hold.
/// A rule that reported both would report a wrong answer bug on most of TPC-H and would be turned
/// off within the week, which is worse than a rule that catches half of what it names.
#[must_use]
pub(crate) fn contradicted(operator: &Operator) -> Option<(u64, u64)> {
    let estimated = operator.estimated_rows?;
    let produced = operator.rows_out;
    let ceiling = match operator.estimate_class? {
        Class::Exact => estimated,
        Class::Certified { bound, direction } => top(estimated, bound, direction),
        Class::Estimated => return None,
    };
    (produced > ceiling).then_some((estimated, produced))
}

/// The most rows a certificate says there can be.
///
/// The direction says which side of the value the truth is on and the bound says how far from it, so
/// `at most` is the value itself and `at least` is the value plus the bound. A bound of one, which is
/// what a limit's ceiling carries, is the single sided claim it is meant to be and this is the side
/// it claims.
fn top(value: u64, bound: f64, direction: Direction) -> u64 {
    #[expect(clippy::cast_precision_loss, reason = "a certificate is checked, not answered from")]
    let raised = value as f64 * (1.0 + bound);
    match direction {
        Direction::AtMost => value,
        Direction::AtLeast | Direction::Within => ceiling(raised),
    }
}

/// The smallest row count at or above a bound, and every row count for one that overflows.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value came from a row count and both ends are clamped"
)]
fn ceiling(bound: f64) -> u64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "the comparison is against the top of the range"
    )]
    if bound >= u64::MAX as f64 {
        u64::MAX
    } else if bound <= 0.0 {
        0
    } else {
        bound.ceil() as u64
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::{Class, Direction};

    use super::{QErrors, Spread, contradicted, q_error, word};
    use crate::document::{Document, Operator};

    fn operator(class: Class, estimated: u64, produced: u64) -> Operator {
        let mut operator = Operator::new(0, 0, "Scan");
        operator.estimated_rows = Some(estimated);
        operator.estimate_class = Some(class);
        operator.rows_out = produced;
        operator
    }

    #[test]
    fn a_perfect_estimate_is_its_own_bucket() {
        let mut spread = Spread::new();
        spread.record(100, 100);
        assert_eq!(spread.total(), 1);
        assert_eq!(spread.perfect(), 1);
        assert_eq!(spread.to_string(), "1 at 1");
    }

    #[test]
    fn the_buckets_are_powers_of_ten_and_the_boundaries_land_in_the_lower_one() {
        let mut spread = Spread::new();
        for (estimated, produced) in [(10, 20), (10, 100), (10, 1_000), (10, 10_000)] {
            spread.record(estimated, produced);
        }
        assert_eq!(
            spread.to_string(),
            "1 up to 2, 1 up to 10, 1 up to 100, 1 up to 1,000, worst 1,000.0"
        );
    }

    #[test]
    fn an_estimate_of_nothing_is_measured_against_one_row_rather_than_against_zero() {
        // Otherwise the ratio is an infinity, and an operator that was estimated at nothing and
        // produced three rows is off by three rather than by infinitely much.
        assert_eq!(q_error(0, 3), (3, 1));
        assert_eq!(q_error(3, 0), (3, 1));
        let mut spread = Spread::new();
        spread.record(0, 3);
        assert_eq!(spread.to_string(), "1 up to 10, worst 3.0");
    }

    #[test]
    fn the_classes_are_counted_apart_and_the_word_leaves_the_certificate_off() {
        let mut errors = QErrors::new();
        errors.record(Some(Class::Exact), 100, 100);
        errors.record(Some(Class::Estimated), 100, 1);
        errors.record(None, 100, 1);
        assert_eq!(errors.total(), 2);
        assert_eq!(errors.exact().to_string(), "1 at 1");
        assert_eq!(errors.estimated().to_string(), "1 up to 100, worst 100.0");
        assert_eq!(
            word(Some(Class::Certified { bound: 0.5, direction: Direction::AtMost })),
            "certified"
        );
    }

    #[test]
    fn a_document_measures_the_operators_it_had_a_number_for() {
        let mut document = Document::new("select 1");
        document.operators.push(operator(Class::Exact, 8, 8));
        document.operators.push(Operator::new(1, 0, "Filter"));
        let errors = document.q_errors();
        assert_eq!(errors.total(), 1);
        assert_eq!(errors.exact().perfect(), 1);
    }

    #[test]
    fn an_exact_count_that_the_run_produced_more_rows_than_is_a_contradiction() {
        assert_eq!(contradicted(&operator(Class::Exact, 8, 9)), Some((8, 9)));
        assert_eq!(contradicted(&operator(Class::Exact, 8, 8)), None);
    }

    #[test]
    fn producing_fewer_rows_than_an_exact_count_is_execution_and_not_arithmetic() {
        // A limit, a filter the scan applied itself and the key filter a join hands down all take a
        // scan below the count the catalog holds, and the count was right in every one of them.
        assert_eq!(contradicted(&operator(Class::Exact, 1_500_000, 44_275)), None);
    }

    #[test]
    fn a_certificate_is_contradicted_by_the_far_side_of_its_bound() {
        let ceiling = Class::Certified { bound: 1.0, direction: Direction::AtMost };
        assert_eq!(contradicted(&operator(ceiling, 10, 11)), Some((10, 11)));
        assert_eq!(contradicted(&operator(ceiling, 10, 0)), None);
        // At least a hundred within a fifth is at most a hundred and twenty, so the hundred and
        // twentieth row is inside it and the next one is not.
        let lower = Class::Certified { bound: 0.2, direction: Direction::AtLeast };
        assert_eq!(contradicted(&operator(lower, 100, 120)), None);
        assert_eq!(contradicted(&operator(lower, 100, 121)), Some((100, 121)));
    }

    #[test]
    fn a_guess_claims_nothing_so_nothing_contradicts_it() {
        assert_eq!(contradicted(&operator(Class::Estimated, 10, 1_000_000)), None);
    }
}

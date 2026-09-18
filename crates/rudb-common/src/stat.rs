//! What the statistics layer answers with, and how much of it is safe to believe.
//!
//! One type comes back from every statistics question, [`Stat`], and it is either a value with a
//! class attached or it is [`Stat::Unknown`]. `Unknown` is an answer rather than a failure, and it
//! is not zero, not one and not a default constant. The caller decides what to do without a number,
//! which is a decision the caller can make well and this layer cannot. A subsystem that invents a
//! number instead of saying `Unknown` produces plans that are confidently wrong, and the reason
//! those plans are hard to fix later is that nothing in them records which number was made up.
//!
//! Here at rank 0 because the answers are produced at rank 5 by the storage layer, at rank 8 by the
//! catalog, at rank 11 by the optimizer and at rank 12 by an operator that has just finished its
//! build side, and consumed in most of the same places. See `spec/stats/04-in-memory.md` section
//! 4.1, which this module is the implementation of.
//!
//! # The class is the point
//!
//! [`Class`] is what makes three optimizations legal rather than merely attractive.
//! `spec/stats/05-every-query.md` section 5.10 states the rule that constant folding on a statistic
//! is legal only when the statistic is [`Class::Exact`], and [`Stat::exact_value`] is the one way to
//! ask for a number under that rule. Narrowing a `DECIMAL(15,2)` sum into an `i64` is correct
//! because the bound is exact and wrong if it is a guess. Seeding a top-n threshold from a quantile
//! is correct because the certificate says the seed cannot exclude a qualifying row, which is what
//! [`Class::Certified`] carries and what an estimate does not have.
//!
//! [`Class::Estimated`] carries its [`Source`] because `EXPLAIN` prints it, and because a bad plan
//! is diagnosed by asking which number was wrong and where it came from. A sketch that was merged
//! badly and a default constant that nobody noticed produce the same wrong row count and want
//! different fixes.
//!
//! # What the histogram reads today
//!
//! The one producer wired up is the optimizer's row count estimator, and the [`Classes`] histogram
//! it fills is the G0 baseline that the rest of the series moves. It does not read all `Unknown`.
//! A base table scan gets its count from the catalog and is [`Class::Exact`], a `LIMIT` over an
//! input nobody counted is [`Class::Certified`] because the limit is a real ceiling, a table
//! function or a dependent join is `Unknown`, and everything above the first filter, group by or
//! join is [`Class::Estimated`] from [`Source::Constant`], which is the literal selectivity guess
//! the estimator has always used. So the honest zero is the share of decisions resting on a
//! constant rather than a hundred percent unknown, and that share is what the series drives down.
//!
//! Nothing reads the class back yet. It is written into `EXPLAIN` and into the metrics document so
//! that the ablation of `spec/stats/09-measurement.md` section 9.3 has a number to compare against,
//! and no plan choice turns on it until a later milestone puts real statistics behind it.

use std::fmt;

/// One statistic, or the honest absence of one.
///
/// `Known` carries the value and how much to trust it. `Unknown` is what a question has no answer
/// to, and the whole design rests on callers treating it as a case rather than as a zero.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stat<T> {
    /// A value, and what kind of knowledge it is.
    Known {
        /// The number, bound, flag or set the question asked for.
        value: T,
        /// How much of it is known rather than guessed.
        class: Class,
    },
    /// No answer. Not a zero, not a one and not a default.
    Unknown,
}

/// How much of an answer is knowledge.
///
/// Three cases, ordered by how much they permit. `Exact` permits anything, including changing an
/// answer by folding a predicate away. `Certified` permits a decision whose fallback survives being
/// wrong, which is the case a safe bound is for. `Estimated` permits choosing between two plans that
/// produce the same rows, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Class {
    /// The value is the value. A count that was counted, a null count that was maintained, a
    /// minimum that was compared.
    Exact,
    /// The value is wrong by no more than `bound`, as a fraction of itself, and the structure that
    /// produced it can prove that.
    ///
    /// A quantile summary with an epsilon is the usual source. The number is what makes a threshold
    /// safe to seed from, so a producer that cannot state one should say `Estimated` instead of
    /// picking a bound that sounds about right.
    Certified {
        /// The relative error bound, where `0.01` is one percent.
        bound: f64,
    },
    /// The value is a guess, and this is where it came from.
    Estimated {
        /// What produced the guess, because `EXPLAIN` prints it.
        source: Source,
    },
}

/// Where an estimate came from.
///
/// Printed by `EXPLAIN`, so these are the words a person reads when a plan went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// A distinct-count sketch, outside the regime where it is exact.
    Sketch,
    /// A quantile summary, read without its certificate.
    Quantile,
    /// The stored sample.
    Sample,
    /// A minimum and a maximum, interpolated between.
    Zone,
    /// A dictionary's size standing in for a distinct count.
    Dictionary,
    /// A frequency synopsis.
    Synopsis,
    /// A propagation rule over another operator's statistics.
    Propagation,
    /// Something a previous execution measured, from the observation log.
    Observation,
    /// A constant in the source. The weakest answer that is not `Unknown`, and the one worth
    /// finding in an `EXPLAIN` because it means nobody had a number here at all.
    Constant,
}

impl Source {
    /// The word `EXPLAIN` prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sketch => "sketch",
            Self::Quantile => "quantile",
            Self::Sample => "sample",
            Self::Zone => "zone",
            Self::Dictionary => "dictionary",
            Self::Synopsis => "synopsis",
            Self::Propagation => "propagation",
            Self::Observation => "observation",
            Self::Constant => "constant",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Class {
    /// Whether this is knowledge rather than a guess, which is the test the folding rule applies.
    #[must_use]
    pub const fn is_exact(self) -> bool {
        matches!(self, Self::Exact)
    }

    /// The class of an answer computed from two others.
    ///
    /// Exact combined with anything else is the anything else, which is the honest direction and the
    /// easy one to get backwards. Two certified bounds add, because a combination of two bounded
    /// errors is bounded by their sum, and the sum saturates at one because a bound of more than a
    /// hundred percent says nothing that `Estimated` does not say. Anything involving an estimate is
    /// an estimate, and the source of a combination is [`Source::Propagation`] unless one of the two
    /// was a bare constant, in which case the answer is as weak as the constant was.
    #[must_use]
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Exact, Self::Exact) => Self::Exact,
            (Self::Exact, class) | (class, Self::Exact) => class,
            (Self::Certified { bound: left }, Self::Certified { bound: right }) => {
                Self::Certified { bound: (left + right).min(1.0) }
            }
            (Self::Estimated { source: Source::Constant }, _)
            | (_, Self::Estimated { source: Source::Constant }) => {
                Self::Estimated { source: Source::Constant }
            }
            _ => Self::Estimated { source: Source::Propagation },
        }
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact => f.write_str("exact"),
            Self::Certified { bound } => write!(f, "certified to {:.2}%", bound * 100.0),
            Self::Estimated { source } => write!(f, "estimated from {source}"),
        }
    }
}

impl<T> Stat<T> {
    /// A value that was counted, compared or maintained rather than guessed.
    pub const fn exact(value: T) -> Self {
        Self::Known { value, class: Class::Exact }
    }

    /// A value wrong by no more than `bound` as a fraction of itself.
    pub const fn certified(value: T, bound: f64) -> Self {
        Self::Known { value, class: Class::Certified { bound } }
    }

    /// A guess, and where it came from.
    pub const fn estimated(value: T, source: Source) -> Self {
        Self::Known { value, class: Class::Estimated { source } }
    }

    /// Whether there is an answer at all.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        matches!(self, Self::Known { .. })
    }

    /// Whether there is no answer.
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// The value, whatever its class, for a decision that only chooses between equivalent plans.
    #[must_use]
    pub const fn value(&self) -> Option<&T> {
        match self {
            Self::Known { value, .. } => Some(value),
            Self::Unknown => None,
        }
    }

    /// The value, but only when it is exact.
    ///
    /// The one door for a decision that changes an answer if the number is wrong. Folding a
    /// predicate away, narrowing arithmetic, dropping an aggregate: all of them ask here, and all of
    /// them take today's path when the answer is `None`. See `spec/stats/05-every-query.md` section
    /// 5.10, which is where the rule is stated and where the warning about breaking it in good faith
    /// is written down.
    #[must_use]
    pub const fn exact_value(&self) -> Option<&T> {
        match self {
            Self::Known { value, class: Class::Exact } => Some(value),
            _ => None,
        }
    }

    /// How much of the answer is knowledge, or `None` when there is no answer.
    #[must_use]
    pub const fn class(&self) -> Option<Class> {
        match self {
            Self::Known { class, .. } => Some(*class),
            Self::Unknown => None,
        }
    }

    /// The value, or what the caller decided to do without one.
    #[must_use]
    pub fn unwrap_or(self, default: T) -> T {
        match self {
            Self::Known { value, .. } => value,
            Self::Unknown => default,
        }
    }

    /// The same answer about a different quantity, with the class carried across unchanged.
    ///
    /// For a transformation that cannot lose knowledge, such as reading a row count as a byte count
    /// through a fixed width. A transformation that does lose knowledge should build its answer with
    /// the class it deserves rather than mapping.
    #[must_use]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Stat<U> {
        match self {
            Self::Known { value, class } => Stat::Known { value: f(value), class },
            Self::Unknown => Stat::Unknown,
        }
    }

    /// An answer computed from two, unknown when either is unknown, classed by [`Class::combine`].
    #[must_use]
    pub fn zip<U, V>(self, other: Stat<U>, f: impl FnOnce(T, U) -> V) -> Stat<V> {
        match (self, other) {
            (
                Self::Known { value: left, class: first },
                Stat::Known { value: right, class: second },
            ) => Stat::Known { value: f(left, right), class: first.combine(second) },
            _ => Stat::Unknown,
        }
    }
}

impl<T> Default for Stat<T> {
    /// `Unknown`, because a statistic nobody filled in is a statistic nobody knows.
    fn default() -> Self {
        Self::Unknown
    }
}

impl<T: fmt::Display> fmt::Display for Stat<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Known { value, class } => write!(f, "{value} ({class})"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// How many decisions were made on what.
///
/// The class histogram of `spec/stats/09-measurement.md` section 9.5. For a whole suite, the
/// fraction of the planner's decisions that were exact, certified, estimated or unknown, which is
/// the direct measurement of whether the statistics layer is doing its job. It is more diagnostic
/// than q-error for the first several milestones, because early on the estimates are bad for the
/// boring reason that there are none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Classes {
    exact: u64,
    certified: u64,
    estimated: u64,
    unknown: u64,
}

impl Classes {
    /// An empty histogram.
    #[must_use]
    pub const fn new() -> Self {
        Self { exact: 0, certified: 0, estimated: 0, unknown: 0 }
    }

    /// Counts one decision.
    pub fn record<T>(&mut self, stat: &Stat<T>) {
        self.record_class(stat.class());
    }

    /// Counts one decision whose class is already in hand.
    pub fn record_class(&mut self, class: Option<Class>) {
        match class {
            Some(Class::Exact) => self.exact += 1,
            Some(Class::Certified { .. }) => self.certified += 1,
            Some(Class::Estimated { .. }) => self.estimated += 1,
            None => self.unknown += 1,
        }
    }

    /// Decisions made on an exact number.
    #[must_use]
    pub const fn exact(self) -> u64 {
        self.exact
    }

    /// Decisions made on a bounded number.
    #[must_use]
    pub const fn certified(self) -> u64 {
        self.certified
    }

    /// Decisions made on a guess.
    #[must_use]
    pub const fn estimated(self) -> u64 {
        self.estimated
    }

    /// Decisions made with no number at all.
    #[must_use]
    pub const fn unknown(self) -> u64 {
        self.unknown
    }

    /// Every decision counted.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.exact + self.certified + self.estimated + self.unknown
    }

    /// The fraction of decisions that had a number of any kind behind them, zero for an empty
    /// histogram.
    #[must_use]
    pub fn known_share(self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        #[expect(clippy::cast_precision_loss, reason = "a share is a report and not an answer")]
        {
            (total - self.unknown) as f64 / total as f64
        }
    }

    /// Adds another histogram into this one, for a report that covers a suite rather than a query.
    pub fn merge(&mut self, other: Self) {
        self.exact += other.exact;
        self.certified += other.certified;
        self.estimated += other.estimated;
        self.unknown += other.unknown;
    }
}

impl fmt::Display for Classes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "exact {}, certified {}, estimated {}, unknown {}",
            self.exact, self.certified, self.estimated, self.unknown
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_the_default() {
        let stat: Stat<u64> = Stat::default();
        assert!(stat.is_unknown());
        assert_eq!(stat.value(), None);
        assert_eq!(stat.class(), None);
        assert_eq!(stat.unwrap_or(7), 7);
    }

    #[test]
    fn only_an_exact_answer_comes_back_from_exact_value() {
        assert_eq!(Stat::exact(4_u64).exact_value(), Some(&4));
        assert_eq!(Stat::certified(4_u64, 0.01).exact_value(), None);
        assert_eq!(Stat::estimated(4_u64, Source::Sketch).exact_value(), None);
        assert_eq!(Stat::<u64>::Unknown.exact_value(), None);
    }

    #[test]
    fn a_class_degrades_when_it_is_combined() {
        assert_eq!(Class::Exact.combine(Class::Exact), Class::Exact);
        assert_eq!(
            Class::Exact.combine(Class::Estimated { source: Source::Sample }),
            Class::Estimated { source: Source::Sample }
        );
        assert_eq!(
            Class::Certified { bound: 0.01 }.combine(Class::Certified { bound: 0.02 }),
            Class::Certified { bound: 0.03 }
        );
        assert_eq!(
            Class::Estimated { source: Source::Sketch }
                .combine(Class::Estimated { source: Source::Sample }),
            Class::Estimated { source: Source::Propagation }
        );
        assert_eq!(
            Class::Estimated { source: Source::Sketch }
                .combine(Class::Estimated { source: Source::Constant }),
            Class::Estimated { source: Source::Constant }
        );
    }

    #[test]
    fn a_certified_bound_saturates_rather_than_growing_past_everything() {
        assert_eq!(
            Class::Certified { bound: 0.8 }.combine(Class::Certified { bound: 0.7 }),
            Class::Certified { bound: 1.0 }
        );
    }

    #[test]
    fn zip_is_unknown_when_either_side_is() {
        let known = Stat::exact(10_u64);
        let unknown = Stat::<u64>::Unknown;
        assert_eq!(known.zip(unknown, |left, right| left + right), Stat::Unknown);
        assert_eq!(unknown.zip(known, |left, right| left + right), Stat::Unknown);
        assert_eq!(known.zip(Stat::exact(5), |left, right| left + right), Stat::exact(15));
    }

    #[test]
    fn map_carries_the_class() {
        let bytes = Stat::certified(100_u64, 0.05).map(|rows| rows * 8);
        assert_eq!(bytes, Stat::certified(800, 0.05));
    }

    #[test]
    fn the_histogram_counts_what_it_was_shown() {
        let mut classes = Classes::new();
        classes.record(&Stat::exact(1_u64));
        classes.record(&Stat::certified(1_u64, 0.1));
        classes.record(&Stat::estimated(1_u64, Source::Zone));
        classes.record(&Stat::<u64>::Unknown);
        assert_eq!(classes.total(), 4);
        assert_eq!(classes.exact(), 1);
        assert_eq!(classes.known_share(), 0.75);
        assert_eq!(classes.to_string(), "exact 1, certified 1, estimated 1, unknown 1");

        let mut all = Classes::new();
        all.merge(classes);
        all.merge(classes);
        assert_eq!(all.total(), 8);
    }

    #[test]
    fn an_empty_histogram_knows_nothing_rather_than_everything() {
        assert_eq!(Classes::new().known_share(), 0.0);
        assert_eq!(Classes::new().total(), 0);
    }

    #[test]
    fn an_answer_prints_its_provenance() {
        assert_eq!(Stat::exact(12_u64).to_string(), "12 (exact)");
        assert_eq!(Stat::certified(12_u64, 0.025).to_string(), "12 (certified to 2.50%)");
        assert_eq!(
            Stat::estimated(12_u64, Source::Sketch).to_string(),
            "12 (estimated from sketch)"
        );
        assert_eq!(Stat::<u64>::Unknown.to_string(), "unknown");
    }
}

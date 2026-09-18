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
//! # Provenance belongs to every answer, not only to the guesses
//!
//! [`Provenance`] used to hang off [`Class::Estimated`] alone, on the reasoning that a bad plan is
//! diagnosed by asking which guess was wrong and where it came from. That is half the job. An exact
//! number is also worth attributing, because an exact row count out of the catalog and an exact
//! join cardinality out of a link header are different kinds of exact and a reader of `EXPLAIN` has
//! to be able to tell them apart. So provenance is a field of [`Stat::Known`] and every constructor
//! takes one, per `spec/stats/02-the-catalogue.md` section 2.1.1.
//!
//! [`Provenance::Default`] is how a hardcoded constant admits to being one, and it is the word to
//! search an `EXPLAIN` for, because it means nobody had a number at that node at all.
//! [`Provenance::Observed`] is a measurement from a previous execution and is kept distinguishable
//! from a measurement of the file, so that a reader can tell a fact about the data from a fact about
//! history.
//!
//! # The three uses
//!
//! A class on its own does not say what a caller may do with it. The missing column is the use, and
//! `spec/stats/05-every-query.md` section 5.1.1 names three of them. [`Stat::answer`] is for a
//! statistic that *is* the result and takes [`Class::Exact`], or a certificate the caller can
//! discharge. [`Stat::enable`] is for a rewrite that would be wrong if the number were wrong and
//! takes [`Class::Exact`] and nothing else, because a bound is not an equality. [`Stat::decide`] is
//! for choosing between two plans that produce the same rows and takes anything, including nothing,
//! because a decision made from `Unknown` is a decision made from a documented default.
//!
//! Those are three methods rather than three comments because the difference between them is the
//! difference between a slow query and a wrong answer, and the strictest of the three is the one a
//! future contributor is most likely to break in good faith.
//!
//! # What the histogram reads today
//!
//! The one producer wired up is the optimizer's row count estimator, and the [`Classes`] histogram
//! it fills is the G0 baseline that the rest of the series moves. It does not read all `Unknown`.
//! A base table scan gets its count from the catalog and is [`Class::Exact`], a `LIMIT` over an
//! input nobody counted is [`Class::Certified`] because the limit is a real ceiling, a table
//! function or a dependent join is `Unknown`, and everything above the first filter, group by or
//! join is [`Class::Estimated`] from [`Provenance::Default`], which is the literal selectivity guess
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
    /// A value, what kind of knowledge it is, and where it came from.
    Known {
        /// The number, bound, flag or set the question asked for.
        value: T,
        /// How much of it is known rather than guessed.
        class: Class,
        /// What produced it, which is what `EXPLAIN` prints next to the class.
        provenance: Provenance,
    },
    /// No answer. Not a zero, not a one and not a default.
    ///
    /// Not written, not resident, or not applicable. This is the ordinary answer for a statistic
    /// whose load has just been scheduled, because `spec/stats/04-in-memory.md` says no query ever
    /// waits on one, and it is the honest one.
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
    /// The value is wrong by no more than `bound`, as a fraction of itself, in the direction
    /// `direction` says, and the structure that produced it can prove that.
    ///
    /// A quantile summary with an epsilon is the usual source. The number is what makes a threshold
    /// safe to seed from, so a producer that cannot state one should say `Estimated` instead of
    /// picking a bound that sounds about right.
    Certified {
        /// The relative error bound, where `0.01` is one percent.
        bound: f64,
        /// Which side of the value the bound is on.
        direction: Direction,
    },
    /// The value is a guess. Where it came from is the provenance beside it.
    Estimated,
}

/// Which side of a value a certificate bounds.
///
/// A certificate that does not say the direction is a certificate a consumer cannot use, because
/// the whole point of the class is picking the end of the range whose failure you can afford, per
/// `spec/stats/05-every-query.md` section 5.1. Under-reserving memory spills and over-reserving
/// starves, and they are not the same cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// The truth is no larger than the value. A frequency synopsis `omitted_max` is this.
    AtMost,
    /// The truth is no smaller than the value. A lower bound on a distinct count out of a sketch is
    /// this.
    AtLeast,
    /// The truth is within the bound on either side. A quantile boundary with an epsilon is this.
    Within,
}

impl Direction {
    /// The word `EXPLAIN` prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::AtMost => "at most",
            Self::AtLeast => "at least",
            Self::Within => "within",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What produced an answer.
///
/// Printed by `EXPLAIN` next to the class, so these are the words a person reads when a plan went
/// wrong. It names the source rather than the value, and it is carried by exact answers as well as
/// by guesses, because an exact count out of a catalog and an exact join cardinality out of a link
/// header want different follow up questions when one of them turns out to be stale.
///
/// The list is `spec/stats/02-the-catalogue.md` section 2.1.1's fourteen plus [`Self::Propagation`],
/// which the specification does not name because it is not a source of data. It is what a number
/// derived from two others says about itself, and leaving it out would mean a combination inherited
/// the provenance of whichever operand happened to be on the left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provenance {
    /// A maintained row count. Exact.
    RowCount,
    /// A minimum and a maximum, either answered from or interpolated between.
    ZoneMap,
    /// A maintained null count. Exact.
    NullCount,
    /// A distinct-count sketch, outside the regime where it is exact.
    Sketch,
    /// A frequency synopsis, with or without its certificate discharged.
    FrequencySynopsis,
    /// A quantile summary.
    Quantiles,
    /// A dictionary's size, standing in for or answering a distinct count.
    Dictionary,
    /// A persisted sortedness flag.
    Sortedness,
    /// A persisted distinctness flag.
    Distinctness,
    /// The four numbers in a relationship's link header, or a cardinality derived from them. Exact,
    /// and the most valuable kind of exact there is, because join cardinality is where every cost
    /// model in the literature goes wrong by orders of magnitude.
    LinkHeader,
    /// A relationship's degree distribution.
    DegreeDistribution,
    /// The stored sample.
    Sample,
    /// A constant in the source. The weakest answer that is not `Unknown`, and the one worth
    /// searching an `EXPLAIN` for, because it means nobody had a number at that node at all.
    Default,
    /// Something a previous execution measured, out of the observation log. Kept apart from every
    /// other variant here on purpose: the rest are facts about the file and this one is a fact about
    /// history.
    Observed,
    /// A rule applied over two other answers. Not a source of data, and the honest thing to say
    /// about a number that was derived rather than read.
    Propagation,
}

impl Provenance {
    /// The word `EXPLAIN` prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::RowCount => "row count",
            Self::ZoneMap => "zone map",
            Self::NullCount => "null count",
            Self::Sketch => "sketch",
            Self::FrequencySynopsis => "frequency synopsis",
            Self::Quantiles => "quantiles",
            Self::Dictionary => "dictionary",
            Self::Sortedness => "sortedness",
            Self::Distinctness => "distinctness",
            Self::LinkHeader => "link header",
            Self::DegreeDistribution => "degree distribution",
            Self::Sample => "sample",
            Self::Default => "default",
            Self::Observed => "observed",
            Self::Propagation => "propagation",
        }
    }

    /// Whether this is a fact about a previous execution rather than about the file.
    ///
    /// The one question a consumer of `spec/stats/06-the-reward.md`'s tier 1 corrections has to be
    /// able to ask, because an observation is `Exact` only for the generation it was taken on.
    #[must_use]
    pub const fn is_observed(self) -> bool {
        matches!(self, Self::Observed)
    }
}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a caller intends to do with an answer.
///
/// The three uses of `spec/stats/05-every-query.md` section 5.1.1. A consumer declares one, the
/// class rule follows from it rather than from the consumer's judgement, and `EXPLAIN` prints which
/// one happened. See [`Stat::answer`], [`Stat::enable`] and [`Stat::decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Use {
    /// The statistic is the result. Entitled to `Exact`, and to `Certified` where the consumer can
    /// discharge the proof obligation and has a fallback for when it cannot.
    Answer,
    /// The statistic licenses a rewrite that would be wrong if the statistic were wrong. Entitled to
    /// `Exact` only.
    Enable,
    /// The statistic chooses between two plans that produce the same rows. Entitled to any class,
    /// including none.
    Decide,
}

impl Use {
    /// The word `EXPLAIN` prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Enable => "enable",
            Self::Decide => "decide",
        }
    }

    /// Whether a class is enough for this use.
    ///
    /// `Answer` says yes to a certificate here and the caller still has to discharge it, which is
    /// what [`Stat::answer_certified`] is for. This function is the class rule and not the whole
    /// obligation.
    #[must_use]
    pub const fn permits(self, class: Option<Class>) -> bool {
        match self {
            // Any class, including none, because a decision made from nothing is a decision made
            // from a documented default and the default is printed as one.
            Self::Decide => true,
            // Exact always, and a certificate only where the caller discharges it.
            Self::Answer => matches!(class, Some(Class::Exact | Class::Certified { .. })),
            // Exact and nothing else, because a bound is not an equality and these rewrites need
            // an equality.
            Self::Enable => matches!(class, Some(Class::Exact)),
        }
    }
}

impl fmt::Display for Use {
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
    /// hundred percent says nothing that `Estimated` does not say. Two certificates that bound
    /// opposite sides combine to [`Direction::Within`], because that is all that is still provable about
    /// the pair. Anything involving an estimate is an estimate.
    #[must_use]
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Exact, Self::Exact) => Self::Exact,
            (Self::Exact, class) | (class, Self::Exact) => class,
            (
                Self::Certified { bound: left, direction: first },
                Self::Certified { bound: right, direction: second },
            ) => Self::Certified {
                bound: (left + right).min(1.0),
                direction: if first == second { first } else { Direction::Within },
            },
            _ => Self::Estimated,
        }
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact => f.write_str("exact"),
            Self::Certified { bound, direction } => {
                write!(f, "certified {direction} {:.2}%", bound * 100.0)
            }
            Self::Estimated => f.write_str("estimated"),
        }
    }
}

impl<T> Stat<T> {
    /// A value that was counted, compared or maintained rather than guessed, and what produced it.
    pub const fn exact(value: T, provenance: Provenance) -> Self {
        Self::Known { value, class: Class::Exact, provenance }
    }

    /// A value wrong by no more than `bound` as a fraction of itself, on the side `direction` says.
    pub const fn certified(
        value: T,
        bound: f64,
        direction: Direction,
        provenance: Provenance,
    ) -> Self {
        Self::Known { value, class: Class::Certified { bound, direction }, provenance }
    }

    /// A guess, and where it came from.
    pub const fn estimated(value: T, provenance: Provenance) -> Self {
        Self::Known { value, class: Class::Estimated, provenance }
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

    /// The value, whatever its class.
    ///
    /// Present because plenty of code wants to print a number or compare two of them without making
    /// a claim about either. A caller that is about to act on it wants [`Self::decide`],
    /// [`Self::answer`] or [`Self::enable`] instead, because those say which of the three uses is
    /// happening and this one does not.
    #[must_use]
    pub const fn value(&self) -> Option<&T> {
        match self {
            Self::Known { value, .. } => Some(value),
            Self::Unknown => None,
        }
    }

    /// The value for a decision that chooses between two plans producing the same rows.
    ///
    /// Build side, grouping strategy, join order, reduction schedule, memory reservation, parallel
    /// degree. Entitled to any class, so this is [`Self::value`] under a name that says what is
    /// being done with it. `None` means the caller takes its documented default, and the worst case
    /// is a slow query with a printed reason.
    #[must_use]
    pub const fn decide(&self) -> Option<&T> {
        self.value()
    }

    /// The value for a rewrite that would be wrong if the value were wrong.
    ///
    /// Join elimination, sort elimination, distinct elimination, group by elimination, partition
    /// pruning, an exact `IN` list filter, narrowing arithmetic, folding a predicate away. Exact and
    /// nothing else, because a bound is not an equality and these need an equality.
    ///
    /// This is the strictest of the three and the one easiest to get wrong, because an enabling
    /// rewrite on a statistic that is merely close does not produce a slow query, it produces a
    /// wrong answer. See `spec/stats/05-every-query.md` sections 5.1.1 and 5.10.
    #[must_use]
    pub const fn enable(&self) -> Option<&T> {
        match self {
            Self::Known { value, class: Class::Exact, .. } => Some(value),
            _ => None,
        }
    }

    /// The value for a statistic that is itself the result, where that value is exact.
    ///
    /// `COUNT(*)` out of a row count, `MIN` out of a zone map whose bounds are values rather than
    /// widened bounds. A certified answer does not come back from here, because answering from a
    /// certificate needs the proof obligation discharged and this function has nothing to discharge
    /// it with. Use [`Self::answer_certified`] for that case and keep the fallback.
    #[must_use]
    pub const fn answer(&self) -> Option<&T> {
        self.enable()
    }

    /// The value for a statistic that is itself the result, where a certificate is acceptable and
    /// the caller can discharge it.
    ///
    /// `discharge` is handed the bound and its direction and says whether this particular query can
    /// live with them. A top-k group by out of a frequency synopsis is the case this exists for: the
    /// synopsis answers when the k-th count is above the certified maximum of everything it omitted,
    /// and does not otherwise. A caller that returns `true` unconditionally has written
    /// [`Self::decide`] with extra steps and should say so.
    #[must_use]
    pub fn answer_certified(&self, discharge: impl FnOnce(f64, Direction) -> bool) -> Option<&T> {
        match self {
            Self::Known { value, class: Class::Exact, .. } => Some(value),
            Self::Known { value, class: Class::Certified { bound, direction }, .. } => {
                discharge(*bound, *direction).then_some(value)
            }
            _ => None,
        }
    }

    /// The value, but only when it is exact.
    ///
    /// The older name for [`Self::enable`], kept because the rule it enforces is stated under this
    /// name in `spec/stats/05-every-query.md` section 5.10 and because a door that changes an answer
    /// is worth being able to grep for two ways.
    #[must_use]
    pub const fn exact_value(&self) -> Option<&T> {
        self.enable()
    }

    /// How much of the answer is knowledge, or `None` when there is no answer.
    #[must_use]
    pub const fn class(&self) -> Option<Class> {
        match self {
            Self::Known { class, .. } => Some(*class),
            Self::Unknown => None,
        }
    }

    /// What produced the answer, or `None` when there is no answer.
    #[must_use]
    pub const fn provenance(&self) -> Option<Provenance> {
        match self {
            Self::Known { provenance, .. } => Some(*provenance),
            Self::Unknown => None,
        }
    }

    /// Whether this answer is enough for that use, per the class rule.
    #[must_use]
    pub const fn permits(&self, use_: Use) -> bool {
        use_.permits(self.class())
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
            Self::Known { value, class, provenance } => {
                Stat::Known { value: f(value), class, provenance }
            }
            Self::Unknown => Stat::Unknown,
        }
    }

    /// An answer computed from two, unknown when either is unknown, classed by [`Class::combine`].
    ///
    /// The provenance of the result is [`Provenance::Propagation`] unless both sides agree, because
    /// a number derived from a zone map and a row count came from neither of them on its own.
    #[must_use]
    pub fn zip<U, V>(self, other: Stat<U>, f: impl FnOnce(T, U) -> V) -> Stat<V> {
        match (self, other) {
            (
                Self::Known { value: left, class: first, provenance: from },
                Stat::Known { value: right, class: second, provenance: also },
            ) => Stat::Known {
                value: f(left, right),
                class: first.combine(second),
                provenance: if from == also { from } else { Provenance::Propagation },
            },
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
            Self::Known { value, class, provenance } => {
                write!(f, "{value} ({class} from {provenance})")
            }
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
            Some(Class::Estimated) => self.estimated += 1,
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

    /// A certificate to hang a test on, so the tests below read as tests rather than as arguments.
    const ROUGHLY: Class = Class::Certified { bound: 0.01, direction: Direction::Within };

    #[test]
    fn only_an_exact_answer_comes_back_from_exact_value() {
        assert_eq!(Stat::exact(4_u64, Provenance::RowCount).exact_value(), Some(&4));
        assert_eq!(
            Stat::certified(4_u64, 0.01, Direction::Within, Provenance::Quantiles).exact_value(),
            None
        );
        assert_eq!(Stat::estimated(4_u64, Provenance::Sketch).exact_value(), None);
        assert_eq!(Stat::<u64>::Unknown.exact_value(), None);
    }

    #[test]
    fn the_three_uses_are_entitled_to_different_classes() {
        let exact = Stat::exact(4_u64, Provenance::RowCount);
        let certified =
            Stat::certified(4_u64, 0.01, Direction::AtMost, Provenance::FrequencySynopsis);
        let estimated = Stat::estimated(4_u64, Provenance::Default);
        let unknown = Stat::<u64>::Unknown;

        // Enable is the strict one. Exact and nothing else, because a bound is not an equality.
        assert_eq!(exact.enable(), Some(&4));
        assert_eq!(certified.enable(), None);
        assert_eq!(estimated.enable(), None);
        assert_eq!(unknown.enable(), None);

        // Answer without a discharge is the same door, because there is nothing here to discharge a
        // certificate with.
        assert_eq!(certified.answer(), None);
        assert_eq!(certified.answer_certified(|bound, _| bound < 0.05), Some(&4));
        assert_eq!(certified.answer_certified(|bound, _| bound < 0.001), None);
        // A discharge is never asked about a guess, however generous it is.
        assert_eq!(estimated.answer_certified(|_, _| true), None);

        // Decide takes anything, and Unknown is a documented default rather than a failure.
        assert_eq!(exact.decide(), Some(&4));
        assert_eq!(estimated.decide(), Some(&4));
        assert_eq!(unknown.decide(), None);

        assert!(exact.permits(Use::Enable));
        assert!(!certified.permits(Use::Enable));
        assert!(certified.permits(Use::Answer));
        assert!(!estimated.permits(Use::Answer));
        assert!(unknown.permits(Use::Decide));
    }

    #[test]
    fn a_class_degrades_when_it_is_combined() {
        assert_eq!(Class::Exact.combine(Class::Exact), Class::Exact);
        assert_eq!(Class::Exact.combine(Class::Estimated), Class::Estimated);
        assert_eq!(Class::Exact.combine(ROUGHLY), ROUGHLY);
        assert_eq!(
            Class::Certified { bound: 0.01, direction: Direction::AtMost }
                .combine(Class::Certified { bound: 0.02, direction: Direction::AtMost }),
            Class::Certified { bound: 0.03, direction: Direction::AtMost }
        );
        assert_eq!(Class::Estimated.combine(ROUGHLY), Class::Estimated);
        assert_eq!(Class::Estimated.combine(Class::Estimated), Class::Estimated);
    }

    #[test]
    fn two_certificates_bounding_opposite_sides_only_bound_both() {
        assert_eq!(
            Class::Certified { bound: 0.01, direction: Direction::AtMost }
                .combine(Class::Certified { bound: 0.02, direction: Direction::AtLeast }),
            Class::Certified { bound: 0.03, direction: Direction::Within }
        );
    }

    #[test]
    fn a_certified_bound_saturates_rather_than_growing_past_everything() {
        assert_eq!(
            Class::Certified { bound: 0.8, direction: Direction::Within }
                .combine(Class::Certified { bound: 0.7, direction: Direction::Within }),
            Class::Certified { bound: 1.0, direction: Direction::Within }
        );
    }

    #[test]
    fn zip_is_unknown_when_either_side_is() {
        let known = Stat::exact(10_u64, Provenance::RowCount);
        let unknown = Stat::<u64>::Unknown;
        assert_eq!(known.zip(unknown, |left, right| left + right), Stat::Unknown);
        assert_eq!(unknown.zip(known, |left, right| left + right), Stat::Unknown);
        assert_eq!(
            known.zip(Stat::exact(5, Provenance::RowCount), |left, right| left + right),
            Stat::exact(15, Provenance::RowCount)
        );
    }

    #[test]
    fn a_derived_answer_says_it_was_derived_rather_than_naming_one_side() {
        let rows = Stat::exact(10_u64, Provenance::RowCount);
        let nulls = Stat::exact(2_u64, Provenance::NullCount);
        let counted = rows.zip(nulls, |rows, nulls| rows - nulls);
        assert_eq!(counted.value(), Some(&8));
        assert_eq!(counted.class(), Some(Class::Exact));
        assert_eq!(counted.provenance(), Some(Provenance::Propagation));
    }

    #[test]
    fn map_carries_the_class_and_the_provenance() {
        let bytes = Stat::certified(100_u64, 0.05, Direction::Within, Provenance::Quantiles)
            .map(|rows| rows * 8);
        assert_eq!(bytes, Stat::certified(800, 0.05, Direction::Within, Provenance::Quantiles));
    }

    #[test]
    fn the_histogram_counts_what_it_was_shown() {
        let mut classes = Classes::new();
        classes.record(&Stat::exact(1_u64, Provenance::RowCount));
        classes.record(&Stat::certified(1_u64, 0.1, Direction::Within, Provenance::Quantiles));
        classes.record(&Stat::estimated(1_u64, Provenance::ZoneMap));
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
    fn an_answer_prints_its_class_and_its_provenance() {
        assert_eq!(
            Stat::exact(12_u64, Provenance::RowCount).to_string(),
            "12 (exact from row count)"
        );
        assert_eq!(
            Stat::certified(12_u64, 0.025, Direction::AtMost, Provenance::FrequencySynopsis)
                .to_string(),
            "12 (certified at most 2.50% from frequency synopsis)"
        );
        assert_eq!(
            Stat::estimated(12_u64, Provenance::Default).to_string(),
            "12 (estimated from default)"
        );
        assert_eq!(Stat::<u64>::Unknown.to_string(), "unknown");
    }

    #[test]
    fn an_exact_number_says_where_it_came_from_too() {
        // The whole reason provenance moved off Estimated. Two exact row counts, one out of a
        // catalog and one out of a link header, and a reader of EXPLAIN can tell them apart.
        let counted = Stat::exact(1_000_u64, Provenance::RowCount);
        let joined = Stat::exact(1_000_u64, Provenance::LinkHeader);
        assert_eq!(counted.class(), joined.class());
        assert_ne!(counted.provenance(), joined.provenance());
        assert_ne!(counted.to_string(), joined.to_string());
    }

    #[test]
    fn an_observation_is_distinguishable_from_a_measurement_of_the_file() {
        assert!(Provenance::Observed.is_observed());
        for provenance in [
            Provenance::RowCount,
            Provenance::ZoneMap,
            Provenance::NullCount,
            Provenance::Sketch,
            Provenance::FrequencySynopsis,
            Provenance::Quantiles,
            Provenance::Dictionary,
            Provenance::Sortedness,
            Provenance::Distinctness,
            Provenance::LinkHeader,
            Provenance::DegreeDistribution,
            Provenance::Sample,
            Provenance::Default,
            Provenance::Propagation,
        ] {
            assert!(!provenance.is_observed(), "{provenance} is not an observation");
        }
    }
}

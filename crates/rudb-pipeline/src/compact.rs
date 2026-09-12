//! The `chunk.compaction` seam: whether the rows a filter kept are worth copying out of the chunk.
//!
//! A filter here produces a selection over the chunk rather than a copy of it, which costs nothing
//! now and one redirection on every later read of every kept row. Compacting is the other side of
//! the trade, a copy now and no redirection afterwards. Which one is right depends on what happens
//! to the rows next, two published designs disagree about how to decide, so this is a seam with
//! three implementations in the tree rather than a constant somebody picked once.
//!
//! # What the paper found
//!
//! Data Chunk Compaction in Vectorized Execution, Qiao, Zhang and others, SIGMOD 2025. It replaces
//! the fixed threshold that every engine has some version of with a gain function evaluated per
//! operator, and measures up to ten percent end to end in DuckDB. The part that matters here is not
//! the size of the number, it is that the right threshold is not a constant.
//!
//! Our own measurement, written up in [`Chunk::compact`], reached the same place from the other
//! end. Compacting loses to selecting at every selectivity from one percent to a hundred when there
//! is one later pass over the kept rows, and beats it at every selectivity from one percent to a
//! hundred when there are sixteen. Selectivity barely moves the line and the number of later passes
//! moves it a lot, which is why the fixed rule is in the tree as the thing to beat rather than as
//! the default.
//!
//! # What is here and what is not
//!
//! The decision is per chunk and it decides between [`Chunk::select`] and [`Chunk::compact`]. The
//! other half of the paper is a buffer that merges several sparse chunks into a full one, which no
//! operator here can do yet, because [`Stream::push`](crate::Stream::push) transforms one chunk in
//! place and has nowhere to keep a remainder. That is a change to the operator interface rather
//! than to this seam, and it is the follow up.
//!
//! Nothing here reads a row. [`Compaction::worth_it`] is asked once per chunk and the copy itself
//! is [`Chunk::compact`], which is a gather per column.

use std::mem;

use rudb_common::{LogicalType, Result};
use rudb_metrics::Span;
use rudb_seam::{Context, Provenance, Registry, SeamId, Strategy};
use rudb_vector::{Chunk, Selection};

/// Whether the rows a filter kept are copied out of the chunk or left as a selection over it.
///
/// Crossed once per chunk. The decision is not per row and the implementations never see one.
pub trait Compaction: Strategy {
    /// Whether the rows `kept` of `chunk` are worth copying into a chunk of their own.
    fn worth_it(&self, chunk: &Chunk, kept: &Selection, gauge: &mut Gauge) -> bool;

    /// Whether this implementation wants to be told what a copy cost.
    ///
    /// Two clock reads per compacted chunk, which is nothing next to the copy and is not nothing
    /// next to a decision that was going to be the same either way, so an implementation that does
    /// not learn says so and is not charged for it.
    fn watches(&self) -> bool {
        false
    }

    /// What the copy the caller just did cost, for an implementation that learns its own constants.
    ///
    /// Only called when [`Compaction::watches`] said yes and only after a chunk was compacted.
    fn copy_took(&self, gauge: &mut Gauge, copied: &Copied) {
        let _ = (gauge, copied);
    }
}

/// What one compaction copied, and how long it took.
#[derive(Debug, Clone, Copy)]
pub struct Copied {
    /// How many rows came out.
    pub rows: usize,
    /// How many bytes the compacted chunk holds, which is what the copy moved.
    pub bytes: usize,
    /// Wall nanoseconds the copy took.
    pub nanos: u64,
}

/// What the seam carries for one pipeline instance.
///
/// The strategies are shared: one object in the registry answers for every query in the process, so
/// nothing a query knows can live in one. This is where that goes, and it is per instance for the
/// same reason [`Stream::Local`](crate::Stream::Local) is, so that thirty two threads running one
/// pipeline learn thirty two times rather than fighting over one number.
///
/// It holds the plan time fact that the gain function needs, which is how many more times the rows
/// will be read, and the one machine dependent constant that can be measured while the query runs,
/// which is how fast this machine copies.
#[derive(Debug, Clone)]
pub struct Gauge {
    passes: u32,
    rate: f64,
    chunks: u64,
    compactions: u64,
}

impl Gauge {
    /// A gauge for a filter whose kept rows will be read `passes` more times.
    #[must_use]
    pub fn new(passes: u32) -> Self {
        Self { passes, rate: RATE_NS, chunks: 0, compactions: 0 }
    }

    /// How many more times the kept rows will be read, counted from the plan above the filter.
    #[must_use]
    pub fn passes(&self) -> u32 {
        self.passes
    }

    /// Nanoseconds per byte, as it started out or as this machine has been seen to manage.
    #[must_use]
    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// How many chunks this instance has decided about.
    #[must_use]
    pub fn chunks(&self) -> u64 {
        self.chunks
    }

    /// How many of them it copied.
    #[must_use]
    pub fn compactions(&self) -> u64 {
        self.compactions
    }

    /// Fold one measured copy into the rate.
    ///
    /// An exponentially weighted average rather than a running mean, because the first chunk of a
    /// query is timed on a cold cache and a mean never forgets it. The same reason the conjunct
    /// ordering in `rudb-exec` uses a window rather than a whole scan average.
    fn learn(&mut self, copied: &Copied) {
        if copied.bytes == 0 || copied.nanos == 0 {
            return;
        }
        let sample = copied.nanos as f64 / copied.bytes as f64;
        self.rate = self.rate.mul_add(1.0 - WEIGHT, sample * WEIGHT);
    }
}

/// Narrow a chunk to the rows a selection kept, the way the seam says to.
///
/// The one place [`Chunk::select`] and [`Chunk::compact`] are chosen between, so that an operator
/// that filters is one line and the reason it is that line is here.
///
/// [`Chunk::select`] and [`Chunk::compact`] both take the chunk by value, because taking it by
/// value is what lets the first of them move the payload rather than copy it, and a push operator
/// holds a `&mut`. So the chunk is swapped out for an empty one, narrowed, and put back. The empty
/// one is never observed, since a failure here fails the query.
///
/// # Errors
///
/// If the selection points past the end of the chunk, or if the seam asked for a copy of a column
/// with no flat layout, which today means the nested types and which
/// [`Strategy::applicable`](rudb_seam::Strategy::applicable) already keeps the compacting
/// implementations away from.
pub fn narrow(
    how: &dyn Compaction,
    chunk: &mut Chunk,
    kept: &Selection,
    gauge: &mut Gauge,
) -> Result<()> {
    gauge.chunks += 1;
    if !how.worth_it(chunk, kept, gauge) {
        let whole = mem::replace(chunk, Chunk::empty(&[]));
        *chunk = whole.select(kept)?;
        return Ok(());
    }
    let span = how.watches().then(Span::start);
    let whole = mem::replace(chunk, Chunk::empty(&[]));
    *chunk = whole.compact(kept)?;
    gauge.compactions += 1;
    if let Some(span) = span {
        let (wall, _) = span.stop();
        let copied = Copied { rows: chunk.len(), bytes: chunk.footprint(), nanos: wall };
        how.copy_took(gauge, &copied);
    }
    Ok(())
}

/// The registry, which is the one line `rudb-exec` adds to its `register.rs`.
///
/// The reference is the default, which is the honest position for a seam whose alternatives have
/// not been measured on our own suite yet. Selecting is also what the engine did before this seam
/// existed, so the default build answers every query with the same bytes it did yesterday and the
/// two alternatives are a sweep away rather than a surprise.
#[must_use]
pub fn compaction() -> Registry<dyn Compaction> {
    Registry::<dyn Compaction>::builder(SeamId::ChunkCompaction)
        .reference(Box::new(Never))
        .alternative(Box::new(FixedThreshold))
        .alternative(Box::new(LearnedGain))
        .build()
}

/// Nanoseconds per byte to start from, which is ten gigabytes a second.
const RATE_NS: f64 = 0.1;

/// How much of the rate one measured copy moves.
const WEIGHT: f64 = 0.25;

/// Nanoseconds one later read of one row through a selection costs.
///
/// A dependent load out of the code array in front of the load that was going to happen anyway, so
/// a fraction of a nanosecond when the codes are in cache and much more when they are not. This is
/// the first estimate and the sweep is what turns it into a measurement.
const INDIRECT_NS: f64 = 0.6;

/// Nanoseconds one column of a compaction costs before a byte of it is copied.
const PER_COLUMN_NS: f64 = 60.0;

/// The fraction of a chunk below which the fixed rule copies, as a denominator.
const THRESHOLD: usize = 5;

/// Whether a type can be copied out of a chunk at all.
///
/// The nested types have no flat layout to gather into yet, so a compaction of one is an error
/// rather than a slow answer. Both compacting implementations decline a plan that has one in it,
/// which leaves the reference, which handles everything.
fn gatherable(context: &Context<'_>) -> bool {
    !context.types().iter().any(LogicalType::is_nested)
}

/// Never copies. The rows a filter kept stay a selection over the chunk they came from.
#[derive(Debug)]
struct Never;

impl Strategy for Never {
    fn name(&self) -> &'static str {
        "never"
    }

    fn describe(&self) -> &'static str {
        "leaves the kept rows as a selection over the chunk they came from"
    }

    fn provenance(&self) -> Provenance {
        Provenance::Reference
    }

    fn applicable(&self, _context: &Context<'_>) -> bool {
        true
    }
}

impl Compaction for Never {
    fn worth_it(&self, _chunk: &Chunk, _kept: &Selection, _gauge: &mut Gauge) -> bool {
        false
    }
}

/// Copies when the survivors are a small enough fraction of the chunk.
///
/// The rule every engine has a version of, and the one the paper measures against. It is in the
/// tree because a comparison against the thing everybody does is worth more than a comparison
/// against nothing, and because it is the shape of rule our own measurement says is wrong: the
/// copy costs what the kept rows cost and so does the redirection it saves, so the fraction that
/// survived mostly cancels out of the answer.
#[derive(Debug)]
struct FixedThreshold;

impl Strategy for FixedThreshold {
    fn name(&self) -> &'static str {
        "fixed-threshold"
    }

    fn describe(&self) -> &'static str {
        "copies the kept rows when they are under a fifth of the chunk"
    }

    fn provenance(&self) -> Provenance {
        Provenance::Ours
    }

    fn applicable(&self, context: &Context<'_>) -> bool {
        gatherable(context)
    }
}

impl Compaction for FixedThreshold {
    fn worth_it(&self, chunk: &Chunk, kept: &Selection, _gauge: &mut Gauge) -> bool {
        kept.len() * THRESHOLD < chunk.len()
    }
}

/// Copies when a gain function says the copy costs less than the redirections it saves.
///
/// The gain is in nanoseconds on both sides. What a copy costs is the kept rows times the bytes a
/// row of this chunk holds times what this machine has been seen to copy at, plus a fixed amount
/// per column for the allocation that is not proportional to anything. What it saves is the kept
/// rows times how many more times they will be read times what one redirected read costs.
///
/// The kept row count is on both sides and very nearly cancels, which is the whole finding. What is
/// left is a comparison between how wide the rows are and how often they will be read again, and
/// neither of those is the selectivity that the fixed rule is written in terms of.
///
/// The learned part is the copy rate. It starts at ten gigabytes a second and moves towards
/// whatever the copies this query actually does take, which is the one constant in the model that
/// is about the machine rather than about the query. The rest of the model is measured by a sweep
/// and then written down, not learned per query, because a query does not run long enough to learn
/// what a redirected read costs and pretending otherwise would be a number with nothing behind it.
#[derive(Debug)]
struct LearnedGain;

impl Strategy for LearnedGain {
    fn name(&self) -> &'static str {
        "learned-gain"
    }

    fn describe(&self) -> &'static str {
        "copies when a gain function beats the redirections the copy would save"
    }

    fn provenance(&self) -> Provenance {
        Provenance::Paper {
            title: "Data Chunk Compaction in Vectorized Execution",
            venue: "SIGMOD",
            year: 2025,
        }
    }

    fn applicable(&self, context: &Context<'_>) -> bool {
        gatherable(context)
    }
}

impl Compaction for LearnedGain {
    fn worth_it(&self, chunk: &Chunk, kept: &Selection, gauge: &mut Gauge) -> bool {
        let rows = chunk.len();
        if rows == 0 {
            return false;
        }
        // A selection of nothing still holds the whole chunk it selected from, because the codes
        // are empty and the payload they point at is not. Copying nothing is an allocation per
        // column and it lets the payload go, so this is the one case the gain function is not
        // asked about.
        if kept.is_empty() {
            return true;
        }
        let per_row = chunk.footprint() as f64 / rows as f64;
        let bytes = kept.len() as f64 * per_row;
        let copy = bytes.mul_add(gauge.rate(), chunk.width() as f64 * PER_COLUMN_NS);
        let redirected = kept.len() as f64 * f64::from(gauge.passes()) * INDIRECT_NS;
        redirected > copy
    }

    fn watches(&self) -> bool {
        true
    }

    fn copy_took(&self, gauge: &mut Gauge, copied: &Copied) {
        gauge.learn(copied);
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_seam::{Context, Settings};
    use rudb_vector::{Chunk, Data, Selection, Vector};

    use super::{Compaction, Copied, Gauge, SeamId, compaction, narrow};

    /// A chunk of `columns` integer columns and `rows` rows.
    fn chunk(columns: usize, rows: usize) -> Chunk {
        let values: Vec<i32> = (0..rows as i32).collect();
        let built: Vec<Vector> = (0..columns)
            .map(|_| {
                Vector::flat(LogicalType::Integer, Data::Int32(values.clone().into()))
                    .expect("integers are an i32 layout")
            })
            .collect();
        Chunk::with_rows(built, rows).expect("every column is the same length")
    }

    /// The first `kept` rows of `rows`.
    fn first(kept: usize) -> Selection {
        Selection::from_predicate(kept, |_| true)
    }

    fn strategy(name: &str) -> &'static dyn Compaction {
        // The registry outlives the test, which is what a strategy reference out of one needs.
        let registry = Box::leak(Box::new(compaction()));
        registry.by_name(name).expect("the registry has this one")
    }

    #[test]
    fn the_registry_has_the_three_the_milestone_asked_for() {
        let registry = compaction();
        assert_eq!(registry.names(), vec!["never", "fixed-threshold", "learned-gain"]);
        assert_eq!(registry.reference().name(), "never");
        assert_eq!(registry.default().name(), "never");
    }

    #[test]
    fn the_default_is_the_one_that_copies_nothing() {
        let settings = Settings::new();
        let context = Context::new(SeamId::ChunkCompaction, &settings);
        let registry = compaction();
        let chosen = registry.choose(&context).expect("something is applicable");
        assert_eq!(chosen.name(), "never");
    }

    #[test]
    fn a_nested_column_leaves_only_the_one_that_copies_nothing() {
        let mut settings = Settings::new();
        settings.pin(SeamId::ChunkCompaction, "fixed-threshold");
        let types = [LogicalType::List(Box::new(LogicalType::Integer))];
        let context = Context::new(SeamId::ChunkCompaction, &settings).with_types(&types);
        let registry = compaction();
        assert!(
            registry.choose(&context).is_err(),
            "a pin to something that cannot run is an error"
        );

        let settings = Settings::new();
        let context = Context::new(SeamId::ChunkCompaction, &settings).with_types(&types);
        let chosen = registry.choose(&context).expect("the reference handles everything");
        assert_eq!(chosen.name(), "never");
    }

    /// Both narrowings answer with the same rows, which is the whole of what correctness means
    /// here. One of them holds the chunk it came from and the other does not, and no query can tell
    /// the difference by reading values out.
    #[test]
    fn every_implementation_keeps_the_same_rows() {
        for name in ["never", "fixed-threshold", "learned-gain"] {
            let mut narrowed = chunk(2, 100);
            let kept = Selection::from_predicate(100, |row| row % 7 == 0);
            let mut gauge = Gauge::new(4);
            narrow(strategy(name), &mut narrowed, &kept, &mut gauge).expect("the chunk narrows");
            let answered: Vec<Value> =
                (0..narrowed.len()).map(|row| narrowed.value_at(row, 1)).collect();
            let expected: Vec<Value> =
                (0..100i32).filter(|row| row % 7 == 0).map(Value::Integer).collect();
            assert_eq!(answered, expected, "{name} kept different rows");
            assert_eq!(gauge.chunks(), 1);
        }
    }

    #[test]
    fn the_reference_copies_nothing_however_selective_the_filter_was() {
        let never = strategy("never");
        let mut gauge = Gauge::new(16);
        assert!(!never.worth_it(&chunk(1, 1000), &first(1), &mut gauge));
        assert!(!never.worth_it(&chunk(1, 1000), &first(999), &mut gauge));
    }

    #[test]
    fn the_fixed_rule_copies_under_a_fifth_and_not_over_it() {
        let fixed = strategy("fixed-threshold");
        let mut gauge = Gauge::new(1);
        assert!(fixed.worth_it(&chunk(1, 1000), &first(199), &mut gauge));
        assert!(!fixed.worth_it(&chunk(1, 1000), &first(200), &mut gauge));
        assert!(!fixed.worth_it(&chunk(1, 1000), &first(1000), &mut gauge));
    }

    /// The finding, as a test. The fixed rule answers differently at two selectivities and the same
    /// way whatever happens to the rows afterwards, and the gain function is the other way round.
    #[test]
    fn the_gain_function_reads_the_passes_and_not_the_selectivity() {
        let learned = strategy("learned-gain");
        let mut once = Gauge::new(1);
        let mut often = Gauge::new(32);
        assert!(!learned.worth_it(&chunk(2, 1000), &first(100), &mut once));
        assert!(!learned.worth_it(&chunk(2, 1000), &first(900), &mut once));
        assert!(learned.worth_it(&chunk(2, 1000), &first(100), &mut often));
        assert!(learned.worth_it(&chunk(2, 1000), &first(900), &mut often));
    }

    /// Twenty rows out of a thousand is where the per column cost of the copy is most of what the
    /// copy is, and the gain function is the only one of the three that has that term at all.
    #[test]
    fn the_gain_function_declines_a_copy_too_small_to_pay_for_its_own_allocation() {
        let learned = strategy("learned-gain");
        let mut gauge = Gauge::new(8);
        assert!(!learned.worth_it(&chunk(8, 1000), &first(2), &mut gauge));
        assert!(learned.worth_it(&chunk(8, 1000), &first(900), &mut gauge));
    }

    #[test]
    fn an_empty_selection_is_copied_so_that_the_chunk_it_came_from_can_go() {
        let learned = strategy("learned-gain");
        let mut gauge = Gauge::new(1);
        assert!(learned.worth_it(&chunk(4, 1000), &Selection::empty(), &mut gauge));
    }

    #[test]
    fn a_slow_copy_moves_the_rate_towards_what_was_measured() {
        let learned = strategy("learned-gain");
        let mut gauge = Gauge::new(4);
        let started = gauge.rate();
        learned.copy_took(&mut gauge, &Copied { rows: 100, bytes: 1000, nanos: 4000 });
        assert!(gauge.rate() > started, "four nanoseconds a byte is slower than the prior");
        let once = gauge.rate();
        learned.copy_took(&mut gauge, &Copied { rows: 100, bytes: 1000, nanos: 4000 });
        assert!(gauge.rate() > once, "a second sample moves it further");
        assert!(gauge.rate() < 4.0, "and it is an average rather than the last sample");
    }

    #[test]
    fn a_copy_of_nothing_teaches_nothing() {
        let learned = strategy("learned-gain");
        let mut gauge = Gauge::new(4);
        let started = gauge.rate();
        learned.copy_took(&mut gauge, &Copied { rows: 0, bytes: 0, nanos: 120 });
        assert_eq!(gauge.rate(), started);
    }

    /// What a machine that copies slowly does to the decision. The rate is the only thing the
    /// implementation learns and it is there to move the line, so a test that never checks that it
    /// does is a test of an accumulator rather than of a strategy.
    #[test]
    fn a_machine_that_copies_slowly_stops_copying() {
        let learned = strategy("learned-gain");
        let mut gauge = Gauge::new(8);
        assert!(learned.worth_it(&chunk(2, 1000), &first(500), &mut gauge));
        for _ in 0..20 {
            learned.copy_took(&mut gauge, &Copied { rows: 500, bytes: 4000, nanos: 40_000 });
        }
        assert!(!learned.worth_it(&chunk(2, 1000), &first(500), &mut gauge));
    }

    #[test]
    fn only_the_one_that_learns_asks_to_be_timed() {
        assert!(!strategy("never").watches());
        assert!(!strategy("fixed-threshold").watches());
        assert!(strategy("learned-gain").watches());
    }

    #[test]
    fn a_compacted_chunk_is_counted_and_a_selected_one_is_not() {
        let mut narrowed = chunk(2, 1000);
        let mut gauge = Gauge::new(1);
        narrow(strategy("fixed-threshold"), &mut narrowed, &first(10), &mut gauge)
            .expect("the chunk narrows");
        assert_eq!((gauge.chunks(), gauge.compactions()), (1, 1));

        let mut narrowed = chunk(2, 1000);
        let mut gauge = Gauge::new(1);
        narrow(strategy("never"), &mut narrowed, &first(10), &mut gauge)
            .expect("the chunk narrows");
        assert_eq!((gauge.chunks(), gauge.compactions()), (1, 0));
    }
}

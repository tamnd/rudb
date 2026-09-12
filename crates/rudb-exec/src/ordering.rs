//! Which operand of a connective a filter runs first, decided from what the last few chunks did.
//!
//! A threaded filter runs the conjuncts of an `AND` one at a time and stops as soon as nothing is
//! left, so the order it runs them in is most of what the threading is worth. A predicate of four
//! conjuncts where one of them rejects almost every row costs a quarter as much with that one in
//! front as it does with that one at the back.
//!
//! The plan's order is the optimizer's guess, made before the query ran, out of whatever statistics
//! the catalog had. This is the same question asked again with the answers in hand: over the last
//! few chunks, how many rows did each operand throw away and what did it cost to find out.
//!
//! # A window rather than an average
//!
//! `hits` is clustered by time and every table worth scanning is clustered by something. A predicate
//! on a clustered column matches everything in one part of the scan and nothing in another, and a
//! running average over the whole scan converges on a number that is right nowhere and then stops
//! moving, because by the hundred thousandth chunk the first one still weighs as much as the last.
//! A window of the most recent chunks is wrong for a moment after the data changes underneath it and
//! right again afterwards, which is the behaviour worth having.
//!
//! # What is measured and what is estimated
//!
//! Selectivity is measured, one observation per operand per chunk. Cost is estimated by the caller
//! from the shape of the expression rather than timed. A clock read per operand per chunk is a real
//! fraction of what a chunk of work costs at 1024 rows, and the estimate is enough to separate a
//! string function from an integer comparison, which is the difference that decides the order. The
//! rank function below is the one place that would change if a benchmark ever says otherwise, and
//! timing would go behind it rather than beside it.

use rudb_kernels::Connective;

/// How many chunks of history an operand keeps.
///
/// Sixteen chunks is sixteen thousand rows, which is long enough that one unusual chunk does not
/// reorder the predicate and short enough that a run of them does.
const WINDOW: usize = 16;

/// What one connective has learned about its operands, and the order it runs them in now.
///
/// One of these per connective step, held in the per pipeline scratch rather than in the prepared
/// expression, because it changes as chunks go by and a prepared expression is shared by every
/// thread running the pipeline. Two threads over the same plan learn separately, which is sound as
/// well as convenient: a thread is looking at its own morsels and on a clustered table those are not
/// the same rows.
#[derive(Debug)]
pub(crate) struct Ordering {
    /// Which connective this is, which is the whole of what the rank means.
    op: Connective,
    /// Operand positions, in the order they are run.
    order: Vec<usize>,
    /// What each operand costs to run once over a chunk, relative to one fixed width comparison.
    costs: Vec<f64>,
    /// What each operand did with the chunks it saw.
    seen: Vec<Window>,
    /// Scratch for [`Ordering::relearn`], so that working the order out allocates nothing.
    ranks: Vec<f64>,
}

impl Ordering {
    /// An ordering over operands that cost `costs`, running in the order the plan gave.
    pub(crate) fn new(op: Connective, costs: Vec<f64>) -> Self {
        let len = costs.len();
        Self {
            op,
            order: (0..len).collect(),
            costs,
            seen: (0..len).map(|_| Window::default()).collect(),
            ranks: vec![0.0; len],
        }
    }

    /// The operand position to run in `slot`, where `slot` counts through the operands in the
    /// order they are run rather than the order they are written.
    pub(crate) fn at(&self, slot: usize) -> usize {
        self.order.get(slot).copied().unwrap_or(slot)
    }

    /// The order the operands are run in.
    ///
    /// For the tests that say the learning happened, since nothing in the engine asks a connective
    /// what order it settled on. It runs the operands itself.
    #[cfg(test)]
    pub(crate) fn order(&self) -> &[usize] {
        &self.order
    }

    /// What one operand did with one chunk: it was given `given` rows and answered with `kept`.
    ///
    /// An operand given no rows is not an observation. It happens on the chunk a conjunct in front
    /// of it emptied, it says nothing about what the operand does, and counting it would pull every
    /// selectivity in the predicate towards a number that no operand produced.
    pub(crate) fn observed(&mut self, operand: usize, given: usize, kept: usize) {
        if given == 0 {
            return;
        }
        if let Some(window) = self.seen.get_mut(operand) {
            window.record(given, kept);
        }
    }

    /// Works the order out again from what the windows hold.
    ///
    /// A stable sort, so operands that have learned the same thing stay in the order the plan gave
    /// them. That matters more than it looks: an untrained operand and an operand that costs
    /// nothing both rank above everything, and without stability a predicate of three free column
    /// references would shuffle itself every chunk for no reason.
    pub(crate) fn relearn(&mut self) {
        for operand in 0..self.ranks.len() {
            self.ranks[operand] = self.rank(operand);
        }
        let ranks = &self.ranks;
        self.order.sort_by(|&left, &right| ranks[right].total_cmp(&ranks[left]));
    }

    /// What running an operand early is worth, which is the work it takes off the operands behind
    /// it for what it costs to run.
    ///
    /// For an `AND` that is the fraction of rows it rejects, because a rejected row is one nothing
    /// after it looks at. For an `OR` it is the fraction it accepts, for the same reason with the
    /// sign turned around. Dividing by the cost is what keeps a `LIKE` that rejects everything from
    /// going in front of an integer comparison that rejects nearly everything for a twentieth of the
    /// work.
    ///
    /// Two things rank above every measurement. An operand that has not run yet, so that a conjunct
    /// an early break has been keeping off the chunk gets one chunk to say what it does rather than
    /// sitting behind the same conjunct for the rest of the scan. And an operand that costs nothing,
    /// which is a bare column reference read in place: nothing it rejects could have been bought
    /// more cheaply.
    fn rank(&self, operand: usize) -> f64 {
        let Some(passed) = self.seen[operand].passed() else {
            return f64::INFINITY;
        };
        if self.costs[operand] <= 0.0 {
            return f64::INFINITY;
        }
        let worth = match self.op {
            Connective::And => 1.0 - passed,
            Connective::Or => passed,
        };
        worth / self.costs[operand]
    }
}

/// The rows an operand was given and the rows it answered with, over the last [`WINDOW`] chunks.
///
/// A ring with the totals carried alongside it, so that a chunk costs one subtraction and one
/// addition rather than a walk over the history, and the history never grows.
#[derive(Debug, Default)]
struct Window {
    /// The observations, oldest overwritten first.
    ring: [(u32, u32); WINDOW],
    /// Where the next observation goes.
    at: usize,
    /// The rows given, over everything in the ring.
    given: u64,
    /// The rows kept, over everything in the ring.
    kept: u64,
}

impl Window {
    /// Records a chunk, forgetting the oldest one.
    fn record(&mut self, given: usize, kept: usize) {
        let given = u32::try_from(given).unwrap_or(u32::MAX);
        let kept = u32::try_from(kept).unwrap_or(u32::MAX);
        let (stale_given, stale_kept) = self.ring[self.at];
        self.given = self.given + u64::from(given) - u64::from(stale_given);
        self.kept = self.kept + u64::from(kept) - u64::from(stale_kept);
        self.ring[self.at] = (given, kept);
        self.at = (self.at + 1) % WINDOW;
    }

    /// The fraction of the rows it was given that it answered with, or `None` for an operand that
    /// has not run over a row yet.
    fn passed(&self) -> Option<f64> {
        (self.given > 0).then(|| self.kept as f64 / self.given as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::{Ordering, WINDOW, Window};
    use rudb_kernels::Connective;

    /// An ordering over `costs`, with no history yet.
    fn ordering(op: Connective, costs: &[f64]) -> Ordering {
        Ordering::new(op, costs.to_vec())
    }

    /// One chunk of a thousand rows through every operand, each keeping the rows it is told to.
    fn chunk(ordering: &mut Ordering, kept: &[usize]) {
        for (operand, &rows) in kept.iter().enumerate() {
            ordering.observed(operand, 1000, rows);
        }
        ordering.relearn();
    }

    #[test]
    fn a_connective_that_has_seen_nothing_runs_its_operands_in_the_order_it_was_given() {
        let mut ordering = ordering(Connective::And, &[1.0, 1.0, 1.0]);
        assert_eq!(ordering.order(), &[0, 1, 2]);
        ordering.relearn();
        assert_eq!(ordering.order(), &[0, 1, 2], "nothing observed is nothing to go on");
    }

    #[test]
    fn the_conjunct_that_rejects_the_most_for_the_least_goes_first() {
        let mut ordering = ordering(Connective::And, &[1.0, 1.0, 1.0]);
        // The middle one keeps a tenth of what it sees and the other two keep nearly everything.
        chunk(&mut ordering, &[900, 100, 800]);
        assert_eq!(ordering.order(), &[1, 2, 0]);
    }

    #[test]
    fn a_cheap_conjunct_beats_an_expensive_one_that_rejects_a_little_more() {
        // A string function against an integer comparison, which is the pair this exists for. The
        // function rejects more rows and still goes second, because it costs twenty times as much.
        let mut ordering = ordering(Connective::And, &[20.0, 1.0]);
        chunk(&mut ordering, &[100, 200]);
        assert_eq!(ordering.order(), &[1, 0]);
        // And it does go first once the comparison stops rejecting anything.
        chunk(&mut ordering, &[100, 1000]);
        for _ in 0..WINDOW {
            chunk(&mut ordering, &[100, 1000]);
        }
        assert_eq!(ordering.order(), &[0, 1]);
    }

    #[test]
    fn an_or_runs_the_branch_that_accepts_the_most_first() {
        // The opposite of the `AND` rule, because a row a branch accepts is one the branches after
        // it never see. The first branch here accepts a tenth and the second nine tenths.
        let mut ordering = ordering(Connective::Or, &[1.0, 1.0]);
        chunk(&mut ordering, &[100, 900]);
        assert_eq!(ordering.order(), &[1, 0]);
    }

    #[test]
    fn an_operand_that_has_not_run_goes_in_front_of_every_operand_that_has() {
        let mut ordering = ordering(Connective::And, &[1.0, 1.0]);
        // The second conjunct never ran, which is what an early break does to the conjunct behind
        // it. Leaving it where it is would keep it there for the rest of the scan.
        ordering.observed(0, 1000, 10);
        ordering.relearn();
        assert_eq!(ordering.order(), &[1, 0]);
    }

    #[test]
    fn a_conjunct_that_costs_nothing_goes_first_whatever_it_rejects() {
        // A bare column reference, read in place out of the chunk. There is no cheaper way to
        // reject a row than one that costs nothing to ask.
        let mut ordering = ordering(Connective::And, &[0.0, 1.0]);
        chunk(&mut ordering, &[990, 10]);
        assert_eq!(ordering.order(), &[0, 1]);
    }

    #[test]
    fn what_the_window_holds_is_the_recent_past_and_not_the_whole_scan() {
        // The shape a clustered column produces: a conjunct rejects everything for a while and then
        // rejects nothing. A whole scan average would still be reordering the predicate around the
        // first half a long time after the data stopped looking like that.
        let mut ordering = ordering(Connective::And, &[1.0, 1.0]);
        for _ in 0..WINDOW {
            chunk(&mut ordering, &[0, 1000]);
        }
        assert_eq!(ordering.order(), &[0, 1]);
        for _ in 0..WINDOW {
            chunk(&mut ordering, &[1000, 0]);
        }
        assert_eq!(ordering.order(), &[1, 0], "the first half is out of the window by now");
    }

    #[test]
    fn a_chunk_that_reached_an_operand_with_no_rows_is_not_an_observation() {
        let mut ordering = ordering(Connective::And, &[1.0, 1.0]);
        // The first conjunct rejected every row, so the second was handed an empty selection. That
        // is a chunk the second one told us nothing about and it still ranks as untrained.
        ordering.observed(0, 1000, 0);
        ordering.observed(1, 0, 0);
        ordering.relearn();
        assert_eq!(ordering.order(), &[1, 0], "an operand given nothing has still not run");
    }

    #[test]
    fn the_totals_a_window_carries_are_what_a_walk_over_it_would_say() {
        let mut window = Window::default();
        for at in 0..WINDOW * 2 {
            window.record(100, at % 10);
        }
        let walked: u64 = window.ring.iter().map(|&(_, kept)| u64::from(kept)).sum();
        assert_eq!(window.kept, walked);
        assert_eq!(window.given, 100 * WINDOW as u64);
    }
}

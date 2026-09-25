//! How good a row has to be, told to the scan by the top N above it while both are running.
//!
//! `ORDER BY EventTime LIMIT 10` reads a million rows to hand back ten. The top N throws almost all
//! of them away, and it knows very early which ones it is going to throw away: once it holds ten
//! candidates, the tenth of them is a key nothing worse than can ever come out. A part of the file
//! whose smallest EventTime is already worse than that key holds nothing the query wants, and the
//! two ends of every part are in the file next to the part itself. So the scan can skip it without
//! reading a byte of it.
//!
//! On ClickBench 24 that is most of the file. The filter keeps 90 of 974 parts under a cutoff that
//! starts at nothing and tightens as the scan walks, against 30 parts for a cutoff that was somehow
//! perfect from the first row, and 974 for no cutoff at all. The instruction count of the query
//! falls by about ten times.
//!
//! # Why this cannot be a pushed down filter
//!
//! [`crate::sideways`] is the same shape of thing for joins and it refuses this case by name: a
//! `LIMIT` over a `SORT` decides which rows come out by counting them, so a scan that drops rows
//! under one changes which rows reach the limit. That refusal is about a filter from somewhere else
//! being pushed under the limit. This is the limit itself, and the argument below is why it is
//! allowed to do what nothing above it is.
//!
//! It is also why the cutoff cannot travel the way a pushed down filter travels.
//! [`crate::source::Scan::testing`] settles its tests on the first call and keeps them, because a
//! filter that answered one thing while the morsels were cut and another while they were read would
//! be a scan whose work was divided by one set of rows and done over a different one. A cutoff is
//! the opposite: it is worth nothing until rows have been read and it only tightens afterwards. So
//! it is asked per part, at the moment the part would be read, and never asked while the morsels are
//! cut.
//!
//! # Why the answer does not change
//!
//! Take `ORDER BY k LIMIT n` and let `G` be the key of the last row of the true answer. An instance
//! of the top N that holds `n` candidates holds `n` rows of the input, and the `n`th best of a
//! subset is never better than the `n`th best of the whole, so that instance's worst candidate is
//! always at least as bad as `G`. Any one of them is therefore a sound cutoff, and the tightest of
//! them is the best one, which is why the instances combine theirs with a smallest wins rule.
//!
//! A part is skipped only when every row in it is strictly worse than the cutoff, which by the above
//! makes every row in it strictly worse than `G`, so no row in it is in the answer. Strictly, not at
//! worst equally: a part holding a row that ties `G` is read like any other, so the rule never has
//! to reason about which of two equal rows wins.
//!
//! Ties between rows that do win are settled by where the row arrived, which is a morsel number and
//! an offset inside that morsel, see [`crate::sort::Place`]. Skipping a part changes neither. The
//! morsel numbering comes from the runs the scan cut before any of this started, and the offset is
//! counted over the rows the top N was handed, so removing rows that lose leaves every row that wins
//! in the same order relative to the others as it was. The ten rows that come out are the ten that
//! came out before, in the same order.
//!
//! # What it refuses
//!
//! Only the first sort key, because a part whose first key is all worse than the cutoff's first key
//! is worse whatever the later keys hold, and a part that ties on the first key says nothing.
//!
//! Only a key that reads a column of the scan, through the same walk down a filter and a projection
//! that [`crate::sideways::beneath`] does. An expression over a column has no bounds in the file.
//!
//! Only `NULLS LAST`, because under `NULLS FIRST` a null beats every value, and the two ends of a
//! part say nothing about how many nulls are in it at the point this is asked. The null count is
//! stored beside the ends and a later pass can use it, which is the one thing this leaves on the
//! table.
//!
//! A null worst candidate publishes nothing under `NULLS LAST` either, since it means the instance's
//! tenth best row is a null and no value is worse than that, so there would be nothing to exclude.

use std::cmp::Ordering;
use std::sync::{Arc, OnceLock, RwLock};

use rudb_common::bounds::{Bound, Op};
use rudb_plan::{ColumnBinding, Expr, Plan, Slice, SortKey};

/// The cutoff one top N shares with the scan under it.
///
/// Armed once while the query is built, then written by every instance of the top N and read by
/// every instance of the scan for as long as the query runs. One lock, taken for reading once per
/// part the scan hands back and for writing only when an instance has something better to say than
/// whatever is already there.
#[derive(Debug, Default)]
pub(crate) struct Cutoff {
    /// The scan column the first sort key reads and the comparison a surviving row has to pass.
    ///
    /// Written while the query is built and read afterwards. `None` on everything this refuses, and
    /// a cutoff that was never armed answers nothing to everything.
    about: OnceLock<(ColumnBinding, Op)>,
    /// The worst candidate of the best placed instance that has filled its candidates.
    ///
    /// `None` until some instance has held the bound of rows at once. It only ever tightens.
    worst: RwLock<Option<Bound>>,
}

impl Cutoff {
    /// A cutoff nobody has armed, which is what a top N that cannot use one leaves behind.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Says which scan column the ordering is on and which way it runs.
    ///
    /// Called at most once, while the query is being built and before anything runs. A second call
    /// is ignored rather than refused, because the only caller is the one place in [`crate::build`]
    /// that makes one of these.
    pub(crate) fn about(&self, binding: ColumnBinding, op: Op) {
        let _ = self.about.set((binding, op));
    }

    /// Whether a top N should bother working out its worst candidate for this.
    pub(crate) fn armed(&self) -> bool {
        self.about.get().is_some()
    }

    /// Records that some instance now holds a full set of candidates whose worst key is `bound`.
    ///
    /// Tightening rather than replacing, in the direction the ordering runs: ascending keeps the
    /// smallest of what the instances have said and descending keeps the largest. Either is sound on
    /// its own, by the argument in the module doc, and the tightest of them excludes the most.
    pub(crate) fn reached(&self, bound: Bound) {
        let Some(&(_, op)) = self.about.get() else { return };
        // Read first so that a query whose cutoff has settled, which is almost the whole of one,
        // takes the shared lock and not the exclusive one.
        if let Ok(held) = self.worst.read()
            && held.as_ref().is_some_and(|held| !improves(op, held, &bound))
        {
            return;
        }
        let Ok(mut held) = self.worst.write() else { return };
        *held = Some(match held.take() {
            Some(old) => tightest(op, old, bound),
            None => bound,
        });
    }

    /// The column of scan `index` the ordering is on and which way it runs, before any instance has
    /// said anything. `None` when this was never armed or is about some other scan.
    ///
    /// What the scan asks while it cuts its morsels, to hand out first the parts most likely to hold
    /// the rows the top N wants, so that the cutoff is tight after a few parts rather than after the
    /// scan has walked a stretch of the file in whatever order it was written.
    pub(crate) fn ordered(&self, index: u32) -> Option<(usize, Op)> {
        let &(binding, op) = self.about.get()?;
        (binding.table == index).then_some((binding.column as usize, op))
    }

    /// The test a scan of `index` should measure its parts against right now.
    ///
    /// `None` until this was armed, the column it is about is one of this scan's and some instance
    /// has filled its candidates. The position is a position in the scan's own table index, which is
    /// what a binding that has been walked down to the scan already is.
    pub(crate) fn probe(&self, index: u32) -> Option<(usize, Op, Bound)> {
        let &(binding, op) = self.about.get()?;
        if binding.table != index {
            return None;
        }
        let worst = self.worst.read().ok()?.clone()?;
        Some((binding.column as usize, op, worst))
    }
}

/// The column a top N's ordering is on and the comparison a row has to pass to still be wanted.
///
/// `None` for everything this module refuses: a top N with no keys, a `NULLS FIRST` ordering, and a
/// first key that computes something rather than reading a column. The binding is the one the top N's
/// input hands it, so a caller still has to walk it down to the scan with
/// [`crate::sideways::beneath`].
pub(crate) fn ordering(plan: &Plan, keys: Slice) -> Option<(ColumnBinding, Op)> {
    let &SortKey { expr, descending, nulls_first } = plan.sort_key_list(keys).first()?;
    if nulls_first {
        return None;
    }
    let Expr::Column(binding) = *plan.expr(expr) else { return None };
    // Written with the column on the left, as every probe is. Ascending wants the rows at or below
    // the cutoff and descending wants the rows at or above it, and a part excluded by either is a
    // part whose rows are all strictly worse than it.
    Some((binding, if descending { Op::GreaterOrEqual } else { Op::LessOrEqual }))
}

/// Whether `bound` would exclude more than `held` already does.
///
/// Ascending compares with `<=`, so the smaller of the two leaves fewer rows, and descending
/// compares with `>=`, so the larger one does. A pair of bounds no ordering compares, which would be
/// two different domains out of one expression, improves nothing and leaves what is there.
fn improves(op: Op, held: &Bound, bound: &Bound) -> bool {
    match op {
        Op::LessOrEqual => held.order(bound) == Some(Ordering::Greater),
        _ => held.order(bound) == Some(Ordering::Less),
    }
}

/// The tighter of two cutoffs, which is the smaller ascending and the larger descending.
fn tightest(op: Op, held: Bound, bound: Bound) -> Bound {
    match op {
        Op::LessOrEqual => held.smaller(bound),
        _ => held.larger(bound),
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::bounds::{Bound, Op};
    use rudb_plan::ColumnBinding;

    use super::Cutoff;

    #[test]
    fn an_unarmed_cutoff_answers_nothing_and_keeps_nothing() {
        let cutoff = Cutoff::new();
        assert!(!cutoff.armed());
        cutoff.reached(Bound::Int(7));
        assert!(cutoff.probe(0).is_none(), "nothing was ever said about a column");
    }

    #[test]
    fn a_cutoff_with_no_candidates_yet_excludes_nothing() {
        let cutoff = Cutoff::new();
        cutoff.about(ColumnBinding::new(0, 3), Op::LessOrEqual);
        assert!(cutoff.armed());
        assert!(cutoff.probe(0).is_none(), "no instance has filled its candidates");
    }

    #[test]
    fn a_scan_of_another_table_is_told_nothing() {
        let cutoff = Cutoff::new();
        cutoff.about(ColumnBinding::new(1, 3), Op::LessOrEqual);
        cutoff.reached(Bound::Int(7));
        assert!(cutoff.probe(0).is_none());
        assert!(cutoff.probe(1).is_some());
    }

    #[test]
    fn ascending_keeps_the_smallest_worst_any_instance_has_had() {
        let cutoff = Cutoff::new();
        cutoff.about(ColumnBinding::new(0, 0), Op::LessOrEqual);
        cutoff.reached(Bound::Int(40));
        cutoff.reached(Bound::Int(10));
        cutoff.reached(Bound::Int(30));
        assert_eq!(cutoff.probe(0), Some((0, Op::LessOrEqual, Bound::Int(10))));
    }

    #[test]
    fn descending_keeps_the_largest() {
        let cutoff = Cutoff::new();
        cutoff.about(ColumnBinding::new(0, 0), Op::GreaterOrEqual);
        cutoff.reached(Bound::Int(10));
        cutoff.reached(Bound::Int(40));
        cutoff.reached(Bound::Int(30));
        assert_eq!(cutoff.probe(0), Some((0, Op::GreaterOrEqual, Bound::Int(40))));
    }
}

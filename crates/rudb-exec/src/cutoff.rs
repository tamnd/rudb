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
//! that [`crate::sideways::beneath`] does, or a key that an expression works out from one such
//! column without ever putting two of its values the other way round. An expression like that has
//! no bounds in the file, but it has them at one remove: the key of every row in a part is at least
//! the key of the part's smallest value and at most the key of its largest, so the expression is
//! worked out at the two ends the file stores and the part is measured against those. See
//! [`Through`] for which expressions those are and why the benchmark wants them.
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
use rudb_common::{Field, LogicalType};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan, Slice, SortKey};
use rudb_storage::zone::Range;
use rudb_vector::{Chunk, Vector};

use crate::prepared::Prepared;
use crate::schema::Schema;
use crate::sideways;

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
    /// How the key is worked out from that column, when the first sort key is an expression over it
    /// rather than the column itself. Written while the query is built, like `about`.
    through: OnceLock<Through>,
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
    ///
    /// `None` as well when the key is an expression over the column, since the worst candidate is
    /// then a key and not a value of the column. [`Self::through`] is what such a scan asks instead.
    pub(crate) fn probe(&self, index: u32) -> Option<(usize, Op, Bound)> {
        if self.through.get().is_some() {
            return None;
        }
        self.worst_for(index)
    }

    /// The same as [`Self::probe`] for a key that is an expression over the column, with what works
    /// the key out from the column's stored ends.
    pub(crate) fn through(&self, index: u32) -> Option<(usize, Op, Bound, &Through)> {
        let through = self.through.get()?;
        let (column, op, worst) = self.worst_for(index)?;
        Some((column, op, worst, through))
    }

    fn worst_for(&self, index: u32) -> Option<(usize, Op, Bound)> {
        let &(binding, op) = self.about.get()?;
        if binding.table != index {
            return None;
        }
        let worst = self.worst.read().ok()?.clone()?;
        Some((binding.column as usize, op, worst))
    }
}

/// The first sort key of a top N worked out from the scan column it is an expression over.
///
/// ClickBench reads its Parquet file through a view that turns the stored seconds into a timestamp,
/// `TIMESTAMP '1970-01-01' + INTERVAL (EventTime) SECOND`, so `ORDER BY EventTime LIMIT 10` orders
/// by that expression and not by a column. With the cutoff refused, q25 and q27 read and sort every
/// row the filter keeps, 0.14 and 0.22 seconds of CPU, where over the same rows stored natively the
/// cutoff rules out nearly every part and they take a hundredth of that.
///
/// The expressions taken are the ones that never put two values of their column the other way
/// round, which [`rising`] decides from their shape, so a part whose smallest value already has a
/// key worse than the cutoff has nothing but rows worse than it. The key at an end is worked out by
/// running the expression over that one value, so it is whatever the query would have computed.
#[derive(Debug)]
pub(crate) struct Through {
    /// The expression, prepared against the one column it reads.
    key: Prepared,
    /// The type of that column, which is what a stored end is read back as.
    column: LogicalType,
}

impl Through {
    /// Prepares `key` against the column `leaf` reads, which is the one column [`rising`] found.
    fn of(plan: &Plan, key: ExprRef, leaf: ExprRef) -> Option<Self> {
        let Expr::Column(binding) = *plan.expr(leaf) else { return None };
        let column = plan.expr_type(leaf).clone();
        let schema = Schema::new(vec![Field::new("key", column.clone())], vec![binding]).ok()?;
        let key = Prepared::one(plan, key, &schema).ok()?;
        Some(Self { key, column })
    }

    /// The key a row holding `end` in the column has, or `None` when it cannot be worked out, which
    /// includes an end the expression fails on or turns into a null.
    fn at(&self, end: &Bound) -> Option<Bound> {
        let value = end.into_value(&self.column)?;
        let chunk =
            Chunk::new(vec![Vector::from_values(self.column.clone(), &[value]).ok()?]).ok()?;
        let mut scratch = self.key.scratch();
        let key = self.key.evaluate_one(&chunk, &mut scratch).ok()?;
        Bound::of_value(&key.value_at(0))
    }

    /// Whether every row of a part whose column spans `range` has a key strictly worse than `worst`.
    ///
    /// Ascending, the smallest key in the part is the key of its smallest value, and descending the
    /// largest is the key of its largest. A part with no ends, which is a part of nulls or one whose
    /// ends were not kept, is read.
    pub(crate) fn beaten(&self, op: Op, worst: &Bound, range: &Range) -> bool {
        match op {
            Op::LessOrEqual => range
                .low
                .as_ref()
                .and_then(|low| self.at(low))
                .is_some_and(|key| key.order(worst) == Some(Ordering::Greater)),
            _ => range
                .high
                .as_ref()
                .and_then(|high| self.at(high))
                .is_some_and(|key| key.order(worst) == Some(Ordering::Less)),
        }
    }
}

/// Arms `cutoff` for a top N over `input` ordered by `keys`, when its first key reaches a column of
/// a scan below.
///
/// Nothing is armed for everything this module refuses: a top N with no keys, a `NULLS FIRST`
/// ordering, and a first key that is neither a column nor an expression [`rising`] takes. The walk
/// down to the scan is the one [`crate::sideways::beneath`] makes, through filters, projections and
/// the driving side of a join, except that one projection on the way may compute the key rather
/// than pass a column on.
pub(crate) fn arm(cutoff: &Cutoff, plan: &Plan, input: NodeRef, keys: Slice) -> Option<()> {
    let &SortKey { expr, descending, nulls_first } = plan.sort_key_list(keys).first()?;
    if nulls_first {
        return None;
    }
    let mut through = None;
    let mut binding = match *plan.expr(expr) {
        Expr::Column(binding) => binding,
        _ => {
            let leaf = rising(plan, expr)?;
            through = Some((expr, leaf));
            let Expr::Column(binding) = *plan.expr(leaf) else { return None };
            binding
        }
    };
    let mut at = input;
    loop {
        match *plan.node(at) {
            Node::Get { index, .. } | Node::TableFunction { index, .. } => {
                if binding.table != index {
                    return None;
                }
                break;
            }
            Node::Filter { input, .. } => at = input,
            Node::Project { input, index, exprs, .. } => {
                if binding.table == index {
                    let expr = *plan.expr_list(exprs).get(binding.column as usize)?;
                    binding = match *plan.expr(expr) {
                        Expr::Column(inner) => inner,
                        _ if through.is_none() => {
                            let leaf = rising(plan, expr)?;
                            through = Some((expr, leaf));
                            let Expr::Column(inner) = *plan.expr(leaf) else { return None };
                            inner
                        }
                        _ => return None,
                    };
                }
                at = input;
            }
            ref node @ (Node::Join { .. } | Node::LinkJoin { .. }) => at = sideways::through(node)?,
            _ => return None,
        }
    }
    if let Some((key, leaf)) = through {
        cutoff.through.set(Through::of(plan, key, leaf)?).ok()?;
    }
    // Written with the column on the left, as every probe is. Ascending wants the rows at or below
    // the cutoff and descending wants the rows at or above it, and a part excluded by either is a
    // part whose rows are all strictly worse than it.
    cutoff.about(binding, if descending { Op::GreaterOrEqual } else { Op::LessOrEqual });
    Some(())
}

/// The column reference inside `expr`, when `expr` reads one column and never gives a smaller
/// value of it a larger answer than a larger one.
///
/// Decided from the shape alone, and only shapes whose every step keeps the order: the column
/// itself, a cast from a whole number or a decimal to another number and between a date and a
/// timestamp, a constant added or taken away, and a whole number of seconds, minutes, hours,
/// milliseconds or microseconds turned into an interval to add to a constant timestamp. An answer
/// that comes out null at some value is a key every other row is ordered ahead of, under the
/// `NULLS LAST` this is armed for, so it bends nothing.
fn rising(plan: &Plan, expr: ExprRef) -> Option<ExprRef> {
    match *plan.expr(expr) {
        Expr::Column(_) => Some(expr),
        Expr::Cast { input, .. } => {
            let (from, to) = (plan.expr_type(input), plan.expr_type(expr));
            let kept = ((from.is_integer() || matches!(from, LogicalType::Decimal { .. }))
                && to.is_numeric())
                || matches!(
                    (from, to),
                    (LogicalType::Date, LogicalType::Timestamp)
                        | (LogicalType::Timestamp, LogicalType::Date)
                );
            if kept { rising(plan, input) } else { None }
        }
        Expr::Function { name, args } => {
            let ty = plan.expr_type(expr);
            match (plan.string(name), plan.expr_list(args)) {
                ("+", &[one, other]) if shifted(ty) => match (fixed(plan, one), fixed(plan, other))
                {
                    (true, false) => moving(plan, other),
                    (false, true) => moving(plan, one),
                    _ => None,
                },
                ("-", &[one, other]) if shifted(ty) && fixed(plan, other) && !fixed(plan, one) => {
                    moving(plan, one)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// The column the moving side of a sum reads, which is [`rising`] except for an interval.
///
/// Two intervals are ordered by a length that counts a month as thirty days, and a timestamp moved
/// by them is not, so an interval only counts when it is a whole number of one fixed unit.
fn moving(plan: &Plan, expr: ExprRef) -> Option<ExprRef> {
    if plan.expr_type(expr) != &LogicalType::Interval {
        return rising(plan, expr);
    }
    let Expr::Function { name, args } = *plan.expr(expr) else { return None };
    let &[count] = plan.expr_list(args) else { return None };
    matches!(
        plan.string(name),
        "to_seconds" | "to_minutes" | "to_hours" | "to_milliseconds" | "to_microseconds"
    )
    .then(|| rising(plan, count))
    .flatten()
}

/// Whether a sum of this type keeps the order of its moving side, which a number, a date and a
/// timestamp do.
fn shifted(ty: &LogicalType) -> bool {
    ty.is_numeric() || matches!(ty, LogicalType::Date | LogicalType::Timestamp)
}

/// Whether `expr` is a constant that is not null.
fn fixed(plan: &Plan, expr: ExprRef) -> bool {
    matches!(*plan.expr(expr), Expr::Constant(value) if !plan.value(value).is_null())
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
    use rudb_plan::{ColumnBinding, Node, Plan};
    use rudb_storage::zone::Range;

    use super::{Cutoff, arm};

    /// The seconds column of the Parquet view, turned into a timestamp the way the view does it.
    const STAMPED: &str =
        "\"+\"(0::TIMESTAMP, to_seconds(CAST(#0.0::BIGINT)::DOUBLE)::INTERVAL)::TIMESTAMP";

    /// Arms a fresh cutoff for the top N `text` is, and gives it back armed or not.
    fn armed(text: &str) -> std::sync::Arc<Cutoff> {
        let plan = Plan::parse(text).expect("a well formed plan");
        let Node::TopN { input, keys, .. } = *plan.node(plan.root()) else {
            panic!("the root of that text is a top N");
        };
        let cutoff = Cutoff::new();
        let _ = arm(&cutoff, &plan, input, keys);
        cutoff
    }

    fn spanning(low: i128, high: i128) -> Range {
        Range {
            low: Some(Bound::Int(low)),
            high: Some(Bound::Int(high)),
            nulls: 0,
            exact: true,
            sum: None,
        }
    }

    fn stamp(seconds: i128) -> Bound {
        Bound::Scaled { unscaled: seconds * 1_000_000, scale: 6 }
    }

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

    #[test]
    fn a_key_computed_from_a_column_is_measured_through_the_expression() {
        let cutoff = armed(&format!(
            "TopN 10 offset 0 [#1.0::TIMESTAMP ASC NULLS LAST]\n  Project #1 [{STAMPED} AS e]\n    Get memory.main.t AS t #0 [x::BIGINT]"
        ));
        assert_eq!(cutoff.ordered(0), Some((0, Op::LessOrEqual)));
        assert_eq!(cutoff.probe(0), None, "nothing has been reached yet");
        cutoff.reached(stamp(1_000));
        assert_eq!(cutoff.probe(0), None, "the worst key is not a value of the column");
        let (column, op, worst, through) =
            cutoff.through(0).expect("an expression key is measured through it");
        assert_eq!((column, op), (0, Op::LessOrEqual));
        assert!(
            through.beaten(op, &worst, &spanning(1_001, 5_000)),
            "every row is later than the worst"
        );
        assert!(!through.beaten(op, &worst, &spanning(1_000, 5_000)), "a tie is read");
        assert!(!through.beaten(op, &worst, &spanning(10, 20)), "earlier rows are read");
        let nothing = Range { low: None, high: None, nulls: 4, exact: true, sum: None };
        assert!(!through.beaten(op, &worst, &nothing), "a part with no ends is read");
    }

    #[test]
    fn descending_measures_the_high_end_through_the_expression() {
        let cutoff = armed(&format!(
            "TopN 10 offset 0 [{STAMPED} DESC NULLS LAST]\n  Get memory.main.t AS t #0 [x::BIGINT]"
        ));
        cutoff.reached(stamp(1_000));
        let (_, op, worst, through) =
            cutoff.through(0).expect("a key over the scan column is measured");
        assert_eq!(op, Op::GreaterOrEqual);
        assert!(through.beaten(op, &worst, &spanning(10, 999)));
        assert!(!through.beaten(op, &worst, &spanning(10, 1_000)));
    }

    #[test]
    fn a_key_that_can_put_values_the_other_way_round_arms_nothing() {
        for key in [
            "\"*\"(#0.0::BIGINT, #0.0::BIGINT)::BIGINT",
            "\"-\"(0::BIGINT, #0.0::BIGINT)::BIGINT",
            "\"+\"(#0.0::BIGINT, #0.0::BIGINT)::BIGINT",
        ] {
            let cutoff = armed(&format!(
                "TopN 10 offset 0 [{key} ASC NULLS LAST]\n  Get memory.main.t AS t #0 [x::BIGINT]"
            ));
            assert!(!cutoff.armed(), "{key}");
        }
        let cutoff = armed(&format!(
            "TopN 10 offset 0 [{STAMPED} ASC NULLS FIRST]\n  Get memory.main.t AS t #0 [x::BIGINT]"
        ));
        assert!(!cutoff.armed(), "nulls first");
    }
}

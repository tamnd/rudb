//! What a hash join tells the scan under its driving side, once its build side is in.
//!
//! By the time a join has gathered one side it knows something about the other side's key that no
//! statistic could have told the planner: the exact smallest and largest key it will ever match. A
//! driving row outside that range matches nothing, and a whole stored chunk outside it is a chunk
//! the scan never reads, never decompresses and never decodes. That is the tier
//! `spec/planner/09-runtime-filters-and-adaptivity.md` section 09.2 calls always on, and it is the
//! paragraph of `spec/engine/08-join.md` section 8.5 about composing with zone maps. It costs two
//! comparisons per chunk against numbers that are already in memory and it carries no bytes of its
//! own.
//!
//! # Why it is a handoff rather than an argument
//!
//! The two sides of a join are two pipelines with an edge between them, and this crosses that edge
//! in the same direction the rows do. The build pipeline finishes before the driving one starts,
//! which is what the edge means, so by the time the driving side is asked how to divide its work the
//! range is known. What carries it is one shared object: the join arms it while the query is being
//! built, the sink at the end of the build side fills it while the build side runs, and the scan
//! reads it when it is asked for its morsels. Nothing locks, because each of the three steps happens
//! strictly after the one before it.
//!
//! # What it refuses, and why each refusal is a wrong answer avoided
//!
//! A scan that drops rows is only allowed where the join was going to drop them anyway.
//!
//! **The kind.** An inner join and a semi join throw away a driving row that matches nothing, so
//! dropping it earlier is the same answer. A left, an anti and a single join all answer with that
//! row, so dropping it is a row missing from the result. Only the first two arm this.
//!
//! **The null rule.** `NULL = NULL` is null, so a driving row whose key is null matches nothing and
//! a range that excludes it is excluding a row that was going to go. `IS NOT DISTINCT FROM` is the
//! other rule for the same value and under it two nulls match, so a range, which is about order and
//! has nothing to say about nulls, is not allowed to decide anything. Only `=` arms this.
//!
//! **The shape below the join.** The range is a fact about one column of one stored table, so what
//! consumes it has to be the scan of that table with nothing between the two that decides which rows
//! survive by counting rather than by value. A `LIMIT` over a `SORT` is the case that says so:
//! filtering the scan under it changes which rows reach the limit, which changes the answer even
//! though every row removed would have failed the join. [`crate::build`] walks down through a filter
//! and a projection and stops at anything else.
//!
//! Anything this cannot arm is a query that runs exactly as it did before, because a scan with no
//! range handed to it asks nothing and reads everything.

use std::sync::{Arc, OnceLock};

use rudb_common::SessionTimeZone;
use rudb_common::bounds::{Bound, Op};
use rudb_plan::{ColumnBinding, ExprRef, Plan};
use rudb_storage::Range;
use rudb_vector::Vector;

use crate::schema::Schema;

/// The edge one join's range crosses, shared between the join, its build side's sink and one scan.
///
/// Every field is written once and read afterwards, in the order the fields are declared, which is
/// the order the three steps happen in. A [`Sideways`] that was never armed answers nothing to
/// everything, which is what a join that cannot use one leaves behind.
#[derive(Debug, Default)]
pub(crate) struct Sideways<'a> {
    /// How to read the build side's key out of its chunks. Written by the join while the query is
    /// being built, and read by the sink at the end of the build side.
    keyed: OnceLock<Keyed<'a>>,
    /// The column of the driving side the key is compared against, which is the column this is
    /// about. Written at the same moment as `keyed` and read by the scan.
    binding: OnceLock<ColumnBinding>,
    /// What the build side turned out to hold, `None` where the column had no ordered bound or
    /// where the build side had no rows with a key at all. Written by the sink when the build side
    /// finishes and read by the scan when it is asked for its morsels.
    range: OnceLock<Option<(Bound, Bound)>>,
}

/// How to read one key column out of a chunk of the build side.
///
/// The expression rather than a column number, because the binder writes `p.k::INTEGER = b.k` as a
/// cast around one operand and the value that goes in the table is the cast one. This is the same
/// expression the hash table is built on, evaluated the same way.
#[derive(Debug)]
pub(crate) struct Keyed<'a> {
    plan: &'a Plan,
    expr: ExprRef,
    schema: Schema,
    time_zone: SessionTimeZone,
}

impl<'a> Keyed<'a> {
    /// The key expression `expr` over rows shaped like `schema`.
    pub(crate) fn new(
        plan: &'a Plan,
        expr: ExprRef,
        schema: Schema,
        time_zone: SessionTimeZone,
    ) -> Self {
        Self { plan, expr, schema, time_zone }
    }

    /// The three things the evaluator wants, so that the caller cannot put them in the wrong order.
    pub(crate) fn parts(&self) -> (&'a Plan, [ExprRef; 1], &Schema, SessionTimeZone) {
        (self.plan, [self.expr], &self.schema, self.time_zone)
    }
}

impl<'a> Sideways<'a> {
    /// A handoff nobody has armed, which is what a join that cannot use one leaves behind.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Says how the build side's key is read, for the sink that is about to walk it.
    ///
    /// Called at most once, while the query is being built and before anything runs. A second call
    /// is ignored rather than refused, because the only caller is the one place in [`crate::build`]
    /// that makes one of these and a second arming would be a bug there rather than in a query.
    pub(crate) fn keying(&self, keyed: Keyed<'a>) {
        let _ = self.keyed.set(keyed);
    }

    /// Says which driving column the range is going to be about, for the scan that reads it.
    ///
    /// Separate from [`Sideways::keying`] because the two are read by different operators and a scan
    /// that knows the column has no use for the expression that produced the range.
    pub(crate) fn about(&self, binding: ColumnBinding) {
        let _ = self.binding.set(binding);
    }

    /// How to read the build side's key, for the sink that is about to walk it.
    pub(crate) fn keyed(&self) -> Option<&Keyed<'a>> {
        self.keyed.get()
    }

    /// Records what the build side held. Called once, when the build side's pipeline finishes.
    pub(crate) fn found(&self, range: Option<(Bound, Bound)>) {
        let _ = self.range.set(range);
    }

    /// The tests a scan of `index` should add to the ones the plan already gave it.
    ///
    /// Empty unless this was armed, the build side has finished, it found a range, and the column
    /// the range is about is one of this scan's. The positions are positions in the scan's
    /// projection, which is what a binding into a scan's own table index already is.
    pub(crate) fn tests(&self, index: u32) -> Vec<(usize, Op, Bound)> {
        let (Some(binding), Some(Some((low, high)))) = (self.binding.get(), self.range.get())
        else {
            return Vec::new();
        };
        if binding.table != index {
            return Vec::new();
        }
        let column = binding.column as usize;
        vec![(column, Op::GreaterOrEqual, low.clone()), (column, Op::LessOrEqual, high.clone())]
    }
}

/// The smallest and largest key one side of a join holds, accumulated a chunk at a time.
///
/// One pass per chunk over one column, which is the whole cost of this filter on the side that
/// builds it. It is the gathered side, which is the smaller of the two by the time the optimizer has
/// chosen which way round to run the join, and the pass is the same pass the zone maps on disk are
/// written by.
#[derive(Debug, Default, Clone)]
pub(crate) struct Extremes {
    low: Option<Bound>,
    high: Option<Bound>,
}

impl Extremes {
    /// Widens this to cover one more chunk's worth of keys.
    ///
    /// A column with no ordered bound in it, which is a column of all nulls or of a type no bound
    /// compares with, widens this by nothing. That is right for the nulls, because a null key
    /// matches nothing under the rule this is armed for, and right for the type, because a column
    /// this cannot summarize leaves the range as it was and the range is only ever used to exclude.
    pub(crate) fn widen(&mut self, keys: &Vector) {
        let range = Range::of(keys);
        if let Some(low) = range.low {
            self.low = Some(match self.low.take() {
                Some(held) => held.smaller(low),
                None => low,
            });
        }
        if let Some(high) = range.high {
            self.high = Some(match self.high.take() {
                Some(held) => held.larger(high),
                None => high,
            });
        }
    }

    /// Both ends, or nothing when either end is missing.
    ///
    /// Both or neither, because a range with one open end excludes nothing on that side and a caller
    /// that had to check would be a caller that could forget.
    pub(crate) fn into_range(self) -> Option<(Bound, Bound)> {
        Some((self.low?, self.high?))
    }

    /// Takes the other instance's ends into this one, for a sink combining what its instances saw.
    pub(crate) fn absorb(&mut self, other: Self) {
        if let Some(low) = other.low {
            self.low = Some(match self.low.take() {
                Some(held) => held.smaller(low),
                None => low,
            });
        }
        if let Some(high) = other.high {
            self.high = Some(match self.high.take() {
                Some(held) => held.larger(high),
                None => high,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::bounds::{Bound, Op};
    use rudb_common::{LogicalType, Value};
    use rudb_plan::ColumnBinding;
    use rudb_vector::Vector;

    use super::{Extremes, Sideways};

    fn column(values: &[Option<i32>]) -> Vector {
        let values: Vec<Value> =
            values.iter().map(|value| value.map_or(Value::Null, Value::Integer)).collect();
        Vector::from_values(LogicalType::Integer, &values).expect("a column of integers")
    }

    #[test]
    fn the_range_of_several_chunks_covers_every_one_of_them() {
        let mut extremes = Extremes::default();
        extremes.widen(&column(&[Some(5), Some(9)]));
        extremes.widen(&column(&[Some(2), Some(7)]));
        assert_eq!(extremes.into_range(), Some((Bound::Int(2), Bound::Int(9))));
    }

    /// A null is not a key under the rule this filter is armed for, so it widens nothing.
    #[test]
    fn a_column_of_nulls_widens_nothing() {
        let mut extremes = Extremes::default();
        extremes.widen(&column(&[Some(4)]));
        extremes.widen(&column(&[None, None]));
        assert_eq!(extremes.into_range(), Some((Bound::Int(4), Bound::Int(4))));
    }

    /// A build side with no keys at all leaves no range, which a scan reads as nothing to add.
    #[test]
    fn nothing_seen_is_no_range() {
        assert_eq!(Extremes::default().into_range(), None);
    }

    #[test]
    fn two_instances_combine_into_the_range_covering_both() {
        let mut left = Extremes::default();
        left.widen(&column(&[Some(3)]));
        let mut right = Extremes::default();
        right.widen(&column(&[Some(8)]));
        left.absorb(right);
        assert_eq!(left.into_range(), Some((Bound::Int(3), Bound::Int(8))));
    }

    /// The scan asks by table index, so a range about another table's column is not this scan's.
    #[test]
    fn a_scan_is_told_only_about_its_own_column() {
        let sideways = Sideways::new();
        sideways.found(Some((Bound::Int(1), Bound::Int(4))));
        // Armed without a key expression, which the scan does not read.
        sideways.about(ColumnBinding::new(7, 2));

        assert!(sideways.tests(8).is_empty(), "another table's scan");
        assert_eq!(
            sideways.tests(7),
            vec![(2, Op::GreaterOrEqual, Bound::Int(1)), (2, Op::LessOrEqual, Bound::Int(4)),]
        );
    }

    /// A join that never armed one, and a build side that finished with no range, both answer
    /// nothing, which is a scan that reads everything exactly as it did before.
    #[test]
    fn an_unarmed_handoff_and_an_empty_build_side_both_say_nothing() {
        let unarmed = Sideways::new();
        assert!(unarmed.tests(1).is_empty());

        let empty = Sideways::new();
        empty.about(ColumnBinding::new(1, 0));
        empty.found(None);
        assert!(empty.tests(1).is_empty());
    }
}

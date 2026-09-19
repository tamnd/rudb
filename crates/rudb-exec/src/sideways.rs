//! What a hash join tells the scan under its driving side, once its build side is in.
//!
//! By the time a join has gathered one side it knows something about the other side's key that no
//! statistic could have told the planner: the exact set of keys it will ever match. A driving row
//! outside that set matches nothing, so a scan that drops it does the join's work earlier and over
//! less.
//!
//! Two tiers of the same fact, in the order they cost anything.
//!
//! The range, which is the smallest and the largest key. A whole stored chunk outside it is a chunk
//! the scan never reads, never decompresses and never decodes. That is the tier
//! `spec/planner/09-runtime-filters-and-adaptivity.md` section 09.2 calls always on, and it is the
//! paragraph of `spec/engine/08-join.md` section 8.5 about composing with zone maps. It costs two
//! comparisons per chunk against numbers that are already in memory and it carries no bytes of its
//! own.
//!
//! The filter, which is the keys themselves to the precision ten bits each buys. It answers about a
//! row rather than a chunk, so it is what is left for a fact table whose join key is spread over its
//! whole domain, where the range covers every chunk and rules out nothing. That is section 8.5's
//! first paragraph and the largest win it names. It costs a hash of the key column per chunk on both
//! sides and one cache line touched per row, and its bytes are proportional to the build side's
//! rows rather than to the driving side's.
//!
//! # Why it is a handoff rather than an argument
//!
//! The two sides of a join are two pipelines with an edge between them, and this crosses that edge
//! in the same direction the rows do. The build pipeline finishes before the driving one starts,
//! which is what the edge means, so by the time the driving side is asked how to divide its work
//! both tiers are known. What carries them is one shared object: the join arms it while the query is
//! being built, the sink at the end of the build side fills it as that side finishes, and the scan
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
//! dropping it is dropping a row that was going to go. `IS NOT DISTINCT FROM` is the other rule for
//! the same value and under it two nulls match, so neither a range, which is about order, nor a
//! filter, which was built without the nulls in it, is allowed to decide anything. Only `=` arms
//! this.
//!
//! **The shape below the join.** Both tiers are facts about one column of one stored table, so what
//! consumes them has to be the scan of that table with nothing between the two that decides which
//! rows survive by counting rather than by value. A `LIMIT` over a `SORT` is the case that says so:
//! filtering the scan under it changes which rows reach the limit, which changes the answer even
//! though every row removed would have failed the join. [`crate::build`] walks down through a filter
//! and a projection and stops at anything else.
//!
//! Anything this cannot arm is a query that runs exactly as it did before, because a scan handed
//! nothing asks nothing and reads everything.
//!
//! # The column the join names is not the column the scan produces
//!
//! A projection binds its output against a table index of its own, so the driving column a join
//! knows about is `#1.0` where the scan under it produces `#0.0`, and a scan asked about a binding
//! into some other table answers nothing. That is one node between the two and it is there in every
//! plan that projects, which is every plan over a view and every plan pushdown has been through, so
//! taking the join's binding as it stands is a runtime filter that is built, handed over and never
//! read. [`beneath`] is the walk that turns the one into the other, down the same two nodes the
//! builder allows and through nothing else, and a projection that computes its column rather than
//! passing one through ends the walk with nothing, because a fact about a value says nothing about
//! what an expression over it produces.

use std::sync::{Arc, OnceLock};

use rudb_common::bounds::{Bound, Op};
use rudb_common::{Result, SessionTimeZone};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan};
use rudb_storage::{Blocked, Range};
use rudb_vector::{Chunk, Vector};

use crate::expr::evaluate_all_in_time_zone;
use crate::lookup::has_nulls;
use crate::schema::Schema;
use crate::table::{Across, hash};

/// The most a runtime filter may take, which is thirty two megabytes.
///
/// Ten bits a key is twenty six million keys inside that, and a build side larger than that is one
/// where the driving side is unlikely to be the one worth reading less of. Past it the range is
/// still handed over and the filter is not, which is the same answer for fewer bytes.
const BUDGET: usize = 32 << 20;

/// The edge one join's runtime filter crosses, shared between the join, its build side's sink and
/// one scan.
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
    /// What the build side turned out to hold. Written by the sink when the build side finishes and
    /// read by the scan when it is asked for its morsels.
    found: OnceLock<Found>,
}

/// What one side of a join holds, as much of it as was worth keeping.
///
/// Both halves are optional and they are optional separately. A side with no keyed row at all has
/// neither. A side whose key column has no ordered bound, which is a type no [`Bound`] compares,
/// has no range and may still have a filter. A side too large for [`BUDGET`] has a range and no
/// filter.
#[derive(Debug, Default)]
pub(crate) struct Found {
    /// The smallest and the largest key, both ends or neither.
    range: Option<(Bound, Bound)>,
    /// The keys themselves, to the precision ten bits each buys.
    filter: Option<Blocked>,
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
    pub(crate) fn found(&self, found: Found) {
        let _ = self.found.set(found);
    }

    /// The tests a scan of `index` should add to the ones the plan already gave it.
    ///
    /// Empty unless this was armed, the build side has finished, it found a range, and the column
    /// the range is about is one of this scan's. The positions are positions in the scan's
    /// projection, which is what a binding into a scan's own table index already is.
    pub(crate) fn tests(&self, index: u32) -> Vec<(usize, Op, Bound)> {
        let (Some(binding), Some(Some((low, high)))) =
            (self.binding.get(), self.found.get().map(|found| &found.range))
        else {
            return Vec::new();
        };
        if binding.table != index {
            return Vec::new();
        }
        let column = binding.column as usize;
        vec![(column, Op::GreaterOrEqual, low.clone()), (column, Op::LessOrEqual, high.clone())]
    }

    /// The filter a scan of `index` should put its rows through, and which of its columns.
    ///
    /// `None` on everything the tests above answer nothing about, and also on a build side that was
    /// too large for one. The position is the same projection position, because a row is dropped by
    /// reading the column the scan has just produced rather than by reading the table.
    pub(crate) fn sifting(&self, index: u32) -> Option<(usize, &Blocked)> {
        let binding = self.binding.get()?;
        if binding.table != index {
            return None;
        }
        Some((binding.column as usize, self.found.get()?.filter.as_ref()?))
    }
}

/// The same column as `binding`, named the way the scan at the bottom of `node` names it.
///
/// The join knows its driving column as the projection above the scan binds it, and the scan knows
/// its own columns, so somebody has to walk between the two. This does, down the two nodes
/// [`crate::build`] lets a runtime filter through, and it is the same walk for the same reason: a
/// filter keeps rows and renames nothing, so the binding goes through it untouched, and a projection
/// rebinds, so the binding becomes whatever the expression in that position is.
///
/// `None` unless the walk ends at a scan of the table the binding is about by then. A projection
/// whose column at that position is an expression rather than a column ends it, because a set of
/// values says nothing about what an expression over them produces, and so does a node the builder
/// would have refused anyway, which is here as well so that the two cannot drift apart.
pub(crate) fn beneath(plan: &Plan, node: NodeRef, binding: ColumnBinding) -> Option<ColumnBinding> {
    let mut at = node;
    let mut binding = binding;
    loop {
        match *plan.node(at) {
            Node::Get { index, .. } | Node::TableFunction { index, .. } => {
                return (binding.table == index).then_some(binding);
            }
            Node::Filter { input, .. } => at = input,
            Node::Project { input, index, exprs, .. } => {
                if binding.table == index {
                    let exprs = plan.expr_list(exprs);
                    let at = exprs.get(binding.column as usize)?;
                    let Expr::Column(inner) = *plan.expr(*at) else { return None };
                    binding = inner;
                }
                at = input;
            }
            _ => return None,
        }
    }
}

/// What the build side holds, read off the chunks it was gathered into.
///
/// One pass over one column of the smaller side of the join, at the moment that side is complete.
/// At the moment rather than as the chunks arrive, because the filter has to be sized before the
/// first key goes into it and the row count is exact only once the side has finished. Two passes
/// would be the alternative and the second of them is this one.
///
/// The key is the expression rather than a column number, because the binder writes `p.k::INTEGER =
/// b.k` as a cast around one operand and the value that goes into the hash table is the cast one.
/// The hash is the same hash the hash table takes, over the value rather than over a dictionary
/// code, which is what lets the scan on the other side hash its raw column and get the same word.
///
/// # Errors
///
/// Whatever evaluating the key expression raises, which is what the hash table build would have
/// raised over the same rows a moment later.
pub(crate) fn found(keyed: &Keyed<'_>, chunks: &[Chunk]) -> Result<Found> {
    let (plan, exprs, schema, time_zone) = keyed.parts();
    let rows: usize = chunks.iter().map(Chunk::len).sum();
    let mut extremes = Extremes::default();
    let mut filter = Blocked::sized(rows, BUDGET);
    let mut hashes = Vec::new();
    for chunk in chunks {
        let keys = evaluate_all_in_time_zone(plan, &exprs, schema, chunk, time_zone)?;
        let Some(keys) = keys.first() else { continue };
        extremes.widen(keys);
        let Some(filter) = filter.as_mut() else { continue };
        hash(std::slice::from_ref(keys), chunk.len(), &mut hashes, Across::TwoInputs);
        // A null key matches nothing under the rule this is armed for, so it is left out here and a
        // driving row holding one is dropped by the filter it is missing from. That is the same
        // answer the hash table gives and it is arrived at a scan earlier.
        let nullable = has_nulls(keys, chunk.len());
        for (row, &word) in hashes.iter().enumerate() {
            if nullable && keys.is_null_at(row) {
                continue;
            }
            filter.add(word);
        }
    }
    Ok(Found { range: extremes.into_range(), filter })
}

/// The smallest and largest key one side of a join holds, widened a chunk at a time.
///
/// The cheap half of [`found`], and the half that survives a column the filter could not be sized
/// for. Two comparisons a chunk against numbers already in a register.
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
}

impl Found {
    /// A build side that turned out to hold this, for the tests that stand in for one.
    #[cfg(test)]
    pub(crate) fn of(range: Option<(Bound, Bound)>, filter: Option<Blocked>) -> Self {
        Self { range, filter }
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::bounds::{Bound, Op};
    use rudb_common::{Field, LogicalType, SessionTimeZone, Value};
    use rudb_plan::{ColumnBinding, Expr, ExprRef, Plan};
    use rudb_storage::Blocked;
    use rudb_vector::{Chunk, Vector};

    use super::{Across, Extremes, Found, Keyed, Schema, Sideways, beneath, found, hash};

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

    /// The scan asks by table index, so a range about another table's column is not this scan's.
    #[test]
    fn a_scan_is_told_only_about_its_own_column() {
        let sideways = Sideways::new();
        sideways.found(Found::of(Some((Bound::Int(1), Bound::Int(4))), None));
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
        assert!(unarmed.sifting(1).is_none());

        let empty = Sideways::new();
        empty.about(ColumnBinding::new(1, 0));
        empty.found(Found::of(None, None));
        assert!(empty.tests(1).is_empty());
        assert!(empty.sifting(1).is_none());
    }

    /// One chunk of one integer column, which is the shape a build side of one key column has.
    fn chunk(values: &[Option<i32>]) -> Chunk {
        Chunk::new(vec![column(values)]).expect("one column is one length")
    }

    /// A side of that column, cut into the chunks it would have arrived in.
    fn chunks(values: &[Option<i32>]) -> Vec<Chunk> {
        values.chunks(512).map(chunk).collect()
    }

    /// A key that is the only column of the side, which is what a `dim.k` in a join condition is.
    fn key(plan: &mut Plan) -> (ExprRef, Schema) {
        let expr = plan.add_expr(Expr::Column(ColumnBinding::new(1, 0)), LogicalType::Integer);
        let schema = Schema::numbered(vec![Field::new("k", LogicalType::Integer)], 1);
        (expr, schema)
    }

    /// Whether the filter would let a row holding each of `values` through, hashed the way the scan
    /// on the other side hashes the column it has just read.
    fn through(filter: &Blocked, values: &[Option<i32>]) -> Vec<bool> {
        let probe = column(values);
        let mut hashes = Vec::new();
        hash(std::slice::from_ref(&probe), values.len(), &mut hashes, Across::TwoInputs);
        hashes.iter().map(|&word| filter.holds(word)).collect()
    }

    #[test]
    fn a_build_side_is_read_for_both_its_range_and_its_keys() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());

        let found = found(&keyed, &[chunk(&[Some(5), Some(9)]), chunk(&[Some(2)])])
            .expect("a column of integers");

        assert_eq!(found.range, Some((Bound::Int(2), Bound::Int(9))));
        let filter = found.filter.expect("a filter over three keys");
        assert_eq!(through(&filter, &[Some(5), Some(9), Some(2)]), [true, true, true]);
    }

    /// The property the whole thing rests on: a filter says no about a key that is in it never, at
    /// any size. This one is forced small enough that its false positive rate is high, which is
    /// what makes a false negative show up rather than hide.
    #[test]
    fn no_key_that_went_in_is_ever_turned_away() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let keys: Vec<Option<i32>> = (0..4_000).map(|value| Some(value * 7 + 11)).collect();

        let found = found(&keyed, &chunks(&keys)).expect("a column of integers");

        let filter = found.filter.expect("a filter over four thousand keys");
        assert!(through(&filter, &keys).into_iter().all(|held| held), "a key it was given");
    }

    /// And the point of it: a key that never went in is usually turned away. Usually rather than
    /// always, because that is what a filter this size promises, and the join behind it is what
    /// makes the difference correct rather than merely rare.
    #[test]
    fn a_key_the_build_side_never_held_is_nearly_always_turned_away() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let keys: Vec<Option<i32>> = (0..4_000).map(|value| Some(value * 7 + 11)).collect();
        let absent: Vec<Option<i32>> =
            (0..4_000).map(|value| Some(value * 7 + 1_000_000)).collect();

        let found = found(&keyed, &chunks(&keys)).expect("a column of integers");

        let filter = found.filter.expect("a filter over four thousand keys");
        let through = through(&filter, &absent).into_iter().filter(|&held| held).count();
        assert!(through < absent.len() / 10, "{through} of {} got through", absent.len());
    }

    /// A null key matches nothing under the rule this is armed for, so it is not a key the filter
    /// holds, and a driving row holding one is dropped by a filter it was never put in.
    #[test]
    fn a_null_is_not_a_key_the_filter_holds() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());

        let found = found(&keyed, &[chunk(&[Some(3), None, Some(4)])]).expect("integers");

        assert_eq!(found.range, Some((Bound::Int(3), Bound::Int(4))));
        assert_eq!(through(&found.filter.expect("a filter"), &[None]), [false]);
    }

    /// A side that gathered nothing leaves a filter that holds nothing, which is a scan that drops
    /// every row it reads, which is the right answer for a join whose other side is empty.
    #[test]
    fn an_empty_build_side_turns_every_driving_row_away() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());

        let found = found(&keyed, &[]).expect("nothing to read");

        assert_eq!(found.range, None);
        assert_eq!(through(&found.filter.expect("a filter of no keys"), &[Some(1)]), [false]);
    }

    /// A driving side written as plan text, which is how every other operator test in this crate
    /// builds one.
    fn driving(text: &str) -> Plan {
        Plan::parse(text).expect("the plan text round trips")
    }

    /// The case that is in every plan over a view: the join names the projection's column and the
    /// scan under it names its own, and without the walk between them the filter is built, handed
    /// over and read by nobody.
    #[test]
    fn a_projection_between_the_join_and_the_scan_renames_the_column_the_filter_is_about() {
        let plan = driving(
            "Project #1 [#0.1::INTEGER AS k]\n  \
             TableFunction read_parquet args=['f'::VARCHAR] #0 [a::INTEGER, k::INTEGER]",
        );

        assert_eq!(
            beneath(&plan, plan.root(), ColumnBinding::new(1, 0)),
            Some(ColumnBinding::new(0, 1)),
            "the scan's own name for the projection's column"
        );
    }

    /// A filter keeps rows and renames nothing, so the binding goes through it as it stands, and
    /// this is the shape a join over a filtered fact table drives with.
    #[test]
    fn a_filter_between_the_two_leaves_the_binding_alone() {
        let plan = driving(
            "Project #1 [#0.0::INTEGER AS k]\n  \
             Filter (#0.0::INTEGER > 3::INTEGER)::BOOLEAN\n    \
             TableFunction read_parquet args=['f'::VARCHAR] #0 [k::INTEGER]",
        );

        assert_eq!(
            beneath(&plan, plan.root(), ColumnBinding::new(1, 0)),
            Some(ColumnBinding::new(0, 0))
        );
    }

    /// A projection that computes its column ends the walk, because a set of values says nothing
    /// about what an expression over them produces, and a scan dropping rows on that would be rows
    /// missing from the answer.
    #[test]
    fn a_computed_column_is_not_a_column_the_filter_can_be_about() {
        let plan = driving(
            "Project #1 [(#0.0::INTEGER > 3::INTEGER)::BOOLEAN AS k]\n  \
             TableFunction read_parquet args=['f'::VARCHAR] #0 [k::INTEGER]",
        );

        assert_eq!(beneath(&plan, plan.root(), ColumnBinding::new(1, 0)), None);
    }

    /// And the walk has to end at the scan the binding is about by then, so a driving side with a
    /// node in the way, or one about another table's column, arms nothing.
    #[test]
    fn a_walk_that_does_not_reach_the_scan_it_is_about_arms_nothing() {
        let plan = driving(
            "Limit 5 offset 0\n  \
             TableFunction read_parquet args=['f'::VARCHAR] #0 [k::INTEGER]",
        );

        assert_eq!(
            beneath(&plan, plan.root(), ColumnBinding::new(0, 0)),
            None,
            "a node in the way"
        );

        let plan = driving("TableFunction read_parquet args=['f'::VARCHAR] #0 [k::INTEGER]");

        assert_eq!(
            beneath(&plan, plan.root(), ColumnBinding::new(3, 0)),
            None,
            "another table's column"
        );
    }
}

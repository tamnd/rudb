//! Pushing the keys an outer query asks about into the aggregate that answers it.
//!
//! A correlated scalar subquery comes out of decorrelation as a grouped aggregate joined back to
//! the outer query on the correlated columns. TPC-H q17 is the plain case. `l_quantity < (SELECT
//! 0.2 * avg(l_quantity) FROM lineitem WHERE l_partkey = p_partkey)` becomes an aggregate that
//! groups the whole of lineitem by `l_partkey`, and a single join that puts each group's average
//! beside the outer row whose part key it belongs to.
//!
//! The outer query asks about two hundred part keys, because the parts it kept are the ones with
//! brand 23 in a medium box. The aggregate answers about two hundred thousand, because that is how
//! many part keys lineitem holds. So it reads six million rows and builds two hundred thousand
//! groups to hand back two hundred of them, and the other 199,800 are built, updated six million
//! times between them, and thrown away by the join above.
//!
//! What this pass does is give the aggregate the keys before it starts. The outer side already
//! holds them: they are a column of the relation the join's condition reads them out of, which for
//! q17 is the filtered scan of part. Copying that relation and semi joining the aggregate's input
//! against it leaves the aggregate reading the six hundred lineitem rows whose part key the outer
//! query is going to ask about, and building two hundred groups rather than two hundred thousand.
//!
//! # Why it is the same query
//!
//! The join above the aggregate produces nothing for a group that matches no outer row. That is
//! true of an inner join and a single join by definition, of a left join because the padding is for
//! an unmatched left row and not an unmatched right one, and of a semi, anti or mark join because
//! all three produce their left side and read the right one only to answer a question about it. So
//! a group the join never matches contributes nothing to the answer, and not building it changes no
//! row that comes out.
//!
//! The semi join is what says which groups those are. Each of its conditions compares one group of
//! the aggregate against the same key the join above compares it against, so an input row survives
//! the semi join exactly when its group could match on that column, and a group the outer side does
//! not hold the key of has no row left to build it from. The comparison is the one the join was
//! written with rather than a fresh `=`, which is what keeps a null safe join back null safe: a
//! group with a null key is asked about above and so has to survive below.
//!
//! Some of the join's conditions and not all of them is allowed and is what q20 would take. A row
//! that matches on every column matches on a subset of them, so a semi join over a subset keeps
//! every row a semi join over all of them would keep and possibly more, and keeping more is the
//! direction that is safe.
//!
//! The relation is copied rather than shared. Two parents pointing at one subtree is a shape the
//! rest of the optimizer does not expect, and a materialisation in front of it would make the outer
//! side wait for a relation it is the only producer of. What the copy costs is one more scan of a
//! table some filter has already been found to cut down hard, which is the same table scanned again
//! and not a new one, and the guard below is what keeps that cost in proportion.
//!
//! # What it refuses
//!
//! A key source estimated at more than a tenth of the rows the aggregate reads. The semi join is
//! not free: it builds a table of the source's rows and probes it with every row the aggregate was
//! going to read, so a source the size of the input is a second pass over the data that removes
//! nothing. q20 is the query that refuses. Its correlated keys come from partsupp, all eight
//! hundred thousand rows of it, and every part key and supplier key lineitem holds is in there, so
//! the semi join would keep every row it was given and charge a hash table for saying so.
//!
//! A key source with no filter under it, however small it is. The keys of a whole relation joined
//! to a fact table are usually all of the keys the fact table holds, because that is what a foreign
//! key is, so the semi join keeps every row and charges a hash table for saying so. q15 is the
//! query that refuses. It joins supplier to the revenue of each supplier over one quarter, and all
//! ten thousand suppliers are inside the tenth the guard above allows, but every supplier key in
//! lineitem is a supplier, so nothing would come out of it. The filter is what makes a source
//! selective, and [`estimate::Side`] is where the plan records whether there is one.
//!
//! A source that is not a scan with filters and projections over it. The copy has to reproduce the
//! relation exactly, and a subtree with a join in it is one where the estimate above is about the
//! join rather than about a table, a subtree with an aggregate in it is one the copy would run
//! twice, and a scan of a materialised query is one whose rows are produced by a node above it.
//!
//! A source holding an expression that can answer twice differently. A copy of a relation is the
//! same relation only if reading it twice reads the same rows.
//!
//! A group that is anything but a bare column, and a group whose type is not the type of the key it
//! is compared against. Both of those are the semi join's condition having to be built out of
//! something other than the two column reads the join above is already built out of.
//!
//! A join whose unmatched right rows reach the answer, which is a right join and a full join.
//!
//! An aggregate whose input is already a semi join. That is the shape this pass writes, so one that
//! is already there is taken as its own work from a previous run and the aggregate is left alone.
//! Without it the fixed sequence would add a second semi join over the first every time it ran.

use rudb_common::{LogicalType, Result, Stat};
use rudb_plan::{
    BuildSide, ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan, Slice,
};

use crate::estimate::{self, Facts};
use crate::pass::{Context, Pass, top_down};
use crate::tables::{TableSet, produced};
use crate::walk;

/// How many times the rows the aggregate reads have to outnumber the keys for the push to be worth
/// it.
///
/// The semi join costs a hash table of the keys and a probe per row read, and it saves whatever
/// share of those rows it removes. A tenth is where a source that is a filtered dimension table
/// stops looking like one: q17's two hundred part keys against six million lineitem rows and q2's
/// eight hundred against eight hundred thousand partsupp rows are both far inside it, and q20's
/// eight hundred thousand partsupp rows against nine hundred thousand lineitem rows are not.
const WORTH_IT: u64 = 10;

/// Pushes the keys the outer query holds into the aggregate that answers about them.
#[derive(Debug, Clone, Copy)]
pub struct GroupKeyPushdown;

impl Pass for GroupKeyPushdown {
    /// Local rather than one of DuckDB's forty four, which reaches the same plan a different way.
    ///
    /// DuckDB leaves the delim join in place for this shape and lets the subquery's side read the
    /// duplicate eliminated keys out of it, so the name it would answer to over there is the
    /// deliminator declining to fire. That name is taken here by the pass that does fire.
    fn name(&self) -> &'static str {
        "group_key_pushdown"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        push(plan, context.facts());
        Ok(())
    }
}

/// Pushes every set of keys in `plan` that is worth pushing.
///
/// One at a time, with the plan walked again after each. A push rebuilds the path from the
/// aggregate it changed back to the root, so an aggregate on that path is at a reference the walk
/// that found it no longer names, and applying a list of pushes gathered in one sweep would skip
/// the ones the earlier pushes moved. Skipping them is not a wrong plan but it is a plan that
/// depends on how many times the sequence has run, which is the settle check failing.
///
/// The loop ends because a push leaves a semi join under the aggregate it fired on and the match
/// refuses an aggregate that has one, so each round has one fewer candidate than the last. The
/// bound on the rounds is there for the case where that reasoning is wrong rather than for a case
/// anything has hit.
pub fn push(plan: &mut Plan, stats: &Facts) {
    for _ in 0..plan.node_count() {
        let found = top_down(plan).into_iter().find_map(|node| matched(plan, node, stats));
        let Some(found) = found else { return };
        let Some(rebuilt) = written(plan, &found) else { return };
        let mut changed = false;
        let root = walk::restack(plan, plan.root(), &mut changed, &mut |_, here| {
            (here == found.aggregate).then_some(rebuilt)
        });
        if !changed {
            return;
        }
        plan.set_root(root);
    }
}

/// One push, worked out before anything is written.
struct Push {
    /// The aggregate whose input the semi join goes under.
    aggregate: NodeRef,
    /// The relation in the outer side that holds the keys, which is what gets copied.
    source: NodeRef,
    /// One condition per group the outer side holds the key of.
    pairs: Vec<Pair>,
}

/// A group of the aggregate and the outer column the join above compares it against.
struct Pair {
    /// The group expression, written against the aggregate's input.
    group: ExprRef,
    /// The outer column, written against the relation it is read out of.
    key: ExprRef,
    /// The comparison the join above was written with.
    op: CompareOp,
    /// The type of the join's condition, which is the type the semi join's gets.
    ty: LogicalType,
}

/// What the join at `node` would let this pass push, or nothing if it is the wrong shape.
fn matched(plan: &Plan, node: NodeRef, stats: &Facts) -> Option<Push> {
    let Node::Join { left, right, kind, conditions, .. } = *plan.node(node) else {
        return None;
    };
    if !drops_unmatched(kind) {
        return None;
    }
    let outer = produced(plan, left);
    let mut aggregate = None;
    let mut pairs = Vec::new();
    for &condition in plan.expr_list(conditions) {
        let Expr::Compare { op, left: one, right: other } = *plan.expr(condition) else {
            continue;
        };
        if !matches!(op, CompareOp::Equal | CompareOp::NotDistinctFrom) {
            continue;
        }
        // The condition is written either way round, and the two sides of a join produce disjoint
        // sets of columns, so trying both orders finds at most one of them.
        for (inner, held) in [(one, other), (other, one)] {
            let Expr::Column(binding) = *plan.expr(inner) else {
                continue;
            };
            let Expr::Column(read) = *plan.expr(held) else {
                continue;
            };
            if !outer.contains(read.table) {
                continue;
            }
            let Some((at, position)) = traced(plan, right, binding) else {
                continue;
            };
            if aggregate.is_some_and(|was| was != at) {
                continue;
            }
            let Node::Aggregate { groups, .. } = *plan.node(at) else {
                continue;
            };
            let Some(&group) = plan.expr_list(groups).get(position) else {
                continue;
            };
            if !matches!(*plan.expr(group), Expr::Column(_))
                || plan.expr_type(group) != plan.expr_type(held)
            {
                continue;
            }
            aggregate = Some(at);
            pairs.push(Pair { group, key: held, op, ty: plan.expr_type(condition).clone() });
            break;
        }
    }
    let aggregate = aggregate.filter(|_| !pairs.is_empty())?;
    let Node::Aggregate { input, .. } = *plan.node(aggregate) else {
        return None;
    };
    if matches!(*plan.node(input), Node::Join { kind: JoinKind::Semi, .. }) {
        return None;
    }
    let mut wanted = TableSet::new();
    for pair in &pairs {
        walk::columns(plan, pair.key, &mut |binding| wanted.insert(binding.table));
    }
    let source = narrowest(plan, left, &wanted)?;
    if !copyable(plan, source) {
        return None;
    }
    let keys = estimate::side(plan, source, stats)?;
    let reads = estimate::rows(plan, input, stats)?;
    if keys.rows >= keys.base || keys.rows.saturating_mul(WORTH_IT) > reads {
        return None;
    }
    Some(Push { aggregate, source, pairs })
}

/// Whether a row of this join's right side that matches nothing is a row that reaches the answer.
const fn drops_unmatched(kind: JoinKind) -> bool {
    match kind {
        JoinKind::Inner
        | JoinKind::Left
        | JoinKind::Semi
        | JoinKind::Anti
        | JoinKind::Mark
        | JoinKind::Single => true,
        JoinKind::Right | JoinKind::Full | JoinKind::Positional => false,
    }
}

/// The aggregate `binding` is a group of, and which group it is, seen through what sits above it.
///
/// A projection because decorrelation writes one over the aggregate to put the correlated key back
/// beside the answer, and a filter because a `HAVING` binds to one there. Both of them hand a
/// binding down unchanged or name the expression it stands for, and neither changes which rows the
/// groups underneath were built from.
fn traced(plan: &Plan, at: NodeRef, binding: ColumnBinding) -> Option<(NodeRef, usize)> {
    match *plan.node(at) {
        Node::Aggregate { index, groups, .. } if index == binding.table => {
            let position = usize::try_from(binding.column).ok()?;
            (position < plan.expr_list(groups).len()).then_some((at, position))
        }
        Node::Project { input, index, exprs, .. } if index == binding.table => {
            let position = usize::try_from(binding.column).ok()?;
            let &expr = plan.expr_list(exprs).get(position)?;
            let Expr::Column(below) = *plan.expr(expr) else {
                return None;
            };
            traced(plan, input, below)
        }
        Node::Filter { input, .. } => traced(plan, input, binding),
        _ => None,
    }
}

/// The smallest node under `at` that produces every table in `wanted` and nothing else.
///
/// Smallest because the copy is of whatever this hands back, and a node higher up is one that
/// carries the joins above the relation the keys are read out of. In q17 the outer side is lineitem
/// joined to the filtered parts, and what holds the part keys is the part side of it.
///
/// The descent stops at the first node producing nothing but the tables wanted, rather than going
/// on down to the scan itself. What sits in between is the filter that makes the source worth
/// copying at all, and a copy taken from below it is a copy of the whole table.
fn narrowest(plan: &Plan, at: NodeRef, wanted: &TableSet) -> Option<NodeRef> {
    if !wanted.is_subset_of(&produced(plan, at)) {
        return None;
    }
    if produced(plan, at).is_subset_of(wanted) {
        return Some(at);
    }
    for child in plan.node(at).children().into_iter().flatten() {
        if wanted.is_subset_of(&produced(plan, child)) {
            return narrowest(plan, child, wanted);
        }
    }
    Some(at)
}

/// Whether the subtree under `at` is one [`copied`] can reproduce.
fn copyable(plan: &Plan, at: NodeRef) -> bool {
    match *plan.node(at) {
        Node::Get { .. } | Node::TableFunction { .. } => true,
        Node::Filter { input, predicate } => {
            !walk::volatile(plan, predicate) && copyable(plan, input)
        }
        Node::Project { input, exprs, .. } => {
            plan.expr_list(exprs).iter().all(|&expr| !walk::volatile(plan, expr))
                && copyable(plan, input)
        }
        _ => false,
    }
}

/// The plan with the semi join written under the aggregate, as a new aggregate node.
///
/// A new node rather than the old one edited, because the semi join reads the aggregate's input and
/// so has to be appended behind whatever reads it, and the old aggregate is already in front of the
/// place the semi join can go. [`walk::restack`] then puts the new one where the old one was.
fn written(plan: &mut Plan, push: &Push) -> Option<NodeRef> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(push.aggregate) else {
        return None;
    };
    let mut renames = Vec::new();
    let source = copied(plan, push.source, &mut renames)?;
    let mut conditions = Vec::new();
    for pair in &push.pairs {
        let against = renamed(plan, pair.key, &renames);
        if against == pair.key {
            return None;
        }
        let compare = Expr::Compare { op: pair.op, left: pair.group, right: against };
        conditions.push(plan.add_expr(compare, pair.ty.clone()));
    }
    let conditions = plan.add_expr_list(&conditions);
    let filtered = plan.add_node(Node::Join {
        left: input,
        right: source,
        kind: JoinKind::Semi,
        conditions,
        build: BuildSide::Right,
    });
    Some(plan.add_node(Node::Aggregate { input: filtered, index, groups, aggregates }))
}

/// A second copy of the relation under `at`, with a table index of its own per operator.
///
/// `renames` collects the index each copied operator was given, which is what [`renamed`] rewrites
/// the expressions above it with. It is filled in the order the copy is built, so an operator's own
/// index goes in after its input's expressions have been rewritten and not before.
fn copied(plan: &mut Plan, at: NodeRef, renames: &mut Vec<(u32, u32)>) -> Option<NodeRef> {
    let span = plan.node_span(at);
    match plan.node(at).clone() {
        Node::Get { catalog, schema, table, alias, index, columns } => {
            let fresh = walk::fresh_index(plan);
            let copy = Node::Get { catalog, schema, table, alias, index: fresh, columns };
            let node = plan.add_node_at(copy, span);
            carry(plan, index, fresh, columns);
            renames.push((index, fresh));
            Some(node)
        }
        Node::TableFunction { index, function, args, options, settings, columns } => {
            let fresh = walk::fresh_index(plan);
            let copy =
                Node::TableFunction { index: fresh, function, args, options, settings, columns };
            let node = plan.add_node_at(copy, span);
            carry(plan, index, fresh, columns);
            renames.push((index, fresh));
            Some(node)
        }
        Node::Filter { input, predicate } => {
            let below = copied(plan, input, renames)?;
            let predicate = renamed(plan, predicate, renames);
            Some(plan.add_node_at(Node::Filter { input: below, predicate }, span))
        }
        Node::Project { input, index, exprs, names } => {
            let below = copied(plan, input, renames)?;
            let held = plan.expr_list(exprs).to_vec();
            let rewritten: Vec<ExprRef> =
                held.iter().map(|&expr| renamed(plan, expr, renames)).collect();
            let exprs = plan.add_expr_list(&rewritten);
            let fresh = walk::fresh_index(plan);
            let copy = Node::Project { input: below, index: fresh, exprs, names };
            let node = plan.add_node_at(copy, span);
            renames.push((index, fresh));
            Some(node)
        }
        _ => None,
    }
}

/// Gives the copy of a scan what the binder found out about the original.
///
/// The counts, the bounds and the distinct values are all recorded against the table index, and the
/// copy has an index of its own, so without this it reads back as a relation nobody measured. It is
/// the same table read the same way, so the answers are the same answers, and a copy the estimates
/// say nothing about is one the passes after this cannot size or pick a build side for.
fn carry(plan: &mut Plan, was: u32, fresh: u32, columns: Slice) {
    plan.measure(fresh, plan.measured(was));
    if let Some(zones) = plan.zones(was).cloned() {
        plan.set_zones(fresh, zones);
    }
    for name in plan.field_list(columns).iter().map(|field| field.name.clone()).collect::<Vec<_>>()
    {
        let distinct = plan.distinct_measured(was, &name);
        if !matches!(distinct, Stat::Unknown) {
            plan.measure_distinct(fresh, &name, distinct);
        }
    }
}

/// `expr` with every column of a copied operator read out of the copy instead.
///
/// Appended rather than edited, because the expression it is written from is still read by the
/// relation that was copied.
fn renamed(plan: &mut Plan, expr: ExprRef, renames: &[(u32, u32)]) -> ExprRef {
    if let Expr::Column(binding) = *plan.expr(expr) {
        let Some(&(_, fresh)) = renames.iter().find(|&&(was, _)| was == binding.table) else {
            return expr;
        };
        let ty = plan.expr_type(expr).clone();
        let span = plan.expr_span(expr);
        let moved = Expr::Column(ColumnBinding::new(fresh, binding.column));
        return plan.add_expr_at(moved, ty, span);
    }
    walk::rebuild(plan, expr, &mut |plan, inner| renamed(plan, inner, renames))
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::push;
    use crate::estimate::Facts;

    /// The row counts join ordering's own tests use, so that a test here reads against those.
    fn counts() -> Facts {
        let mut counts = Facts::new();
        for (table, rows) in [("t", 1000), ("u", 10), ("v", 100), ("w", 100_000)] {
            counts.record("memory", "main", table, rows);
        }
        counts
    }

    /// What the plan a text prints looks like once the pass has run over it.
    ///
    /// Run twice, since a pass that pushed something the second time would be one the optimizer's
    /// idempotence check trips over on the first debug build to see a plan like this.
    fn pushed(text: &str) -> String {
        let counts = counts();
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        push(&mut plan, &counts);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        let once = plan.to_string();
        push(&mut plan, &counts);
        assert_eq!(plan.to_string(), once, "{text} pushed again on a second run");
        once
    }

    #[test]
    fn the_keys_of_a_filtered_source_reach_the_aggregate() {
        assert_eq!(
            pushed(concat!(
                "Join SINGLE on=[(#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN]\n",
                "  Filter (#0.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
                "    Get memory.main.u AS u #0 [k::INTEGER, b::INTEGER]\n",
                "  Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
                "    Get memory.main.w AS w #2 [k::INTEGER, v::INTEGER]\n",
            )),
            concat!(
                "Join SINGLE on=[(#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN]\n",
                "  Filter (#0.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
                "    Get memory.main.u AS u #0 [k::INTEGER, b::INTEGER]\n",
                "  Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
                "    Join SEMI on=[(#2.0::INTEGER = #3.0::INTEGER)::BOOLEAN]\n",
                "      Get memory.main.w AS w #2 [k::INTEGER, v::INTEGER]\n",
                "      Filter (#3.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
                "        Get memory.main.u AS u #3 [k::INTEGER, b::INTEGER]\n",
            )
        );
    }

    #[test]
    fn the_aggregate_is_found_through_the_projection_decorrelation_leaves_over_it() {
        assert_eq!(
            pushed(concat!(
                "Join SINGLE on=[(#3.0::INTEGER = #0.0::INTEGER)::BOOLEAN]\n",
                "  Filter (#0.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
                "    Get memory.main.u AS u #0 [k::INTEGER, b::INTEGER]\n",
                "  Project #3 [#1.0::INTEGER AS k, #1.1::BIGINT AS n]\n",
                "    Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
                "      Get memory.main.w AS w #2 [k::INTEGER, v::INTEGER]\n",
            )),
            concat!(
                "Join SINGLE on=[(#3.0::INTEGER = #0.0::INTEGER)::BOOLEAN]\n",
                "  Filter (#0.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
                "    Get memory.main.u AS u #0 [k::INTEGER, b::INTEGER]\n",
                "  Project #3 [#1.0::INTEGER AS k, #1.1::BIGINT AS n]\n",
                "    Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
                "      Join SEMI on=[(#2.0::INTEGER = #4.0::INTEGER)::BOOLEAN]\n",
                "        Get memory.main.w AS w #2 [k::INTEGER, v::INTEGER]\n",
                "        Filter (#4.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
                "          Get memory.main.u AS u #4 [k::INTEGER, b::INTEGER]\n",
            )
        );
    }

    #[test]
    fn a_semi_join_already_under_the_aggregate_is_left_where_it_is() {
        let text = concat!(
            "Join SINGLE on=[(#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN]\n",
            "  Filter (#0.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "    Get memory.main.u AS u #0 [k::INTEGER, b::INTEGER]\n",
            "  Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
            "    Join SEMI on=[(#2.0::INTEGER = #3.0::INTEGER)::BOOLEAN]\n",
            "      Get memory.main.w AS w #2 [k::INTEGER, v::INTEGER]\n",
            "      Filter (#3.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "        Get memory.main.u AS u #3 [k::INTEGER, b::INTEGER]\n",
        );
        assert_eq!(pushed(text), text);
    }

    #[test]
    fn a_source_nothing_filters_is_refused() {
        let text = concat!(
            "Join INNER on=[(#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN]\n",
            "  Get memory.main.u AS u #0 [k::INTEGER, b::INTEGER]\n",
            "  Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
            "    Get memory.main.w AS w #2 [k::INTEGER, v::INTEGER]\n",
        );
        assert_eq!(pushed(text), text);
    }

    #[test]
    fn a_source_too_big_against_what_the_aggregate_reads_is_refused() {
        let text = concat!(
            "Join SINGLE on=[(#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN]\n",
            "  Filter (#0.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "    Get memory.main.w AS w #0 [k::INTEGER, b::INTEGER]\n",
            "  Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
            "    Get memory.main.t AS t #2 [k::INTEGER, v::INTEGER]\n",
        );
        assert_eq!(pushed(text), text);
    }

    #[test]
    fn a_source_the_copy_cannot_reproduce_is_refused() {
        let text = concat!(
            "Join SINGLE on=[(#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN]\n",
            "  Filter (#0.0::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "    Aggregate #0 groups=[#4.0::INTEGER] aggregates=[]\n",
            "      Get memory.main.u AS u #4 [k::INTEGER]\n",
            "  Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
            "    Get memory.main.w AS w #2 [k::INTEGER, v::INTEGER]\n",
        );
        assert_eq!(pushed(text), text);
    }

    #[test]
    fn a_join_whose_unmatched_groups_reach_the_answer_is_refused() {
        let text = concat!(
            "Join RIGHT on=[(#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN]\n",
            "  Filter (#0.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "    Get memory.main.u AS u #0 [k::INTEGER, b::INTEGER]\n",
            "  Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
            "    Get memory.main.w AS w #2 [k::INTEGER, v::INTEGER]\n",
        );
        assert_eq!(pushed(text), text);
    }

    #[test]
    fn a_group_that_is_not_a_bare_column_is_refused() {
        let text = concat!(
            "Join SINGLE on=[(#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN]\n",
            "  Filter (#0.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "    Get memory.main.u AS u #0 [k::INTEGER, b::INTEGER]\n",
            "  Aggregate #1 groups=[\"+\"(#2.0::INTEGER, 1::INTEGER)::INTEGER] aggregates=[count_star()::BIGINT]\n",
            "    Get memory.main.w AS w #2 [k::INTEGER, v::INTEGER]\n",
        );
        assert_eq!(pushed(text), text);
    }

    #[test]
    fn a_condition_over_something_other_than_a_group_is_refused() {
        let text = concat!(
            "Join SINGLE on=[(#1.1::BIGINT = #0.0::BIGINT)::BOOLEAN]\n",
            "  Filter (#0.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "    Get memory.main.u AS u #0 [k::BIGINT, b::INTEGER]\n",
            "  Aggregate #1 groups=[#2.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
            "    Get memory.main.w AS w #2 [k::INTEGER, v::INTEGER]\n",
        );
        assert_eq!(pushed(text), text);
    }
}

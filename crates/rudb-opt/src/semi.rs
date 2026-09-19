//! Semi joins: making them, and then moving them to where they are worth having.
//!
//! The first two passes here turn something else into a semi join. The third, [`SemiPushdown`],
//! moves one down past the inner join under it, which is what makes the first two worth doing on a
//! query whose existence test is written over a join of several tables.
//!
//! # Turning a mark join whose marker is only ever tested into a semi join
//!
//! `WHERE x IN (SELECT ...)` binds to a mark join with a filter over its marker column. The mark
//! join answers, for every driving row, whether the subquery holds a matching value, and the filter
//! then keeps the rows where that answer was true. A semi join is the same question with the answer
//! applied rather than written down.
//!
//! The two are exactly equal and the three valued answer is what makes them equal rather than what
//! stands in the way. A filter keeps a row where its predicate is true and drops it where the
//! predicate is false or null, and a marker is true exactly where the driving row matched. So the
//! rows a filter over a marker keeps are the rows that matched, which is what a semi join produces,
//! and the null case needs no argument because false and null are both dropped either way.
//!
//! What it saves is not the comparison. A mark join has to answer for every driving row, including
//! the ones that will be thrown away, so it is a sink in the pipeline: it gathers its whole driving
//! side into rows before it produces anything. A semi join is a stream and passes a chunk through
//! as it arrives. On TPC-H q18 the driving side is the three way join of customer, orders and
//! lineitem, so that is six million wide rows held as boxed values to answer a question about one
//! column of them.
//!
//! # What it refuses
//!
//! A predicate that is anything but a bare read of the marker column. `WHERE NOT (x IN (...))` is
//! an anti join and `WHERE x IN (...) OR y > 3` is neither, and the second is the one worth being
//! careful about: the marker is genuinely needed as a value there, because what the row means
//! depends on the other half of the disjunction.
//!
//! A plan where anything else reads a column of the mark join's gathered side. A mark join produces
//! its driving side, the gathered side's columns and the marker, while a semi join produces the
//! driving side alone, so a projection above that still reads one of those columns would be reading
//! a column that no longer exists. In practice nothing does, since the subquery's own output is not
//! something the outer query named, but a rewrite that assumes it is a rewrite that produces an
//! invalid plan the one time it is wrong.
//!
//! # Rewriting in place
//!
//! The join is written over the filter's slot, the same way `topn` writes a top N over a limit's,
//! and for the same reason. What ends up in the filter's slot points at the join's two inputs,
//! both of which are behind the join, which is behind the filter, so the arena's rule that a node
//! may only point backwards still holds and nothing that pointed at the filter has to be rebuilt.

use std::collections::VecDeque;

use rudb_common::Result;
use rudb_plan::{BuildSide, Expr, JoinKind, Node, NodeRef, Plan, Slice};

use crate::estimate::{self, Facts};
use crate::pass::{Context, Pass, top_down};
use crate::tables::{TableSet, Tables, produced};
use crate::walk;

/// Rewrites a mark join under a filter on its marker into a semi join.
#[derive(Debug, Clone, Copy)]
pub struct MarkToSemi;

impl Pass for MarkToSemi {
    /// Local rather than one of DuckDB's forty four, which have no name for this.
    ///
    /// DuckDB does the same rewrite inside its filter pushdown, and borrowing that name would mean
    /// `SET disabled_optimizers = 'filter_pushdown'` turned off two passes here and one there.
    fn name(&self) -> &'static str {
        "mark_to_semi"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        convert(plan);
        Ok(())
    }
}

/// Rewrites every filtered mark join in `plan` into a semi join.
pub fn convert(plan: &mut Plan) {
    for node in top_down(plan) {
        let Node::Filter { input, predicate } = *plan.node(node) else {
            continue;
        };
        let Node::Join { left, right, kind: JoinKind::Mark, conditions, build } = *plan.node(input)
        else {
            continue;
        };
        let Expr::Column(tested) = *plan.expr(predicate) else {
            continue;
        };
        // The marker is the last column the gathered side produces, which is what the operator
        // takes it to be, so a side whose columns cannot be listed is a side whose marker cannot be
        // named either.
        let Some(outputs) = walk::outputs(plan, right) else {
            continue;
        };
        let Some((marker, _)) = outputs.last() else {
            continue;
        };
        if tested != *marker || read_above(plan, right, node, input) {
            continue;
        }
        *plan.node_mut(node) = Node::Join { left, right, kind: JoinKind::Semi, conditions, build };
    }
}

/// Rewrites an inner join under a duplicate eliminating aggregate into a semi join.
#[derive(Debug, Clone, Copy)]
pub struct DistinctToSemi;

impl Pass for DistinctToSemi {
    /// Local, and next to `mark_to_semi` because it answers the same question a different way.
    fn name(&self) -> &'static str {
        "distinct_to_semi"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        narrow(plan);
        Ok(())
    }
}

/// Rewrites every inner join under a duplicate eliminating aggregate in `plan` into a semi join.
///
/// An aggregate with group expressions and no aggregate expressions is a duplicate eliminator, and
/// one whose groups all read the left side of the join underneath it is asking which of the left
/// side's rows had a match. That is a semi join and the inner join is the expensive way to ask it,
/// because the inner join produces a row per pair and the aggregate then throws all but one of each
/// group away.
///
/// Decorrelation is where this comes from. `EXISTS (SELECT * FROM l2 WHERE l2.k = l1.k AND ...)`
/// unnests into the correlated keys joined against the subquery's own relation and then made
/// distinct again, since the join can match a key more than once and the flag the outer query wants
/// is one per key. On TPC-H q21 that relation is lineitem and the join is lineitem against itself
/// on the order key, so every order key with four lines under it makes sixteen pairs, and six
/// million rows on each side become twenty four million pairs that the aggregate immediately cuts
/// back to six hundred thousand. A semi join stops at the first match and never builds the rest.
///
/// The aggregate stays where it is rather than going away with the join. The semi join's output is
/// already one row per left row so the aggregate has little left to do, but saying it has none
/// means proving the left side was already distinct on those columns, which is a separate thing to
/// know and not what this pass is about.
///
/// # What it refuses
///
/// An aggregate with an aggregate expression in it, which is a real aggregate and not a duplicate
/// eliminator, and one with a group that is anything but a bare column of the left side. A group
/// reading the right side is a group the semi join would take the column of away, and a group over
/// an expression is one this would have to reason about rather than carry.
///
/// A join anything else points at. Changing the kind in place changes what the node produces and
/// how many rows it produces, and a second parent is a second reader that did not ask for either.
///
/// A plan where anything outside the join reads a column of the right side, on the same grounds
/// [`convert`] refuses one.
pub fn narrow(plan: &mut Plan) {
    for node in top_down(plan) {
        let Node::Aggregate { input, groups, aggregates, .. } = *plan.node(node) else {
            continue;
        };
        if !plan.expr_list(aggregates).is_empty() {
            continue;
        }
        let Node::Join { left, right, kind: JoinKind::Inner, conditions, build } =
            *plan.node(input)
        else {
            continue;
        };
        let driving = produced(plan, left);
        let grouped = plan.expr_list(groups).iter().all(|&group| match *plan.expr(group) {
            Expr::Column(binding) => driving.contains(binding.table),
            _ => false,
        });
        if !grouped || parents(plan, input) != 1 || read_above(plan, right, node, input) {
            continue;
        }
        *plan.node_mut(input) = Node::Join { left, right, kind: JoinKind::Semi, conditions, build };
    }
}

/// Moves a semi or an anti join below the inner join under it, when it only reads one side.
#[derive(Debug, Clone, Copy)]
pub struct SemiPushdown;

impl Pass for SemiPushdown {
    /// Local. DuckDB does this inside its filter pushdown, and borrowing that name would mean one
    /// `SET disabled_optimizers = 'filter_pushdown'` turned off two passes here and one there.
    fn name(&self) -> &'static str {
        "semi_pushdown"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        lower(plan, context.facts());
        Ok(())
    }
}

/// Moves every semi and anti join in `plan` as far below the inner joins under it as it will go.
///
/// A semi join is a filter. It keeps the rows of its driving side that have a match on the other
/// one and produces nothing of its own, so a row of the driving side either survives or does not
/// and no row is ever duplicated or padded. That makes it exactly as movable as a predicate, and
/// the rule is the one filter pushdown uses: a filter over a join may go into whichever side reads
/// every column it needs.
///
/// Read `SEMI(INNER(a, b), r)` where the semi join's conditions read `a` and `r` and nothing of
/// `b`. A pair of a row of `a` and a row of `b` survives when the inner join's conditions hold of
/// the pair and some row of `r` matches the half from `a`. The second half of that is a question
/// about the row of `a` alone, so asking it first and joining what survives to `b` keeps exactly
/// the same pairs, in the same order and with the same multiplicity. An anti join is the same
/// argument with no row of `r` matching instead of some row matching.
///
/// TPC-H q18 is the query that needs it. `o_orderkey IN (SELECT l_orderkey FROM lineitem GROUP BY
/// l_orderkey HAVING sum(l_quantity) > 300)` has 57 answers at SF1, and the semi join for it sat
/// above the three way join of customer, orders and lineitem. So the plan built all 6,001,215 pairs
/// of an order with its lines and then kept 399 of them, and it held 6.8 GiB at the peak to do it.
/// The condition reads `o_orderkey` and nothing else, so the semi join now goes below the join to
/// lineitem and then below the join to customer, and orders goes from 1,500,000 rows to 57 before
/// anything is joined to it at all.
///
/// # What it refuses
///
/// A condition that reads both sides of the inner join, which is a condition neither side can
/// answer on its own, and a semi join with no conditions, which keeps every driving row or none of
/// them depending on whether the other side has a row in it and is not a thing to move.
///
/// And a join that looks like it cuts its side down rather than building it up. The move is not
/// free: afterwards the semi join runs over whatever its new driving side produces rather than over
/// what the join above it produced, so a join selective enough to produce fewer rows than the side
/// moved into turns the move into more work rather than less. q21 is that shape. Its semi join sits
/// over a join of 1,200,243 lineitem rows to the ten thousand suppliers a nation filter cuts to
/// four hundred, which produces 75,871 rows, and moving the semi join below it made it probe
/// sixteen times as many rows and the query a third slower.
///
/// Which of the two a join does cannot be read off the estimates, since
/// [`crate::estimate::matched`] scores a join as the larger of its two sides or the keyspace
/// reading of its conditions, whichever is bigger, and so never puts a join below either side. What
/// the estimates do say is how big the two sides are, and the answer used here is that a join to a
/// side smaller than the one being moved into is more often a lookup into a dimension table some
/// filter has already cut down, which is the shape that contracts, than it is a join to a fact
/// table, which is the shape that expands. So the other side has to be estimated at least as large
/// as the side being moved into, and a side with no estimate at all refuses.
///
/// A cross product is exempt, because its output is the product of the two and so is never below
/// the side being moved into whatever the sizes are.
///
/// Nothing is rewritten in place. A node is added for the move and the nodes above it are added
/// again over it, the way join ordering rebuilds a region, so a subtree two parents share is left
/// alone for the parent that did not ask.
pub fn lower(plan: &mut Plan, stats: &Facts) {
    let mut tables = Tables::new();
    let root = rebuilt(plan, plan.root(), &mut tables, stats);
    plan.set_root(root);
}

/// Rewrites the plan under `at`, returning `at` itself where nothing under it changed.
fn rebuilt(plan: &mut Plan, at: NodeRef, tables: &mut Tables, stats: &Facts) -> NodeRef {
    let children: Vec<NodeRef> = plan.node(at).children().into_iter().flatten().collect();
    let done: Vec<NodeRef> =
        children.iter().map(|&child| rebuilt(plan, child, tables, stats)).collect();
    let here = if done == children {
        at
    } else {
        let mut node = plan.node(at).clone();
        walk::replace_children(&mut node, &done);
        plan.add_node(node)
    };
    moved(plan, here, tables, stats).unwrap_or(here)
}

/// What the join a semi join is moving past is, since a cross product is one with no conditions.
#[derive(Debug, Clone, Copy)]
enum Under {
    Join(Slice, BuildSide),
    Cross,
}

/// The semi join at `at` moved into one side of the join under it, or nothing where it stays.
fn moved(plan: &mut Plan, at: NodeRef, tables: &mut Tables, stats: &Facts) -> Option<NodeRef> {
    let Node::Join { left, right, kind, conditions, build } = *plan.node(at) else {
        return None;
    };
    if !matches!(kind, JoinKind::Semi | JoinKind::Anti) || plan.expr_list(conditions).is_empty() {
        return None;
    }
    let (first, second, under) = match *plan.node(left) {
        Node::Join { left: a, right: b, kind: JoinKind::Inner, conditions: list, build } => {
            (a, b, Under::Join(list, build))
        }
        Node::CrossProduct { left: a, right: b } => (a, b, Under::Cross),
        _ => return None,
    };
    // Everything the conditions read. What the gathered side produces is in there too, and stays in
    // there, because a side the conditions may be tested against is that side and the gathered one
    // together.
    let mut needed = TableSet::new();
    for condition in plan.expr_list(conditions).to_vec() {
        needed.extend(&tables.of(plan, condition));
    }
    let gathered = produced(plan, right);
    let into_first = answers(plan, first, &gathered, &needed);
    let into_second = !into_first && answers(plan, second, &gathered, &needed);
    if !into_first && !into_second {
        return None;
    }
    let side = if into_first { first } else { second };
    let other = if into_first { second } else { first };
    if !expands(plan, side, other, under, stats) {
        return None;
    }
    let semi = plan.add_node(Node::Join { left: side, right, kind, conditions, build });
    // As far down as it goes rather than one step, because the side it just moved into can be
    // another inner join and the walk above this has already been past it.
    let semi = moved(plan, semi, tables, stats).unwrap_or(semi);
    let (left, right) = if into_first { (semi, second) } else { (first, semi) };
    Some(plan.add_node(match under {
        Under::Join(conditions, build) => {
            Node::Join { left, right, kind: JoinKind::Inner, conditions, build }
        }
        Under::Cross => Node::CrossProduct { left, right },
    }))
}

/// Whether the join under the semi join looks like it builds `side` up rather than cuts it down.
///
/// A cross product does, always. A join does when the side it joins `side` to is at least as big,
/// which is the reading of the two estimates the doc on [`lower`] argues for, and a side with no
/// estimate is a side nothing can be read off at all.
fn expands(plan: &Plan, side: NodeRef, other: NodeRef, under: Under, stats: &Facts) -> bool {
    let Under::Join(conditions, _) = under else {
        return true;
    };
    if plan.expr_list(conditions).is_empty() {
        return true;
    }
    let (Some(side), Some(other)) =
        (estimate::rows(plan, side, stats), estimate::rows(plan, other, stats))
    else {
        return false;
    };
    other >= side
}

/// Whether `side` and the gathered side between them produce everything the conditions read.
fn answers(plan: &Plan, side: NodeRef, gathered: &TableSet, needed: &TableSet) -> bool {
    let mut reachable = produced(plan, side);
    reachable.extend(gathered);
    needed.is_subset_of(&reachable)
}

/// How many nodes of `plan` point at `at`.
///
/// The arena lets two parents share a subtree and decorrelation makes that happen, so a rewrite
/// that changes what a node produces has to know it is the only one asking.
fn parents(plan: &Plan, at: NodeRef) -> usize {
    let mut found = 0;
    for node in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
        found +=
            plan.node(node).children().into_iter().flatten().filter(|&child| child == at).count();
    }
    found
}

/// Whether anything but the join and the filter over it reads a column the gathered side produces.
///
/// The gathered side's own nodes read those columns and are not what this is asking about, so the
/// subtree under `right` is walked first and then skipped. The join is skipped because its
/// conditions read the gathered side by definition and a semi join keeps them, and the filter
/// because the whole point is that it is going away.
fn read_above(plan: &Plan, right: NodeRef, filter: NodeRef, join: NodeRef) -> bool {
    let gathered = produced(plan, right);
    let inside = subtree(plan, right);
    let mut found = false;
    for at in top_down(plan) {
        if at == filter || at == join || inside.contains(&at) {
            continue;
        }
        walk::node_columns(plan, at, &mut |_, binding| {
            found |= gathered.contains(binding.table);
        });
    }
    found
}

/// Every node at or under `at`, which is [`top_down`] started somewhere other than the root.
fn subtree(plan: &Plan, at: NodeRef) -> Vec<NodeRef> {
    let mut found = Vec::new();
    let mut pending = VecDeque::from([at]);
    while let Some(node) = pending.pop_front() {
        if found.contains(&node) {
            continue;
        }
        found.push(node);
        pending.extend(plan.node(node).children().into_iter().flatten());
    }
    found
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::{convert, lower, narrow};
    use crate::estimate::Facts;

    /// What the plan a text prints looks like once the pass has run over it.
    fn converted(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        convert(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    /// The same for the pass that narrows an inner join under a duplicate eliminator.
    fn narrowed(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        narrow(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    /// The same for the pass that moves a semi join below the join under it, with the row counts
    /// join ordering's own tests use. Run twice, since a pass that moved something the second time
    /// would be one the optimizer's idempotence check trips over on the first debug build to see a
    /// plan like this.
    fn lowered(text: &str) -> String {
        let mut counts = Facts::new();
        for (table, rows) in [("t", 1000), ("u", 10), ("v", 100), ("w", 100_000)] {
            counts.record("memory", "main", table, rows);
        }
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        lower(&mut plan, &counts);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        let once = plan.to_string();
        lower(&mut plan, &counts);
        assert_eq!(plan.to_string(), once, "{text} moved again on a second run");
        once
    }

    #[test]
    fn a_filter_on_the_marker_becomes_the_join_itself() {
        assert_eq!(
            converted(concat!(
                "Filter #1.1::BOOLEAN\n",
                "  Join MARK on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
                "    Project #1 [#2.0::BIGINT AS k, TRUE::BOOLEAN AS mark]\n",
                "      Get memory.main.u AS u #2 [k::BIGINT]\n",
            )),
            concat!(
                "Join SEMI on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "  Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
                "  Project #1 [#2.0::BIGINT AS k, TRUE::BOOLEAN AS mark]\n",
                "    Get memory.main.u AS u #2 [k::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_filter_on_something_other_than_the_marker_is_left_alone() {
        let text = concat!(
            "Filter (#0.0::BIGINT > 3::BIGINT)::BOOLEAN\n",
            "  Join MARK on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "    Project #1 [#2.0::BIGINT AS k, TRUE::BOOLEAN AS mark]\n",
            "      Get memory.main.u AS u #2 [k::BIGINT]\n",
        );
        assert_eq!(converted(text), text);
    }

    #[test]
    fn a_filter_on_a_gathered_column_that_is_not_the_marker_is_left_alone() {
        let text = concat!(
            "Filter (#1.0::BIGINT > 3::BIGINT)::BOOLEAN\n",
            "  Join MARK on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "    Project #1 [#2.0::BIGINT AS k, TRUE::BOOLEAN AS mark]\n",
            "      Get memory.main.u AS u #2 [k::BIGINT]\n",
        );
        assert_eq!(converted(text), text);
    }

    #[test]
    fn a_gathered_column_read_above_the_filter_stops_the_rewrite() {
        let text = concat!(
            "Project #3 [#1.0::BIGINT AS k]\n",
            "  Filter #1.1::BOOLEAN\n",
            "    Join MARK on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "      Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "      Project #1 [#2.0::BIGINT AS k, TRUE::BOOLEAN AS mark]\n",
            "        Get memory.main.u AS u #2 [k::BIGINT]\n",
        );
        assert_eq!(converted(text), text);
    }

    #[test]
    fn a_driving_column_read_above_the_filter_does_not_stop_it() {
        assert_eq!(
            converted(concat!(
                "Project #3 [#0.0::BIGINT AS a]\n",
                "  Filter #1.1::BOOLEAN\n",
                "    Join MARK on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "      Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
                "      Project #1 [#2.0::BIGINT AS k, TRUE::BOOLEAN AS mark]\n",
                "        Get memory.main.u AS u #2 [k::BIGINT]\n",
            )),
            concat!(
                "Project #3 [#0.0::BIGINT AS a]\n",
                "  Join SEMI on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
                "    Project #1 [#2.0::BIGINT AS k, TRUE::BOOLEAN AS mark]\n",
                "      Get memory.main.u AS u #2 [k::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_filter_over_a_join_that_is_not_a_mark_is_left_alone() {
        let text = concat!(
            "Filter #1.1::BOOLEAN\n",
            "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "    Project #1 [#2.0::BIGINT AS k, TRUE::BOOLEAN AS mark]\n",
            "      Get memory.main.u AS u #2 [k::BIGINT]\n",
        );
        assert_eq!(converted(text), text);
    }

    #[test]
    fn a_duplicate_eliminator_over_an_inner_join_makes_the_join_a_semi_join() {
        assert_eq!(
            narrowed(concat!(
                "Aggregate #3 groups=[#0.0::BIGINT] aggregates=[]\n",
                "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
                "    Get memory.main.u AS u #1 [k::BIGINT]\n",
            )),
            concat!(
                "Aggregate #3 groups=[#0.0::BIGINT] aggregates=[]\n",
                "  Join SEMI on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
                "    Get memory.main.u AS u #1 [k::BIGINT]\n",
            )
        );
    }

    #[test]
    fn an_aggregate_that_actually_aggregates_is_left_alone() {
        let text = concat!(
            "Aggregate #3 groups=[#0.0::BIGINT] aggregates=[count_star()::BIGINT]\n",
            "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "    Get memory.main.u AS u #1 [k::BIGINT]\n",
        );
        assert_eq!(narrowed(text), text);
    }

    #[test]
    fn a_group_that_reads_the_gathered_side_is_left_alone() {
        let text = concat!(
            "Aggregate #3 groups=[#0.0::BIGINT, #1.0::BIGINT] aggregates=[]\n",
            "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "    Get memory.main.u AS u #1 [k::BIGINT]\n",
        );
        assert_eq!(narrowed(text), text);
    }

    #[test]
    fn a_group_over_an_expression_rather_than_a_column_is_left_alone() {
        let text = concat!(
            "Aggregate #3 groups=[\"+\"(#0.0::BIGINT, 1::BIGINT)::BIGINT] aggregates=[]\n",
            "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "    Get memory.main.u AS u #1 [k::BIGINT]\n",
        );
        assert_eq!(narrowed(text), text);
    }

    #[test]
    fn a_join_a_second_node_also_points_at_is_left_alone() {
        // Decorrelation shares a subtree between the two halves of a query that asked the same
        // question twice, and a kind written into a shared node is written for both readers.
        let text = concat!(
            "Join INNER on=[(#0.0::BIGINT = #4.0::BIGINT)::BOOLEAN]\n",
            "  Aggregate #3 groups=[#0.0::BIGINT] aggregates=[]\n",
            "    Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "      Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "      Get memory.main.u AS u #1 [k::BIGINT]\n",
            "  Project #4 [#0.0::BIGINT AS a]\n",
            "    Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "      Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "      Get memory.main.u AS u #1 [k::BIGINT]\n",
        );
        assert_eq!(narrowed(text), text);
    }

    #[test]
    fn a_gathered_column_read_somewhere_else_stops_the_narrowing() {
        let text = concat!(
            "Project #4 [#3.0::BIGINT AS a, #1.0::BIGINT AS k]\n",
            "  Aggregate #3 groups=[#0.0::BIGINT] aggregates=[]\n",
            "    Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "      Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "      Get memory.main.u AS u #1 [k::BIGINT]\n",
        );
        assert_eq!(narrowed(text), text);
    }

    #[test]
    fn a_semi_join_moves_into_the_side_of_the_inner_join_its_condition_reads() {
        assert_eq!(
            lowered(concat!(
                "Join SEMI on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.u AS u #0 [b::BIGINT]\n",
                "    Get memory.main.t AS t #1 [a::BIGINT]\n",
                "  Get memory.main.v AS v #2 [c::BIGINT]\n",
            )),
            concat!(
                "Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "  Join SEMI on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.u AS u #0 [b::BIGINT]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "  Get memory.main.t AS t #1 [a::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_semi_join_that_reads_the_second_side_moves_into_that_one() {
        assert_eq!(
            lowered(concat!(
                "Join SEMI on=[(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT]\n",
                "    Get memory.main.u AS u #1 [b::BIGINT]\n",
                "  Get memory.main.v AS v #2 [c::BIGINT]\n",
            )),
            concat!(
                "Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "  Get memory.main.t AS t #0 [a::BIGINT]\n",
                "  Join SEMI on=[(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.u AS u #1 [b::BIGINT]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_condition_reading_both_sides_of_the_inner_join_stays_above_it() {
        let text = concat!(
            "Join SEMI on=[(\"+\"(#0.0::BIGINT, #1.0::BIGINT)::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.u AS u #0 [b::BIGINT]\n",
            "    Get memory.main.t AS t #1 [a::BIGINT]\n",
            "  Get memory.main.v AS v #2 [c::BIGINT]\n",
        );
        assert_eq!(lowered(text), text);
    }

    #[test]
    fn an_anti_join_moves_the_same_way_a_semi_join_does() {
        assert_eq!(
            lowered(concat!(
                "Join ANTI on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.u AS u #0 [b::BIGINT]\n",
                "    Get memory.main.t AS t #1 [a::BIGINT]\n",
                "  Get memory.main.v AS v #2 [c::BIGINT]\n",
            )),
            concat!(
                "Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "  Join ANTI on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.u AS u #0 [b::BIGINT]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "  Get memory.main.t AS t #1 [a::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_semi_join_over_a_cross_product_moves_whatever_the_two_sides_weigh() {
        // The side moved into is the thousand row one and the side left behind is the ten row one,
        // which a join would refuse on. A cross product cannot produce fewer rows than the side.
        assert_eq!(
            lowered(concat!(
                "Join SEMI on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "  CrossProduct\n",
                "    Get memory.main.t AS t #0 [a::BIGINT]\n",
                "    Get memory.main.u AS u #1 [b::BIGINT]\n",
                "  Get memory.main.v AS v #2 [c::BIGINT]\n",
            )),
            concat!(
                "CrossProduct\n",
                "  Join SEMI on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "  Get memory.main.u AS u #1 [b::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_join_to_a_smaller_side_keeps_the_semi_join_above_it() {
        // q21 in miniature. The join cuts the thousand rows down rather than building them up, so
        // the semi join runs over fewer rows where it is than it would underneath.
        let text = concat!(
            "Join SEMI on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            "  Get memory.main.v AS v #2 [c::BIGINT]\n",
        );
        assert_eq!(lowered(text), text);
    }

    #[test]
    fn a_side_with_no_row_count_keeps_the_semi_join_above_it() {
        let text = concat!(
            "Join SEMI on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.u AS u #0 [b::BIGINT]\n",
            "    Get memory.main.x AS x #1 [a::BIGINT]\n",
            "  Get memory.main.v AS v #2 [c::BIGINT]\n",
        );
        assert_eq!(lowered(text), text);
    }

    #[test]
    fn a_semi_join_with_no_conditions_is_left_where_it_is() {
        let text = concat!(
            "Join SEMI on=[]\n",
            "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.u AS u #0 [b::BIGINT]\n",
            "    Get memory.main.t AS t #1 [a::BIGINT]\n",
            "  Get memory.main.v AS v #2 [c::BIGINT]\n",
        );
        assert_eq!(lowered(text), text);
    }

    #[test]
    fn a_semi_join_over_an_ordinary_node_is_left_where_it_is() {
        let text = concat!(
            "Join SEMI on=[(#3.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "  Project #3 [#0.0::BIGINT AS a]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            "  Get memory.main.v AS v #2 [c::BIGINT]\n",
        );
        assert_eq!(lowered(text), text);
    }

    #[test]
    fn a_semi_join_goes_past_two_inner_joins_in_a_row() {
        // The shape q18 has, with the semi join written over the join of three tables and reading
        // a column of the one furthest down.
        assert_eq!(
            lowered(concat!(
                "Join SEMI on=[(#0.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
                "  Join INNER on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "      Get memory.main.u AS u #0 [b::BIGINT]\n",
                "      Get memory.main.t AS t #1 [a::BIGINT]\n",
                "    Get memory.main.w AS w #2 [d::BIGINT]\n",
                "  Get memory.main.v AS v #3 [c::BIGINT]\n",
            )),
            concat!(
                "Join INNER on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    Join SEMI on=[(#0.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
                "      Get memory.main.u AS u #0 [b::BIGINT]\n",
                "      Get memory.main.v AS v #3 [c::BIGINT]\n",
                "    Get memory.main.t AS t #1 [a::BIGINT]\n",
                "  Get memory.main.w AS w #2 [d::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_condition_that_reads_the_gathered_side_as_well_still_moves() {
        // What the semi join reads of its own gathered side goes with it, so only the part of the
        // condition that reads the driving side decides which way it goes.
        assert_eq!(
            lowered(concat!(
                "Join SEMI on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN, ",
                "(#2.1::BIGINT > #0.0::BIGINT)::BOOLEAN]\n",
                "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.u AS u #0 [b::BIGINT]\n",
                "    Get memory.main.t AS t #1 [a::BIGINT]\n",
                "  Get memory.main.v AS v #2 [c::BIGINT, e::BIGINT]\n",
            )),
            concat!(
                "Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "  Join SEMI on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN, ",
                "(#2.1::BIGINT > #0.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.u AS u #0 [b::BIGINT]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT, e::BIGINT]\n",
                "  Get memory.main.t AS t #1 [a::BIGINT]\n",
            )
        );
    }
}

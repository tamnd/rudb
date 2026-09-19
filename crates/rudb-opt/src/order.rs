//! Choosing which order a run of inner joins runs in.
//!
//! Until this, the order was the order of the `FROM` list. Filter pushdown walks down the cross
//! products the binder made and turns a predicate into a join condition at the first level where
//! both of the columns it reads are available, which is a rule about where a predicate may go and
//! not a rule about which join is worth doing first. It produces a correct plan in whatever order
//! the query was typed in.
//!
//! On TPC-H q9 that costs a cross product. The `FROM` list is part, supplier, lineitem, partsupp,
//! orders, nation, and no condition in the query reads part and supplier and nothing else, because
//! the two of them are joined through lineitem. So the first two entries of the list become a
//! genuine cross product, ten thousand suppliers against every part whose name has green in it,
//! which is a hundred million rows built to answer a query whose answer has 175 rows in it.
//!
//! # What it does
//!
//! A run of inner joins and cross products with nothing else between them is one region, and the
//! things it joins are its leaves. Every condition of every join in the region is a condition of the
//! region, because an inner join is associative and commutative and its conditions are anded, so any
//! of them may be tested at any join that has the columns it reads. That is the whole licence this
//! pass needs and it is why nothing here rewrites an expression: a column is bound to the operator
//! that produces it rather than to a position in a row, so moving a join does not move a column.
//!
//! The order is chosen greedily. Repeatedly take the pair of parts with a condition between them
//! that produces the fewest rows, join them, and put the result back, until one part is left. A pair
//! with a condition between them is scored with [`crate::estimate::matched`], which is the larger of
//! the two sides and the keyspace reading of the conditions that become testable at that pair,
//! whichever is bigger. A pair with no condition between them is taken only when no pair in the
//! region has one, which is the case where a cross product is the only thing left to build, and
//! `cheapest` is where that rule is and why it is not a tie break.
//!
//! The order that comes out replaces the one that was there when the sum of the rows its joins
//! produce is smaller, and never when it builds more cross products than the region already had.
//! Both orders are scored the same way and by the same function the greedy step minimises one pair
//! at a time.
//!
//! Greedy rather than the dynamic program over connected subgraphs that the literature wants, which
//! is exponential in the number of leaves and needs a deadline and a fallback. Greedy is one pass
//! over the pairs per join and it gets the orders that matter here, which are the ones where a join
//! on a low cardinality key is being built before the joins that would have cut the sides down. The
//! search goes in when there is a query it gets wrong.
//!
//! # What it refuses
//!
//! A region where any leaf has no row estimate. Two sides cannot be compared when one of them is
//! unknown, and picking an order from a number that was made up to fill the gap is how a plan gets
//! worse rather than better.
//!
//! A condition that reads something no leaf of the region produces, which is a correlated reference
//! the unnesting pass has not taken out yet, and a condition that reads only one leaf, which is a
//! filter written as a join condition. Neither of those is a thing to reason about here, and a
//! region with one in it is left exactly as it was.
//!
//! An order that does not cost less than the order the query was written in. The cost is the sum of
//! the rows the joins produce, which is the measure the greedy step is minimising one pair at a
//! time, and the plan that was already there is scored the same way and kept when it wins. That
//! keeps this pass off every query whose `FROM` list was already in a sensible order, which is most
//! of them, and it means a plan can only be replaced by one this pass believes is better rather than
//! by one it merely built later.
//!
//! An order that builds more cross products than the region already had. A cross product is worse
//! than a join with a condition on it whatever the two sides are, and that is true without knowing a
//! single distinct count, so it is refused whatever the rest of the sum says.
//!
//! This pass was held to removing cross products and nothing else until #917, because the only thing
//! the cost function could say about a join with a condition on it was the containment assumption,
//! and on q5 that is wrong by two orders: it put customer joined to supplier on `nationkey` at a
//! hundred and fifty thousand rows, the size of the larger side, when there are twenty five nations
//! in the table and the answer is sixty million. Greedy believed it, built that join first and made
//! q5 seventy times slower. The distinct counts are what took the refusal out, and the same join now
//! scores at what it produces, so greedy leaves it until the sides have been cut down.

use rudb_common::Result;
use rudb_plan::{BuildSide, ExprRef, JoinKind, Node, NodeRef, Plan};

use crate::estimate::{self, Facts};
use crate::pass::{Context, Pass};
use crate::tables::{TableSet, Tables, produced};
use crate::walk;

/// Reorders each run of inner joins by how many rows the orders are estimated to produce.
#[derive(Debug, Clone, Copy)]
pub struct JoinOrder;

impl Pass for JoinOrder {
    /// DuckDB's name for the same job, which is one of the forty four `duckdb_optimizers()` lists.
    fn name(&self) -> &'static str {
        "join_order"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        reorder(plan, context.facts());
        Ok(())
    }
}

/// Reorders every run of inner joins in `plan`.
pub fn reorder(plan: &mut Plan, stats: &Facts) {
    let mut tables = Tables::new();
    let root = rebuild(plan, plan.root(), &mut tables, stats);
    plan.set_root(root);
}

/// One part of a region as the search has it so far, which starts as a leaf and ends as the region.
struct Part {
    /// Which entry of the build list produces its rows.
    build: usize,
    /// Which table indices those rows carry, which is what says whether a condition can be tested.
    tables: TableSet,
    /// How many rows it is estimated to produce.
    rows: u64,
}

/// One node of the order the search chose, before any of it is put in the arena.
///
/// The search is scored against the order that was already there and loses most of the time, so it
/// writes down what it would build and builds it only if it wins. A pass that added nodes while
/// searching would leave the arena holding a plan nobody runs every time it decided not to.
enum Build {
    /// Something the region joins, which is already a node.
    Leaf(NodeRef),
    /// A join of two entries, or a cross product where there are no conditions between them.
    Pair { left: usize, right: usize, conditions: Vec<ExprRef> },
}

/// Rewrites the plan under `at`, returning `at` itself where nothing under it changed.
fn rebuild(plan: &mut Plan, at: NodeRef, tables: &mut Tables, stats: &Facts) -> NodeRef {
    if joining(plan, at) {
        let mut leaves = Vec::new();
        let mut conditions = Vec::new();
        gather(plan, at, &mut leaves, &mut conditions);
        let rebuilt: Vec<NodeRef> =
            leaves.iter().map(|&leaf| rebuild(plan, leaf, tables, stats)).collect();
        // Two leaves is one join and there is nothing to choose. The region is still rebuilt where a
        // leaf changed under it, which the walk below does.
        let chosen = match leaves.len() {
            0..=2 => None,
            _ => order(plan, at, &rebuilt, &conditions, tables, stats),
        };
        if let Some(chosen) = chosen {
            return chosen;
        }
        if rebuilt == leaves {
            return at;
        }
        return restack(plan, at, &leaves, &rebuilt);
    }
    let children: Vec<NodeRef> = plan.node(at).children().into_iter().flatten().collect();
    let rebuilt: Vec<NodeRef> =
        children.iter().map(|&child| rebuild(plan, child, tables, stats)).collect();
    if rebuilt == children {
        return at;
    }
    let mut node = plan.node(at).clone();
    walk::replace_children(&mut node, &rebuilt);
    plan.add_node(node)
}

/// Whether this node is part of a region, which is an inner join or a cross product and nothing else.
fn joining(plan: &Plan, at: NodeRef) -> bool {
    matches!(*plan.node(at), Node::CrossProduct { .. } | Node::Join { kind: JoinKind::Inner, .. })
}

/// The leaves and the conditions of the region rooted at `at`, in the order the region holds them.
fn gather(plan: &Plan, at: NodeRef, leaves: &mut Vec<NodeRef>, conditions: &mut Vec<ExprRef>) {
    match *plan.node(at) {
        Node::CrossProduct { left, right } => {
            gather(plan, left, leaves, conditions);
            gather(plan, right, leaves, conditions);
        }
        Node::Join { left, right, kind: JoinKind::Inner, conditions: list, .. } => {
            conditions.extend_from_slice(plan.expr_list(list));
            gather(plan, left, leaves, conditions);
            gather(plan, right, leaves, conditions);
        }
        _ => leaves.push(at),
    }
}

/// Rebuilds the region rooted at `at` with each leaf replaced by what it rebuilt to.
///
/// The shape is the shape that was already there. This is the path for a region the search declined
/// or did not improve on, where something underneath a leaf changed anyway.
fn restack(plan: &mut Plan, at: NodeRef, leaves: &[NodeRef], rebuilt: &[NodeRef]) -> NodeRef {
    if let Some(found) = leaves.iter().position(|&leaf| leaf == at) {
        return rebuilt[found];
    }
    let children: Vec<NodeRef> = plan.node(at).children().into_iter().flatten().collect();
    let children: Vec<NodeRef> =
        children.into_iter().map(|child| restack(plan, child, leaves, rebuilt)).collect();
    let mut node = plan.node(at).clone();
    walk::replace_children(&mut node, &children);
    plan.add_node(node)
}

/// The region built in the order the search chose, or `None` where it will not choose one.
fn order(
    plan: &mut Plan,
    at: NodeRef,
    leaves: &[NodeRef],
    conditions: &[ExprRef],
    tables: &mut Tables,
    stats: &Facts,
) -> Option<NodeRef> {
    let mut builds: Vec<Build> = leaves.iter().map(|&leaf| Build::Leaf(leaf)).collect();
    let mut parts = Vec::with_capacity(leaves.len());
    for (build, &leaf) in leaves.iter().enumerate() {
        parts.push(Part {
            build,
            tables: produced(plan, leaf),
            rows: estimate::rows(plan, leaf, stats)?,
        });
    }
    let mut whole = TableSet::new();
    for part in &parts {
        whole.extend(&part.tables);
    }
    let mut pending: Vec<(ExprRef, TableSet)> = Vec::with_capacity(conditions.len());
    for &condition in conditions {
        let reads = tables.of(plan, condition);
        // A condition that reaches outside the region belongs to a scope this pass is not reasoning
        // about, and one that reads a single leaf is a filter rather than an edge. Either way the
        // region is left as it was rather than rebuilt around a condition nobody can place.
        if !reads.is_subset_of(&whole) || parts.iter().any(|part| reads.is_subset_of(&part.tables))
        {
            return None;
        }
        pending.push((condition, reads));
    }
    let (_, before, was) = cost(plan, at, stats)?;
    let mut after = 0u64;
    let mut built = 0usize;
    while parts.len() > 1 {
        let (left, right, rows) = cheapest(plan, &parts, &pending, stats);
        let mut union = parts[left].tables.clone();
        union.extend(&parts[right].tables);
        let conditions: Vec<ExprRef> = pending
            .iter()
            .filter(|(_, reads)| reads.is_subset_of(&union))
            .map(|(condition, _)| *condition)
            .collect();
        pending.retain(|(_, reads)| !reads.is_subset_of(&union));
        // The larger index comes out first, so the smaller one is still where it was.
        let right = parts.remove(right);
        let left = parts.remove(left);
        built += usize::from(conditions.is_empty());
        builds.push(Build::Pair { left: left.build, right: right.build, conditions });
        after = after.saturating_add(rows);
        parts.push(Part { build: builds.len() - 1, tables: union, rows });
    }
    // An order that builds more cross products than the region already had is refused whatever the
    // sum says, because a cross product is worse than a join with a condition on it whatever the two
    // sides are and no estimate is needed to know that. Otherwise the sum decides, which it can now
    // that the rows it adds up come from [`estimate::matched`] rather than from the containment
    // assumption alone. That is the change #917 made: a join on a low cardinality key is scored at
    // what it produces, so the order that puts one first no longer looks like the cheap one.
    if built > was || after >= before {
        return None;
    }
    Some(put(plan, &builds, parts[0].build))
}

/// Puts one entry of the build list and everything under it into the arena.
///
/// Depth first, so both inputs of a join are in the arena before the join is, which is the arena's
/// rule that a node may only point backwards.
fn put(plan: &mut Plan, builds: &[Build], at: usize) -> NodeRef {
    match &builds[at] {
        Build::Leaf(node) => *node,
        Build::Pair { left, right, conditions } => {
            let conditions = conditions.clone();
            let left = put(plan, builds, *left);
            let right = put(plan, builds, *right);
            if conditions.is_empty() {
                return plan.add_node(Node::CrossProduct { left, right });
            }
            let conditions = plan.add_expr_list(&conditions);
            // The build side the pass that chooses one will read as it ends up, which is
            // [`crate::sides`] and runs after this.
            plan.add_node(Node::Join {
                left,
                right,
                kind: JoinKind::Inner,
                conditions,
                build: BuildSide::default(),
            })
        }
    }
}

/// The pair of parts to join next, and how many rows it is estimated to produce.
///
/// A pair with a condition between them beats a pair without one however few rows the second would
/// produce, which is not a tie break but the first thing asked. TPC-H q7 is why. It reads nation
/// twice, once for the supplier's nation and once for the customer's, and the two copies have no
/// condition between them, so the cheapest pair in the region by any row count is those two: the
/// product of twenty five rows with twenty five is six hundred and twenty five, which is fewer than
/// any real join in the query produces. Taking it puts both copies of nation on one side, and the
/// joins after it are then joins to a side that carries a column the condition does not mention, so
/// each of them multiplies by twenty five rather than matching. The query ran out of memory. A cross
/// product is a thing to build when the region leaves no choice, and never because it looked cheap.
///
/// After that, the fewest rows, then the pair whose two inputs are smallest between them, then the
/// pair that came first, which is what makes the choice the same on every run.
fn cheapest(
    plan: &Plan,
    parts: &[Part],
    pending: &[(ExprRef, TableSet)],
    stats: &Facts,
) -> (usize, usize, u64) {
    let mut best: Option<Pick> = None;
    for left in 0..parts.len() {
        for right in left + 1..parts.len() {
            let mut union = parts[left].tables.clone();
            union.extend(&parts[right].tables);
            let testable: Vec<ExprRef> = pending
                .iter()
                .filter(|(_, reads)| reads.is_subset_of(&union))
                .map(|(condition, _)| *condition)
                .collect();
            let linked = !testable.is_empty();
            let rows = if linked {
                let keys = estimate::keyspace_of(plan, &testable, stats);
                estimate::matched(parts[left].rows, parts[right].rows, keys)
            } else {
                parts[left].rows.saturating_mul(parts[right].rows)
            };
            let order = (!linked, rows, parts[left].rows.saturating_add(parts[right].rows));
            if best.is_none_or(|held| order < held.order) {
                best = Some(Pick { left, right, rows, order });
            }
        }
    }
    let best = best.expect("a region has at least two parts");
    (best.left, best.right, best.rows)
}

/// One pair [`cheapest`] is considering, with what it would cost and where that puts it.
#[derive(Clone, Copy)]
struct Pick {
    /// The part on the left, by position.
    left: usize,
    /// The part on the right, by position.
    right: usize,
    /// How many rows joining the two is estimated to produce.
    rows: u64,
    /// What the pairs are sorted by: unconnected last, then the rows, then the two inputs together.
    order: (bool, u64, u64),
}

/// What the region already there produces and what it costs, by the measure the search minimises.
///
/// The rows, the sum of the rows of every join in it, and how many of those joins have no condition
/// between their two sides. Scored with the same two rules the search uses rather than with
/// [`crate::estimate`], because the two have to be the same measure for the comparison to mean
/// anything, and because this is the measure the greedy step is minimising one pair at a time.
fn cost(plan: &Plan, at: NodeRef, stats: &Facts) -> Option<(u64, u64, usize)> {
    let (left, right, testable) = match *plan.node(at) {
        Node::CrossProduct { left, right } => (left, right, Vec::new()),
        Node::Join { left, right, kind: JoinKind::Inner, conditions, .. } => {
            (left, right, plan.expr_list(conditions).to_vec())
        }
        _ => return Some((estimate::rows(plan, at, stats)?, 0, 0)),
    };
    let linked = !testable.is_empty();
    let (left, under_left, crossed_left) = cost(plan, left, stats)?;
    let (right, under_right, crossed_right) = cost(plan, right, stats)?;
    let rows = if linked {
        let keys = estimate::keyspace_of(plan, &testable, stats);
        estimate::matched(left, right, keys)
    } else {
        left.saturating_mul(right)
    };
    Some((
        rows,
        under_left.saturating_add(under_right).saturating_add(rows),
        crossed_left + crossed_right + usize::from(!linked),
    ))
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use crate::estimate::Facts;

    use super::reorder;

    /// What the plan a text prints looks like once the pass has run over it.
    ///
    /// The tables are counted here rather than in each test, because the pass refuses a region with
    /// an uncounted leaf in it and a test that forgot to count one would pass by being refused.
    fn ordered(text: &str) -> String {
        let mut counts = Facts::new();
        for (table, rows) in [("t", 1000), ("u", 10), ("v", 100), ("w", 100_000)] {
            counts.record("memory", "main", table, rows);
        }
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        reorder(&mut plan, &counts);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    /// The same with distinct counts handed in as well, the counts named table then column.
    fn counted(text: &str, columns: &[(&str, &str, u64)]) -> String {
        let mut counts = Facts::new();
        for (table, rows) in [("t", 1000), ("u", 10), ("v", 100), ("w", 100_000)] {
            counts.record("memory", "main", table, rows);
        }
        for (table, column, distinct) in columns {
            counts.record_distinct("memory", "main", table, column, *distinct);
        }
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        reorder(&mut plan, &counts);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    /// TPC-H q9's shape. `t` and `v` are the two entries of the `FROM` list that have no condition
    /// between them, and filter pushdown makes them a cross product because they are written next to
    /// each other. Both of them have a condition to `u`, so the cross product is avoidable.
    #[test]
    fn a_cross_product_the_conditions_can_avoid_is_not_built() {
        assert_eq!(
            ordered(concat!(
                "Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, ",
                "(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "  CrossProduct\n",
                "    Get memory.main.t AS t #0 [a::BIGINT]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "  Get memory.main.u AS u #1 [b::BIGINT]\n",
            )),
            concat!(
                "Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN]\n",
                "  Get memory.main.t AS t #0 [a::BIGINT]\n",
                "  Join INNER on=[(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            )
        );
    }

    #[test]
    fn an_order_the_search_does_not_improve_on_is_left_exactly_as_it_was() {
        // The two smallest joined first and the largest last, which is what the search would pick,
        // so the plan is kept rather than rebuilt into the same shape with different node numbers.
        let text = concat!(
            "Join INNER on=[(#3.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "  Get memory.main.w AS w #3 [d::BIGINT]\n",
            "  Join INNER on=[(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            "    Get memory.main.v AS v #2 [c::BIGINT]\n",
        );
        assert_eq!(ordered(text), text);
    }

    #[test]
    fn a_region_with_a_leaf_nobody_counted_is_left_alone() {
        // `x` is in no table this test counted, so its side is unknown and there is nothing to
        // compare the cross product against. The cross product stays.
        let text = concat!(
            "Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, ",
            "(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "  CrossProduct\n",
            "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            "    Get memory.main.x AS x #2 [c::BIGINT]\n",
            "  Get memory.main.u AS u #1 [b::BIGINT]\n",
        );
        assert_eq!(ordered(text), text);
    }

    #[test]
    fn a_condition_that_reads_one_leaf_stops_the_search() {
        // A join condition over a single side is a filter that ended up written as a condition, and
        // placing it is a question about where a filter goes rather than about join order.
        let text = concat!(
            "Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, ",
            "(#2.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "  CrossProduct\n",
            "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            "    Get memory.main.v AS v #2 [c::BIGINT]\n",
            "  Get memory.main.u AS u #1 [b::BIGINT]\n",
        );
        assert_eq!(ordered(text), text);
    }

    #[test]
    fn an_outer_join_is_not_part_of_a_region() {
        // A left join is neither associative nor commutative with an inner join in the general case,
        // so the region stops at it and the cross product above it has two leaves and nothing to
        // reorder.
        let text = concat!(
            "Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN]\n",
            "  Join LEFT on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            "    Get memory.main.v AS v #2 [c::BIGINT]\n",
            "  Get memory.main.u AS u #1 [b::BIGINT]\n",
        );
        assert_eq!(ordered(text), text);
    }

    #[test]
    fn two_small_parts_with_no_condition_between_them_are_not_joined_to_each_other() {
        // TPC-H q7 and q8 read nation twice and have no condition between the two copies, so the
        // product of the two is the cheapest pair in the region by row count and is the wrong pair
        // by a long way. Here `u` and `v` are the two copies and `t` is what both of them join to.
        // The cross product of `w` and `u` is what lets the search act at all, and the order it
        // builds has to take that one out without putting `u` and `v` together instead.
        assert_eq!(
            ordered(concat!(
                "Join INNER on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "  Join INNER on=[(#3.0::BIGINT = #0.0::BIGINT)::BOOLEAN, ",
                "(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    CrossProduct\n",
                "      Get memory.main.w AS w #3 [d::BIGINT]\n",
                "      Get memory.main.u AS u #1 [b::BIGINT]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT]\n",
                "  Get memory.main.v AS v #2 [c::BIGINT]\n",
            )),
            concat!(
                "Join INNER on=[(#3.0::BIGINT = #0.0::BIGINT)::BOOLEAN]\n",
                "  Get memory.main.w AS w #3 [d::BIGINT]\n",
                "  Join INNER on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "    Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "      Get memory.main.u AS u #1 [b::BIGINT]\n",
                "      Get memory.main.t AS t #0 [a::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_region_with_no_path_between_any_of_it_still_builds_the_smallest_middle() {
        // Nothing here has a condition to anything, so the answer is the product either way and
        // every order builds two cross products. What differs is what is held in between: crossing
        // the hundred thousand with the hundred first makes ten million rows to cross again, and
        // taking the two small ones first makes a thousand. The sum is the measure and it says so.
        assert_eq!(
            ordered(concat!(
                "CrossProduct\n",
                "  CrossProduct\n",
                "    Get memory.main.w AS w #3 [d::BIGINT]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "  Get memory.main.u AS u #1 [b::BIGINT]\n",
            )),
            concat!(
                "CrossProduct\n",
                "  Get memory.main.w AS w #3 [d::BIGINT]\n",
                "  CrossProduct\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            )
        );
    }

    #[test]
    fn an_order_the_search_reaches_and_does_not_beat_leaves_the_region_alone() {
        // `v` joins to nothing, so one cross product is built however the region is ordered. Greedy
        // takes the linked pair before any unlinked one, so it builds the same shape that is there
        // and scores it the same, and an order that only ties is not an order worth rebuilding for.
        let text = concat!(
            "CrossProduct\n",
            "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            "  Get memory.main.v AS v #2 [c::BIGINT]\n",
        );
        assert_eq!(ordered(text), text);
    }

    #[test]
    fn a_join_on_a_column_with_two_values_in_it_is_left_until_the_sides_have_been_cut_down() {
        // The q5 shape. `u` joins to `w` on a column with two values in it and `t` joins to `w` on
        // a key. Both pairs read the same under the containment assumption, which puts each of them
        // at the size of `w`, and the tie goes to the pair whose inputs are smaller between them,
        // which is `u` and `w`. That is the wrong one: ten rows against a hundred thousand on two
        // values is half a million rows and not a hundred thousand.
        let text = concat!(
            "Join INNER on=[(#0.0::BIGINT = #3.1::BIGINT)::BOOLEAN]\n",
            "  Join INNER on=[(#1.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            "    Get memory.main.w AS w #3 [d::BIGINT, e::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::BIGINT]\n",
        );
        // Nobody counted anything, so the pass has nothing to say and the region stays as written.
        assert_eq!(ordered(text), text);
        // With the counts the low cardinality pair is scored at what it produces, so greedy joins
        // `t` to `w` on the key first and leaves `u` for last.
        assert_eq!(
            counted(text, &[("u", "b", 2), ("w", "d", 2), ("t", "a", 1000), ("w", "e", 100_000)]),
            concat!(
                "Join INNER on=[(#1.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
                "  Get memory.main.u AS u #1 [b::BIGINT]\n",
                "  Join INNER on=[(#0.0::BIGINT = #3.1::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.w AS w #3 [d::BIGINT, e::BIGINT]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_region_under_something_else_is_reordered_and_what_is_above_it_is_rebuilt() {
        assert_eq!(
            ordered(concat!(
                "Project #4 [#0.0::BIGINT AS a]\n",
                "  Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, ",
                "(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    CrossProduct\n",
                "      Get memory.main.t AS t #0 [a::BIGINT]\n",
                "      Get memory.main.v AS v #2 [c::BIGINT]\n",
                "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            )),
            concat!(
                "Project #4 [#0.0::BIGINT AS a]\n",
                "  Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT]\n",
                "    Join INNER on=[(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "      Get memory.main.v AS v #2 [c::BIGINT]\n",
                "      Get memory.main.u AS u #1 [b::BIGINT]\n",
            )
        );
    }
}

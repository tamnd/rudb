//! Giving a scan the keys of a join it sits below but that no runtime filter can reach.
//!
//! A hash join hands its build side's keys to the scan under its driving side once that side is in,
//! which is `rudb_exec`'s `sideways`. The walk down to the scan goes through filters, projections
//! and the driving side of each inner join on the way, because those are where a row the join above
//! would drop can be dropped earlier. A scan on the gathered side of a join further down is out of
//! its reach, and that is where a join on more than one key often puts one of its tables.
//!
//! TPC-H q05 is the case. The last join matches the lineitem rows that came through customer and
//! orders against the suppliers of Asia on two columns, `l_suppkey = s_suppkey` and `c_nationkey =
//! s_nationkey`. The first reaches the lineitem scan, which is the driving side all the way down.
//! The second is about customer, and customer is the gathered side of its join with orders, so all
//! 150,000 customers went into that join where the 30,000 in Asia would have done, and the orders
//! and then the lineitem rows of the other four regions were matched, carried and gathered before
//! the last join threw them away.
//!
//! What this pass writes is the fact the join was going to apply, applied at the scan. Over the
//! customer scan goes a semi join against a copy of the supplier side, on the same columns, so the
//! scan keeps the customers whose nation has a supplier in Asia. The semi join is a hash join like
//! any other and hands its own keys to the scan it drives, so what the scan reads is a test of five
//! nation keys per row.
//!
//! # Why it is the same query
//!
//! The join is an inner join, so a row with no match on the other side is not in the answer. The
//! path from the join down to the scan is inner joins, the left side of semi joins, filters, and
//! projections that pass the column through, and none of them changes the column's value or adds a
//! row that was not built from a scan row. So a scan row whose key the other side does not hold
//! only ever reaches the join in rows carrying that key, and every one of them is dropped there.
//! Dropping it at the scan is the same answer. A null key matches nothing under `=` at either
//! place. The copy is the same relation as the original only if reading it twice reads the same
//! rows, so a side with anything volatile in it is refused.
//!
//! # What it refuses
//!
//! A key the runtime filter already reaches, because that filter is the same fact for the price of
//! a pass over the build side's keys, where this pays for a second copy of the side. That is the
//! first equality of q05, and every join whose two sides are one key each.
//!
//! A side that restricts nothing, since its keys are every key the scan has. That is the same test
//! [`crate::keys`] makes, and it is structural rather than a number.
//!
//! A copy that costs more than it can save. The copy scans its tables again, so the rows those
//! tables hold together have to be a tenth of the rows the scan reads or less. That is a bound on
//! the work rather than on what it removes, because the rows a join between two filtered dimension
//! tables keeps are a number the estimates do not know well.
//!
//! A scan that already has a semi join over it. That is the shape this pass writes, so it is taken
//! as its own work from an earlier run, and without it the fixed sequence would write a second one.
//!
//! It runs after the build sides are chosen, because which scans the runtime filters reach depends
//! on them.

use rudb_common::Result;
use rudb_plan::{
    BuildSide, ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan,
};

use crate::estimate::{self, Facts};
use crate::keys::{copied, copyable, descent, renamed};
use crate::pass::{Context, Pass, top_down};
use crate::tables::{TableSet, produced};
use crate::walk;

/// How many times the rows the scan reads have to outnumber the rows the copy scans.
const WORTH_IT: u64 = 10;

/// Gives a scan the keys of a join above it that its runtime filters cannot bring down.
#[derive(Debug, Clone, Copy)]
pub struct JoinKeyReach;

impl Pass for JoinKeyReach {
    /// Local, since DuckDB has no runtime filter that stops where this one starts.
    fn name(&self) -> &'static str {
        "join_key_reach"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        reach(plan, context.facts());
        Ok(())
    }
}

/// Writes every semi join worth writing, one at a time with the plan walked again after each, for
/// the reason [`crate::keys::push`] gives.
pub fn reach(plan: &mut Plan, stats: &Facts) {
    for _ in 0..plan.node_count() {
        let found = top_down(plan).into_iter().find_map(|node| matched(plan, node, stats));
        let Some(found) = found else { return };
        let Some(rebuilt) = written(plan, &found) else { return };
        let mut changed = false;
        let root = walk::restack(plan, plan.root(), &mut changed, &mut |_, here| {
            (here == found.scan).then_some(rebuilt)
        });
        if !changed {
            return;
        }
        plan.set_root(root);
    }
}

/// One semi join, worked out before anything is written.
struct Reach {
    /// The scan, or the filters right over it, that the semi join goes over.
    scan: NodeRef,
    /// The scan's key column.
    key: ColumnBinding,
    /// The relation on the other side of the join holding the keys, which is what gets copied.
    source: NodeRef,
    /// The other side's key column, read out of `source`.
    held: ExprRef,
    /// The condition the join was written with, whose types the semi join's takes.
    condition: ExprRef,
}

/// What the join at `node` would let this pass write, or nothing.
fn matched(plan: &Plan, node: NodeRef, stats: &Facts) -> Option<Reach> {
    let Node::Join { left, right, kind: JoinKind::Inner, conditions, build } = *plan.node(node)
    else {
        return None;
    };
    let (driving, gathered) = match build {
        BuildSide::Right => (left, right),
        BuildSide::Left => (right, left),
    };
    let (near, far) = (produced(plan, driving), produced(plan, gathered));
    for &condition in plan.expr_list(conditions) {
        let Expr::Compare { op: CompareOp::Equal, left: one, right: other } = *plan.expr(condition)
        else {
            continue;
        };
        let (&Expr::Column(was), &Expr::Column(is)) = (plan.expr(one), plan.expr(other)) else {
            continue;
        };
        let (key, held, table) = if near.contains(was.table) && far.contains(is.table) {
            (was, other, is.table)
        } else if near.contains(is.table) && far.contains(was.table) {
            (is, one, was.table)
        } else {
            continue;
        };
        if reached(plan, driving, key) {
            continue;
        }
        let Some((scan, key)) = scanned(plan, driving, key) else { continue };
        let Some(reads) = estimate::rows(plan, scan, stats) else { continue };
        let wanted = TableSet::of(table);
        let source = descent(plan, gathered, &wanted).into_iter().rev().find(|&at| {
            copyable(plan, at)
                && estimate::side(plan, at, stats).is_some_and(|side| side.rows < side.base)
                && scans(plan, at, stats).is_some_and(|rows| rows.saturating_mul(WORTH_IT) <= reads)
        });
        let Some(source) = source else { continue };
        return Some(Reach { scan, key, source, held, condition });
    }
    None
}

/// Whether a runtime filter from a join over `at` reaches the scan of `key`.
///
/// The walk `rudb_exec` makes, down through filters, projections that pass the column on, and the
/// driving side of an inner join or a semi join that gathers its right side.
fn reached(plan: &Plan, at: NodeRef, key: ColumnBinding) -> bool {
    let (mut at, mut key) = (at, key);
    loop {
        match *plan.node(at) {
            Node::Get { index, .. } | Node::TableFunction { index, .. } => {
                return index == key.table;
            }
            Node::Filter { input, .. } => at = input,
            Node::Project { input, index, exprs, .. } => {
                if key.table == index {
                    let Some(&expr) = plan.expr_list(exprs).get(key.column as usize) else {
                        return false;
                    };
                    let Expr::Column(inner) = *plan.expr(expr) else { return false };
                    key = inner;
                }
                at = input;
            }
            Node::Join { left, right, kind, build, .. } => match (kind, build) {
                (JoinKind::Inner, BuildSide::Left) => at = right,
                (JoinKind::Inner | JoinKind::Semi, BuildSide::Right) => at = left,
                _ => return false,
            },
            _ => return false,
        }
    }
}

/// The scan `key` is read out of, with the filters right over it, and the key as that scan names
/// it, through operators that keep the column's value and add no row.
fn scanned(plan: &Plan, at: NodeRef, key: ColumnBinding) -> Option<(NodeRef, ColumnBinding)> {
    match *plan.node(at) {
        Node::Get { index, .. } => (index == key.table).then_some((at, key)),
        Node::Filter { input, .. } => {
            let found = scanned(plan, input, key)?;
            // The filters over a scan are applied by the scan, so the semi join goes over them
            // rather than between them and it.
            Some(if found.0 == input { (at, found.1) } else { found })
        }
        Node::Project { input, index, exprs, .. } => {
            if key.table != index {
                return scanned(plan, input, key);
            }
            let &expr = plan.expr_list(exprs).get(usize::try_from(key.column).ok()?)?;
            let Expr::Column(inner) = *plan.expr(expr) else { return None };
            scanned(plan, input, inner)
        }
        Node::Join { left, right, kind: JoinKind::Inner, .. } => {
            let side = if produced(plan, left).contains(key.table) { left } else { right };
            scanned(plan, side, key)
        }
        Node::Join { left, kind: JoinKind::Semi, .. } => {
            let found = scanned(plan, left, key)?;
            (found.0 != left).then_some(found)
        }
        _ => None,
    }
}

/// How many rows the tables under `at` hold together, which is what a copy of it scans.
fn scans(plan: &Plan, at: NodeRef, stats: &Facts) -> Option<u64> {
    match *plan.node(at) {
        Node::Get { .. } | Node::TableFunction { .. } => estimate::rows(plan, at, stats),
        _ => {
            plan.node(at).children().into_iter().flatten().try_fold(0u64, |total, child| {
                Some(total.saturating_add(scans(plan, child, stats)?))
            })
        }
    }
}

/// The semi join over the scan, as a new node for [`walk::restack`] to put where the scan was.
fn written(plan: &mut Plan, reach: &Reach) -> Option<NodeRef> {
    let mut renames = Vec::new();
    let source = copied(plan, reach.source, &mut renames)?;
    let against = renamed(plan, reach.held, &renames);
    if against == reach.held {
        return None;
    }
    let ty = plan.expr_type(against).clone();
    let key = plan.add_expr(Expr::Column(reach.key), ty);
    let compare = Expr::Compare { op: CompareOp::Equal, left: key, right: against };
    let condition = plan.add_expr(compare, plan.expr_type(reach.condition).clone());
    let conditions = plan.add_expr_list(&[condition]);
    Some(plan.add_node(Node::Join {
        left: reach.scan,
        right: source,
        kind: JoinKind::Semi,
        conditions,
        build: BuildSide::Right,
    }))
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::reach;
    use crate::estimate::Facts;

    fn counts() -> Facts {
        let mut counts = Facts::new();
        for (table, rows) in [("t", 100_000), ("u", 100), ("w", 1_000_000)] {
            counts.record("memory", "main", table, rows);
        }
        counts
    }

    fn reached(text: &str) -> String {
        let counts = counts();
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        reach(&mut plan, &counts);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        let once = plan.to_string();
        reach(&mut plan, &counts);
        assert_eq!(plan.to_string(), once, "{text} reached again on a second run");
        once
    }

    #[test]
    fn a_scan_on_the_gathered_side_below_gets_the_keys() {
        assert_eq!(
            reached(concat!(
                "Join INNER on=[(#0.1::INTEGER = #2.0::INTEGER)::BOOLEAN]\n",
                "  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN] build=left\n",
                "    Get memory.main.t AS t #0 [k::INTEGER, n::INTEGER]\n",
                "    Get memory.main.w AS w #1 [k::INTEGER, v::INTEGER]\n",
                "  Filter (#2.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
                "    Get memory.main.u AS u #2 [k::INTEGER, b::INTEGER]\n",
            )),
            concat!(
                "Join INNER on=[(#0.1::INTEGER = #2.0::INTEGER)::BOOLEAN]\n",
                "  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN] build=left\n",
                "    Join SEMI on=[(#0.1::INTEGER = #3.0::INTEGER)::BOOLEAN]\n",
                "      Get memory.main.t AS t #0 [k::INTEGER, n::INTEGER]\n",
                "      Filter (#3.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
                "        Get memory.main.u AS u #3 [k::INTEGER, b::INTEGER]\n",
                "    Get memory.main.w AS w #1 [k::INTEGER, v::INTEGER]\n",
                "  Filter (#2.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
                "    Get memory.main.u AS u #2 [k::INTEGER, b::INTEGER]\n",
            )
        );
    }

    #[test]
    fn a_scan_the_runtime_filter_reaches_is_left_alone() {
        let text = concat!(
            "Join INNER on=[(#0.1::INTEGER = #2.0::INTEGER)::BOOLEAN]\n",
            "  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [k::INTEGER, n::INTEGER]\n",
            "    Get memory.main.w AS w #1 [k::INTEGER, v::INTEGER]\n",
            "  Filter (#2.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "    Get memory.main.u AS u #2 [k::INTEGER, b::INTEGER]\n",
        );
        assert_eq!(reached(text), text);
    }

    #[test]
    fn a_side_that_restricts_nothing_is_not_copied() {
        let text = concat!(
            "Join INNER on=[(#0.1::INTEGER = #2.0::INTEGER)::BOOLEAN]\n",
            "  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN] build=left\n",
            "    Get memory.main.t AS t #0 [k::INTEGER, n::INTEGER]\n",
            "    Get memory.main.w AS w #1 [k::INTEGER, v::INTEGER]\n",
            "  Get memory.main.u AS u #2 [k::INTEGER, b::INTEGER]\n",
        );
        assert_eq!(reached(text), text);
    }

    #[test]
    fn a_copy_as_large_as_the_scan_is_not_made() {
        let text = concat!(
            "Join INNER on=[(#0.1::INTEGER = #2.0::INTEGER)::BOOLEAN]\n",
            "  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN] build=left\n",
            "    Get memory.main.u AS u #0 [k::INTEGER, n::INTEGER]\n",
            "    Get memory.main.w AS w #1 [k::INTEGER, v::INTEGER]\n",
            "  Filter (#2.1::INTEGER = 3::INTEGER)::BOOLEAN\n",
            "    Get memory.main.t AS t #2 [k::INTEGER, b::INTEGER]\n",
        );
        assert_eq!(reached(text), text);
    }
}

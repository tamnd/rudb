//! Turning a mark join whose marker is only ever tested into a semi join.
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
use rudb_plan::{Expr, JoinKind, Node, NodeRef, Plan};

use crate::pass::{Context, Pass, top_down};
use crate::tables::produced;
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

    use super::convert;

    /// What the plan a text prints looks like once the pass has run over it.
    fn converted(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        convert(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
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
}

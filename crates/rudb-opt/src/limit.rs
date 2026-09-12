//! Moving a limit below the projection above it.
//!
//! A projection produces one row for every row it is given, so a limit above it and a limit below it
//! keep the same rows. Below is the cheaper of the two, because the expressions are then evaluated
//! for the rows that come out rather than for the rows that were going to be thrown away.
//!
//! The size of that is one chunk of work and not one table of it. Every operator here is pulled a
//! chunk at a time, so `LIMIT 10` over a scan of a hundred million rows already reads one chunk and
//! stops, and what this pass removes is the difference between projecting the 1024 rows of that
//! chunk and projecting the ten that were asked for. The reason to do it anyway is that it is the
//! plan the binary produces, `EXPLAIN` is meant to agree with it, and #102 wants the committed plan
//! baselines to be a reviewed diff rather than a running disagreement.
//!
//! # What it refuses
//!
//! Everything that is not a projection. A filter, a `DISTINCT` and a set operation all produce fewer
//! rows than they are given, so a limit below one of them is a different query. A sort and a top N
//! produce their rows in an order the limit is choosing from, so a limit below one of them is a
//! different query too. The binary refuses the same list, which is what `EXPLAIN` on the pinned
//! build says for a limit over a union and for a limit over a `DISTINCT`.
//!
//! It is not refused for a projection that calls a volatile function. `SELECT random() FROM t LIMIT
//! 10` is ten calls afterwards and 1024 before, and both of them are ten random numbers, because
//! nothing above the projection is reading the value to decide anything. That is the difference from
//! filter pushdown, where copying a volatile call into two places means the row that passed the test
//! is not the row that comes out.
//!
//! # Swapping two slots
//!
//! The rewrite cannot append. An expression or a node may only point at one behind it in the arena,
//! so a projection that has to end up above a limit cannot be built after the limit it points at.
//! What happens instead is that the two nodes trade places in the slots they already occupy: the
//! limit's slot takes the projection, pointing at the projection's old slot, and the projection's
//! slot takes the limit, pointing at what the projection used to read. Both of those point backwards,
//! whatever pointed at the limit now finds the projection, and no node is added.
//!
//! Trading the contents of a slot is only safe when nothing else is looking at it. The binder builds
//! a tree, since there are no common table expressions yet and a view is bound again at each
//! reference rather than shared, so this does not come up today. It is checked anyway rather than
//! assumed, because the first plan with a shared subtree in it would otherwise get a wrong answer
//! rather than a worse one.

use rudb_common::Result;
use rudb_plan::{Node, NodeRef, Plan};

use crate::pass::{Context, Pass, top_down};

/// Moves every limit below the projection above it.
#[derive(Debug, Clone, Copy)]
pub struct LimitPushdown;

impl Pass for LimitPushdown {
    fn name(&self) -> &'static str {
        "limit_pushdown"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        push(plan);
        Ok(())
    }
}

/// Moves every limit in `plan` as far below the projections above it as it goes.
///
/// Each limit is walked all the way down rather than one level per run, so a run of projections is
/// crossed once and the plan this leaves behind is the plan it would leave behind again.
pub fn push(plan: &mut Plan) {
    let shared = shared(plan);
    for node in top_down(plan) {
        let mut at = node;
        while let Some(below) = swap(plan, at, &shared) {
            at = below;
        }
    }
}

/// Trades the limit at `at` with the projection under it, and hands back the slot the limit moved to.
///
/// `None` when there is nothing to do, which is anything that is not a limit over a projection, and
/// a projection something other than this limit also points at.
fn swap(plan: &mut Plan, at: NodeRef, shared: &[NodeRef]) -> Option<NodeRef> {
    let Node::Limit { input, count, offset } = *plan.node(at) else {
        return None;
    };
    let Node::Project { input: under, index, exprs, names } = *plan.node(input) else {
        return None;
    };
    if shared.contains(&input) {
        return None;
    }
    *plan.node_mut(input) = Node::Limit { input: under, count, offset };
    *plan.node_mut(at) = Node::Project { input, index, exprs, names };
    Some(input)
}

/// The nodes more than one reachable node points at.
///
/// Empty on every plan the binder builds today. It is worked out once per run rather than asked for
/// per swap, because the answer does not change: a swap rewrites two slots and points them at nodes
/// that were already being pointed at.
fn shared(plan: &Plan) -> Vec<NodeRef> {
    let mut seen = Vec::new();
    let mut twice = Vec::new();
    for node in top_down(plan) {
        for child in plan.node(node).children().into_iter().flatten() {
            if seen.contains(&child) {
                twice.push(child);
            } else {
                seen.push(child);
            }
        }
    }
    twice
}

#[cfg(test)]
mod tests {
    use rudb_plan::{JoinKind, Node, Plan, Slice};

    use super::push;

    /// What the plan a text prints looks like once the pass has run over it.
    fn pushed(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        push(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    #[test]
    fn a_limit_over_a_projection_ends_up_under_it() {
        assert_eq!(
            pushed(concat!(
                "Limit 10 offset 0\n",
                "  Project #1 [#0.0::INTEGER AS a]\n",
                "    Get memory.main.t AS t #0 [a::INTEGER]\n",
            )),
            concat!(
                "Project #1 [#0.0::INTEGER AS a]\n",
                "  Limit 10 offset 0\n",
                "    Get memory.main.t AS t #0 [a::INTEGER]\n",
            )
        );
    }

    /// The offset comes along, because a projection produces one row per row and the fifth row out
    /// of it is the fifth row into it.
    #[test]
    fn the_offset_and_a_limit_of_everything_come_along_too() {
        assert_eq!(
            pushed(concat!(
                "Limit ALL offset 5\n",
                "  Project #1 [#0.0::INTEGER AS a]\n",
                "    Get memory.main.t AS t #0 [a::INTEGER]\n",
            )),
            concat!(
                "Project #1 [#0.0::INTEGER AS a]\n",
                "  Limit ALL offset 5\n",
                "    Get memory.main.t AS t #0 [a::INTEGER]\n",
            )
        );
    }

    /// A run of projections is crossed in one run, which is what makes the pass settle.
    #[test]
    fn a_limit_crosses_every_projection_above_the_one_it_started_over() {
        let text = concat!(
            "Limit 3 offset 0\n",
            "  Project #2 [#1.0::INTEGER AS a]\n",
            "    Project #1 [#0.0::INTEGER AS a]\n",
            "      Get memory.main.t AS t #0 [a::INTEGER]\n",
        );
        let once = pushed(text);
        assert_eq!(
            once,
            concat!(
                "Project #2 [#1.0::INTEGER AS a]\n",
                "  Project #1 [#0.0::INTEGER AS a]\n",
                "    Limit 3 offset 0\n",
                "      Get memory.main.t AS t #0 [a::INTEGER]\n",
            )
        );
        assert_eq!(pushed(&once), once);
    }

    /// Each of these produces fewer rows than it is given or produces them in an order the limit is
    /// choosing from, so a limit below one of them is a different query.
    #[test]
    fn a_limit_over_anything_that_is_not_a_projection_stays_where_it_is() {
        for below in [
            "  Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
            "  Sort [#0.0::INTEGER ASC NULLS LAST]\n",
            "  Distinct on=[]\n",
        ] {
            let text =
                format!("Limit 10 offset 0\n{below}    Get memory.main.t AS t #0 [a::INTEGER]\n");
            assert_eq!(pushed(&text), text);
        }
    }

    #[test]
    fn a_projection_with_no_limit_over_it_is_left_alone() {
        let text = concat!(
            "Project #1 [#0.0::INTEGER AS a]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
        );
        assert_eq!(pushed(text), text);
    }

    /// A projection two things point at cannot trade places with one of them, because the other one
    /// would then be reading a limit where it had been reading a projection. No plan the binder
    /// builds looks like this, so the shape is put together by hand.
    #[test]
    fn a_projection_something_else_is_also_reading_is_left_alone() {
        let mut plan = Plan::new();
        let leaf =
            plan.add_node(Node::Values { index: 0, columns: Slice::EMPTY, rows: Slice::EMPTY });
        let project = plan.add_node(Node::Project {
            input: leaf,
            index: 1,
            exprs: Slice::EMPTY,
            names: Slice::EMPTY,
        });
        let limit = plan.add_node(Node::Limit { input: project, count: Some(10), offset: 0 });
        let join = plan.add_node(Node::Join {
            left: limit,
            right: project,
            kind: JoinKind::Inner,
            conditions: Slice::EMPTY,
        });
        plan.set_root(join);
        let before = plan.to_string();
        push(&mut plan);
        plan.validate().expect("the plan is still well formed");
        assert_eq!(plan.to_string(), before);
    }
}

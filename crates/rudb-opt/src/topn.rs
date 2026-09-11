//! Turning a limit over a sort into a top N.
//!
//! A sort has to see every row before it can emit the first one, so it holds the whole input. A limit
//! above it then throws almost all of that away. The two together are one operator that holds only
//! the rows that could still come out, which on `ORDER BY x LIMIT 10` over a hundred million rows is
//! ten rows rather than a hundred million, and the same answer either way.
//!
//! Almost every ClickBench query ends in `ORDER BY ... LIMIT`, which is why this is worth a pass of
//! its own rather than something the executor notices. Measured on one million rows of the ClickBench
//! table, `SELECT "WatchID" FROM ... ORDER BY "EventTime" LIMIT 10` was 2.678 seconds before it and
//! the pinned binary answers the same query in 0.063.
//!
//! # What it refuses
//!
//! `LIMIT ALL` over a sort, which is a sort. There would be no bound to hold, so there is nothing to
//! fuse and the pair is left as it was.
//!
//! The offset comes along rather than staying above, because the rows that are skipped still have to
//! be found before there is anything to skip them from, so a top N with an offset holds `count +
//! offset` rows and hands the first `offset` of them to nobody.
//!
//! # Rewriting in place
//!
//! Every other pass here appends, because an expression may only refer to one behind it in the arena.
//! This one writes over the limit's slot, which is allowed for the same reason: the node that ends up
//! there points at the sort's input, which is behind the sort, which is behind the limit. Writing over
//! the slot is what keeps whatever pointed at the limit pointing at the node that replaced it, with
//! no rebuild of the path back to the root.

use rudb_common::Result;
use rudb_plan::{Node, Plan};

use crate::pass::{Context, Pass, top_down};

/// Fuses a limit over a sort into one operator.
#[derive(Debug, Clone, Copy)]
pub struct TopN;

impl Pass for TopN {
    fn name(&self) -> &'static str {
        "top_n"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        fuse(plan);
        Ok(())
    }
}

/// Rewrites every limit over a sort in `plan` into a top N.
pub fn fuse(plan: &mut Plan) {
    for node in top_down(plan) {
        let (input, count, offset) = match *plan.node(node) {
            Node::Limit { input, count: Some(count), offset } => (input, count, offset),
            _ => continue,
        };
        let (below, keys) = match *plan.node(input) {
            Node::Sort { input: below, keys } => (below, keys),
            _ => continue,
        };
        *plan.node_mut(node) = Node::TopN { input: below, keys, count, offset };
    }
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::fuse;

    /// What the plan a text prints looks like once the pass has run over it.
    fn fused(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        fuse(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    #[test]
    fn a_limit_over_a_sort_becomes_one_operator() {
        assert_eq!(
            fused(concat!(
                "Limit 10 offset 0\n",
                "  Sort [#0.1::INTEGER ASC NULLS LAST]\n",
                "    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )),
            concat!(
                "TopN 10 offset 0 [#0.1::INTEGER ASC NULLS LAST]\n",
                "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )
        );
    }

    #[test]
    fn the_offset_comes_along_with_the_count() {
        assert_eq!(
            fused(concat!(
                "Limit 5 offset 20\n",
                "  Sort [#0.0::INTEGER DESC NULLS FIRST, #0.1::INTEGER ASC NULLS LAST]\n",
                "    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )),
            concat!(
                "TopN 5 offset 20 [#0.0::INTEGER DESC NULLS FIRST, #0.1::INTEGER ASC NULLS LAST]\n",
                "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )
        );
    }

    #[test]
    fn a_limit_with_no_count_is_left_alone_because_there_is_no_bound_to_hold() {
        let text = concat!(
            "Limit ALL offset 4\n",
            "  Sort [#0.0::INTEGER ASC NULLS LAST]\n",
            "    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
        );
        assert_eq!(fused(text), text);
    }

    #[test]
    fn a_limit_over_anything_else_is_left_alone() {
        let text = concat!(
            "Limit 10 offset 0\n",
            "  Distinct on=[]\n",
            "    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
        );
        assert_eq!(fused(text), text);
    }

    #[test]
    fn a_sort_with_no_limit_over_it_is_still_a_sort() {
        let text = concat!(
            "Sort [#0.0::INTEGER ASC NULLS LAST]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
        );
        assert_eq!(fused(text), text);
    }

    #[test]
    fn running_it_twice_is_running_it_once() {
        let text = concat!(
            "Limit 3 offset 1\n",
            "  Sort [#0.0::INTEGER ASC NULLS LAST]\n",
            "    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
        );
        let once = fused(text);
        assert_eq!(fused(&once), once);
    }
}

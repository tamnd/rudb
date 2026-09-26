//! Dropping the sort keys that hold one value for every row.
//!
//! A key that is the same on every row puts no row ahead of any other, so a sort that has only keys
//! like that leaves the rows in whatever order they came, and one that has some of them sorts the
//! same way without them. Nobody writes `ORDER BY 1` meaning the number, but a plan arrives at one
//! all the same. ClickBench 33 groups on `WatchID, ClientIP` and orders by the count, and when
//! `WatchID` holds no value twice the aggregate becomes a projection with `COUNT(*)` written as the
//! literal 1, which leaves a sort over ten million rows on a column that says 1 on all of them.
//!
//! Taking the sort out leaves the limit that was over it with nothing between it and the scan but
//! projections, which limit pushdown carries down to the scan, and the scan reads ten rows instead
//! of all of them. That is why this runs in front of limit pushdown. Any ten rows are the answer,
//! because a sort of rows that tie may give them back in any order.
//!
//! A key is a constant when it is a literal, a cast of one, or a column that a projection below
//! wrote as one of those, found by following the column down through the operators that hand their
//! input's columns up unchanged. Anything else is left as a key, which at worst is a sort that did
//! not need to run.

use rudb_common::Result;
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan, SortKey};

use crate::pass::{Context, Pass};
use crate::walk;

/// Drops the sort keys that are the same on every row.
#[derive(Debug, Clone, Copy)]
pub struct ConstantOrder;

impl Pass for ConstantOrder {
    fn name(&self) -> &'static str {
        "constant_order"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        drop_all(plan);
        Ok(())
    }
}

/// Rewrites every sort in `plan` that has a key holding one value for every row.
pub fn drop_all(plan: &mut Plan) {
    let mut moved = false;
    let root = walk::restack(plan, plan.root(), &mut moved, &mut drop_keys);
    if moved {
        plan.set_root(root);
    }
}

/// What stands in for the sort at `at` once its constant keys are gone.
fn drop_keys(plan: &mut Plan, at: NodeRef) -> Option<NodeRef> {
    let Node::Sort { input, keys } = *plan.node(at) else { return None };
    let all = plan.sort_key_list(keys).to_vec();
    let kept: Vec<SortKey> =
        all.iter().filter(|key| !constant(plan, input, key.expr)).cloned().collect();
    if kept.len() == all.len() {
        return None;
    }
    if kept.is_empty() {
        return Some(input);
    }
    let keys = plan.add_sort_keys(&kept);
    let span = plan.node_span(at);
    Some(plan.add_node_at(Node::Sort { input, keys }, span))
}

/// Whether `expr`, read over the output of `below`, is the same value on every row.
fn constant(plan: &Plan, below: NodeRef, expr: ExprRef) -> bool {
    match *plan.expr(expr) {
        Expr::Constant(_) => true,
        Expr::Cast { input, .. } => constant(plan, below, input),
        Expr::Column(binding) => written(plan, below, binding),
        _ => false,
    }
}

/// Whether the column `binding` names is written as a constant by a projection under `at`.
fn written(plan: &Plan, at: NodeRef, binding: ColumnBinding) -> bool {
    match *plan.node(at) {
        Node::Project { input, index, exprs, .. } if index == binding.table => plan
            .expr_list(exprs)
            .get(binding.column as usize)
            .is_some_and(|&expr| constant(plan, input, expr)),
        Node::Filter { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::TopN { input, .. } => written(plan, input, binding),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::drop_all;

    fn dropped(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        drop_all(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    #[test]
    fn a_sort_on_a_column_a_projection_wrote_as_a_literal_is_no_sort() {
        assert_eq!(
            dropped(concat!(
                "Limit 10 offset 0\n",
                "  Sort [#1.1::BIGINT DESC NULLS LAST]\n",
                "    Project #1 [#0.0::INTEGER AS a, 1::BIGINT AS c]\n",
                "      Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )),
            concat!(
                "Limit 10 offset 0\n",
                "  Project #1 [#0.0::INTEGER AS a, 1::BIGINT AS c]\n",
                "    Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )
        );
    }

    #[test]
    fn only_the_constant_keys_go() {
        assert_eq!(
            dropped(concat!(
                "Limit 10 offset 0\n",
                "  Sort [#1.1::BIGINT DESC NULLS LAST, #1.0::INTEGER ASC NULLS LAST]\n",
                "    Project #1 [#0.0::INTEGER AS a, 1::BIGINT AS c]\n",
                "      Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )),
            concat!(
                "Limit 10 offset 0\n",
                "  Sort [#1.0::INTEGER ASC NULLS LAST]\n",
                "    Project #1 [#0.0::INTEGER AS a, 1::BIGINT AS c]\n",
                "      Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )
        );
    }

    #[test]
    fn a_sort_on_a_column_of_the_rows_stays() {
        let text = concat!(
            "Limit 10 offset 0\n",
            "  Sort [#1.0::INTEGER DESC NULLS LAST]\n",
            "    Project #1 [#0.0::INTEGER AS a, 1::BIGINT AS c]\n",
            "      Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
        );
        assert_eq!(dropped(text), text);
    }
}

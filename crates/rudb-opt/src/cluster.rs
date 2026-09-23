//! When a grouped aggregate's key is a column the table is stored in ascending order of, so a group
//! is finished as soon as the key moves past it.
//!
//! A hash aggregate holds every group until the last row has arrived, because the next row could
//! belong to any of them. Over rows sorted on the key that is not true. Once the key has moved on,
//! the group behind it can never see another row, and a group that can never see another row does
//! not need a bucket, a hash, a probe, or a place in the merge between threads. TPC-H's lineitem is
//! stored in order key order, so `GROUP BY l_orderkey` in q18 is a million and a half groups that
//! each close within a few rows of opening.
//!
//! The executor does the work, a chunk at a time. What it needs from here is the promise that the
//! key arrives in ascending order, and that promise has two halves. The first is the store's: the
//! binder marks the columns whose summary says the rows never go down and hold no null, which is
//! `Plan::mark_ascending`. The second is the plan's: nothing between the scan and the aggregate may
//! reorder or merge rows. A filter drops rows and keeps the order of the rest, and a projection
//! that carries the column through unchanged keeps it too. Anything else, a join, a sort, a union,
//! stops the walk, because what comes out of it is in whatever order it chose.
//!
//! # What it does not do
//!
//! It does not trust the promise with the answer. The executor checks every chunk it is handed for
//! being in ascending order before it closes a group out of it, and a chunk that is not goes through
//! the hash table the way every chunk did before. A stale summary is a slower query and not a wrong
//! one.
//!
//! It is one key column and not several. A group by on two columns where the first is the sorted one
//! closes the same way in principle, and the executor side of that is a separate piece of work.

use rudb_common::Result;
use rudb_common::rules::Rule;
use rudb_plan::{ColumnBinding, Expr, Node, NodeRef, Plan};

use crate::pass::{Context, Pass, top_down};

/// Marks every grouped aggregate whose one key arrives in ascending order.
///
/// A rudb name rather than a DuckDB one, for the reason [`crate::dense::AggregateDense`] gives.
/// `SET disabled_optimizers = 'aggregate_cluster'` turns the pass off, and `SET stats_closed_groups
/// = 'off'` turns off [`Rule::ClosedGroups`], which is the rule.
#[derive(Debug)]
pub struct AggregateCluster;

impl Pass for AggregateCluster {
    fn name(&self) -> &'static str {
        "aggregate_cluster"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if context.allows(Rule::ClosedGroups) {
            cluster(plan);
        }
        Ok(())
    }
}

/// Records every aggregate in `plan` whose key the walk down to its scan keeps in order.
fn cluster(plan: &mut Plan) {
    let mut found = Vec::new();
    for node in top_down(plan) {
        let Node::Aggregate { input, index, groups, .. } = *plan.node(node) else {
            continue;
        };
        let &[key] = plan.expr_list(groups) else { continue };
        let &Expr::Column(binding) = plan.expr(key) else { continue };
        if sorted(plan, input, binding, 16) {
            found.push(index);
        }
    }
    for index in found {
        plan.cluster(index);
    }
}

/// Whether `binding`, read off the output of `node`, is a stored column in ascending order.
///
/// The budget is against a malformed plan, the way it is in the estimator's walk.
fn sorted(plan: &Plan, node: NodeRef, binding: ColumnBinding, depth: u32) -> bool {
    let Some(depth) = depth.checked_sub(1) else { return false };
    match *plan.node(node) {
        Node::Filter { input, .. } => sorted(plan, input, binding, depth),
        Node::Project { input, index, exprs, .. } if index == binding.table => {
            let Some(&carried) = plan.expr_list(exprs).get(binding.column as usize) else {
                return false;
            };
            let &Expr::Column(carried) = plan.expr(carried) else { return false };
            sorted(plan, input, carried, depth)
        }
        Node::Get { index, columns, .. } if index == binding.table => plan
            .field_list(columns)
            .get(binding.column as usize)
            .is_some_and(|field| plan.ascending(index, &field.name)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::{AggregateCluster, Rule};
    use crate::pass::{Context, Pass};

    const SCAN: &str = "Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]";

    fn plan(text: &str) -> Plan {
        let mut plan = Plan::parse(text).expect("a plan that parses");
        plan.mark_ascending(0, "a");
        plan
    }

    fn run(plan: &mut Plan, context: &Context) {
        AggregateCluster.run(plan, context).expect("a pass that cannot fail");
    }

    #[test]
    fn a_key_the_table_is_sorted_on_is_clustered() {
        let mut plan =
            plan(&format!("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  {SCAN}\n"));
        run(&mut plan, &Context::new());
        assert!(plan.clustered(1));
    }

    #[test]
    fn a_key_the_table_is_not_sorted_on_is_left_alone() {
        let mut plan =
            plan(&format!("Aggregate #1 groups=[#0.1::INTEGER] aggregates=[]\n  {SCAN}\n"));
        run(&mut plan, &Context::new());
        assert!(!plan.clustered(1));
    }

    #[test]
    fn two_keys_are_left_alone() {
        let mut plan = plan(&format!(
            "Aggregate #1 groups=[#0.0::INTEGER, #0.1::INTEGER] aggregates=[]\n  {SCAN}\n"
        ));
        run(&mut plan, &Context::new());
        assert!(!plan.clustered(1));
    }

    #[test]
    fn a_filter_keeps_the_order() {
        let mut plan = plan(&format!(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  Filter (#0.1::INTEGER > 3::INTEGER)::BOOLEAN\n    {SCAN}\n"
        ));
        run(&mut plan, &Context::new());
        assert!(plan.clustered(1));
    }

    #[test]
    fn the_rule_s_own_setting_turns_it_off() {
        let mut plan =
            plan(&format!("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[]\n  {SCAN}\n"));
        let mut context = Context::new();
        let mut rules = rudb_common::rules::Rules::default();
        rules.set(Rule::ClosedGroups, false);
        context.govern(rules);
        run(&mut plan, &context);
        assert!(!plan.clustered(1));
    }
}

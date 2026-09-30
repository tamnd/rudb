//! Choosing the rows of a top N before the joins that only add columns to them.
//!
//! A join over a relationship with exactly one parent per child row neither drops a child row nor
//! repeats one, which is the certificate [`crate::eliminate`] deletes joins on. Where the join has
//! to stay because its parent columns are read, the same certificate says something else: every row
//! the join emits is one child row with its parent's columns put next to it. So when the top N above
//! the join orders by child columns alone, the rows it keeps are the joins of the child rows a top N
//! over the child would keep, and the parent columns of every other child row are fetched, decoded
//! and carried for nothing.
//!
//! TPC-H q10 is that shape twice. The revenue is summed per `o_custkey`, the sums are joined to
//! `customer` for the name, address, phone and comment, and the result to `nation` for its name, and
//! only then are the twenty largest sums kept. Before this pass the four text columns were decoded
//! for every customer with a returned item in the quarter, about thirty eight thousand of them at
//! scale factor one, to keep twenty. After it the twenty sums are chosen first and the joins see
//! twenty rows.
//!
//! # What it writes
//!
//! A second top N, holding `count + offset` rows with no offset, directly above the child side of
//! the lowest such join it can reach. The original stays where it was, because a hash join does not
//! keep the order of either input, so the rows still have to be put in order and the offset still
//! has to be skipped. That top N now sorts `count + offset` rows, which costs nothing.
//!
//! Rows that tie on the keys at the boundary may come out differently, since the lower top N picks
//! among them before the join rather than after it. SQL leaves that choice open, and the upper top N
//! would have made an arbitrary one too.
//!
//! # How far down it goes
//!
//! Through a projection, where each key is a column the projection forwards or computes, and the key
//! is rewritten as the projection's expression. Through an inner join whose one equality is a
//! verified relationship, with the keys all read from the child side and the parent side a bare
//! scan, which is the same test [`crate::eliminate`] makes. Nothing else: a filter above a join
//! drops rows after the choice, and an aggregate or a distinct changes which rows there are.
//!
//! A key the projection computes can go below it, but not below a second projection, because the
//! rewrite only substitutes a column for a column. That is a limit of this walk rather than of the
//! idea, and it is not one TPC-H needs lifted.
//!
//! # Where this sits in the sequence
//!
//! Right after [`crate::topn`], because what it moves is a top N and that pass is the one that makes
//! them. Before the build side choice, which then sees the small side it made and builds on it, and
//! before the link join rewrite, which would otherwise have replaced the join with a link read of
//! every child row. It is governed by the same rule as join elimination, because the licence is the
//! same certificate.

use rudb_common::Result;
use rudb_common::rules::Rule;
use rudb_plan::{Expr, JoinKind, Node, NodeRef, Plan, SortKey};

use crate::eliminate::{equated_pair, indices, verified};
use crate::pass::{Context, Pass};
use crate::walk;

/// Puts a copy of a top N below the joins that only add parent columns to the rows it keeps.
#[derive(Debug, Clone, Copy)]
pub struct TopNThroughLinks;

impl Pass for TopNThroughLinks {
    fn name(&self) -> &'static str {
        "top_n_through_links"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if context.links().is_empty() || !context.allows(Rule::JoinElimination) {
            return Ok(());
        }
        let mut changed = false;
        let root = plan.root();
        let rebuilt = walk::restack(plan, root, &mut changed, &mut |plan, at| {
            let Node::TopN { input, keys, count, offset } = *plan.node(at) else {
                return None;
            };
            let held = count.checked_add(offset)?;
            let keys = plan.sort_key_list(keys).to_vec();
            let Lowered::Crossed(lowered) = lower(plan, input, &keys, held, context) else {
                return None;
            };
            let mut node = plan.node(at).clone();
            walk::replace_children(&mut node, &[lowered]);
            let span = plan.node_span(at);
            Some(plan.add_node_at(node, span))
        });
        if changed {
            plan.set_root(rebuilt);
        }
        Ok(())
    }
}

/// What became of the subtree a top N was pushed into.
enum Lowered {
    /// It was rebuilt with the top N below at least one join, and this is its new root.
    Crossed(NodeRef),
    /// The top N is already there, put there by an earlier run of the sequence.
    Done,
    /// There was no join it could cross.
    Stuck,
}

/// The subtree under `at` with a top N of `held` rows on `keys` put below the joins it can cross.
fn lower(plan: &mut Plan, at: NodeRef, keys: &[SortKey], held: u64, context: &Context) -> Lowered {
    let rebuilt = match *plan.node(at) {
        Node::Project { input, index, exprs, .. } => {
            let Some(below) = forwarded(plan, keys, index, exprs) else {
                return Lowered::Stuck;
            };
            match lower(plan, input, &below, held, context) {
                Lowered::Crossed(lowered) => vec![lowered],
                other => return other,
            }
        }
        Node::Join { left, right, kind: JoinKind::Inner, conditions, .. } => {
            let Some(pair) = equated_pair(plan, conditions) else {
                return Lowered::Stuck;
            };
            let Some((child, parent)) =
                [(left, right), (right, left)].into_iter().find(|&(child, parent)| {
                    reads_only(plan, keys, child) && verified(plan, child, parent, pair, context)
                })
            else {
                return Lowered::Stuck;
            };
            let lowered = match lower(plan, child, keys, held, context) {
                Lowered::Crossed(lowered) => lowered,
                Lowered::Done => return Lowered::Done,
                Lowered::Stuck if matches!(plan.node(child), Node::TopN { .. }) => {
                    return Lowered::Done;
                }
                Lowered::Stuck => {
                    let keys = plan.add_sort_keys(keys);
                    let span = plan.node_span(child);
                    plan.add_node_at(
                        Node::TopN { input: child, keys, count: held, offset: 0 },
                        span,
                    )
                }
            };
            if child == left { vec![lowered, parent] } else { vec![parent, lowered] }
        }
        _ => return Lowered::Stuck,
    };
    let mut node = plan.node(at).clone();
    walk::replace_children(&mut node, &rebuilt);
    let span = plan.node_span(at);
    Lowered::Crossed(plan.add_node_at(node, span))
}

/// The keys written in terms of a projection's input, when each is a column the projection makes.
fn forwarded(
    plan: &Plan,
    keys: &[SortKey],
    index: u32,
    exprs: rudb_plan::Slice,
) -> Option<Vec<SortKey>> {
    let mut below = Vec::with_capacity(keys.len());
    for key in keys {
        let Expr::Column(binding) = *plan.expr(key.expr) else {
            return None;
        };
        if binding.table != index {
            return None;
        }
        let expr = *plan.expr_list(exprs).get(binding.column as usize)?;
        if walk::volatile(plan, expr) {
            return None;
        }
        below.push(SortKey { expr, ..*key });
    }
    Some(below)
}

/// Whether every column the keys read is one `side` produces.
fn reads_only(plan: &Plan, keys: &[SortKey], side: NodeRef) -> bool {
    let mut produced = Vec::new();
    indices(plan, side, &mut produced);
    let mut inside = true;
    for key in keys {
        walk::columns(plan, key.expr, &mut |binding| inside &= produced.contains(&binding.table));
    }
    inside
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::rules::{Rule, Rules};
    use rudb_plan::Plan;

    use super::TopNThroughLinks;
    use crate::link::Linked;
    use crate::pass::{Context, Pass};

    fn context(links: Vec<Linked>) -> Context {
        let mut context = Context::new();
        context.relate(Arc::new(links));
        context
    }

    fn verified() -> Vec<Linked> {
        vec![Linked::verified("orders", "o_custkey", "customer", "c_custkey")]
    }

    fn rewritten(text: &str, context: &Context) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        TopNThroughLinks.run(&mut plan, context).expect("the pass does not fail");
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    /// The largest orders with their customer's name, which is the q10 shape without the sums.
    const ORDERS: &str = concat!(
        "TopN 3 offset 2 [#2.0::BIGINT DESC NULLS LAST]\n",
        "  Project #2 [#0.2::BIGINT AS total, #1.1::VARCHAR AS name]\n",
        "    Join INNER on=[(#0.1::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
        "      Get memory.main.orders AS orders #0 [o_orderkey::BIGINT, o_custkey::BIGINT, o_total::BIGINT]\n",
        "      Get memory.main.customer AS customer #1 [c_custkey::BIGINT, c_name::VARCHAR]\n",
    );

    #[test]
    fn the_rows_are_chosen_before_the_join_that_names_them() {
        assert_eq!(
            rewritten(ORDERS, &context(verified())),
            concat!(
                "TopN 3 offset 2 [#2.0::BIGINT DESC NULLS LAST]\n",
                "  Project #2 [#0.2::BIGINT AS total, #1.1::VARCHAR AS name]\n",
                "    Join INNER on=[(#0.1::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "      TopN 5 offset 0 [#0.2::BIGINT DESC NULLS LAST]\n",
                "        Get memory.main.orders AS orders #0 [o_orderkey::BIGINT, o_custkey::BIGINT, o_total::BIGINT]\n",
                "      Get memory.main.customer AS customer #1 [c_custkey::BIGINT, c_name::VARCHAR]\n",
            )
        );
    }

    #[test]
    fn running_it_twice_is_running_it_once() {
        let context = context(verified());
        let once = rewritten(ORDERS, &context);
        assert_eq!(rewritten(&once, &context), once);
    }

    #[test]
    fn a_key_read_from_the_parent_keeps_the_top_n_above_the_join() {
        let text = ORDERS.replace("[#2.0::BIGINT DESC", "[#2.1::VARCHAR DESC");
        assert_eq!(rewritten(&text, &context(verified())), text);
    }

    #[test]
    fn a_relationship_that_is_not_exactly_one_licenses_nothing() {
        // A child row with no parent is dropped by the join, so the rows chosen below it could be
        // rows that never come out.
        let built = vec![Linked::built("orders", "o_custkey", "customer", "c_custkey")];
        assert_eq!(rewritten(ORDERS, &context(built)), ORDERS);
    }

    #[test]
    fn a_filtered_parent_is_not_the_table_the_certificate_is_about() {
        let text = concat!(
            "TopN 3 offset 0 [#2.0::BIGINT DESC NULLS LAST]\n",
            "  Project #2 [#0.2::BIGINT AS total, #1.1::VARCHAR AS name]\n",
            "    Join INNER on=[(#0.1::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "      Get memory.main.orders AS orders #0 [o_orderkey::BIGINT, o_custkey::BIGINT, o_total::BIGINT]\n",
            "      Filter (#1.0::BIGINT > 5::BIGINT)::BOOLEAN\n",
            "        Get memory.main.customer AS customer #1 [c_custkey::BIGINT, c_name::VARCHAR]\n",
        );
        assert_eq!(rewritten(text, &context(verified())), text);
    }

    #[test]
    fn a_filter_above_the_join_stops_the_walk() {
        let text = concat!(
            "TopN 3 offset 0 [#0.2::BIGINT DESC NULLS LAST]\n",
            "  Filter (#1.1::VARCHAR = 'x'::VARCHAR)::BOOLEAN\n",
            "    Join INNER on=[(#0.1::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "      Get memory.main.orders AS orders #0 [o_orderkey::BIGINT, o_custkey::BIGINT, o_total::BIGINT]\n",
            "      Get memory.main.customer AS customer #1 [c_custkey::BIGINT, c_name::VARCHAR]\n",
        );
        assert_eq!(rewritten(text, &context(verified())), text);
    }

    #[test]
    fn the_key_can_come_up_through_a_sum_per_customer() {
        // q10 and q18: the orders are summed per customer first, and a sum per `o_custkey` has
        // exactly one customer for the same reason an order does.
        let text = concat!(
            "TopN 20 offset 0 [#3.1::HUGEINT DESC NULLS LAST]\n",
            "  Join INNER on=[(#1.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.customer AS customer #1 [c_custkey::BIGINT, c_name::VARCHAR]\n",
            "    Aggregate #3 groups=[#0.1::BIGINT] aggregates=[sum(#0.2::BIGINT)::HUGEINT]\n",
            "      Get memory.main.orders AS orders #0 [o_orderkey::BIGINT, o_custkey::BIGINT, o_total::BIGINT]\n",
        );
        let done = rewritten(text, &context(verified()));
        assert!(
            done.contains(
                "    TopN 20 offset 0 [#3.1::HUGEINT DESC NULLS LAST]\n      Aggregate #3"
            ),
            "the twenty largest sums are chosen before the customers are joined to them:\n{done}"
        );
    }

    #[test]
    fn the_rule_turns_it_off() {
        let mut rules = Rules::new();
        rules.set(Rule::JoinElimination, false);
        let mut context = context(verified());
        context.govern(rules);
        assert_eq!(rewritten(ORDERS, &context), ORDERS);
    }
}

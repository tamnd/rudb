//! Filter pushdown, the half that stays on one input.
//!
//! A filter above an operator reads every row the operator produced. The same filter below it reads
//! every row the operator was going to be given, and the operator then does its work on fewer rows.
//! That is the whole rule, and `spec/09-optimizer.md` section 9.2 puts it first by value for the
//! same reason column pruning is first by size: the cheapest row is the one nothing above the scan
//! ever sees.
//!
//! #102 asks for this in two pull requests and this is the first. It is the framework and the
//! operators with one input. A join is the second, because a predicate that moves to the wrong side
//! of an outer join turns a row that should have been padded with nulls into a row that is not there
//! at all, and that belongs in a pull request whose diff is only that.
//!
//! # Where a predicate stops
//!
//! Each rule was read off the pinned binary with `EXPLAIN` rather than reasoned about, and the four
//! that matter are in #209 with the query that shows each one.
//!
//! Through a projection, with the predicate rewritten in terms of what the projection computes from,
//! so `WHERE y > 2` over `x + 1 AS y` arrives at the scan as `(x + 1) > 2`. Not when the rewrite
//! would copy a volatile call: `WHERE r > 0.5` over `random() AS r` would then call `random()` once
//! to decide and once to report, and the row that passed the filter is not the row that comes out.
//!
//! Through a group by, for a predicate over group keys only, which is the `HAVING` that is not about
//! an aggregate. A predicate over a group key cannot remove part of a group, only all of it, so the
//! groups that survive are the same groups and each of them saw the same rows.
//!
//! Through a sort and through a plain `DISTINCT`, unchanged, since neither of them renames anything
//! and the distinct rows that satisfy a predicate are the distinct rows of the ones that satisfy it.
//!
//! Not through a limit, where fewer rows going in means different rows coming out. Not through
//! `DISTINCT ON`, which is a limit of one per group wearing another keyword. Not through a set
//! operation, which lines its two sides up by position, so a predicate written against its output
//! has to be mapped onto each side before it can move, and that mapping is the same one column
//! pruning does not have yet either.
//!
//! # Taking the conjunction apart
//!
//! `WHERE a AND b` is two predicates. Splitting them is most of the value of the pass, because the
//! half that can reach the scan is usually not the half that cannot, and a filter that moves only
//! when all of it can move is a filter that stays put on every real query. What is left over is put
//! back together with `AND` wherever it stopped, so two filters that end up in the same place come
//! out as one node.
//!
//! # What it leaves behind
//!
//! A plan this has run over prints the same the second time, which is the property
//! [`crate::optimize_with`] asserts in a debug build. It is not the same arena: the pass takes every
//! filter apart and builds the one it ends with, rather than working out in advance that it had
//! nothing to do, so the old filter is left in the arena with nothing pointing at it. That costs a
//! few nodes on a plan and is the reason every pass walks from the root rather than over the arena.

use rudb_common::{LogicalType, Result};
use rudb_plan::{ColumnBinding, ConjunctionOp, Expr, ExprRef, Node, NodeRef, Plan, Slice};

use crate::fold::VOLATILE;
use crate::pass::{Context, Pass};
use crate::walk;

/// Moves every predicate as far down the plan as it can go.
#[derive(Debug, Clone, Copy)]
pub struct FilterPushdown;

impl Pass for FilterPushdown {
    fn name(&self) -> &'static str {
        "filter_pushdown"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        push(plan);
        Ok(())
    }
}

/// Moves every predicate in `plan` as far down as it can go.
pub fn push(plan: &mut Plan) {
    let root = node(plan, plan.root(), Vec::new());
    plan.set_root(root);
}

/// Rewrites the plan under `at` with `pending` applied somewhere at or below it.
///
/// Every predicate in `pending` is written against the columns `at` produces, and every one of them
/// lands: either an operator below takes it, or it is put back as a filter at the deepest point that
/// would have it. Nothing is dropped, so a rule that refuses to move something is slow rather than
/// wrong, which is the shape every rule here is written in.
///
/// The node that comes back is `at` itself when nothing under it moved, so a subtree the pass has
/// nothing to say about is left alone rather than copied.
fn node(plan: &mut Plan, at: NodeRef, pending: Vec<ExprRef>) -> NodeRef {
    match *plan.node(at) {
        // The filter disappears here and is rebuilt wherever its parts stop, which is what merges
        // two filters in a row into one and what lets half of an AND go further than the other half.
        Node::Filter { input, predicate } => {
            let mut parts = pending;
            split(plan, predicate, &mut parts);
            node(plan, input, parts)
        }

        Node::Project { input, index, exprs, names } => {
            let held = plan.expr_list(exprs).to_vec();
            let (down, stay) = partition(plan, pending, index, &held);
            let moved = down.into_iter().map(|part| substitute(plan, part, index, &held));
            let moved: Vec<ExprRef> = moved.collect();
            let rebuilt = node(plan, input, moved);
            let above = if rebuilt == input {
                at
            } else {
                plan.add_node(Node::Project { input: rebuilt, index, exprs, names })
            };
            filter(plan, above, stay)
        }

        // The output is the group expressions and then the aggregates, so a predicate over an
        // aggregate names a column past the end of the group list and `partition` refuses it on the
        // bounds check rather than on a rule of its own.
        Node::Aggregate { input, index, groups, aggregates } => {
            let keys = plan.expr_list(groups).to_vec();
            let (down, stay) = partition(plan, pending, index, &keys);
            let moved = down.into_iter().map(|part| substitute(plan, part, index, &keys));
            let moved: Vec<ExprRef> = moved.collect();
            let rebuilt = node(plan, input, moved);
            let above = if rebuilt == input {
                at
            } else {
                plan.add_node(Node::Aggregate { input: rebuilt, index, groups, aggregates })
            };
            filter(plan, above, stay)
        }

        // Neither of these introduces a table index, so a predicate written against what comes out
        // is already written against what goes in.
        Node::Sort { input, keys } => {
            let rebuilt = node(plan, input, pending);
            if rebuilt == input { at } else { plan.add_node(Node::Sort { input: rebuilt, keys }) }
        }
        Node::Distinct { input, on } => {
            let whole_row = plan.expr_list(on).is_empty();
            let (down, stay) =
                if whole_row { (pending, Vec::new()) } else { (Vec::new(), pending) };
            let rebuilt = node(plan, input, down);
            let above = if rebuilt == input {
                at
            } else {
                plan.add_node(Node::Distinct { input: rebuilt, on })
            };
            filter(plan, above, stay)
        }

        // Nothing goes through a limit and the recursion happens anyway, because a filter that is
        // already below the limit still has somewhere to go.
        Node::Limit { input, count, offset } => {
            let rebuilt = node(plan, input, Vec::new());
            let above = if rebuilt == input {
                at
            } else {
                plan.add_node(Node::Limit { input: rebuilt, count, offset })
            };
            filter(plan, above, pending)
        }

        // The second pull request. Both sides are still walked, so a filter written inside one side
        // of a join reaches that side's scan today.
        Node::Join { left, right, kind, conditions } => {
            let rebuilt_left = node(plan, left, Vec::new());
            let rebuilt_right = node(plan, right, Vec::new());
            let above = if rebuilt_left == left && rebuilt_right == right {
                at
            } else {
                plan.add_node(Node::Join {
                    left: rebuilt_left,
                    right: rebuilt_right,
                    kind,
                    conditions,
                })
            };
            filter(plan, above, pending)
        }
        Node::CrossProduct { left, right } => {
            let rebuilt_left = node(plan, left, Vec::new());
            let rebuilt_right = node(plan, right, Vec::new());
            let above = if rebuilt_left == left && rebuilt_right == right {
                at
            } else {
                plan.add_node(Node::CrossProduct { left: rebuilt_left, right: rebuilt_right })
            };
            filter(plan, above, pending)
        }
        Node::SetOp { left, right, kind, all, index } => {
            let rebuilt_left = node(plan, left, Vec::new());
            let rebuilt_right = node(plan, right, Vec::new());
            let above = if rebuilt_left == left && rebuilt_right == right {
                at
            } else {
                plan.add_node(Node::SetOp {
                    left: rebuilt_left,
                    right: rebuilt_right,
                    kind,
                    all,
                    index,
                })
            };
            filter(plan, above, pending)
        }

        // The bottom. A scan takes a predicate into its own filter list in E2 and cannot yet, so
        // what reaches here becomes a filter sitting directly on the scan.
        Node::Get { .. } | Node::Values { .. } | Node::TableFunction { .. } | Node::Dummy => {
            filter(plan, at, pending)
        }
    }
}

/// Puts `parts` back as one filter over `input`, or hands back `input` when there are none.
fn filter(plan: &mut Plan, input: NodeRef, parts: Vec<ExprRef>) -> NodeRef {
    let predicate = match parts.len() {
        0 => return input,
        1 => parts[0],
        _ => {
            let children = plan.add_expr_list(&parts);
            let conjunction = Expr::Conjunction { op: ConjunctionOp::And, children };
            plan.add_expr(conjunction, LogicalType::Boolean)
        }
    };
    plan.add_node(Node::Filter { input, predicate })
}

/// Adds every conjunct of `predicate` to `into`.
///
/// Only `AND`. An `OR` is one predicate however it is written, since a row that fails one side of it
/// may still be a row the query wants.
fn split(plan: &Plan, predicate: ExprRef, into: &mut Vec<ExprRef>) {
    if let Expr::Conjunction { op: ConjunctionOp::And, children } = *plan.expr(predicate) {
        for &child in plan.expr_list(children) {
            split(plan, child, into);
        }
    } else {
        into.push(predicate);
    }
}

/// Sorts `pending` into the parts that can be written in terms of `held` and the parts that cannot.
///
/// The first list is what [`substitute`] is allowed to be called with, and the check it does is what
/// makes that call total: every column the predicate reads binds to `index`, is inside `held`, and
/// names an expression that gives the same answer each time it is asked.
fn partition(
    plan: &Plan,
    pending: Vec<ExprRef>,
    index: u32,
    held: &[ExprRef],
) -> (Vec<ExprRef>, Vec<ExprRef>) {
    let mut down = Vec::new();
    let mut stay = Vec::new();
    for part in pending {
        if substitutable(plan, part, index, held) {
            down.push(part);
        } else {
            stay.push(part);
        }
    }
    (down, stay)
}

/// Whether every column `expr` reads can be replaced by what `held` computes it from.
fn substitutable(plan: &Plan, expr: ExprRef, index: u32, held: &[ExprRef]) -> bool {
    let mut answer = true;
    columns(plan, expr, &mut |binding| {
        answer &= binding.table == index;
        answer &= match held.get(binding.column as usize) {
            Some(&source) => !volatile(plan, source),
            None => false,
        };
    });
    answer
}

/// Rewrites every column of `index` in `expr` into what `held` computes it from.
///
/// The indexing is checked by [`substitutable`], which is the only thing that says an expression may
/// be handed to this.
fn substitute(plan: &mut Plan, expr: ExprRef, index: u32, held: &[ExprRef]) -> ExprRef {
    if let Expr::Column(binding) = *plan.expr(expr) {
        return if binding.table == index { held[binding.column as usize] } else { expr };
    }
    walk::rebuild(plan, expr, &mut |plan, child| substitute(plan, child, index, held))
}

/// Calls `found` for every column `expr` reads.
fn columns(plan: &Plan, expr: ExprRef, found: &mut impl FnMut(ColumnBinding)) {
    match *plan.expr(expr) {
        Expr::Column(binding) => found(binding),
        Expr::Constant(_) => {}
        Expr::Cast { input, .. } => columns(plan, input, found),
        Expr::Compare { left, right, .. } => {
            columns(plan, left, found);
            columns(plan, right, found);
        }
        Expr::Conjunction { children, .. } | Expr::Function { args: children, .. } => {
            for &child in plan.expr_list(children) {
                columns(plan, child, found);
            }
        }
        Expr::Aggregate { args, filter, .. } => {
            for &arg in plan.expr_list(args) {
                columns(plan, arg, found);
            }
            if let Some(inner) = filter {
                columns(plan, inner, found);
            }
        }
        Expr::Case { arms, otherwise } => {
            for arm in plan.arm_list(arms) {
                columns(plan, arm.when, found);
                columns(plan, arm.then, found);
            }
            if let Some(inner) = otherwise {
                columns(plan, inner, found);
            }
        }
    }
}

/// Whether asking for this expression twice can give two answers.
///
/// The list is [`VOLATILE`], which is the one folding refuses to fold and is read from the pinned
/// binary's `duckdb_functions()`. rudb answers to none of those names yet, so this is false for
/// everything today and is here so that the first one to land is refused by a pass that already knew
/// about it rather than copied by a pass that had never heard of it.
fn volatile(plan: &Plan, expr: ExprRef) -> bool {
    match *plan.expr(expr) {
        Expr::Column(_) | Expr::Constant(_) => false,
        Expr::Cast { input, .. } => volatile(plan, input),
        Expr::Compare { left, right, .. } => volatile(plan, left) || volatile(plan, right),
        Expr::Conjunction { children, .. } => any_volatile(plan, children),
        Expr::Function { name, args } => {
            VOLATILE.contains(&plan.string(name)) || any_volatile(plan, args)
        }
        Expr::Aggregate { args, filter, .. } => {
            any_volatile(plan, args) || filter.is_some_and(|inner| volatile(plan, inner))
        }
        Expr::Case { arms, otherwise } => {
            plan.arm_list(arms)
                .iter()
                .any(|arm| volatile(plan, arm.when) || volatile(plan, arm.then))
                || otherwise.is_some_and(|inner| volatile(plan, inner))
        }
    }
}

/// Whether any expression in the run is volatile.
fn any_volatile(plan: &Plan, slice: Slice) -> bool {
    plan.expr_list(slice).iter().any(|&expr| volatile(plan, expr))
}

#[cfg(test)]
mod tests {
    use super::FilterPushdown;
    use crate::pass::{Context, Pass};
    use rudb_plan::Plan;

    /// The plan a text prints as after the pass, which is what every assertion here reads.
    fn pushed(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        FilterPushdown
            .run(&mut plan, &Context::new())
            .unwrap_or_else(|error| panic!("{text} did not push: {error}"));
        plan.validate().unwrap_or_else(|error| panic!("{text} pushed to a bad plan: {error}"));
        plan.to_string()
    }

    #[test]
    fn a_filter_over_a_projection_lands_under_it() {
        let before = "\
Filter (#1.0::INTEGER > 1::INTEGER)::BOOLEAN
  Project #1 [#0.0::INTEGER AS x]
    Get memory.main.t AS t #0 [a::INTEGER]
";
        let after = "\
Project #1 [#0.0::INTEGER AS x]
  Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
    Get memory.main.t AS t #0 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_computed_column_is_rewritten_into_what_it_is_computed_from() {
        // The measurement in #209: the binary ends this one in `Filters: (x + 1) > 2`.
        let before = "\
Filter (#1.0::INTEGER > 2::INTEGER)::BOOLEAN
  Project #1 [\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS y]
    Get memory.main.t AS t #0 [a::INTEGER]
";
        let after = "\
Project #1 [\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS y]
  Filter (\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER > 2::INTEGER)::BOOLEAN
    Get memory.main.t AS t #0 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_projection_that_is_not_the_same_twice_keeps_its_filter_above_it() {
        // Two calls to random() are two numbers, so the row that passed the filter would not be the
        // row that came out.
        let text = "\
Filter (#1.0::DOUBLE > 0.5::DOUBLE)::BOOLEAN
  Project #1 [random()::DOUBLE AS r]
    Get memory.main.t AS t #0 [a::INTEGER]
";
        assert_eq!(pushed(text), text);
    }

    #[test]
    fn a_predicate_over_a_group_key_goes_under_the_grouping() {
        let before = "\
Filter (#1.0::INTEGER > 1::INTEGER)::BOOLEAN
  Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]
    Get memory.main.t AS t #0 [a::INTEGER]
";
        let after = "\
Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]
  Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
    Get memory.main.t AS t #0 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_conjunction_is_split_so_the_half_about_a_group_key_moves_on_its_own() {
        let before = "\
Filter ((#1.0::INTEGER > 1::INTEGER)::BOOLEAN AND (#1.1::BIGINT > 2::BIGINT)::BOOLEAN)::BOOLEAN
  Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]
    Get memory.main.t AS t #0 [a::INTEGER]
";
        let after = "\
Filter (#1.1::BIGINT > 2::BIGINT)::BOOLEAN
  Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]
    Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
      Get memory.main.t AS t #0 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_filter_crosses_a_sort() {
        let before = "\
Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
  Sort [#0.0::INTEGER ASC NULLS LAST]
    Get memory.main.t AS t #0 [a::INTEGER]
";
        let after = "\
Sort [#0.0::INTEGER ASC NULLS LAST]
  Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
    Get memory.main.t AS t #0 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_filter_does_not_cross_a_limit() {
        // Fewer rows going in is different rows coming out.
        let text = "\
Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
  Limit 2 offset 0
    Get memory.main.t AS t #0 [a::INTEGER]
";
        assert_eq!(pushed(text), text);
    }

    #[test]
    fn a_filter_crosses_a_plain_distinct_and_not_a_distinct_on() {
        let before = "\
Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
  Distinct on=[]
    Get memory.main.t AS t #0 [a::INTEGER]
";
        let after = "\
Distinct on=[]
  Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
    Get memory.main.t AS t #0 [a::INTEGER]
";
        assert_eq!(pushed(before), after);

        let on = "\
Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
  Distinct on=[#0.0::INTEGER]
    Get memory.main.t AS t #0 [a::INTEGER]
";
        assert_eq!(pushed(on), on);
    }

    #[test]
    fn two_filters_in_a_row_become_one() {
        let before = "\
Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
  Filter (#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN
    Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]
";
        let after = "\
Filter ((#0.0::INTEGER > 1::INTEGER)::BOOLEAN AND (#0.1::VARCHAR = 'a'::VARCHAR)::BOOLEAN)::BOOLEAN
  Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_filter_above_a_join_stays_there_until_the_second_pull_request() {
        let text = "\
Filter (#0.0::INTEGER = 1::INTEGER)::BOOLEAN
  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(text), text);
    }

    #[test]
    fn a_filter_inside_one_side_of_a_join_still_moves_down_that_side() {
        let before = "\
Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
  Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
    Sort [#0.0::INTEGER ASC NULLS LAST]
      Get memory.main.t AS a #0 [a::INTEGER]
  Get memory.main.t AS b #1 [a::INTEGER]
";
        let after = "\
Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
  Sort [#0.0::INTEGER ASC NULLS LAST]
    Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN
      Get memory.main.t AS a #0 [a::INTEGER]
  Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_plan_it_has_already_moved_is_left_where_it_put_it() {
        let before = "\
Filter (#1.0::INTEGER > 1::INTEGER)::BOOLEAN
  Project #1 [#0.0::INTEGER AS x]
    Get memory.main.t AS t #0 [a::INTEGER]
";
        let once = pushed(before);
        assert_eq!(pushed(&once), once);
    }
}

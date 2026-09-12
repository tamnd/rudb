//! Filter pushdown.
//!
//! A filter above an operator reads every row the operator produced. The same filter below it reads
//! every row the operator was going to be given, and the operator then does its work on fewer rows.
//! That is the whole rule, and `spec/09-optimizer.md` section 9.2 puts it first by value for the
//! same reason column pruning is first by size: the cheapest row is the one nothing above the scan
//! ever sees.
//!
//! #102 asks for this in two pull requests. The first was the framework and the operators with one
//! input, and this is the second, which is the joins.
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
//! # Into a join
//!
//! Into the side that produces everything the predicate reads, when the join hands that side's rows
//! on as they are. An inner join keeps both sides, an outer join keeps only the side it is named
//! after, a full outer join keeps neither, and a positional join keeps neither for a reason that has
//! nothing to do with nulls: it pairs rows up by their position, so taking a row out of one side
//! renumbers every row after it. `kept` is that table and is the whole of the rule.
//!
//! What is left over reads both sides. Over an inner join it becomes a condition, because a
//! condition and a filter above the join mean the same thing there and the condition is the one that
//! runs while the pairs are being built. Over anything else it stays where it was. That one measures
//! as a wash today, within the noise on 2000 rows against 50000, and it is kept because it is the
//! plan a hash join wants and because a condition means the join stops materializing pairs that the
//! filter above it was going to throw away.
//!
//! What a cross product does not do is become an inner join. It is the same rewrite, it is the one
//! every textbook has and the one #102 wants, and it is a pessimization here today. Ten thousand
//! rows against fifty thousand with `a.x = b.x` over them takes 1.25 seconds as a cross product with
//! a filter above it and 10.7 seconds as a join with a condition, measured on server2. The reason is
//! in `crates/rudb-exec/src/join.rs`: every join is a nested loop that pairs one left row against a
//! chunk of the right side at a time, where the cross product hands whole chunks on and lets the
//! filter run over them. Writing the better plan for the slower operator is how a pass makes a query
//! slower while looking correct, so this waits for #211.
//!
//! Before any of that, the join is asked what kind of join it really is. An outer join exists to
//! produce rows padded with nulls, so a predicate above it that cannot be true of a padded row turns
//! it into a join that never made them. `crate::nulls` is that question and it runs first, because
//! the answer decides which sides `kept` says may be pushed into: a left join that becomes an inner
//! join in the same visit takes the predicate that converted it straight into the side it could not
//! have been pushed into a moment earlier.
//!
//! # Predicates nobody wrote
//!
//! A predicate can only be pushed into a side that produces what it reads, so a query that restricts
//! one table and joins on a key gives the other table nothing at all. `crate::transitive` is what
//! writes the missing predicate down: an equality between two columns means anything said about one
//! of them is said about the other. It runs where the predicates are, which is at a filter for the
//! equalities somebody wrote in a `WHERE` and at a join for the ones in an `ON`, and everything it
//! produces goes through the rules here like any other predicate.
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
use rudb_plan::{ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan};

use crate::pass::{Context, Pass};
use crate::tables::{TableSet, Tables, produced};
use crate::{nulls, transitive, walk};

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
    let mut tables = Tables::new();
    let root = node(plan, plan.root(), Vec::new(), &mut tables);
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
fn node(plan: &mut Plan, at: NodeRef, pending: Vec<ExprRef>, tables: &mut Tables) -> NodeRef {
    match *plan.node(at) {
        // The filter disappears here and is rebuilt wherever its parts stop, which is what merges
        // two filters in a row into one and what lets half of an AND go further than the other half.
        Node::Filter { input, predicate } => {
            let mut parts = pending;
            split(plan, predicate, &mut parts);
            transitive::within(plan, &mut parts);
            node(plan, input, parts, tables)
        }

        Node::Project { input, index, exprs, names } => {
            let held = plan.expr_list(exprs).to_vec();
            let (down, stay) = partition(plan, pending, index, &held);
            let moved = down.into_iter().map(|part| substitute(plan, part, index, &held));
            let moved: Vec<ExprRef> = moved.collect();
            let rebuilt = node(plan, input, moved, tables);
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
            let rebuilt = node(plan, input, moved, tables);
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
            let rebuilt = node(plan, input, pending, tables);
            if rebuilt == input { at } else { plan.add_node(Node::Sort { input: rebuilt, keys }) }
        }
        Node::Distinct { input, on } => {
            let whole_row = plan.expr_list(on).is_empty();
            let (down, stay) =
                if whole_row { (pending, Vec::new()) } else { (Vec::new(), pending) };
            let rebuilt = node(plan, input, down, tables);
            let above = if rebuilt == input {
                at
            } else {
                plan.add_node(Node::Distinct { input: rebuilt, on })
            };
            filter(plan, above, stay)
        }

        // Nothing goes through a limit and the recursion happens anyway, because a filter that is
        // already below the limit still has somewhere to go. A top N is a limit with a sort inside
        // it, so it holds the same line.
        Node::Limit { input, count, offset } => {
            let rebuilt = node(plan, input, Vec::new(), tables);
            let above = if rebuilt == input {
                at
            } else {
                plan.add_node(Node::Limit { input: rebuilt, count, offset })
            };
            filter(plan, above, pending)
        }
        Node::TopN { input, keys, count, offset } => {
            let rebuilt = node(plan, input, Vec::new(), tables);
            let above = if rebuilt == input {
                at
            } else {
                plan.add_node(Node::TopN { input: rebuilt, keys, count, offset })
            };
            filter(plan, above, pending)
        }

        // A predicate goes into a side when that side produces everything the predicate reads and
        // the join hands that side's rows on as they are. What is left over reads both sides, and
        // for an inner join a condition and a filter above it mean the same thing, so it becomes a
        // condition and runs while the pairs are being built rather than after.
        Node::Join { left, right, kind: written, conditions } => {
            let below = (produced(plan, left), produced(plan, right));
            let kind = nulls::narrow(plan, written, &pending, (&below.0, &below.1));
            let held = plan.expr_list(conditions).to_vec();
            let (extra_left, extra_right) =
                transitive::across(plan, tables, kind, &held, &pending, (&below.0, &below.1));
            let (mut to_left, mut to_right, over) =
                sides(plan, tables, pending, &below, kept(kind));
            to_left.extend(extra_left);
            to_right.extend(extra_right);
            let (added, stay) =
                if kind == JoinKind::Inner { (over, Vec::new()) } else { (Vec::new(), over) };
            let rebuilt_left = node(plan, left, to_left, tables);
            let rebuilt_right = node(plan, right, to_right, tables);
            let rebuilt_conditions = if added.is_empty() {
                conditions
            } else {
                let all: Vec<ExprRef> =
                    plan.expr_list(conditions).to_vec().into_iter().chain(added).collect();
                plan.add_expr_list(&all)
            };
            let above = if rebuilt_left == left
                && rebuilt_right == right
                && rebuilt_conditions == conditions
                && kind == written
            {
                at
            } else {
                plan.add_node(Node::Join {
                    left: rebuilt_left,
                    right: rebuilt_right,
                    kind,
                    conditions: rebuilt_conditions,
                })
            };
            filter(plan, above, stay)
        }

        // Both sides of a cross product are kept as they are, so a predicate over one side goes
        // into it. A predicate over both stays above, for the measured reason in this file's
        // opening, and #211 is where that changes.
        Node::CrossProduct { left, right } => {
            let below = (produced(plan, left), produced(plan, right));
            let (to_left, to_right, over) = sides(plan, tables, pending, &below, (true, true));
            let rebuilt_left = node(plan, left, to_left, tables);
            let rebuilt_right = node(plan, right, to_right, tables);
            let above = if rebuilt_left == left && rebuilt_right == right {
                at
            } else {
                plan.add_node(Node::CrossProduct { left: rebuilt_left, right: rebuilt_right })
            };
            filter(plan, above, over)
        }

        Node::SetOp { left, right, kind, all, index } => {
            let rebuilt_left = node(plan, left, Vec::new(), tables);
            let rebuilt_right = node(plan, right, Vec::new(), tables);
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

/// Which sides of a join hand their rows on as they are.
///
/// The side a predicate may be pushed into. A row of a kept side comes out of the join with its own
/// values in it, once per match or once in total, so a predicate over that side's columns answers
/// the same before the join as after it. A row of the other side may come out padded with nulls it
/// did not have going in, and a predicate that saw the row before the padding is a predicate that
/// saw a different row.
///
/// A positional join keeps neither, which is the one entry here that is not about nulls. It pairs
/// the nth row of one side with the nth row of the other, so removing a row from either side
/// renumbers everything after it and pairs up rows that were never meant to meet.
pub(crate) fn kept(kind: JoinKind) -> (bool, bool) {
    match kind {
        JoinKind::Inner => (true, true),
        JoinKind::Left | JoinKind::Semi | JoinKind::Anti | JoinKind::Single => (true, false),
        JoinKind::Right => (false, true),
        JoinKind::Full | JoinKind::Positional => (false, false),
    }
}

/// Sorts `pending` into what goes into the left side, what goes into the right, and what is left.
///
/// A predicate goes into a side when that side is kept and produces every table the predicate reads.
/// A predicate that reads no table at all is a subset of either side and goes left, which is the
/// same answer wherever it runs.
fn sides(
    plan: &Plan,
    tables: &mut Tables,
    pending: Vec<ExprRef>,
    below: &(TableSet, TableSet),
    kept: (bool, bool),
) -> (Vec<ExprRef>, Vec<ExprRef>, Vec<ExprRef>) {
    let (below_left, below_right) = (&below.0, &below.1);
    let mut to_left = Vec::new();
    let mut to_right = Vec::new();
    let mut over = Vec::new();
    for part in pending {
        let read = tables.of(plan, part);
        if kept.0 && read.is_subset_of(below_left) {
            to_left.push(part);
        } else if kept.1 && read.is_subset_of(below_right) {
            to_right.push(part);
        } else {
            over.push(part);
        }
    }
    (to_left, to_right, over)
}

/// Puts `parts` back as one filter over `input`, or hands back `input` when there are none.
///
/// A conjunct that is a true constant is not put back, because it keeps every row and the only thing
/// it would do is be evaluated once per row to say so. `WHERE true` therefore leaves no filter at
/// all, and `WHERE a AND true` leaves the half that means something. That is where the binary drops
/// it too: the filter is taken apart and rebuilt on the way down, so the conjunct that decides
/// nothing simply never goes back in.
fn filter(plan: &mut Plan, input: NodeRef, parts: Vec<ExprRef>) -> NodeRef {
    let parts: Vec<ExprRef> = parts.into_iter().filter(|&part| !always(plan, part)).collect();
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

/// Whether a predicate keeps every row it is given.
///
/// Only the constant. `a OR NOT a` keeps every row too and is not a constant, and working that out
/// is the satisfiability question that `crate::nulls` and the binary's filter combiner ask, which is
/// a different pass from this one.
fn always(plan: &Plan, predicate: ExprRef) -> bool {
    let Expr::Constant(value) = *plan.expr(predicate) else {
        return false;
    };
    plan.value(value).as_bool() == Some(true)
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
///
/// A source has to be two things. Not volatile, because the substitution writes it down where the
/// predicate reads it and leaves it where it was, so a volatile one is called twice and the row
/// that passed the test is not the row that comes out. And elementwise, because the predicate ends
/// up below the operator that produced the column and an expression reading more than its own row
/// answers differently there. Nothing the binder builds can fail the second test today, since an
/// aggregate is a node and not a projection expression, and it is asked rather than assumed.
fn substitutable(plan: &Plan, expr: ExprRef, index: u32, held: &[ExprRef]) -> bool {
    let mut answer = true;
    walk::columns(plan, expr, &mut |binding| {
        answer &= binding.table == index;
        answer &= match held.get(binding.column as usize) {
            Some(&source) => !walk::volatile(plan, source) && walk::elementwise(plan, source),
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
    fn a_predicate_over_one_side_of_an_inner_join_goes_into_that_side() {
        let before = "\
Filter ((#0.0::INTEGER = 1::INTEGER)::BOOLEAN AND (#1.0::INTEGER = 2::INTEGER)::BOOLEAN)::BOOLEAN
  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        // Each side takes the predicate written against it and the copy of the other one that the
        // join's own equality implies, which is `crate::transitive`.
        let after = "\
Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
  Filter ((#0.0::INTEGER = 1::INTEGER)::BOOLEAN AND (#0.0::INTEGER = 2::INTEGER)::BOOLEAN)::BOOLEAN
    Get memory.main.t AS a #0 [a::INTEGER]
  Filter ((#1.0::INTEGER = 2::INTEGER)::BOOLEAN AND (#1.0::INTEGER = 1::INTEGER)::BOOLEAN)::BOOLEAN
    Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_predicate_over_both_sides_of_an_inner_join_becomes_a_condition_of_it() {
        let before = "\
Filter (#0.1::INTEGER = #1.1::INTEGER)::BOOLEAN
  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER, b::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER, b::INTEGER]
";
        let after = "\
Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN, (#0.1::INTEGER = #1.1::INTEGER)::BOOLEAN]
  Get memory.main.t AS a #0 [a::INTEGER, b::INTEGER]
  Get memory.main.t AS b #1 [a::INTEGER, b::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn only_the_kept_side_of_an_outer_join_takes_a_predicate() {
        // The left side of a LEFT join comes out as it went in. The right side comes out padded with
        // nulls, and a predicate that ran before the padding saw a different row. The predicate over
        // `b` here is `b.a IS NULL`, which is the one shape that wants the padded rows and so the
        // one that leaves the join a left join, since anything else over that side would make it an
        // inner join through `crate::nulls`.
        let before = "\
Filter ((#0.0::INTEGER = 1::INTEGER)::BOOLEAN AND (#1.0::INTEGER IS NOT DISTINCT FROM NULL::\"NULL\")::BOOLEAN)::BOOLEAN
  Join LEFT on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        // The predicate over `b` stays above the join and the one over `a` goes into `a`. The third
        // filter is the copy of the second that the join's equality implies, which may go into `b`
        // even though a predicate may not, for the reason `crate::transitive::droppable` gives.
        let after = "\
Filter (#1.0::INTEGER IS NOT DISTINCT FROM NULL::\"NULL\")::BOOLEAN
  Join LEFT on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Filter (#0.0::INTEGER = 1::INTEGER)::BOOLEAN
      Get memory.main.t AS a #0 [a::INTEGER]
    Filter (#1.0::INTEGER = 1::INTEGER)::BOOLEAN
      Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_predicate_over_both_sides_of_an_outer_join_stays_above_it() {
        // An outer join's condition decides which rows are padded, so moving a filter into it would
        // pad the rows the filter refused instead of dropping them. `IS NOT DISTINCT FROM` rather
        // than `=`, because an equality over the padded side is null over a padded row and would
        // make this an inner join through `crate::nulls`, where this one is true of two nulls.
        let text = "\
Filter (#0.0::INTEGER IS NOT DISTINCT FROM #1.0::INTEGER)::BOOLEAN
  Join LEFT on=[(#0.1::INTEGER = #1.1::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER, b::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER, b::INTEGER]
";
        assert_eq!(pushed(text), text);
    }

    #[test]
    fn a_full_outer_join_takes_nothing_and_neither_does_a_positional_one() {
        let full = "\
Filter (#0.0::INTEGER IS NOT DISTINCT FROM NULL::\"NULL\")::BOOLEAN
  Join FULL on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(full), full);

        // Not about nulls. A positional join pairs the nth row with the nth row, so taking a row out
        // of either side pairs up rows that were never meant to meet.
        let positional = "\
Filter (#0.0::INTEGER = 1::INTEGER)::BOOLEAN
  Join POSITIONAL on=[]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(positional), positional);
    }

    #[test]
    fn a_left_join_whose_padded_rows_are_all_filtered_out_is_an_inner_join() {
        // `SELECT * FROM a LEFT JOIN b ON a.a = b.a WHERE b.a = 2`. Every row the left join makes
        // that an inner join would not has a null `b.a` in it, and the predicate is null over every
        // one of them, so the query asked for an inner join in a roundabout way.
        let before = "\
Filter (#1.0::INTEGER = 2::INTEGER)::BOOLEAN
  Join LEFT on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        // The predicate then goes into the side it could not have been pushed into a moment before,
        // and `crate::transitive` writes the copy of it that the join's equality implies. Both of
        // those follow from the kind, which is why the kind is decided first.
        let after = "\
Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
  Filter (#0.0::INTEGER = 2::INTEGER)::BOOLEAN
    Get memory.main.t AS a #0 [a::INTEGER]
  Filter (#1.0::INTEGER = 2::INTEGER)::BOOLEAN
    Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn is_not_null_over_the_padded_side_is_the_same_rewrite() {
        // The way people write it when they mean it, and the reason `crate::nulls` tracks whether a
        // value is known to be there and not only whether it is known to be null.
        let before = "\
Filter (#1.0::INTEGER IS DISTINCT FROM NULL::\"NULL\")::BOOLEAN
  Join LEFT on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        // Once it is an inner join, `crate::transitive` says the same thing about `a.a`, since the
        // join's equality makes a null on one side a null on the other.
        let after = "\
Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
  Filter (#0.0::INTEGER IS DISTINCT FROM NULL::\"NULL\")::BOOLEAN
    Get memory.main.t AS a #0 [a::INTEGER]
  Filter (#1.0::INTEGER IS DISTINCT FROM NULL::\"NULL\")::BOOLEAN
    Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_full_outer_join_becomes_the_one_sided_join_the_predicate_left_of_it() {
        // Rejecting on the left takes away the rows that came from an unmatched right row, and what
        // is left is every left row with its match or with nulls, which is a left join.
        let before = "\
Filter (#0.0::INTEGER = 1::INTEGER)::BOOLEAN
  Join FULL on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        let after = "\
Join LEFT on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
  Filter (#0.0::INTEGER = 1::INTEGER)::BOOLEAN
    Get memory.main.t AS a #0 [a::INTEGER]
  Filter (#1.0::INTEGER = 1::INTEGER)::BOOLEAN
    Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_predicate_over_one_side_of_a_cross_product_goes_into_that_side() {
        let before = "\
Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
  CrossProduct
    Get memory.main.t AS a #0 [a::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER]
";
        let after = "\
CrossProduct
  Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
    Get memory.main.t AS a #0 [a::INTEGER]
  Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_cross_product_does_not_become_a_join_while_the_join_is_the_slower_operator() {
        // The rewrite every textbook has, kept out until #211, with the measurement in this file's
        // opening. The half of the predicate that reads one side still goes into that side.
        let before = "\
Filter ((#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN AND (#0.1::INTEGER > 5::INTEGER)::BOOLEAN)::BOOLEAN
  CrossProduct
    Get memory.main.t AS a #0 [a::INTEGER, b::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER, b::INTEGER]
";
        let after = "\
Filter (#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN
  CrossProduct
    Filter (#0.1::INTEGER > 5::INTEGER)::BOOLEAN
      Get memory.main.t AS a #0 [a::INTEGER, b::INTEGER]
    Get memory.main.t AS b #1 [a::INTEGER, b::INTEGER]
";
        assert_eq!(pushed(before), after);
    }

    #[test]
    fn a_predicate_over_a_projection_above_a_join_reaches_the_side_it_reads() {
        // Two rewrites in one walk. The projection puts the predicate back in terms of the scans,
        // and only then is it a predicate one side of the join produces everything for.
        let before = "\
Filter (#2.0::INTEGER > 5::INTEGER)::BOOLEAN
  Project #2 [#1.0::INTEGER AS x]
    Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
      Get memory.main.t AS a #0 [a::INTEGER]
      Get memory.main.t AS b #1 [a::INTEGER]
";
        let after = "\
Project #2 [#1.0::INTEGER AS x]
  Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]
    Filter (#0.0::INTEGER > 5::INTEGER)::BOOLEAN
      Get memory.main.t AS a #0 [a::INTEGER]
    Filter (#1.0::INTEGER > 5::INTEGER)::BOOLEAN
      Get memory.main.t AS b #1 [a::INTEGER]
";
        assert_eq!(pushed(before), after);
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

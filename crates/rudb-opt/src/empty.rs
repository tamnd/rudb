//! Replacing a subtree that cannot produce a row with a relation that produces none.
//!
//! `WHERE false` is the query somebody writes to ask for nothing, and a plan that still has a scan
//! under it answers nothing after reading the whole table. On ClickBench that is a hundred million
//! rows off disk to produce an empty result, so this is a speed matter, but it is a correctness one
//! first: `spec/09-optimizer.md` asks that a query with an unsatisfiable predicate not touch the
//! storage it was written against, and a scan that runs is a scan that can report an error about a
//! file the query was never going to read from.
//!
//! # What counts as empty
//!
//! A filter whose predicate is a false or a null constant, since `WHERE` keeps the rows where the
//! predicate is true and neither of those ever is. A limit of zero rows, and the top N it fuses
//! into. A `VALUES` with no rows. Anything above one of those that passes its input through, which
//! is a filter, a sort, a limit, a top N, a `DISTINCT` and a projection.
//!
//! The pullup stops at a group by, which is the operator this pass exists to be careful about. An
//! ungrouped aggregate over no rows produces one row and not none, so `SELECT count(*) FROM t WHERE
//! false` is `0` rather than an empty answer, and a pass that treated the aggregate as empty because
//! its input was would return the wrong number of rows. The empty relation is put under the
//! aggregate and the aggregate stays.
//!
//! It also stops at a join, a cross product and a set operation, for a reason that is about spelling
//! rather than about semantics. An empty relation here is a `Node::Values`, which binds its columns
//! to one table index, and those three produce columns bound to two of them or to an index of their
//! own that is not either side's. Replacing one would mean rewriting every binding above it, so an
//! empty side of a join is left as an empty side of a join, which the executor already handles by
//! finding no rows to pair with.
//!
//! # Why it is a pullup rather than a pushdown
//!
//! The walk is from the root, and the highest node that cannot produce a row is the one replaced, so
//! everything beneath it goes away in one step rather than a level per run. That is also what makes
//! the pass settle: a plan it has run over has an empty `VALUES` where the empty subtree was, and an
//! empty `VALUES` is the answer this pass would give for it again.
//!
//! # Where the always true predicate went
//!
//! The other half of constant pruning, `WHERE true`, is in `crate::filter`. A conjunct that is a
//! true constant is dropped as the pass puts the filter back together, and a filter with nothing
//! left in it is not rebuilt, which is where the binary does it too.

use rudb_common::{Field, Result};
use rudb_plan::{Expr, ExprRef, Node, NodeRef, Plan, Slice};

use crate::pass::{Context, Pass};

/// Replaces a subtree that cannot produce a row with an empty relation.
#[derive(Debug, Clone, Copy)]
pub struct EmptyResultPullup;

impl Pass for EmptyResultPullup {
    fn name(&self) -> &'static str {
        "empty_result_pullup"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        prune(plan);
        Ok(())
    }
}

/// Replaces every highest empty subtree in `plan` with an empty relation.
pub fn prune(plan: &mut Plan) {
    let root = plan.root();
    walk(plan, root);
}

/// Replaces `at` if it is empty, and otherwise looks under it.
///
/// Writing the replacement over the node's own slot rather than appending one is what keeps whatever
/// pointed at it pointing at the right thing. It is allowed because an empty relation is a leaf, and
/// the arena only asks that a node's children sit behind it, which a node with no children does
/// however early its slot is.
fn walk(plan: &mut Plan, at: NodeRef) {
    if !already_empty(plan, at) && empty(plan, at) {
        if let Some((index, columns)) = columns_of(plan, at) {
            let rows = plan.add_rows(&[]);
            *plan.node_mut(at) = Node::Values { index, columns, rows };
            return;
        }
    }
    for child in plan.node(at).children().into_iter().flatten() {
        walk(plan, child);
    }
}

/// Whether this node is already the empty relation, which is what the pass leaves behind.
///
/// Without this the second run would rewrite the `VALUES` it wrote on the first one into another
/// `VALUES` that prints the same, which costs a slot per run and, more to the point, is a pass that
/// keeps finding work on a plan it has already finished with.
fn already_empty(plan: &Plan, at: NodeRef) -> bool {
    match *plan.node(at) {
        Node::Values { rows, .. } => plan.row_list(rows).is_empty(),
        _ => false,
    }
}

/// Whether this node can produce a row.
///
/// Only the operators that pass their input through recurse. Everything else answers false, which
/// for a group by is the rule and not a missing case.
fn empty(plan: &Plan, at: NodeRef) -> bool {
    match *plan.node(at) {
        Node::Values { rows, .. } => plan.row_list(rows).is_empty(),
        Node::Filter { input, predicate } => never(plan, predicate) || empty(plan, input),
        Node::Limit { input, count, .. } => count == Some(0) || empty(plan, input),
        Node::TopN { input, count, .. } => count == 0 || empty(plan, input),
        Node::Sort { input, .. } | Node::Distinct { input, .. } | Node::Project { input, .. } => {
            empty(plan, input)
        }
        _ => false,
    }
}

/// Whether a predicate keeps no row at all.
///
/// A null predicate keeps nothing, the same as a false one. That is `WHERE`'s rule rather than
/// `=`'s, and it is the difference between a `WHERE` and a `CHECK` constraint.
fn never(plan: &Plan, predicate: ExprRef) -> bool {
    let Expr::Constant(value) = *plan.expr(predicate) else {
        return false;
    };
    let value = plan.value(value);
    value.is_null() || value.as_bool() == Some(false)
}

/// The table index and the column list an empty relation standing in for this node would need.
///
/// `None` where the node's output is not one table index with a field list, which is a join, a cross
/// product, a set operation and a group by. A group by could be given one by inventing a name per
/// aggregate, and that would change what `EXPLAIN` prints for a plan the pass had nothing else to do
/// to, so it is refused here rather than guessed at.
fn columns_of(plan: &mut Plan, at: NodeRef) -> Option<(u32, Slice)> {
    match *plan.node(at) {
        Node::Get { index, columns, .. }
        | Node::Values { index, columns, .. }
        | Node::TableFunction { index, columns, .. } => Some((index, columns)),
        // A projection carries its names, so the fields can be read off the names and the types the
        // binder already worked out for the expressions.
        Node::Project { index, exprs, names, .. } => {
            let exprs = plan.expr_list(exprs).to_vec();
            let names = plan.name_list(names).to_vec();
            let fields: Vec<Field> = exprs
                .iter()
                .zip(names)
                .map(|(&expr, name)| Field::new(plan.string(name), plan.expr_type(expr).clone()))
                .collect();
            Some((index, plan.add_fields(&fields)))
        }
        Node::Filter { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::TopN { input, .. }
        | Node::Distinct { input, .. } => columns_of(plan, input),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::prune;

    /// What the plan a text prints looks like once the pass has run over it.
    fn pruned(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        prune(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    #[test]
    fn a_false_predicate_takes_the_scan_with_it() {
        assert_eq!(
            pruned(concat!(
                "Filter FALSE::BOOLEAN\n",
                "  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n",
            )),
            "Values #0 [a::INTEGER, b::INTEGER] rows=[]\n"
        );
    }

    /// A null predicate keeps no row either, which is the rule `WHERE` has and `CHECK` does not.
    #[test]
    fn a_null_predicate_is_as_empty_as_a_false_one() {
        assert_eq!(
            pruned(
                concat!("Filter NULL::BOOLEAN\n", "  Get memory.main.t AS t #0 [a::INTEGER]\n",)
            ),
            "Values #0 [a::INTEGER] rows=[]\n"
        );
    }

    #[test]
    fn a_predicate_that_depends_on_the_row_is_left_alone() {
        let text = concat!(
            "Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
        );
        assert_eq!(pruned(text), text);
    }

    /// The highest node goes, not the lowest one, so the sort and the projection above the filter
    /// are gone in one run rather than one of them per run.
    #[test]
    fn everything_above_the_empty_node_that_passes_rows_through_goes_with_it() {
        assert_eq!(
            pruned(concat!(
                "Project #1 [#0.0::INTEGER AS a]\n",
                "  Sort [#0.0::INTEGER ASC NULLS LAST]\n",
                "    Filter FALSE::BOOLEAN\n",
                "      Get memory.main.t AS t #0 [a::INTEGER]\n",
            )),
            "Values #1 [a::INTEGER] rows=[]\n"
        );
    }

    #[test]
    fn a_limit_of_no_rows_is_an_empty_relation() {
        assert_eq!(
            pruned(concat!("Limit 0 offset 0\n", "  Get memory.main.t AS t #0 [a::INTEGER]\n",)),
            "Values #0 [a::INTEGER] rows=[]\n"
        );
        assert_eq!(
            pruned(concat!(
                "TopN 0 offset 0 [#0.0::INTEGER ASC NULLS LAST]\n",
                "  Get memory.main.t AS t #0 [a::INTEGER]\n",
            )),
            "Values #0 [a::INTEGER] rows=[]\n"
        );
    }

    #[test]
    fn a_limit_of_one_row_is_not() {
        let text = concat!("Limit 1 offset 0\n", "  Get memory.main.t AS t #0 [a::INTEGER]\n",);
        assert_eq!(pruned(text), text);
    }

    /// The case this pass is written to be careful about. An ungrouped aggregate over no rows
    /// produces one row, so the empty relation goes under it and the aggregate stays where it is.
    #[test]
    fn an_aggregate_over_nothing_still_produces_its_row() {
        assert_eq!(
            pruned(concat!(
                "Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]\n",
                "  Filter FALSE::BOOLEAN\n",
                "    Get memory.main.t AS t #0 [a::INTEGER]\n",
            )),
            concat!(
                "Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]\n",
                "  Values #0 [a::INTEGER] rows=[]\n",
            )
        );
    }

    /// An empty side of a join is left where it is, because a `VALUES` binds to one table index and
    /// a join produces two, so replacing the join would mean rewriting every binding above it.
    #[test]
    fn an_empty_side_of_a_join_stays_a_side_of_the_join() {
        assert_eq!(
            pruned(concat!(
                "Join INNER on=[]\n",
                "  Filter FALSE::BOOLEAN\n",
                "    Get memory.main.t AS t #0 [a::INTEGER]\n",
                "  Get memory.main.u AS u #1 [x::INTEGER]\n",
            )),
            concat!(
                "Join INNER on=[]\n",
                "  Values #0 [a::INTEGER] rows=[]\n",
                "  Get memory.main.u AS u #1 [x::INTEGER]\n",
            )
        );
    }

    /// A filter over a join is unsatisfiable and cannot be spelled as an empty relation, so it is
    /// left alone rather than replaced by something with the wrong bindings.
    #[test]
    fn a_false_filter_over_a_join_is_refused_rather_than_guessed_at() {
        let text = concat!(
            "Filter FALSE::BOOLEAN\n",
            "  Join INNER on=[]\n",
            "    Get memory.main.t AS t #0 [a::INTEGER]\n",
            "    Get memory.main.u AS u #1 [x::INTEGER]\n",
        );
        assert_eq!(pruned(text), text);
    }

    #[test]
    fn running_it_twice_is_running_it_once() {
        let text = concat!(
            "Project #1 [#0.0::INTEGER AS a]\n",
            "  Filter FALSE::BOOLEAN\n",
            "    Get memory.main.t AS t #0 [a::INTEGER]\n",
        );
        let once = pruned(text);
        assert_eq!(pruned(&once), once);
    }
}

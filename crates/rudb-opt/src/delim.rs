//! Taking the domain back out of a decorrelated existence test.
//!
//! `domain.rs` and the rules beside it lower a correlated subquery by inventing a relation of the
//! distinct values the correlated columns take, running the subquery once against that relation,
//! and joining the answers back to the outer rows on those same columns. The domain is what makes
//! the inner side run once rather than once per outer row. It is built before anything knows which
//! outer rows there are going to be, so it is taken from whichever branch of the `FROM` list the
//! correlated columns come from, and it is a superset of the values the query ends up asking about.
//!
//! On TPC-H q21 that superset is the whole of lineitem. The outer query is supplier joined to
//! lineitem, orders and nation, and once its filters have run it has seventy eight thousand rows,
//! so each of the two existence tests in it is asked about seventy eight thousand pairs of order key
//! and supplier key. The domain has six million of them, because the branch the keys come from is
//! the lineitem scan and every join that cuts it down sits above the point the domain was taken at.
//! Both existence tests then build a hash table of six million rows to answer a question about
//! seventy eight thousand, and that is most of what q21 costs.
//!
//! What this pass does is notice that by the time the plan is this far along the outer side of the
//! join back is the exact relation the domain was standing in for, and write the whole shape as one
//! semi join against it. The domain goes, the grouping over it goes, the marker projection goes and
//! the join back goes, and what is left is the outer side joined to the subquery's own relation on
//! the conditions the subquery was correlated by. DuckDB calls the pass that does this the
//! deliminator and that is the name it goes by here.
//!
//! # Why it is the same query
//!
//! The shape it matches answers one question: which outer rows have a correlated key that the
//! subquery holds a match for. It answers it in four steps. The domain reduces the outer keys to the
//! distinct values they take, the join under the grouping keeps the domain values the subquery
//! matched, the marker projection writes a constant beside each of those, and the single join puts
//! the marker back beside every outer row whose key is one of them. The filter over the marker then
//! keeps the rows that got one, or the rows that did not.
//!
//! A semi join asks the same question in one step, and an anti join asks its negation. An outer row
//! reaches the output exactly when its key had a match, which is what both spellings say, and
//! neither can produce a row twice: a single join matches at most one row on the right by
//! definition, and a semi join produces each driving row at most once by definition.
//!
//! The nulls need no special handling and that is worth saying, because the join back is written
//! with `IS NOT DISTINCT FROM` rather than `=` for exactly that reason. The domain carries a row for
//! a null key and the null safe comparison finds it, so an outer row with a null key is asked about
//! rather than dropped. After the collapse the same outer row is handed to the same conditions the
//! domain row would have been handed to, with the same null in it, so the answer it gets is the one
//! it was getting before.
//!
//! # What it refuses
//!
//! A marker that is not a constant, or a constant that is null. The filter tests the marker against
//! null to find out whether the single join matched, and that test only means what it is being read
//! to mean if a matched row always carries a value.
//!
//! A domain grouped on anything but bare columns of the outer side. A domain over an expression is
//! one whose values the outer side does not hold and so is not one the outer side can replace.
//!
//! A join back that is anything but one null safe equality per domain column. An extra condition is
//! a condition the collapse would drop, and a missing one is a key nothing lines up.
//!
//! A subquery relation that reads the outer side. Decorrelation is meant to have left it reading
//! nothing but the domain, and if something is still in there then moving it under a plain join
//! would leave it reading a side it cannot see.
//!
//! A plan where anything above reads a column of the marker projection. The collapse produces the
//! outer side's columns and nothing else, on the same grounds `semi.rs` refuses a mark join whose
//! gathered columns are read.
//!
//! # Rewriting in place
//!
//! The semi join is written over the filter's slot, the way `semi.rs` and `topn.rs` write theirs.
//! Both of the sides it points at are behind the single join, which is behind the filter, so the
//! arena's rule that a node may only point backwards still holds and nothing above has to be built
//! again.

use std::collections::HashMap;

use rudb_common::{Result, Value};
use rudb_plan::{
    BuildSide, ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan, Slice,
};

use crate::domain::remap;
use crate::pass::{Context, Pass, top_down};
use crate::tables::{TableSet, produced};
use crate::walk;

/// Rewrites a decorrelated existence test into the semi join it is asking for.
#[derive(Debug, Clone, Copy)]
pub struct Deliminator;

impl Pass for Deliminator {
    /// DuckDB's name for the pass that removes a domain, which is already in [`crate::UPSTREAM`].
    fn name(&self) -> &'static str {
        "deliminator"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        remove(plan);
        Ok(())
    }
}

/// Collapses every decorrelated existence test in `plan` into a semi or an anti join.
pub fn remove(plan: &mut Plan) {
    for node in top_down(plan) {
        let Some(found) = matched(plan, node) else {
            continue;
        };
        let held = plan.expr_list(found.conditions).to_vec();
        let moved: Vec<ExprRef> =
            held.into_iter().map(|condition| remap(plan, condition, &found.moved)).collect();
        let conditions = plan.add_expr_list(&moved);
        *plan.node_mut(node) = Node::Join {
            left: found.left,
            right: found.right,
            kind: found.kind,
            conditions,
            build: BuildSide::default(),
        };
    }
}

/// The parts of one collapsible existence test.
struct Found {
    /// The outer side, which becomes the driving side.
    left: NodeRef,
    /// The subquery's own relation, which becomes the gathered side.
    right: NodeRef,
    /// The conditions the subquery was correlated by, still written against the domain.
    conditions: Slice,
    /// Semi where the filter kept the rows that matched, anti where it kept the rest.
    kind: JoinKind,
    /// Which outer column each domain column was standing in for.
    moved: HashMap<ColumnBinding, ColumnBinding>,
}

/// Reads the shape out of the filter at `node`, or nothing if it is not one of these.
///
/// Every step is a check that the node underneath is the node decorrelation put there, and the
/// order they are written in is the order down the plan: the filter, the join back, the marker
/// projection, the grouping that made the answers distinct, the join to the subquery's relation and
/// the domain itself.
fn matched(plan: &Plan, node: NodeRef) -> Option<Found> {
    let Node::Filter { input, predicate } = *plan.node(node) else {
        return None;
    };
    let (kind, tested) = asked(plan, predicate)?;
    let Node::Join { left, right, kind: JoinKind::Single, conditions, .. } = *plan.node(input)
    else {
        return None;
    };
    let Node::Project { input: distinct, index: marker, exprs, .. } = *plan.node(right) else {
        return None;
    };
    if tested != ColumnBinding::new(marker, 0) {
        return None;
    }
    let projected = plan.expr_list(exprs).to_vec();
    let (&flag, carried) = projected.split_first()?;
    let Expr::Constant(value) = *plan.expr(flag) else {
        return None;
    };
    if matches!(plan.value(value), Value::Null) {
        return None;
    }

    let Node::Aggregate { input: answers, index: distinct_index, groups, aggregates } =
        *plan.node(distinct)
    else {
        return None;
    };
    if !plan.expr_list(aggregates).is_empty() || !columns(plan, carried, distinct_index) {
        return None;
    }
    let Node::Join { left: domain, right: inner, kind: inside, conditions: correlated, .. } =
        *plan.node(answers)
    else {
        return None;
    };
    if !matches!(inside, JoinKind::Inner | JoinKind::Semi) {
        return None;
    }
    let Node::Aggregate { index: domain_index, groups: keys, aggregates: none, .. } =
        *plan.node(domain)
    else {
        return None;
    };
    let grouped = plan.expr_list(groups).to_vec();
    if !plan.expr_list(none).is_empty() || !columns(plan, &grouped, domain_index) {
        return None;
    }

    let outer = produced(plan, left);
    let mut moved = HashMap::new();
    let mut keyed = Vec::new();
    for (position, &key) in plan.expr_list(keys).iter().enumerate() {
        let Expr::Column(binding) = *plan.expr(key) else {
            return None;
        };
        if !outer.contains(binding.table) {
            return None;
        }
        moved.insert(ColumnBinding::new(domain_index, at(position)?), binding);
        keyed.push(binding);
    }
    if keyed.len() != grouped.len() || keyed.len() != carried.len() {
        return None;
    }
    if !lines_up(plan, conditions, marker, &keyed) {
        return None;
    }
    if reads(plan, inner, &outer) || read_above(plan, marker, node, input) {
        return None;
    }
    Some(Found { left, right: inner, conditions: correlated, kind, moved })
}

/// Which join a filter over a marker is asking for, and the marker it reads.
///
/// A bare test that the marker is not null is the existence test itself and is a semi join. The
/// same test with `not` in front of it is what a `NOT EXISTS` binds to and is an anti join. Nothing
/// else here is one of these, and in particular `not` over an anti join is not written, because
/// the rewrite that would produce one does not exist and a double negation folds away before this.
fn asked(plan: &Plan, predicate: ExprRef) -> Option<(JoinKind, ColumnBinding)> {
    if let Expr::Function { name, args } = *plan.expr(predicate) {
        if plan.string(name) != "not" {
            return None;
        }
        let [only] = *plan.expr_list(args) else {
            return None;
        };
        return match asked(plan, only)? {
            (JoinKind::Semi, binding) => Some((JoinKind::Anti, binding)),
            _ => None,
        };
    }
    let Expr::Compare { op: CompareOp::DistinctFrom, left, right } = *plan.expr(predicate) else {
        return None;
    };
    let Expr::Column(binding) = *plan.expr(left) else {
        return None;
    };
    let Expr::Constant(value) = *plan.expr(right) else {
        return None;
    };
    matches!(plan.value(value), Value::Null).then_some((JoinKind::Semi, binding))
}

/// Whether `exprs` reads columns nought upward of `index`, in that order and nothing else.
///
/// Both of the projections between the domain and the outer side carry their input's columns
/// straight through, and a pass that took them on trust would be reading the wrong column the day
/// one of them reorders or drops one.
fn columns(plan: &Plan, exprs: &[ExprRef], index: u32) -> bool {
    exprs.iter().enumerate().all(|(position, &expr)| {
        let Expr::Column(binding) = *plan.expr(expr) else {
            return false;
        };
        at(position).is_some_and(|column| binding == ColumnBinding::new(index, column))
    })
}

/// Whether the join back is one null safe equality per key, each naming a different domain column.
///
/// The count is checked as well as the names, because two conditions on one key and none on another
/// is a shape where the counts agree and a key goes unjoined.
fn lines_up(plan: &Plan, conditions: Slice, marker: u32, keyed: &[ColumnBinding]) -> bool {
    let held = plan.expr_list(conditions);
    if held.len() != keyed.len() {
        return false;
    }
    let mut seen = vec![false; keyed.len()];
    for &condition in held {
        let Expr::Compare { op: CompareOp::NotDistinctFrom, left, right } = *plan.expr(condition)
        else {
            return false;
        };
        let Expr::Column(here) = *plan.expr(left) else {
            return false;
        };
        let Expr::Column(there) = *plan.expr(right) else {
            return false;
        };
        // Whichever way round it was written, one side reads the marker projection and the other
        // reads the outer row the marker is being put back beside.
        let (outer, carried) = if there.table == marker { (here, there) } else { (there, here) };
        if carried.table != marker || carried.column == 0 {
            return false;
        }
        let Ok(position) = usize::try_from(carried.column - 1) else {
            return false;
        };
        if position >= keyed.len() || seen[position] || keyed[position] != outer {
            return false;
        }
        seen[position] = true;
    }
    seen.into_iter().all(|found| found)
}

/// Whether anything in the subtree under `at` reads a column of a table in `outer`.
fn reads(plan: &Plan, at: NodeRef, outer: &TableSet) -> bool {
    let mut found = false;
    walk::node_columns(plan, at, &mut |_, binding| found |= outer.contains(binding.table));
    found || plan.node(at).children().into_iter().flatten().any(|child| reads(plan, child, outer))
}

/// Whether anything but the filter and the join under it reads a column of the marker projection.
///
/// The projection is the only place the marker's table index is produced, so nothing under it reads
/// one either and there is no subtree to skip. The filter is skipped because the whole point is that
/// it is going away, and the join because its conditions read the marker by definition.
fn read_above(plan: &Plan, marker: u32, filter: NodeRef, join: NodeRef) -> bool {
    let mut found = false;
    for at in top_down(plan) {
        if at == filter || at == join {
            continue;
        }
        walk::node_columns(plan, at, &mut |_, binding| found |= binding.table == marker);
    }
    found
}

/// A position in a column list as the column number it is.
fn at(position: usize) -> Option<u32> {
    u32::try_from(position).ok()
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::remove;

    /// What the plan a text prints looks like once the pass has run over it.
    fn removed(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        remove(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    /// The shape decorrelation leaves an `EXISTS` in, with `head` over the marker test.
    fn existence(head: &str) -> String {
        format!(
            concat!(
                "Filter {head}\n",
                "  Join SINGLE on=[(#0.0::BIGINT IS NOT DISTINCT FROM #5.1::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
                "    Project #5 [TRUE::BOOLEAN AS exists, #4.0::BIGINT AS __correlated_1]\n",
                "      Aggregate #4 groups=[#3.0::BIGINT] aggregates=[]\n",
                "        Join SEMI on=[(#1.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
                "          Aggregate #3 groups=[#0.0::BIGINT] aggregates=[]\n",
                "            Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
                "          Get memory.main.u AS u #1 [k::BIGINT]\n",
            ),
            head = head
        )
    }

    /// The marker test itself, which is what an `EXISTS` and a `NOT EXISTS` differ by.
    const TESTED: &str = "(#5.0::BOOLEAN IS DISTINCT FROM NULL::BOOLEAN)::BOOLEAN";

    #[test]
    fn an_existence_test_over_a_domain_becomes_a_semi_join_against_the_outer_side() {
        assert_eq!(
            removed(&existence(TESTED)),
            concat!(
                "Join SEMI on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN]\n",
                "  Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
                "  Get memory.main.u AS u #1 [k::BIGINT]\n",
            )
        );
    }

    #[test]
    fn the_same_test_with_not_in_front_of_it_becomes_an_anti_join() {
        assert_eq!(
            removed(&existence(&format!("not({TESTED})::BOOLEAN"))),
            concat!(
                "Join ANTI on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN]\n",
                "  Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
                "  Get memory.main.u AS u #1 [k::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_filter_on_something_other_than_the_marker_is_left_alone() {
        let text = existence("(#0.0::BIGINT > 3::BIGINT)::BOOLEAN");
        assert_eq!(removed(&text), text);
    }

    #[test]
    fn a_marker_column_read_above_the_filter_stops_the_collapse() {
        let text = concat!(
            "Project #6 [#5.1::BIGINT AS k]\n",
            "  Filter (#5.0::BOOLEAN IS DISTINCT FROM NULL::BOOLEAN)::BOOLEAN\n",
            "    Join SINGLE on=[(#0.0::BIGINT IS NOT DISTINCT FROM #5.1::BIGINT)::BOOLEAN]\n",
            "      Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "      Project #5 [TRUE::BOOLEAN AS exists, #4.0::BIGINT AS __correlated_1]\n",
            "        Aggregate #4 groups=[#3.0::BIGINT] aggregates=[]\n",
            "          Join SEMI on=[(#1.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
            "            Aggregate #3 groups=[#0.0::BIGINT] aggregates=[]\n",
            "              Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "            Get memory.main.u AS u #1 [k::BIGINT]\n",
        );
        assert_eq!(removed(text), text);
    }

    #[test]
    fn a_real_aggregate_under_the_marker_is_not_a_domain_and_is_left_alone() {
        let text = concat!(
            "Filter (#5.0::BOOLEAN IS DISTINCT FROM NULL::BOOLEAN)::BOOLEAN\n",
            "  Join SINGLE on=[(#0.0::BIGINT IS NOT DISTINCT FROM #5.1::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "    Project #5 [TRUE::BOOLEAN AS exists, #4.0::BIGINT AS __correlated_1]\n",
            "      Aggregate #4 groups=[#3.0::BIGINT] aggregates=[count_star()::BIGINT]\n",
            "        Join SEMI on=[(#1.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
            "          Aggregate #3 groups=[#0.0::BIGINT] aggregates=[]\n",
            "            Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "          Get memory.main.u AS u #1 [k::BIGINT]\n",
        );
        assert_eq!(removed(text), text);
    }

    #[test]
    fn a_domain_grouped_on_an_expression_is_left_alone() {
        let text = concat!(
            "Filter (#5.0::BOOLEAN IS DISTINCT FROM NULL::BOOLEAN)::BOOLEAN\n",
            "  Join SINGLE on=[(#0.0::BIGINT IS NOT DISTINCT FROM #5.1::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "    Project #5 [TRUE::BOOLEAN AS exists, #4.0::BIGINT AS __correlated_1]\n",
            "      Aggregate #4 groups=[#3.0::BIGINT] aggregates=[]\n",
            "        Join SEMI on=[(#1.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
            "          Aggregate #3 groups=[\"+\"(#0.0::BIGINT, 1::BIGINT)::BIGINT] aggregates=[]\n",
            "            Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "          Get memory.main.u AS u #1 [k::BIGINT]\n",
        );
        assert_eq!(removed(text), text);
    }

    #[test]
    fn a_subquery_relation_that_still_reads_the_outer_side_is_left_alone() {
        // Nothing decorrelation produces looks like this, and a join whose gathered side reads its
        // driving side is the one thing the collapse cannot write down.
        let text = concat!(
            "Filter (#5.0::BOOLEAN IS DISTINCT FROM NULL::BOOLEAN)::BOOLEAN\n",
            "  Join SINGLE on=[(#0.0::BIGINT IS NOT DISTINCT FROM #5.1::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "    Project #5 [TRUE::BOOLEAN AS exists, #4.0::BIGINT AS __correlated_1]\n",
            "      Aggregate #4 groups=[#3.0::BIGINT] aggregates=[]\n",
            "        Join SEMI on=[(#1.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
            "          Aggregate #3 groups=[#0.0::BIGINT] aggregates=[]\n",
            "            Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "          Filter (#1.0::BIGINT > #0.1::BIGINT)::BOOLEAN\n",
            "            Get memory.main.u AS u #1 [k::BIGINT]\n",
        );
        assert_eq!(removed(text), text);
    }

    #[test]
    fn a_join_back_that_leaves_a_key_unjoined_is_left_alone() {
        // Two conditions on one of the two domain columns and none on the other. The counts agree
        // and the second key is joined on nothing, which is not the shape this reads it as.
        let text = concat!(
            "Filter (#5.0::BOOLEAN IS DISTINCT FROM NULL::BOOLEAN)::BOOLEAN\n",
            "  Join SINGLE on=[(#0.0::BIGINT IS NOT DISTINCT FROM #5.1::BIGINT)::BOOLEAN, \
             (#0.0::BIGINT IS NOT DISTINCT FROM #5.1::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "    Project #5 [TRUE::BOOLEAN AS exists, #4.0::BIGINT AS __correlated_1, \
             #4.1::BIGINT AS __correlated_2]\n",
            "      Aggregate #4 groups=[#3.0::BIGINT, #3.1::BIGINT] aggregates=[]\n",
            "        Join SEMI on=[(#1.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
            "          Aggregate #3 groups=[#0.0::BIGINT, #0.1::BIGINT] aggregates=[]\n",
            "            Get memory.main.t AS t #0 [a::BIGINT, b::BIGINT]\n",
            "          Get memory.main.u AS u #1 [k::BIGINT]\n",
        );
        assert_eq!(removed(text), text);
    }
}

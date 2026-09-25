//! Answering `EXISTS` a row that differs in one column from each group's smallest and largest value.
//!
//! TPC-H q21 asks, for each late line of an order, whether the same order has a line from another
//! supplier, and whether it has a late line from another supplier. Both bind to a semi or an anti
//! join on the order key with one more condition, `l2.l_suppkey <> l1.l_suppkey`, and a hash join
//! answers that by pairing every line with every line of its order and testing the pair. That was
//! the largest part of q21, and most of the pairs it tested were a line against the other lines of
//! an order it already had an answer for.
//!
//! What the question needs from the other side is much less than its rows. A group holds a value
//! other than `c` exactly when its smallest value or its largest value is not `c`: if both are `c`,
//! everything between them is `c` too. So the other side grouped by the join keys down to the
//! smallest and the largest of its column answers the test for every row that meets the group, and
//! the join meets one row per key instead of all of them. On a table stored in key order the
//! grouping is a walk over runs, which is the cheap way to group.
//!
//! # When it is the same answer
//!
//! A semi or an anti join whose conditions are equalities between a column of each side and one
//! `<>` between a column of each side, all bare columns, over a type `min` and `max` order the way
//! `=` compares, which is the integers, the dates and times and the decimals. `IS DISTINCT FROM`
//! and anything else leave the join as it was.
//!
//! Nulls come out the same. A row of the other side whose column is null makes the `<>` null, so it
//! never matched, and `min` and `max` skip it. A group of nothing but nulls has a null smallest and
//! largest, and a comparison with them is null, which a semi join drops and an anti join keeps, the
//! same as before. A driving row whose own column is null matched nothing before and compares null
//! with both ends now. A driving row whose key is null meets no group, since the join compares it
//! with `=`.
//!
//! # What it produces
//!
//! ```text
//! Join SEMI on=[l.k = r.k, r.c <> l.c]
//!   <L>
//!   <R>
//! ```
//!
//! becomes
//!
//! ```text
//! Filter (#5.1 <> l.c OR #5.2 <> l.c)
//!   Join INNER on=[l.k = #5.0]
//!     <L>
//!     Aggregate #5 groups=[r.k] aggregates=[min(r.c), max(r.c)]
//!       <R>
//! ```
//!
//! and an anti join becomes a left join with `(#5.1 <> l.c OR #5.2 <> l.c) IS DISTINCT FROM true`
//! over it, which keeps a row that met no group and a row whose test was null as well as one whose
//! test was false. The groups are one per key, so each driving row meets one at most and comes out
//! once, which is what a semi join does. The join's output is wider than the semi join's, by the
//! group's columns, and the unused column pass removes what nothing above reads.
//!
//! The join reaches the other side's scan through the aggregate, because its keys are the group
//! keys, so the runtime filter and the link a semi join would have sent down still arrive. See
//! `rudb_exec::sideways::beneath`.

use rudb_common::{LogicalType, Result, Value};
use rudb_plan::{
    BuildSide, ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef,
    Plan,
};

use crate::pass::{Context, Pass};
use crate::tables::produced;
use crate::walk;

/// Rewrites a semi or anti join with one `<>` into a join against each group's extremes.
#[derive(Debug, Clone, Copy)]
pub struct ExistsAsExtremes;

impl Pass for ExistsAsExtremes {
    fn name(&self) -> &'static str {
        "exists_extremes"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        rewrite(plan);
        Ok(())
    }
}

/// Rewrites every join in `plan` this applies to.
pub fn rewrite(plan: &mut Plan) {
    let mut moved = false;
    let root = walk::restack(plan, plan.root(), &mut moved, &mut |plan, at| extremes(plan, at));
    if moved {
        plan.set_root(root);
    }
}

/// What a join's conditions say, once each has been found to be about one column of each side.
struct Shape {
    /// The equalities, as the driving side's column and the other side's.
    keys: Vec<(ColumnBinding, ColumnBinding, LogicalType)>,
    /// The `<>`, the same way round.
    differs: (ColumnBinding, ColumnBinding, LogicalType),
}

/// The conditions of a join from `left` to `right` when they are the shape this answers.
fn shape(plan: &Plan, left: NodeRef, right: NodeRef, conditions: &[ExprRef]) -> Option<Shape> {
    let ours = produced(plan, left);
    let theirs = produced(plan, right);
    let mut keys = Vec::new();
    let mut differs = None;
    for &condition in conditions {
        let Expr::Compare { op, left: one, right: other } = *plan.expr(condition) else {
            return None;
        };
        let (Expr::Column(one_at), Expr::Column(other_at)) = (plan.expr(one), plan.expr(other))
        else {
            return None;
        };
        let (driving, held, ty) = if ours.contains(one_at.table) && theirs.contains(other_at.table)
        {
            (*one_at, *other_at, plan.expr_type(one))
        } else if theirs.contains(one_at.table) && ours.contains(other_at.table) {
            (*other_at, *one_at, plan.expr_type(other))
        } else {
            return None;
        };
        if plan.expr_type(one) != plan.expr_type(other) {
            return None;
        }
        match op {
            CompareOp::Equal => keys.push((driving, held, ty.clone())),
            CompareOp::NotEqual if differs.is_none() => {
                let ordered = ty.is_integer()
                    || ty.is_temporal()
                    || matches!(ty, LogicalType::Decimal { .. });
                if !ordered {
                    return None;
                }
                differs = Some((driving, held, ty.clone()));
            }
            _ => return None,
        }
    }
    let differs = differs?;
    (!keys.is_empty()).then_some(Shape { keys, differs })
}

/// The join against the groups' extremes and the filter over it, when `at` is a join this answers.
fn extremes(plan: &mut Plan, at: NodeRef) -> Option<NodeRef> {
    let Node::Join { left, right, kind, conditions, .. } = *plan.node(at) else { return None };
    if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
        return None;
    }
    let listed = plan.expr_list(conditions).to_vec();
    let Shape { keys, differs } = shape(plan, left, right, &listed)?;

    let grouped = walk::fresh_index(plan);
    let column = |plan: &mut Plan, binding: ColumnBinding, ty: &LogicalType| {
        plan.add_expr(Expr::Column(binding), ty.clone())
    };
    let at_group = |position: usize| {
        ColumnBinding::new(grouped, u32::try_from(position).expect("a handful of keys"))
    };

    let groups: Vec<ExprRef> = keys.iter().map(|(_, held, ty)| column(plan, *held, ty)).collect();
    let (driving_c, held_c, ty) = differs;
    let held_c = column(plan, held_c, &ty);
    let calls: Vec<ExprRef> = ["min", "max"]
        .into_iter()
        .map(|name| {
            let args = plan.add_expr_list(&[held_c]);
            let call =
                Expr::Aggregate { name: plan.intern(name), args, distinct: false, filter: None };
            plan.add_expr(call, ty.clone())
        })
        .collect();
    let groups = plan.add_expr_list(&groups);
    let aggregates = plan.add_expr_list(&calls);
    let grouping =
        plan.add_node(Node::Aggregate { input: right, index: grouped, groups, aggregates });

    let mut on = Vec::with_capacity(keys.len());
    for (position, (driving, _, key_ty)) in keys.iter().enumerate() {
        let one = column(plan, *driving, key_ty);
        let other = column(plan, at_group(position), key_ty);
        let equal = Expr::Compare { op: CompareOp::Equal, left: one, right: other };
        on.push(plan.add_expr(equal, LogicalType::Boolean));
    }
    let conditions = plan.add_expr_list(&on);
    let joined = plan.add_node(Node::Join {
        left,
        right: grouping,
        kind: if kind == JoinKind::Semi { JoinKind::Inner } else { JoinKind::Left },
        conditions,
        build: BuildSide::default(),
    });

    let tests: Vec<ExprRef> = [keys.len(), keys.len() + 1]
        .into_iter()
        .map(|position| {
            let end = column(plan, at_group(position), &ty);
            let own = column(plan, driving_c, &ty);
            plan.add_expr(
                Expr::Compare { op: CompareOp::NotEqual, left: end, right: own },
                LogicalType::Boolean,
            )
        })
        .collect();
    let children = plan.add_expr_list(&tests);
    let mut predicate =
        plan.add_expr(Expr::Conjunction { op: ConjunctionOp::Or, children }, LogicalType::Boolean);
    if kind == JoinKind::Anti {
        let yes = plan.add_constant(Value::Boolean(true));
        let kept = Expr::Compare { op: CompareOp::DistinctFrom, left: predicate, right: yes };
        predicate = plan.add_expr(kept, LogicalType::Boolean);
    }
    Some(plan.add_node(Node::Filter { input: joined, predicate }))
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::rewrite;

    fn rewritten(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        rewrite(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        let once = plan.to_string();
        rewrite(&mut plan);
        assert_eq!(plan.to_string(), once, "a second run moved the plan again");
        once
    }

    const SEMI: &str = concat!(
        "Join SEMI on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, (#1.1::BIGINT <> #0.1::BIGINT)::BOOLEAN]\n",
        "  Get memory.main.l AS l1 #0 [k::BIGINT, s::BIGINT]\n",
        "  Get memory.main.l AS l2 #1 [k::BIGINT, s::BIGINT]\n",
    );

    #[test]
    fn a_semi_join_with_one_differs_becomes_a_join_against_the_extremes() {
        assert_eq!(
            rewritten(SEMI),
            concat!(
                "Filter ((#2.1::BIGINT <> #0.1::BIGINT)::BOOLEAN OR (#2.2::BIGINT <> #0.1::BIGINT)::BOOLEAN)::BOOLEAN\n",
                "  Join INNER on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.l AS l1 #0 [k::BIGINT, s::BIGINT]\n",
                "    Aggregate #2 groups=[#1.0::BIGINT] aggregates=[min(#1.1::BIGINT)::BIGINT, max(#1.1::BIGINT)::BIGINT]\n",
                "      Get memory.main.l AS l2 #1 [k::BIGINT, s::BIGINT]\n",
            )
        );
    }

    #[test]
    fn an_anti_join_becomes_a_left_join_that_keeps_a_test_that_was_not_true() {
        let anti = SEMI.replace("Join SEMI", "Join ANTI");
        assert_eq!(
            rewritten(&anti),
            concat!(
                "Filter (((#2.1::BIGINT <> #0.1::BIGINT)::BOOLEAN OR (#2.2::BIGINT <> #0.1::BIGINT)::BOOLEAN)::BOOLEAN IS DISTINCT FROM true)::BOOLEAN\n",
                "  Join LEFT on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.l AS l1 #0 [k::BIGINT, s::BIGINT]\n",
                "    Aggregate #2 groups=[#1.0::BIGINT] aggregates=[min(#1.1::BIGINT)::BIGINT, max(#1.1::BIGINT)::BIGINT]\n",
                "      Get memory.main.l AS l2 #1 [k::BIGINT, s::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_join_of_any_other_shape_is_left_as_it_was() {
        for (from, to) in [
            ("<>", "<"),
            ("<>", "="),
            ("(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, ", ""),
            ("Join SEMI", "Join INNER"),
            ("#1.1::BIGINT <> #0.1::BIGINT", "#1.1::BIGINT IS DISTINCT FROM #0.1::BIGINT"),
        ] {
            let other = SEMI.replace(from, to);
            assert_eq!(rewritten(&other), other, "{from} as {to}");
        }
        let text = SEMI.replace("s::BIGINT", "s::VARCHAR").replace(".1::BIGINT", ".1::VARCHAR");
        assert_eq!(rewritten(&text), text, "a string");
    }
}

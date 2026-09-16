//! Turns correlated subqueries into set-based relational operators.
//!
//! Binding keeps correlation explicit as a dependent join.
//! Execution never implements that node because each rule here has to remove the dependency before the plan can run.
//! The first rule covers a scalar projection over a correlated filter.
//! Inner columns used by the filter are added to the projection as hidden outputs, the predicate is rewritten against those outputs, and the dependent join becomes an ordinary `SINGLE` join.

use std::collections::HashMap;

use rudb_common::{LogicalType, Result};
use rudb_plan::{ColumnBinding, ConjunctionOp, Expr, ExprRef, Node, NodeRef, Plan};

use crate::tables::produced;
use crate::walk;

/// Lowers every dependent join shape this implementation knows.
///
/// This is mandatory plan lowering rather than an optional optimization because the executor does not have, and deliberately will not grow, a per outer row implementation of a dependent join.
pub fn lower(plan: &mut Plan) -> Result<()> {
    let mut changed = false;
    let root = walk::restack(plan, plan.root(), &mut changed, &mut rewrite);
    if changed {
        plan.set_root(root);
    }
    Ok(())
}

fn rewrite(plan: &mut Plan, at: NodeRef) -> Option<NodeRef> {
    let Node::DependentJoin { left, right, kind, conditions } = *plan.node(at) else {
        return None;
    };
    let Node::Project { input: filtered, index, exprs, names } = *plan.node(right) else {
        return None;
    };
    let Node::Filter { input, predicate } = *plan.node(filtered) else {
        return None;
    };

    let outer = produced(plan, left);
    let inner = produced(plan, input);
    let mut correlated = Vec::new();
    let mut local = Vec::new();
    split(plan, predicate, &mut |part| {
        let mut saw_outer = false;
        let mut valid = true;
        walk::columns(plan, part, &mut |binding| {
            saw_outer |= outer.contains(binding.table);
            valid &= outer.contains(binding.table) || inner.contains(binding.table);
        });
        if saw_outer && valid {
            correlated.push(part);
        } else {
            local.push(part);
        }
    });
    if correlated.is_empty() {
        return None;
    }

    let mut projected = plan.expr_list(exprs).to_vec();
    let mut projected_names = plan.name_list(names).to_vec();
    let mut outputs = HashMap::new();
    for (position, &expr) in projected.iter().enumerate() {
        if let Expr::Column(binding) = *plan.expr(expr) {
            outputs.insert(binding, position);
        }
    }
    for &condition in &correlated {
        let mut bindings = Vec::new();
        walk::columns(plan, condition, &mut |binding| {
            if !bindings.contains(&binding) {
                bindings.push(binding);
            }
        });
        for binding in bindings {
            if !inner.contains(binding.table) || outputs.contains_key(&binding) {
                continue;
            }
            let position = projected.len();
            let column = find_column_expr(plan, condition, binding)
                .expect("the condition holds this column");
            let source = plan.add_expr_at(
                Expr::Column(binding),
                plan.expr_type(column).clone(),
                plan.expr_span(column),
            );
            projected.push(source);
            projected_names.push(plan.intern(&format!("__correlated_{position}")));
            outputs.insert(binding, position);
        }
    }

    let rewritten: Vec<ExprRef> = correlated
        .into_iter()
        .map(|condition| replace_inner(plan, condition, index, &outputs))
        .collect();
    let input = make_filter(plan, input, local);
    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&projected_names);
    let right = plan.add_node(Node::Project { input, index, exprs, names });
    let all: Vec<ExprRef> = plan.expr_list(conditions).iter().copied().chain(rewritten).collect();
    let conditions = plan.add_expr_list(&all);
    Some(plan.add_node(Node::Join { left, right, kind, conditions }))
}

fn split(plan: &Plan, expr: ExprRef, found: &mut impl FnMut(ExprRef)) {
    if let Expr::Conjunction { op: ConjunctionOp::And, children } = *plan.expr(expr) {
        for &child in plan.expr_list(children) {
            split(plan, child, found);
        }
    } else {
        found(expr);
    }
}

fn make_filter(plan: &mut Plan, input: NodeRef, parts: Vec<ExprRef>) -> NodeRef {
    let predicate = match parts.len() {
        0 => return input,
        1 => parts[0],
        _ => {
            let children = plan.add_expr_list(&parts);
            plan.add_expr(
                Expr::Conjunction { op: ConjunctionOp::And, children },
                LogicalType::Boolean,
            )
        }
    };
    plan.add_node(Node::Filter { input, predicate })
}

fn replace_inner(
    plan: &mut Plan,
    expr: ExprRef,
    index: u32,
    outputs: &HashMap<ColumnBinding, usize>,
) -> ExprRef {
    if let Expr::Column(binding) = *plan.expr(expr) {
        let Some(&position) = outputs.get(&binding) else {
            return expr;
        };
        let ty = plan.expr_type(expr).clone();
        let span = plan.expr_span(expr);
        return plan.add_expr_at(
            Expr::Column(ColumnBinding::new(
                index,
                u32::try_from(position).expect("projection width"),
            )),
            ty,
            span,
        );
    }
    walk::rebuild(plan, expr, &mut |plan, child| replace_inner(plan, child, index, outputs))
}

fn find_column_expr(plan: &Plan, expr: ExprRef, wanted: ColumnBinding) -> Option<ExprRef> {
    if matches!(*plan.expr(expr), Expr::Column(binding) if binding == wanted) {
        return Some(expr);
    }
    match *plan.expr(expr) {
        Expr::Column(_) | Expr::Constant(_) => None,
        Expr::Cast { input, .. } => find_column_expr(plan, input, wanted),
        Expr::Compare { left, right, .. } => {
            find_column_expr(plan, left, wanted).or_else(|| find_column_expr(plan, right, wanted))
        }
        Expr::Conjunction { children, .. } | Expr::Function { args: children, .. } => {
            plan.expr_list(children).iter().find_map(|&child| find_column_expr(plan, child, wanted))
        }
        Expr::Aggregate { args, filter, .. } => plan
            .expr_list(args)
            .iter()
            .chain(filter.iter())
            .find_map(|&child| find_column_expr(plan, child, wanted)),
        Expr::Case { arms, otherwise } => plan
            .arm_list(arms)
            .iter()
            .flat_map(|arm| [arm.when, arm.then])
            .chain(otherwise)
            .find_map(|child| find_column_expr(plan, child, wanted)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scalar_filter_becomes_a_single_join_with_a_hidden_key() {
        let text = "DependentJoin SINGLE on=[]\n  Get memory.main.outer AS o #0 [k::INTEGER]\n  Project #2 [#1.1::INTEGER AS value]\n    Filter (#1.0::INTEGER = #0.0::INTEGER)::BOOLEAN\n      Get memory.main.inner AS i #1 [k::INTEGER, value::INTEGER]\n";
        let mut plan = Plan::parse(text).expect("a correlated scalar plan");
        lower(&mut plan).expect("unnesting succeeds");
        plan.validate().expect("the ordinary join is valid");
        let after = plan.to_string();
        assert!(after.starts_with("Join SINGLE"), "{after}");
        assert!(after.contains("#1.0::INTEGER AS __correlated_1"), "{after}");
        assert!(after.contains("#2.1::INTEGER = #0.0::INTEGER"), "{after}");
        assert!(!after.contains("DependentJoin"), "{after}");
    }
}

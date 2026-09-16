//! Turns correlated subqueries into set-based relational operators.
//!
//! Binding keeps correlation explicit as a dependent join.
//! Execution never implements that node because each rule here has to remove the dependency before the plan can run.
//! Scalar projections over correlated filters carry inner filter columns as hidden outputs before the dependent join becomes an ordinary `SINGLE` join.
//! Scalar aggregates add equality correlation keys to their grouping, so the inner input is still scanned and aggregated once rather than once per outer row.

use std::collections::HashMap;

use rudb_common::{LogicalType, Result, Value};
use rudb_plan::{
    ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan,
};

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
    if let Some(join) = exists(plan, left, right, kind, conditions) {
        return Some(join);
    }
    if let Some(join) = mark(plan, left, right, kind, conditions) {
        return Some(join);
    }
    if let Some(join) = scalar_aggregate(plan, left, right, kind, conditions) {
        return Some(join);
    }
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

/// Adds equality correlation keys to a scalar aggregate's grouping and joins the grouped result
/// back to the outer input. Aggregates other than counts already have the right missing-group
/// result because a missing joined row is NULL. Counts need a domain join and are left for the
/// next rule rather than being lowered with the wrong empty-input value.
fn scalar_aggregate(
    plan: &mut Plan,
    left: NodeRef,
    right: NodeRef,
    kind: JoinKind,
    conditions: rudb_plan::Slice,
) -> Option<NodeRef> {
    if kind != JoinKind::Single {
        return None;
    }
    let Node::Project { input: grouped, index, exprs, names } = *plan.node(right) else {
        return None;
    };
    let Node::Aggregate { input: filtered, index: aggregate_index, groups, aggregates } =
        *plan.node(grouped)
    else {
        return None;
    };
    if plan.expr_list(aggregates).iter().any(|&call| {
        matches!(*plan.expr(call), Expr::Aggregate { name, .. } if matches!(plan.string(name), "count" | "count_star"))
    }) {
        return None;
    }
    let Node::Filter { input, predicate } = *plan.node(filtered) else {
        return None;
    };

    let outer = produced(plan, left);
    let inner = produced(plan, input);
    let mut correlated = Vec::new();
    let mut local = Vec::new();
    split(plan, predicate, &mut |part| {
        if equality_key(plan, part, &outer, &inner).is_some() {
            correlated.push(part);
        } else {
            local.push(part);
        }
    });
    if correlated.is_empty() || local.iter().any(|&part| reads(plan, part, &outer)) {
        return None;
    }

    let original_groups = plan.expr_list(groups).to_vec();
    let mut grouped_exprs = original_groups.clone();
    let mut inner_keys = Vec::new();
    for &condition in &correlated {
        let (binding, expr) = equality_key(plan, condition, &outer, &inner)?;
        if inner_keys.iter().any(|(held, _, _)| *held == binding) {
            continue;
        }
        let output = grouped_exprs.len();
        grouped_exprs.push(expr);
        inner_keys.push((binding, expr, output));
    }

    let added = inner_keys.len();
    let projected: Vec<ExprRef> = plan
        .expr_list(exprs)
        .to_vec()
        .into_iter()
        .map(|expr| {
            shift_aggregate_outputs(plan, expr, aggregate_index, original_groups.len(), added)
        })
        .collect();
    let mut projected_names = plan.name_list(names).to_vec();
    let mut outputs = HashMap::new();
    let mut projected = projected;
    for (binding, source, aggregate_output) in inner_keys {
        let output = projected.len();
        projected.push(plan.add_expr_at(
            Expr::Column(ColumnBinding::new(
                aggregate_index,
                u32::try_from(aggregate_output).ok()?,
            )),
            plan.expr_type(source).clone(),
            plan.expr_span(source),
        ));
        projected_names.push(plan.intern(&format!("__correlated_{output}")));
        outputs.insert(binding, output);
    }

    let input = make_filter(plan, input, local);
    let groups = plan.add_expr_list(&grouped_exprs);
    let grouped =
        plan.add_node(Node::Aggregate { input, index: aggregate_index, groups, aggregates });
    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&projected_names);
    let right = plan.add_node(Node::Project { input: grouped, index, exprs, names });
    let rewritten: Vec<ExprRef> = correlated
        .into_iter()
        .map(|condition| replace_inner(plan, condition, index, &outputs))
        .collect();
    let all: Vec<ExprRef> = plan.expr_list(conditions).iter().copied().chain(rewritten).collect();
    let conditions = plan.add_expr_list(&all);
    Some(plan.add_node(Node::Join { left, right, kind, conditions }))
}

fn shift_aggregate_outputs(
    plan: &mut Plan,
    expr: ExprRef,
    aggregate_index: u32,
    groups: usize,
    added: usize,
) -> ExprRef {
    if let Expr::Column(binding) = *plan.expr(expr) {
        if binding.table == aggregate_index && binding.column as usize >= groups {
            let ty = plan.expr_type(expr).clone();
            let span = plan.expr_span(expr);
            return plan.add_expr_at(
                Expr::Column(ColumnBinding::new(
                    binding.table,
                    binding.column + u32::try_from(added).expect("aggregate width"),
                )),
                ty,
                span,
            );
        }
        return expr;
    }
    walk::rebuild(plan, expr, &mut |plan, child| {
        shift_aggregate_outputs(plan, child, aggregate_index, groups, added)
    })
}

fn mark(
    plan: &mut Plan,
    left: NodeRef,
    right: NodeRef,
    kind: JoinKind,
    conditions: rudb_plan::Slice,
) -> Option<NodeRef> {
    if kind != JoinKind::Mark {
        return None;
    }
    let Node::Project { input: selected, index, exprs, names } = *plan.node(right) else {
        return None;
    };
    let mut projected = plan.expr_list(exprs).to_vec();
    let mut projected_names = plan.name_list(names).to_vec();
    if projected.len() < 2 {
        return None;
    }
    let Node::Project {
        input: filtered,
        index: selected_index,
        exprs: selected_exprs,
        names: selected_names,
    } = *plan.node(selected)
    else {
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
        if reads(plan, part, &outer) {
            correlated.push(part);
        } else {
            local.push(part);
        }
    });
    if correlated.is_empty() {
        return None;
    }

    let mut selected_outputs = plan.expr_list(selected_exprs).to_vec();
    let mut selected_output_names = plan.name_list(selected_names).to_vec();
    let mut outputs = HashMap::new();
    for (position, &expr) in projected[..projected.len() - 1].iter().enumerate() {
        let Expr::Column(binding) = *plan.expr(expr) else {
            continue;
        };
        if binding.table != selected_index {
            continue;
        }
        let source = *selected_outputs.get(binding.column as usize)?;
        if let Expr::Column(source_binding) = *plan.expr(source) {
            outputs.insert(source_binding, position);
        }
    }
    for &condition in &correlated {
        let mut bindings = Vec::new();
        walk::columns(plan, condition, &mut |binding| {
            if inner.contains(binding.table)
                && !outputs.contains_key(&binding)
                && !bindings.contains(&binding)
            {
                bindings.push(binding);
            }
        });
        for binding in bindings {
            let source = find_column_expr(plan, condition, binding)?;
            let selected_position = selected_outputs.len();
            selected_outputs.push(source);
            selected_output_names.push(plan.intern(&format!("__correlated_{selected_position}")));
            let projected_position = projected.len();
            let projected_source = plan.add_expr_at(
                Expr::Column(ColumnBinding::new(
                    selected_index,
                    u32::try_from(selected_position).ok()?,
                )),
                plan.expr_type(source).clone(),
                plan.expr_span(source),
            );
            projected.push(projected_source);
            projected_names.push(plan.intern(&format!("__correlated_{projected_position}")));
            outputs.insert(binding, projected_position);
        }
    }

    for &condition in &correlated {
        let mut valid = true;
        walk::columns(plan, condition, &mut |binding| {
            valid &= outer.contains(binding.table)
                || (inner.contains(binding.table) && outputs.contains_key(&binding));
        });
        if !valid {
            return None;
        }
    }

    let input = make_filter(plan, input, local);
    let selected_exprs = plan.add_expr_list(&selected_outputs);
    let selected_names = plan.add_name_list(&selected_output_names);
    let selected = plan.add_node(Node::Project {
        input,
        index: selected_index,
        exprs: selected_exprs,
        names: selected_names,
    });
    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&projected_names);
    let right = plan.add_node(Node::Project { input: selected, index, exprs, names });
    let mut all = plan.expr_list(conditions).to_vec();
    for condition in correlated {
        let condition = replace_inner(plan, condition, index, &outputs);
        let span = plan.expr_span(condition);
        let value = plan.add_value(Value::Boolean(true));
        let truth = plan.add_expr_at(Expr::Constant(value), LogicalType::Boolean, span);
        all.push(plan.add_expr_at(
            Expr::Compare { op: CompareOp::NotDistinctFrom, left: condition, right: truth },
            LogicalType::Boolean,
            span,
        ));
    }
    let conditions = plan.add_expr_list(&all);
    Some(plan.add_node(Node::Join { left, right, kind, conditions }))
}

fn exists(
    plan: &mut Plan,
    left: NodeRef,
    right: NodeRef,
    kind: JoinKind,
    conditions: rudb_plan::Slice,
) -> Option<NodeRef> {
    let Node::Project { input: limited, index, exprs: marker_exprs, names: marker_names } =
        *plan.node(right)
    else {
        return None;
    };
    let [marker] = plan.expr_list(marker_exprs) else {
        return None;
    };
    let marker = *marker;
    let Node::Limit { input: selected, count: Some(1), offset: 0 } = *plan.node(limited) else {
        return None;
    };
    let Node::Project { input: filtered, .. } = *plan.node(selected) else {
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
        if equality_key(plan, part, &outer, &inner).is_some() {
            correlated.push(part);
        } else {
            local.push(part);
        }
    });
    if correlated.is_empty() || local.iter().any(|&part| reads(plan, part, &outer)) {
        return None;
    }

    let mut keys = Vec::new();
    let mut outputs = HashMap::new();
    for &condition in &correlated {
        let (binding, expr) = equality_key(plan, condition, &outer, &inner)?;
        if outputs.contains_key(&binding) {
            continue;
        }
        outputs.insert(binding, keys.len() + 1);
        keys.push(expr);
    }
    let input = make_filter(plan, input, local);
    let grouped = walk::fresh_index(plan);
    let groups = plan.add_expr_list(&keys);
    let aggregates = plan.add_expr_list(&[]);
    let input = plan.add_node(Node::Aggregate { input, index: grouped, groups, aggregates });

    let mut projected = vec![marker];
    let mut names = plan.name_list(marker_names).to_vec();
    for (position, &key) in keys.iter().enumerate() {
        let source = plan.add_expr_at(
            Expr::Column(ColumnBinding::new(grouped, u32::try_from(position).ok()?)),
            plan.expr_type(key).clone(),
            plan.expr_span(key),
        );
        projected.push(source);
        names.push(plan.intern(&format!("__correlated_{}", position + 1)));
    }
    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&names);
    let right = plan.add_node(Node::Project { input, index, exprs, names });
    let mut all = plan.expr_list(conditions).to_vec();
    for condition in correlated {
        all.push(replace_inner(plan, condition, index, &outputs));
    }
    let conditions = plan.add_expr_list(&all);
    Some(plan.add_node(Node::Join { left, right, kind, conditions }))
}

fn equality_key(
    plan: &Plan,
    expr: ExprRef,
    outer: &crate::tables::TableSet,
    inner: &crate::tables::TableSet,
) -> Option<(ColumnBinding, ExprRef)> {
    let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(expr) else {
        return None;
    };
    match (plan.expr(left), plan.expr(right)) {
        (Expr::Column(inner_column), Expr::Column(outer_column))
            if inner.contains(inner_column.table) && outer.contains(outer_column.table) =>
        {
            Some((*inner_column, left))
        }
        (Expr::Column(outer_column), Expr::Column(inner_column))
            if outer.contains(outer_column.table) && inner.contains(inner_column.table) =>
        {
            Some((*inner_column, right))
        }
        _ => None,
    }
}

fn reads(plan: &Plan, expr: ExprRef, tables: &crate::tables::TableSet) -> bool {
    let mut yes = false;
    walk::columns(plan, expr, &mut |binding| yes |= tables.contains(binding.table));
    yes
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

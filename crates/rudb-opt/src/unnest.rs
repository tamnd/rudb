//! Turns correlated subqueries into set-based relational operators.
//!
//! Binding keeps correlation explicit as a dependent join.
//! Execution never implements that node because each rule here has to remove the dependency before the plan can run.
//! Scalar projections over correlated filters carry inner filter columns as hidden outputs before the dependent join becomes an ordinary `SINGLE` join.
//! Scalar aggregates add equality keys directly to their grouping or join against a distinct outer-key domain for arbitrary predicates, so the inner input is still scanned and aggregated once rather than once per outer row.

use std::collections::HashMap;

use rudb_common::{LogicalType, Result, Value};
use rudb_plan::{
    Bound, BuildSide, ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node,
    NodeRef, Plan,
};

use crate::domain;
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
    if let Some(join) = exists_domain(plan, left, right, kind, conditions) {
        return Some(join);
    }
    if let Some(join) = mark(plan, left, right, kind, conditions) {
        return Some(join);
    }
    if let Some(join) = scalar_aggregate_domain(plan, left, right, kind, conditions) {
        return Some(join);
    }
    if let Some(join) = scalar_count_aggregate(plan, left, right, kind, conditions) {
        return Some(join);
    }
    if let Some(join) = scalar_aggregate(plan, left, right, kind, conditions) {
        return Some(join);
    }
    if let Some(join) = scalar_projection_domain(plan, left, right, kind, conditions) {
        return Some(join);
    }
    if let Some(join) = scalar_correlated_filter(plan, left, right, kind, conditions) {
        return Some(join);
    }
    domain::lower(plan, left, right, kind, conditions)
}

/// Turns a correlated filter under a scalar projection into an ordinary join on the keys it reads.
///
/// The shape the binder produces for the common correlated subquery, and the plan it gives is the
/// one an equality join would have been written as by hand. The general rule in [`crate::domain`]
/// answers the same query and answers it with a domain and a second read of the outer side, so this
/// is asked first rather than left out.
fn scalar_correlated_filter(
    plan: &mut Plan,
    left: NodeRef,
    right: NodeRef,
    kind: JoinKind,
    conditions: rudb_plan::Slice,
) -> Option<NodeRef> {
    let Node::Project { input: filtered, index, exprs, names } = *plan.node(right) else {
        return None;
    };
    let Node::Filter { input, predicate } = *plan.node(filtered) else {
        return None;
    };

    let outer = produced(plan, left);
    // The projection becomes the right side of an ordinary join, where a column of the left side is
    // not in scope. A projection that reads one cannot go there, so this rule stands aside and the
    // general domain rule takes the query, which hands the outer column to the right side properly.
    // Without this, `SELECT (SELECT s1.i FROM t WHERE s1.i = i) FROM t s1` came out as an internal
    // error about a column not being in a schema: the outer column is a column expression, so it
    // was taken below as an output the condition's own reference to it could be rewritten against,
    // and the join condition ended up comparing the right side with itself. That is #993.
    let mut reads_outer = false;
    for expr in plan.expr_list(exprs).to_vec() {
        walk::columns(plan, expr, &mut |binding| reads_outer |= outer.contains(binding.table));
    }
    if reads_outer {
        return None;
    }
    // Everything under the filter goes to the right side of that join as well, so it has to be free
    // of the outer row for the same reason the projection does. A `HAVING` that reads the outer row
    // is the shape that reaches this: it binds as a filter above the aggregate, so the filter this
    // rule matches is the `HAVING` one and the correlated filter the query really has is two nodes
    // further down, where nothing here was looking. That is #995. [`relation_correlated`] is the
    // same check in the rules below and its note is the general version of this paragraph.
    if relation_correlated(plan, input, &outer) {
        return None;
    }
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
    Some(plan.add_node(Node::Join { left, right, kind, conditions, build: BuildSide::default() }))
}

/// Replays a scalar projection over the distinct outer values it reads.
///
/// A scalar query such as `SELECT (SELECT outer.k + inner.x)` has no inner filter from which to
/// extract a join key. Its independent input is crossed with the outer-key domain, the projection
/// is rewritten against that domain, and a null-safe lookup attaches the result to every original
/// outer row. The final `SINGLE` join still enforces the scalar row-count rule.
fn scalar_projection_domain(
    plan: &mut Plan,
    left: NodeRef,
    right: NodeRef,
    kind: JoinKind,
    conditions: rudb_plan::Slice,
) -> Option<NodeRef> {
    if kind != JoinKind::Single {
        return None;
    }
    let Node::Project { input, index, exprs, names } = *plan.node(right) else {
        return None;
    };
    let outer = produced(plan, left);
    if !independent_leaf(plan, input, &outer) {
        return None;
    }
    let projected = plan.expr_list(exprs).to_vec();
    let mut outer_keys = Vec::new();
    for &expr in &projected {
        walk::columns(plan, expr, &mut |binding| {
            if outer.contains(binding.table) && !outer_keys.contains(&binding) {
                outer_keys.push(binding);
            }
        });
    }
    if outer_keys.is_empty() {
        return None;
    }
    let outer_exprs: Vec<ExprRef> = outer_keys
        .iter()
        .map(|&binding| projected.iter().find_map(|&expr| find_column_expr(plan, expr, binding)))
        .collect::<Option<_>>()?;
    let domain_index = walk::fresh_index(plan);
    let groups = plan.add_expr_list(&outer_exprs);
    let aggregates = plan.add_expr_list(&[]);
    let domain_input = domain::narrow(plan, left, &outer_keys);
    let domain = plan.add_node(Node::Aggregate {
        input: domain_input,
        index: domain_index,
        groups,
        aggregates,
    });
    let replay_input = if matches!(plan.node(input), Node::Dummy) {
        domain
    } else {
        plan.add_node(Node::CrossProduct { left: domain, right: input })
    };
    let outputs: HashMap<ColumnBinding, usize> =
        outer_keys.iter().copied().enumerate().map(|(position, key)| (key, position)).collect();
    let mut rewritten: Vec<ExprRef> = projected
        .into_iter()
        .map(|expr| replace_inner(plan, expr, domain_index, &outputs))
        .collect();
    let mut projected_names = plan.name_list(names).to_vec();
    for (position, &source) in outer_exprs.iter().enumerate() {
        let output = rewritten.len();
        rewritten.push(plan.add_expr_at(
            Expr::Column(ColumnBinding::new(domain_index, u32::try_from(position).ok()?)),
            plan.expr_type(source).clone(),
            plan.expr_span(source),
        ));
        projected_names.push(plan.intern(&format!("__correlated_{output}")));
    }
    let visible = rewritten.len() - outer_exprs.len();
    let exprs = plan.add_expr_list(&rewritten);
    let names = plan.add_name_list(&projected_names);
    let right = plan.add_node(Node::Project { input: replay_input, index, exprs, names });

    let mut all = plan.expr_list(conditions).to_vec();
    for (position, &outer_expr) in outer_exprs.iter().enumerate() {
        let right_expr = plan.add_expr_at(
            Expr::Column(ColumnBinding::new(index, u32::try_from(visible + position).ok()?)),
            plan.expr_type(outer_expr).clone(),
            plan.expr_span(outer_expr),
        );
        all.push(plan.add_expr_at(
            Expr::Compare { op: CompareOp::NotDistinctFrom, left: outer_expr, right: right_expr },
            LogicalType::Boolean,
            plan.expr_span(outer_expr),
        ));
    }
    let conditions = plan.add_expr_list(&all);
    Some(plan.add_node(Node::Join { left, right, kind, conditions, build: BuildSide::default() }))
}

/// Evaluates arbitrary correlated existence predicates once over a distinct domain of outer keys.
/// Equality predicates take the cheaper grouped-inner rule below, while inequalities and
/// expressions use this general rule instead of falling back to one inner execution per outer row.
fn exists_domain(
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
    let Node::Limit { input: selected, count: Bound::Rows(1), offset: Bound::Rows(0) } =
        *plan.node(limited)
    else {
        return None;
    };
    let Node::Project { input: filtered, .. } = *plan.node(selected) else {
        return None;
    };
    let Node::Filter { input, predicate } = *plan.node(filtered) else {
        return None;
    };

    let outer = produced(plan, left);
    if relation_correlated(plan, input, &outer) {
        return None;
    }
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

    let mut outer_keys = Vec::new();
    for &condition in &correlated {
        walk::columns(plan, condition, &mut |binding| {
            if outer.contains(binding.table) && !outer_keys.contains(&binding) {
                outer_keys.push(binding);
            }
        });
    }
    if outer_keys.is_empty() {
        return None;
    }
    let mut outer_exprs = Vec::new();
    for &binding in &outer_keys {
        outer_exprs.push(find_column_expr(plan, predicate, binding)?);
    }
    let domain_index = walk::fresh_index(plan);
    let groups = plan.add_expr_list(&outer_exprs);
    let aggregates = plan.add_expr_list(&[]);
    let domain_input = domain::narrow(plan, left, &outer_keys);
    let domain = plan.add_node(Node::Aggregate {
        input: domain_input,
        index: domain_index,
        groups,
        aggregates,
    });

    let domain_outputs: HashMap<ColumnBinding, usize> =
        outer_keys.iter().copied().enumerate().map(|(position, key)| (key, position)).collect();
    let rewritten: Vec<ExprRef> = correlated
        .into_iter()
        .map(|condition| replace_inner(plan, condition, domain_index, &domain_outputs))
        .collect();
    let inner = make_filter(plan, input, local);
    let domain_conditions = plan.add_expr_list(&rewritten);
    let matches = plan.add_node(Node::Join {
        left: domain,
        right: inner,
        kind: JoinKind::Inner,
        conditions: domain_conditions,
        build: BuildSide::default(),
    });
    let grouped_keys: Vec<ExprRef> = outer_exprs
        .iter()
        .enumerate()
        .map(|(position, source)| {
            plan.add_expr_at(
                Expr::Column(ColumnBinding::new(domain_index, u32::try_from(position).unwrap())),
                plan.expr_type(*source).clone(),
                plan.expr_span(*source),
            )
        })
        .collect();
    let grouped_index = walk::fresh_index(plan);
    let groups = plan.add_expr_list(&grouped_keys);
    let aggregates = plan.add_expr_list(&[]);
    let matches =
        plan.add_node(Node::Aggregate { input: matches, index: grouped_index, groups, aggregates });

    let mut projected = vec![marker];
    let mut names = plan.name_list(marker_names).to_vec();
    for (position, &source) in outer_exprs.iter().enumerate() {
        projected.push(plan.add_expr_at(
            Expr::Column(ColumnBinding::new(grouped_index, u32::try_from(position).ok()?)),
            plan.expr_type(source).clone(),
            plan.expr_span(source),
        ));
        names.push(plan.intern(&format!("__correlated_{}", position + 1)));
    }
    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&names);
    let right = plan.add_node(Node::Project { input: matches, index, exprs, names });

    let mut all = plan.expr_list(conditions).to_vec();
    for (position, &outer_expr) in outer_exprs.iter().enumerate() {
        let right_expr = plan.add_expr_at(
            Expr::Column(ColumnBinding::new(index, u32::try_from(position + 1).ok()?)),
            plan.expr_type(outer_expr).clone(),
            plan.expr_span(outer_expr),
        );
        all.push(plan.add_expr_at(
            Expr::Compare { op: CompareOp::NotDistinctFrom, left: outer_expr, right: right_expr },
            LogicalType::Boolean,
            plan.expr_span(outer_expr),
        ));
    }
    let conditions = plan.add_expr_list(&all);
    Some(plan.add_node(Node::Join { left, right, kind, conditions, build: BuildSide::default() }))
}

/// Decorrelates scalar aggregates whose predicates need more than equality keys.
///
/// The inner side carries a non-null marker through a left join from the distinct outer domain.
/// Counts use that marker to distinguish a real inner row from the padded row that represents an
/// empty group. This also covers predicates such as `IS DISTINCT FROM` and predicates that read
/// only the outer row, where no ordinary inner column is a reliable presence test.
fn scalar_aggregate_domain(
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

    let outer = produced(plan, left);
    let (input, predicate) = match *plan.node(filtered) {
        Node::Filter { input, predicate } => (input, Some(predicate)),
        _ if independent_leaf(plan, filtered, &outer) => (filtered, None),
        _ => return None,
    };
    if relation_correlated(plan, input, &outer) {
        return None;
    }
    let inner = produced(plan, input);
    let mut correlated = Vec::new();
    let mut local = Vec::new();
    if let Some(predicate) = predicate {
        split(plan, predicate, &mut |part| {
            if reads(plan, part, &outer) {
                correlated.push(part);
            } else {
                local.push(part);
            }
        });
    }
    let expression_correlated = plan
        .expr_list(groups)
        .iter()
        .chain(plan.expr_list(aggregates))
        .any(|&expr| reads(plan, expr, &outer));
    if !expression_correlated
        && (correlated.is_empty()
            || correlated.iter().all(|&part| equality_pair(plan, part, &outer, &inner).is_some()))
    {
        return None;
    }

    let mut outer_keys = Vec::new();
    let mut inner_keys = Vec::new();
    let mut relevant = correlated.clone();
    relevant.extend_from_slice(plan.expr_list(groups));
    relevant.extend_from_slice(plan.expr_list(aggregates));
    // The projection is in here so that an outer column it reads becomes a domain key too. It ends
    // up on the right side of an ordinary join where a column of the left side is not in scope, so
    // it has to read the domain instead, and it can only do that if the domain carries it. It does
    // not decide whether this rule takes the query, because a projection that reads the outer row
    // and nothing else is a query the rules below answer better. That is #995.
    relevant.extend_from_slice(plan.expr_list(exprs));
    for &expr in &relevant {
        walk::columns(plan, expr, &mut |binding| {
            if outer.contains(binding.table) && !outer_keys.contains(&binding) {
                outer_keys.push(binding);
            }
            if inner.contains(binding.table) && !inner_keys.contains(&binding) {
                inner_keys.push(binding);
            }
        });
    }
    if outer_keys.is_empty() {
        return None;
    }
    let source = |binding| relevant.iter().find_map(|&expr| find_column_expr(plan, expr, binding));
    let outer_exprs: Vec<ExprRef> =
        outer_keys.iter().map(|&binding| source(binding)).collect::<Option<_>>()?;
    let inner_exprs: Vec<ExprRef> =
        inner_keys.iter().map(|&binding| source(binding)).collect::<Option<_>>()?;

    let domain_index = walk::fresh_index(plan);
    let domain_groups = plan.add_expr_list(&outer_exprs);
    let no_aggregates = plan.add_expr_list(&[]);
    let domain_input = domain::narrow(plan, left, &outer_keys);
    let domain = plan.add_node(Node::Aggregate {
        input: domain_input,
        index: domain_index,
        groups: domain_groups,
        aggregates: no_aggregates,
    });
    let domain_outputs: HashMap<ColumnBinding, usize> =
        outer_keys.iter().copied().enumerate().map(|(position, key)| (key, position)).collect();

    let inner_input = make_filter(plan, input, local);
    let inner_index = walk::fresh_index(plan);
    let mut carried = inner_exprs.clone();
    let marker_position = carried.len();
    let marker = plan.add_constant(Value::Boolean(true));
    carried.push(marker);
    let carried_names: Vec<_> =
        (0..carried.len()).map(|position| plan.intern(&format!("__inner_{position}"))).collect();
    let carried = plan.add_expr_list(&carried);
    let carried_names = plan.add_name_list(&carried_names);
    let inner_input = plan.add_node(Node::Project {
        input: inner_input,
        index: inner_index,
        exprs: carried,
        names: carried_names,
    });
    let inner_outputs: HashMap<ColumnBinding, usize> =
        inner_keys.iter().copied().enumerate().map(|(position, key)| (key, position)).collect();
    let presence = plan.add_expr_at(
        Expr::Column(ColumnBinding::new(inner_index, u32::try_from(marker_position).ok()?)),
        LogicalType::Boolean,
        predicate.map_or_else(|| plan.expr_span(outer_exprs[0]), |expr| plan.expr_span(expr)),
    );

    let domain_conditions: Vec<ExprRef> = correlated
        .iter()
        .map(|&condition| {
            let condition = replace_inner(plan, condition, inner_index, &inner_outputs);
            replace_inner(plan, condition, domain_index, &domain_outputs)
        })
        .collect();
    let domain_conditions = plan.add_expr_list(&domain_conditions);
    let original_groups = plan.expr_list(groups).to_vec();
    // The left join is here to keep a row for a domain key no inner row matched, because an
    // ungrouped aggregate over an empty input is still one row. A subquery that groups is the other
    // case: an empty input produces no groups at all, so it returns no row and the scalar it was
    // read for is NULL. Keeping the padded row there invents a group, and since the group
    // expressions are rewritten to read the domain, a group written on an outer column has the
    // outer value in it and looks real. A `count(*)` over that invented group answered zero where
    // the answer is NULL. With a group there is nothing to pad, so the join is an inner one. #1013.
    let domain_kind = if original_groups.is_empty() { JoinKind::Left } else { JoinKind::Inner };
    let joined = plan.add_node(Node::Join {
        left: domain,
        right: inner_input,
        kind: domain_kind,
        conditions: domain_conditions,
        build: BuildSide::default(),
    });

    let mut grouped_exprs: Vec<ExprRef> = original_groups
        .iter()
        .map(|&group| {
            let group = replace_inner(plan, group, inner_index, &inner_outputs);
            replace_inner(plan, group, domain_index, &domain_outputs)
        })
        .collect();
    for (position, &source) in outer_exprs.iter().enumerate() {
        grouped_exprs.push(plan.add_expr_at(
            Expr::Column(ColumnBinding::new(domain_index, u32::try_from(position).ok()?)),
            plan.expr_type(source).clone(),
            plan.expr_span(source),
        ));
    }
    let calls: Vec<ExprRef> = plan
        .expr_list(aggregates)
        .to_vec()
        .into_iter()
        .map(|call| {
            let call = replace_inner(plan, call, inner_index, &inner_outputs);
            let call = replace_inner(plan, call, domain_index, &domain_outputs);
            count_with_presence(plan, call, presence)
        })
        .collect();

    let added = outer_exprs.len();
    // Every outer column the projection reads is one of the domain keys, and the regrouped aggregate
    // carries each of those as a group of its own, so the read is pointed at that group rather than
    // left reading a side of the join that is no longer underneath it.
    let regrouped: HashMap<ColumnBinding, usize> = outer_keys
        .iter()
        .copied()
        .enumerate()
        .map(|(position, key)| (key, original_groups.len() + position))
        .collect();
    let mut projected: Vec<ExprRef> = plan
        .expr_list(exprs)
        .to_vec()
        .into_iter()
        .map(|expr| {
            let expr =
                shift_aggregate_outputs(plan, expr, aggregate_index, original_groups.len(), added);
            replace_inner(plan, expr, aggregate_index, &regrouped)
        })
        .collect();
    let mut projected_names = plan.name_list(names).to_vec();
    for (position, &source) in outer_exprs.iter().enumerate() {
        let output = projected.len();
        projected.push(plan.add_expr_at(
            Expr::Column(ColumnBinding::new(
                aggregate_index,
                u32::try_from(original_groups.len() + position).ok()?,
            )),
            plan.expr_type(source).clone(),
            plan.expr_span(source),
        ));
        projected_names.push(plan.intern(&format!("__correlated_{output}")));
    }
    let groups = plan.add_expr_list(&grouped_exprs);
    let aggregates = plan.add_expr_list(&calls);
    let grouped = plan.add_node(Node::Aggregate {
        input: joined,
        index: aggregate_index,
        groups,
        aggregates,
    });
    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&projected_names);
    let right = plan.add_node(Node::Project { input: grouped, index, exprs, names });

    let mut all = plan.expr_list(conditions).to_vec();
    for (position, &outer_expr) in outer_exprs.iter().enumerate() {
        let right_expr = plan.add_expr_at(
            Expr::Column(ColumnBinding::new(
                index,
                u32::try_from(plan.expr_list(exprs).len() - added + position).ok()?,
            )),
            plan.expr_type(outer_expr).clone(),
            plan.expr_span(outer_expr),
        );
        all.push(plan.add_expr_at(
            Expr::Compare { op: CompareOp::NotDistinctFrom, left: outer_expr, right: right_expr },
            LogicalType::Boolean,
            plan.expr_span(outer_expr),
        ));
    }
    let conditions = plan.add_expr_list(&all);
    Some(plan.add_node(Node::Join { left, right, kind, conditions, build: BuildSide::default() }))
}

/// Builds the distinct outer-key domain needed by correlated counts.
///
/// Grouping only the matching inner rows would leave no row for a missing key, while a scalar
/// count over an empty input is zero. A left join from the distinct outer domain creates one
/// padded row for that key. Count filters include an inner-key presence test so the padded row is
/// not counted, while the other aggregates keep their ordinary NULL-on-empty behavior.
fn scalar_count_aggregate(
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
    // The padded row this rule creates is the answer to "a scalar count over an empty input is
    // zero", and that sentence is only true when the subquery has no `GROUP BY`. With one, an empty
    // input produces no groups at all, so the subquery returns no row and the answer is NULL rather
    // than zero. Standing aside sends the query to the rules below, which build the same domain
    // without putting a zero where there is no group. That is #1013.
    if !plan.expr_list(groups).is_empty() {
        return None;
    }
    if !plan.expr_list(aggregates).iter().any(|&call| {
        matches!(*plan.expr(call), Expr::Aggregate { name, .. } if matches!(plan.string(name), "count" | "count_star"))
    }) {
        return None;
    }
    let Node::Filter { input, predicate } = *plan.node(filtered) else {
        return None;
    };

    let outer = produced(plan, left);
    // The same argument as in `scalar_correlated_filter` and `scalar_aggregate`. The projection ends
    // up on the right side of an ordinary join and a column of the left side is not in scope there,
    // so a projection that reads the outer row sends the query to the general domain rule. #995.
    if plan.expr_list(exprs).to_vec().iter().any(|&expr| reads(plan, expr, &outer)) {
        return None;
    }
    if relation_correlated(plan, input, &outer) {
        return None;
    }
    let inner = produced(plan, input);
    let mut correlated = Vec::new();
    let mut local = Vec::new();
    split(plan, predicate, &mut |part| {
        if equality_pair(plan, part, &outer, &inner).is_some() {
            correlated.push(part);
        } else {
            local.push(part);
        }
    });
    if correlated.is_empty() || local.iter().any(|&part| reads(plan, part, &outer)) {
        return None;
    }

    let mut keys = Vec::new();
    for &condition in &correlated {
        let (binding, inner_expr, outer_expr) = equality_pair(plan, condition, &outer, &inner)?;
        if keys.iter().any(|(held, _, _)| *held == binding) {
            continue;
        }
        keys.push((binding, inner_expr, outer_expr));
    }
    let domain_index = walk::fresh_index(plan);
    let domain_groups: Vec<ExprRef> = keys.iter().map(|(_, _, outer_expr)| *outer_expr).collect();
    let domain_groups = plan.add_expr_list(&domain_groups);
    let no_aggregates = plan.add_expr_list(&[]);
    let domain_keys: Vec<ColumnBinding> = keys.iter().map(|(binding, _, _)| *binding).collect();
    let domain_input = domain::narrow(plan, left, &domain_keys);
    let domain = plan.add_node(Node::Aggregate {
        input: domain_input,
        index: domain_index,
        groups: domain_groups,
        aggregates: no_aggregates,
    });
    let inner_input = make_filter(plan, input, local);
    let mut domain_conditions = Vec::new();
    for (position, (_, inner_expr, outer_expr)) in keys.iter().enumerate() {
        let domain_key = plan.add_expr_at(
            Expr::Column(ColumnBinding::new(domain_index, u32::try_from(position).ok()?)),
            plan.expr_type(*outer_expr).clone(),
            plan.expr_span(*outer_expr),
        );
        let span = plan.expr_span(*inner_expr);
        domain_conditions.push(plan.add_expr_at(
            Expr::Compare { op: CompareOp::Equal, left: domain_key, right: *inner_expr },
            LogicalType::Boolean,
            span,
        ));
    }
    let domain_conditions = plan.add_expr_list(&domain_conditions);
    let joined = plan.add_node(Node::Join {
        left: domain,
        right: inner_input,
        kind: JoinKind::Left,
        conditions: domain_conditions,
        build: BuildSide::default(),
    });

    let presence = keys.first()?.1;
    let calls: Vec<ExprRef> = plan
        .expr_list(aggregates)
        .to_vec()
        .into_iter()
        .map(|call| count_with_presence(plan, call, presence))
        .collect();
    let original_groups = plan.expr_list(groups).to_vec();
    let mut grouped_exprs = original_groups.clone();
    for (position, (_, _, outer_expr)) in keys.iter().enumerate() {
        grouped_exprs.push(plan.add_expr_at(
            Expr::Column(ColumnBinding::new(domain_index, u32::try_from(position).ok()?)),
            plan.expr_type(*outer_expr).clone(),
            plan.expr_span(*outer_expr),
        ));
    }
    let added = keys.len();
    let mut projected: Vec<ExprRef> = plan
        .expr_list(exprs)
        .to_vec()
        .into_iter()
        .map(|expr| {
            shift_aggregate_outputs(plan, expr, aggregate_index, original_groups.len(), added)
        })
        .collect();
    let mut projected_names = plan.name_list(names).to_vec();
    let mut outputs = HashMap::new();
    for (position, (binding, _, outer_expr)) in keys.into_iter().enumerate() {
        let output = projected.len();
        projected.push(plan.add_expr_at(
            Expr::Column(ColumnBinding::new(
                aggregate_index,
                u32::try_from(original_groups.len() + position).ok()?,
            )),
            plan.expr_type(outer_expr).clone(),
            plan.expr_span(outer_expr),
        ));
        projected_names.push(plan.intern(&format!("__correlated_{output}")));
        outputs.insert(binding, output);
    }

    let groups = plan.add_expr_list(&grouped_exprs);
    let aggregates = plan.add_expr_list(&calls);
    let grouped = plan.add_node(Node::Aggregate {
        input: joined,
        index: aggregate_index,
        groups,
        aggregates,
    });
    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&projected_names);
    let right = plan.add_node(Node::Project { input: grouped, index, exprs, names });
    let rewritten: Vec<ExprRef> = correlated
        .into_iter()
        .map(|condition| {
            let condition = replace_inner(plan, condition, index, &outputs);
            null_safe_equality(plan, condition)
        })
        .collect();
    let all: Vec<ExprRef> = plan.expr_list(conditions).iter().copied().chain(rewritten).collect();
    let conditions = plan.add_expr_list(&all);
    Some(plan.add_node(Node::Join { left, right, kind, conditions, build: BuildSide::default() }))
}

fn null_safe_equality(plan: &mut Plan, expr: ExprRef) -> ExprRef {
    let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(expr) else {
        return expr;
    };
    plan.add_expr_at(
        Expr::Compare { op: CompareOp::NotDistinctFrom, left, right },
        LogicalType::Boolean,
        plan.expr_span(expr),
    )
}

fn count_with_presence(plan: &mut Plan, call: ExprRef, presence: ExprRef) -> ExprRef {
    let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(call) else {
        return call;
    };
    if !matches!(plan.string(name), "count" | "count_star") {
        return call;
    }
    let span = plan.expr_span(call);
    let null = plan.add_value(Value::Null);
    let null = plan.add_expr_at(Expr::Constant(null), plan.expr_type(presence).clone(), span);
    let present = plan.add_expr_at(
        Expr::Compare { op: CompareOp::DistinctFrom, left: presence, right: null },
        LogicalType::Boolean,
        span,
    );
    let filter = if let Some(filter) = filter {
        let children = plan.add_expr_list(&[filter, present]);
        Some(plan.add_expr_at(
            Expr::Conjunction { op: ConjunctionOp::And, children },
            LogicalType::Boolean,
            span,
        ))
    } else {
        Some(present)
    };
    let (name, args, distinct) = if plan.string(name) == "count_star" {
        let name = plan.intern("count");
        let args = plan.add_expr_list(&[presence]);
        (name, args, false)
    } else {
        (name, args, distinct)
    };
    plan.add_expr_at(
        Expr::Aggregate { name, args, distinct, filter },
        plan.expr_type(call).clone(),
        span,
    )
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
    // The projection becomes the right side of an ordinary join, where a column of the left side is
    // not in scope, so a projection that reads one cannot go there. This rule stands aside and the
    // general domain rule takes the query, which hands the outer column to the right side properly.
    // `SELECT (SELECT max(w) + o.k FROM i WHERE i.k = o.k) FROM o` is the shape, and it is the same
    // argument the guard in `scalar_correlated_filter` makes. That is #995.
    if plan.expr_list(exprs).to_vec().iter().any(|&expr| reads(plan, expr, &outer)) {
        return None;
    }
    if relation_correlated(plan, input, &outer) {
        return None;
    }
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
    Some(plan.add_node(Node::Join { left, right, kind, conditions, build: BuildSide::default() }))
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
    if relation_correlated(plan, input, &outer) {
        return None;
    }
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
    Some(plan.add_node(Node::Join { left, right, kind, conditions, build: BuildSide::default() }))
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
    let Node::Limit { input: selected, count: Bound::Rows(1), offset: Bound::Rows(0) } =
        *plan.node(limited)
    else {
        return None;
    };
    let Node::Project { input: filtered, .. } = *plan.node(selected) else {
        return None;
    };
    let Node::Filter { input, predicate } = *plan.node(filtered) else {
        return None;
    };

    let outer = produced(plan, left);
    if relation_correlated(plan, input, &outer) {
        return None;
    }
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
    Some(plan.add_node(Node::Join { left, right, kind, conditions, build: BuildSide::default() }))
}

fn equality_key(
    plan: &Plan,
    expr: ExprRef,
    outer: &crate::tables::TableSet,
    inner: &crate::tables::TableSet,
) -> Option<(ColumnBinding, ExprRef)> {
    equality_pair(plan, expr, outer, inner)
        .map(|(inner_binding, inner_expr, _)| (inner_binding, inner_expr))
}

fn equality_pair(
    plan: &Plan,
    expr: ExprRef,
    outer: &crate::tables::TableSet,
    inner: &crate::tables::TableSet,
) -> Option<(ColumnBinding, ExprRef, ExprRef)> {
    let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(expr) else {
        return None;
    };
    match (plan.expr(left), plan.expr(right)) {
        (Expr::Column(inner_column), Expr::Column(outer_column))
            if inner.contains(inner_column.table) && outer.contains(outer_column.table) =>
        {
            Some((*inner_column, left, right))
        }
        (Expr::Column(outer_column), Expr::Column(inner_column))
            if outer.contains(outer_column.table) && inner.contains(inner_column.table) =>
        {
            Some((*inner_column, right, left))
        }
        _ => None,
    }
}

fn reads(plan: &Plan, expr: ExprRef, tables: &crate::tables::TableSet) -> bool {
    let mut yes = false;
    walk::columns(plan, expr, &mut |binding| yes |= tables.contains(binding.table));
    yes
}

/// Whether the subquery's own relation reads the outer row, which is not a correlation these rules
/// can see.
///
/// Each rule here reads the correlation out of one place, a filter's predicate or a projection's
/// expressions, and builds the domain over the outer columns it finds there. A column of the outer
/// row read anywhere else is invisible to it, and the commonest way to write one is a second
/// subquery nested inside this one whose name resolved past this query to the one above it. The
/// binder hands such a name up to the query that owns it, which is right, so the query in between
/// is not correlated by it and plants its own relation with the reference still sitting in it.
/// These rules then put that relation under a join with the domain, where the outer side is not in
/// scope, and the column is asked of an operator that was never given it.
///
/// The general rule in [`crate::domain`] collects every outer column the whole subtree reads and
/// pushes a domain down to where each one is read, so the answer is to stand aside and let it take
/// the query. That is #999.
fn relation_correlated(plan: &Plan, input: NodeRef, outer: &crate::tables::TableSet) -> bool {
    domain::correlated(plan, input, outer)
}

fn independent_leaf(plan: &Plan, input: NodeRef, outer: &crate::tables::TableSet) -> bool {
    match *plan.node(input) {
        Node::Dummy | Node::Get { .. } => true,
        Node::Values { rows, .. } => plan
            .row_list(rows)
            .iter()
            .all(|row| plan.expr_list(*row).iter().all(|&expr| !reads(plan, expr, outer))),
        Node::TableFunction { args, settings, .. } => plan
            .expr_list(args)
            .iter()
            .chain(plan.expr_list(settings))
            .all(|&expr| !reads(plan, expr, outer)),
        _ => false,
    }
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
        Expr::Aggregate { args, filter, .. } | Expr::Window { args, filter, .. } => plan
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

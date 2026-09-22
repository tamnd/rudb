//! Walking one expression, without deciding what the walk is for.
//!
//! Four things every pass here ends up needing: rewrite the operands, list the columns, ask whether
//! asking twice can give two answers, and ask whether two expressions are the same expression. None
//! of them is interesting and all of them have a match arm per variant, so a variant added to `Expr`
//! and forgotten about is a compile error in one file instead of four.
//!
//! The same argument applies one level up, to nodes, for the two passes that put a new operator
//! somewhere other than the top: [`restack`] is the walk and [`replace_children`] is the match arm
//! per `Node` variant.
//!
//! Folding replaces an operand with its value where it has one, filter pushdown replaces a column
//! reference with whatever the operator below computes that column from, and transitive predicates
//! replace a column reference with the column an equality says it equals. Everything except that one
//! sentence is the same code in all three: visit each operand, and if any of them moved, append a new
//! expression carrying the operands that moved and the type the plan already recorded.
//!
//! Appending rather than editing in place is not a style choice. An expression may only refer to an
//! expression behind it in the arena, which [`Plan::validate`] checks and which is what makes a plan
//! acyclic by construction, so a rewritten operand has to be appended before the operator that reads
//! it can be.
//!
//! [`Plan::validate`]: rudb_plan::Plan::validate

use rudb_common::LogicalType;
use rudb_plan::{Arm, ColumnBinding, Expr, ExprRef, JoinKind, Node, NodeRef, Plan, Slice, SortKey};

use crate::fold::VOLATILE;

/// Rebuilds the tree under `at` bottom up, giving `step` the chance to put something above each node.
///
/// A node may only point at a node behind it in the arena, the same rule expressions follow, so a
/// pass that wants a new operator below an existing one cannot write it in place: the new operator
/// would be appended after the one that has to read it. What it does instead is rebuild the path
/// from the node it changed back up to the root, which is what this walk is.
///
/// `step` is given a node whose children are already rewritten and hands back either nothing, for a
/// node it has no opinion about, or whatever should stand in its place. It may append, and what it
/// appends is behind whatever the caller appends afterwards. `changed` is set when any step fired,
/// which is how the caller knows whether the root moved.
pub(crate) fn restack(
    plan: &mut Plan,
    at: NodeRef,
    changed: &mut bool,
    step: &mut impl FnMut(&mut Plan, NodeRef) -> Option<NodeRef>,
) -> NodeRef {
    let children = plan.node(at).children();
    let rebuilt: Vec<NodeRef> =
        children.into_iter().flatten().map(|child| restack(plan, child, changed, step)).collect();
    let mut here = at;
    if children.into_iter().flatten().zip(&rebuilt).any(|(was, &now)| was != now) {
        let mut node = plan.node(at).clone();
        let span = plan.node_span(at);
        replace_children(&mut node, &rebuilt);
        here = plan.add_node_at(node, span);
    }
    match step(plan, here) {
        Some(above) => {
            *changed = true;
            above
        }
        None => here,
    }
}

/// Points a node at a new set of children, in the order [`Node::children`] hands them back.
pub(crate) fn replace_children(node: &mut Node, children: &[NodeRef]) {
    match node {
        Node::Filter { input, .. }
        | Node::Project { input, .. }
        | Node::Aggregate { input, .. }
        | Node::Window { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::LimitPercent { input, .. }
        | Node::TopN { input, .. }
        | Node::Fetch { input, .. }
        | Node::TableFetch { input, .. }
        | Node::Distinct { input, .. } => *input = children[0],
        Node::Join { left, right, .. }
        | Node::LinkJoin { child: left, parent: right, .. }
        | Node::DependentJoin { left, right, .. }
        | Node::CrossProduct { left, right }
        | Node::SetOp { left, right, .. } => {
            *left = children[0];
            *right = children[1];
        }
        Node::MaterializedCte { definition, body, .. } => {
            *definition = children[0];
            *body = children[1];
        }
        Node::LateralFunction { input, .. } => *input = children[0],
        Node::Get { .. }
        | Node::Dummy
        | Node::Values { .. }
        | Node::TableFunction { .. }
        | Node::CteScan { .. } => {}
    }
}

/// Every column an operator produces, in the order it produces them, with the type of each.
///
/// What this is for is writing a projection that reproduces an operator's output. The width alone
/// is not enough for that: a projection needs an expression per column, and an expression that
/// reads a column needs the binding and the type of it.
///
/// `None` when the output cannot be described this way, which is not a failure to look hard enough
/// but a shape this answer does not fit. A semi or anti join produces the rows of one side and a
/// mark join produces one side plus a column that is not either side's, so a caller that took the
/// two inputs and put them end to end would be writing expressions that read a column nobody has.
/// Saying nothing is what lets the caller refuse rather than build that.
pub(crate) fn outputs(plan: &Plan, at: NodeRef) -> Option<Vec<(ColumnBinding, LogicalType)>> {
    match *plan.node(at) {
        Node::Get { index, columns, .. }
        | Node::Values { index, columns, .. }
        | Node::TableFunction { index, columns, .. }
        | Node::Fetch { index, columns, .. }
        | Node::TableFetch { index, columns, .. }
        | Node::CteScan { index, columns, .. } => Some(
            plan.field_list(columns)
                .iter()
                .enumerate()
                .map(|(position, field)| (binding(index, position), field.ty.clone()))
                .collect(),
        ),
        Node::Dummy => Some(Vec::new()),
        Node::Project { index, exprs, .. } => Some(listed(plan, index, 0, exprs)),
        Node::Aggregate { index, groups, aggregates, .. } => {
            let mut found = listed(plan, index, 0, groups);
            let width = found.len();
            found.extend(listed(plan, index, width, aggregates));
            Some(found)
        }
        // A window appends its results to the row it was given rather than replacing it, so its
        // input's columns are still there with the bindings they had.
        Node::Window { input, index, expressions, .. } => {
            let mut found = outputs(plan, input)?;
            found.extend(listed(plan, index, 0, expressions));
            Some(found)
        }
        // A lateral call appends the function's columns to the row it was called for, the way a
        // cross product appends the right side's, so its input's bindings are still what they were.
        Node::LateralFunction { input, index, columns, .. } => {
            let mut found = outputs(plan, input)?;
            found.extend(
                plan.field_list(columns)
                    .iter()
                    .enumerate()
                    .map(|(position, field)| (binding(index, position), field.ty.clone())),
            );
            Some(found)
        }
        Node::Filter { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::LimitPercent { input, .. }
        | Node::TopN { input, .. }
        | Node::Distinct { input, .. } => outputs(plan, input),
        // A materialisation produces what the query reading it produces. The held columns go to the
        // scans that name it and never past this node.
        Node::MaterializedCte { body, .. } => outputs(plan, body),
        // A set operation binds its output against an index of its own, since it is neither side's
        // columns, and the binder already required the two sides to agree on how many there are.
        Node::SetOp { left, index, .. } => Some(
            outputs(plan, left)?
                .into_iter()
                .enumerate()
                .map(|(position, (_, ty))| (binding(index, position), ty))
                .collect(),
        ),
        Node::Join {
            left,
            right,
            kind:
                JoinKind::Inner
                | JoinKind::Left
                | JoinKind::Right
                | JoinKind::Full
                | JoinKind::Single
                | JoinKind::Positional,
            ..
        }
        | Node::DependentJoin { left, right, .. }
        | Node::CrossProduct { left, right } => {
            let mut found = outputs(plan, left)?;
            found.extend(outputs(plan, right)?);
            Some(found)
        }
        // The same two answers as the join above, for the same reason. Inner and left hand on
        // the child's columns with the parent's gathered beside them, in that order, which is the
        // order the two inputs are named in. Semi and anti never read the parent at all, so there
        // is no pair of sides to put end to end and the honest answer is that this shape does not
        // fit.
        Node::LinkJoin {
            child: left,
            parent: right,
            kind: JoinKind::Inner | JoinKind::Left,
            ..
        } => {
            let mut found = outputs(plan, left)?;
            found.extend(outputs(plan, right)?);
            Some(found)
        }
        Node::Join { .. } | Node::LinkJoin { .. } => None,
    }
}

/// One column of an operator that binds its own expressions, starting at a position.
fn listed(plan: &Plan, index: u32, from: usize, exprs: Slice) -> Vec<(ColumnBinding, LogicalType)> {
    plan.expr_list(exprs)
        .iter()
        .enumerate()
        .map(|(position, &expr)| (binding(index, from + position), plan.expr_type(expr).clone()))
        .collect()
}

/// A binding at a position that came from counting columns rather than from the plan.
fn binding(index: u32, position: usize) -> ColumnBinding {
    ColumnBinding::new(index, u32::try_from(position).expect("a column count fits in a u32"))
}

/// A table index no node in the plan is using.
pub(crate) fn fresh_index(plan: &Plan) -> u32 {
    let mut next = 0;
    for at in 0..plan.node_count() {
        let node = plan.node(u32::try_from(at).unwrap_or(u32::MAX));
        if let Some(index) = node.table_index() {
            next = next.max(index + 1);
        }
    }
    next
}

/// Rewrites the operands of one expression, rebuilding it only if one of them moved.
///
/// One level deep, and `child` decides whether to go further. The two callers want different
/// answers there: folding walks to the leaves, and filter pushdown stops at a column reference,
/// since what it puts in place of one is already written in terms of the operator below.
///
/// The type of the rebuilt expression is the type of the one it replaces. A rewrite that changes the
/// type of an operand has to leave the expression alone instead, because the plan records a type per
/// expression and a caller that guessed a new one would be guessing about a cast.
pub(crate) fn rebuild(
    plan: &mut Plan,
    expr: ExprRef,
    child: &mut impl FnMut(&mut Plan, ExprRef) -> ExprRef,
) -> ExprRef {
    let ty = plan.expr_type(expr).clone();
    let span = plan.expr_span(expr);
    match *plan.expr(expr) {
        Expr::Column(_) | Expr::Constant(_) => expr,
        Expr::Cast { input, try_cast } => {
            let rewritten = child(plan, input);
            if rewritten == input {
                expr
            } else {
                plan.add_expr_at(Expr::Cast { input: rewritten, try_cast }, ty, span)
            }
        }
        Expr::Compare { op, left, right } => {
            let rewritten_left = child(plan, left);
            let rewritten_right = child(plan, right);
            if rewritten_left == left && rewritten_right == right {
                expr
            } else {
                plan.add_expr_at(
                    Expr::Compare { op, left: rewritten_left, right: rewritten_right },
                    ty,
                    span,
                )
            }
        }
        Expr::Conjunction { op, children } => match list(plan, children, child) {
            None => expr,
            Some(children) => plan.add_expr_at(Expr::Conjunction { op, children }, ty, span),
        },
        Expr::Function { name, args } => match list(plan, args, child) {
            None => expr,
            Some(args) => plan.add_expr_at(Expr::Function { name, args }, ty, span),
        },
        Expr::Aggregate { name, args, distinct, filter } => {
            let rewritten_args = list(plan, args, child);
            let rewritten_filter = filter.map(|inner| child(plan, inner));
            if rewritten_args.is_none() && rewritten_filter == filter {
                expr
            } else {
                let args = rewritten_args.unwrap_or(args);
                plan.add_expr_at(
                    Expr::Aggregate { name, args, distinct, filter: rewritten_filter },
                    ty,
                    span,
                )
            }
        }
        Expr::Window { name, args, distinct, filter, ignore_nulls, order } => {
            let rewritten_args = list(plan, args, child);
            let rewritten_filter = filter.map(|inner| child(plan, inner));
            let rewritten_order = keys(plan, order, child);
            if rewritten_args.is_none() && rewritten_filter == filter && rewritten_order.is_none() {
                expr
            } else {
                let args = rewritten_args.unwrap_or(args);
                let order = rewritten_order.unwrap_or(order);
                plan.add_expr_at(
                    Expr::Window {
                        name,
                        args,
                        distinct,
                        filter: rewritten_filter,
                        ignore_nulls,
                        order,
                    },
                    ty,
                    span,
                )
            }
        }
        Expr::Case { arms, otherwise } => {
            let held = plan.arm_list(arms).to_vec();
            let rewritten: Vec<Arm> = held
                .iter()
                .map(|arm| Arm { when: child(plan, arm.when), then: child(plan, arm.then) })
                .collect();
            let rewritten_otherwise = otherwise.map(|inner| child(plan, inner));
            if rewritten == held && rewritten_otherwise == otherwise {
                expr
            } else {
                let arms = plan.add_arms(&rewritten);
                plan.add_expr_at(Expr::Case { arms, otherwise: rewritten_otherwise }, ty, span)
            }
        }
    }
}

/// Rewrites a run of expressions, handing back a new slice only if one of them moved.
///
/// Nothing is appended when nothing moved, so a pass that finds no work adds no slices to the pool
/// and a plan it has already run over is left exactly as it was.
pub(crate) fn list(
    plan: &mut Plan,
    slice: Slice,
    child: &mut impl FnMut(&mut Plan, ExprRef) -> ExprRef,
) -> Option<Slice> {
    let held = plan.expr_list(slice).to_vec();
    let rewritten: Vec<ExprRef> = held.iter().map(|&expr| child(plan, expr)).collect();
    (rewritten != held).then(|| plan.add_expr_list(&rewritten))
}

/// The same for a run of sort keys, of which a window call carries one.
///
/// Only the expression moves. The direction and the null placement belong to the key and not to
/// what it sorts on, so a rewrite that replaced a column with another one leaves both alone.
pub(crate) fn keys(
    plan: &mut Plan,
    slice: Slice,
    child: &mut impl FnMut(&mut Plan, ExprRef) -> ExprRef,
) -> Option<Slice> {
    let held = plan.sort_key_list(slice).to_vec();
    let rewritten: Vec<SortKey> =
        held.iter().map(|key| SortKey { expr: child(plan, key.expr), ..*key }).collect();
    (rewritten != held).then(|| plan.add_sort_keys(&rewritten))
}

/// Calls `found` for every column `expr` reads.
pub(crate) fn columns(plan: &Plan, expr: ExprRef, found: &mut impl FnMut(ColumnBinding)) {
    columns_at(plan, expr, &mut |_, binding| found(binding));
}

/// The same walk, handing back the reference to the column as well as the column.
///
/// A pass that has to write one of those columns down somewhere else needs its type and its span,
/// and the plan records both per expression rather than per binding, so the binding on its own is
/// not enough to build a second reference to the same column with.
///
/// The walk itself is [`Plan::read_columns`], because the binder asks the same question about a
/// subquery it is deciding where to attach and there should be one match arm per `Expr` variant
/// rather than two.
pub(crate) fn columns_at(
    plan: &Plan,
    expr: ExprRef,
    found: &mut impl FnMut(ExprRef, ColumnBinding),
) {
    plan.read_columns(expr, found);
}

/// Calls `found` for every column one node reads, not counting the nodes under it.
///
/// The node on its own rather than the subtree, because the pass that wants this is asking where a
/// column is read rather than whether it is read anywhere, and the walk down is its own business.
/// The walk itself is [`Plan::node_columns`], because the binder asks the same question about the
/// subtree of a query it is moving over a grouping.
pub(crate) fn node_columns(
    plan: &Plan,
    at: NodeRef,
    found: &mut impl FnMut(ExprRef, ColumnBinding),
) {
    plan.node_columns(at, found);
}

/// Whether asking for this expression twice can give two answers.
///
/// The list is [`VOLATILE`], which is the one folding refuses to fold and is read from the pinned
/// binary's `duckdb_functions()`. rudb answers to none of those names yet, so this is false for
/// everything today and is here so that the first one to land is refused by a pass that already knew
/// about it rather than copied by a pass that had never heard of it.
///
/// Copying is where it matters. A pass that moves an expression somewhere else is fine either way,
/// and a pass that writes it down twice has turned one call into two.
pub(crate) fn volatile(plan: &Plan, expr: ExprRef) -> bool {
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
        Expr::Window { args, filter, order, .. } => {
            any_volatile(plan, args)
                || filter.is_some_and(|inner| volatile(plan, inner))
                || plan.sort_key_list(order).iter().any(|key| volatile(plan, key.expr))
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

/// Whether the value for a row is decided by that row on its own.
///
/// The question every pass that moves an expression to a different operator has to answer, because
/// an operator is where the set of rows an expression is evaluated over is decided. An elementwise
/// expression does not care which rows it is evaluated beside, so a pass may put it above a filter
/// or below one and get the same answer for the rows that survive. An aggregate is the opposite: it
/// is a value per group, so a group by with one fewer row in a group answers differently.
///
/// This is not the volatility question. `random()` is elementwise and is not safe to copy, and an
/// aggregate is safe to copy and is not elementwise, so a pass that moves a copy of an expression
/// has to ask both. [`volatile`] is the other one.
///
/// It is false for aggregates and window functions, because both read more than one row. The binder
/// puts either one in its dedicated operator rather than leaving it in a projection, and the check
/// remains here because a pass that treats a window expression as elementwise can push a filter
/// through it and answer the wrong query.
///
/// [`Node::Aggregate`]: rudb_plan::Node::Aggregate
pub(crate) fn elementwise(plan: &Plan, expr: ExprRef) -> bool {
    match *plan.expr(expr) {
        Expr::Column(_) | Expr::Constant(_) => true,
        Expr::Aggregate { .. } | Expr::Window { .. } => false,
        Expr::Cast { input, .. } => elementwise(plan, input),
        Expr::Compare { left, right, .. } => elementwise(plan, left) && elementwise(plan, right),
        Expr::Conjunction { children, .. } | Expr::Function { args: children, .. } => {
            all_elementwise(plan, children)
        }
        Expr::Case { arms, otherwise } => {
            plan.arm_list(arms)
                .iter()
                .all(|arm| elementwise(plan, arm.when) && elementwise(plan, arm.then))
                && otherwise.is_none_or(|inner| elementwise(plan, inner))
        }
    }
}

/// Whether every expression in the run is elementwise.
fn all_elementwise(plan: &Plan, slice: Slice) -> bool {
    plan.expr_list(slice).iter().all(|&expr| elementwise(plan, expr))
}

/// Whether the expression has the same value for every row.
///
/// It reads no column, calls nothing [`volatile`] and holds no aggregate, so its value is decided
/// before the first row is read. That is a wider question than whether it is already a literal:
/// `CAST('abc' AS INTEGER) > 1` is constant and folding leaves it alone, because a fold that raises
/// is abandoned so that the error still comes from running the query.
///
/// An aggregate is false rather than true. It is one value per group, which is constant within a
/// group and not across the input, and the difference between those two is not one this answer can
/// carry, so it says the safe of the two.
pub(crate) fn constant(plan: &Plan, expr: ExprRef) -> bool {
    match *plan.expr(expr) {
        Expr::Constant(_) => true,
        Expr::Column(_) | Expr::Aggregate { .. } | Expr::Window { .. } => false,
        Expr::Cast { input, .. } => constant(plan, input),
        Expr::Compare { left, right, .. } => constant(plan, left) && constant(plan, right),
        Expr::Conjunction { children, .. } => all_constant(plan, children),
        Expr::Function { name, args } => {
            !VOLATILE.contains(&plan.string(name)) && all_constant(plan, args)
        }
        Expr::Case { arms, otherwise } => {
            plan.arm_list(arms)
                .iter()
                .all(|arm| constant(plan, arm.when) && constant(plan, arm.then))
                && otherwise.is_none_or(|inner| constant(plan, inner))
        }
    }
}

/// Whether every expression in the run has the same value for every row.
fn all_constant(plan: &Plan, slice: Slice) -> bool {
    plan.expr_list(slice).iter().all(|&expr| constant(plan, expr))
}

/// Whether two expressions are the same expression written out.
///
/// The arena does not share anything, so an expression built twice is two references to two copies
/// and `one == other` is only true when both came from the same place. Transitive predicates need
/// the other question, since the predicate it derives is often one the query already had, and adding
/// a second copy of it would make the pass produce a different plan each time it ran.
///
/// A constant compares by value rather than by reference for the same reason, and a function by the
/// name it resolved to rather than by where that name is interned.
pub(crate) fn same(plan: &Plan, one: ExprRef, other: ExprRef) -> bool {
    if one == other {
        return true;
    }
    if plan.expr_type(one) != plan.expr_type(other) {
        return false;
    }
    match (plan.expr(one), plan.expr(other)) {
        (Expr::Column(left), Expr::Column(right)) => left == right,
        (Expr::Constant(left), Expr::Constant(right)) => plan.value(*left) == plan.value(*right),
        (
            Expr::Cast { input: left, try_cast: left_try },
            Expr::Cast { input: right, try_cast: right_try },
        ) => left_try == right_try && same(plan, *left, *right),
        (
            Expr::Compare { op: left_op, left: left_one, right: left_other },
            Expr::Compare { op: right_op, left: right_one, right: right_other },
        ) => {
            left_op == right_op
                && same(plan, *left_one, *right_one)
                && same(plan, *left_other, *right_other)
        }
        (
            Expr::Conjunction { op: left_op, children: left },
            Expr::Conjunction { op: right_op, children: right },
        ) => left_op == right_op && same_list(plan, *left, *right),
        (
            Expr::Function { name: left_name, args: left },
            Expr::Function { name: right_name, args: right },
        ) => plan.string(*left_name) == plan.string(*right_name) && same_list(plan, *left, *right),
        (
            Expr::Aggregate {
                name: left_name,
                args: left,
                distinct: left_distinct,
                filter: left_filter,
            },
            Expr::Aggregate {
                name: right_name,
                args: right,
                distinct: right_distinct,
                filter: right_filter,
            },
        ) => {
            plan.string(*left_name) == plan.string(*right_name)
                && left_distinct == right_distinct
                && same_list(plan, *left, *right)
                && same_option(plan, *left_filter, *right_filter)
        }
        (
            Expr::Case { arms: left, otherwise: left_otherwise },
            Expr::Case { arms: right, otherwise: right_otherwise },
        ) => {
            let (left, right) = (plan.arm_list(*left).to_vec(), plan.arm_list(*right).to_vec());
            let (left_otherwise, right_otherwise) = (*left_otherwise, *right_otherwise);
            left.len() == right.len()
                && left.iter().zip(&right).all(|(one, other)| {
                    same(plan, one.when, other.when) && same(plan, one.then, other.then)
                })
                && same_option(plan, left_otherwise, right_otherwise)
        }
        _ => false,
    }
}

/// Whether two runs of expressions are the same run.
fn same_list(plan: &Plan, one: Slice, other: Slice) -> bool {
    let (one, other) = (plan.expr_list(one).to_vec(), plan.expr_list(other).to_vec());
    one.len() == other.len()
        && one.iter().zip(&other).all(|(&left, &right)| same(plan, left, right))
}

/// Whether two optional expressions are the same, counting absent as the same as absent.
fn same_option(plan: &Plan, one: Option<ExprRef>, other: Option<ExprRef>) -> bool {
    match (one, other) {
        (None, None) => true,
        (Some(left), Some(right)) => same(plan, left, right),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use rudb_plan::{Expr, ExprRef, Node, Plan};

    use super::{constant, elementwise, volatile};

    /// The predicate of the filter at the root.
    fn predicate(plan: &Plan) -> ExprRef {
        match *plan.node(plan.root()) {
            Node::Filter { predicate, .. } => predicate,
            _ => panic!("the root is not a filter"),
        }
    }

    /// A plan whose root is a filter holding `text` over a one column scan.
    fn filtered(text: &str) -> Plan {
        let text = format!("Filter {text}\n  Get memory.main.t AS t #0 [a::INTEGER]\n");
        Plan::parse(&text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"))
    }

    #[test]
    fn a_column_is_elementwise_and_is_not_constant() {
        let plan = filtered("(#0.0::INTEGER > 1::INTEGER)::BOOLEAN");
        let predicate = predicate(&plan);
        assert!(elementwise(&plan, predicate));
        assert!(!constant(&plan, predicate));
    }

    #[test]
    fn a_comparison_of_two_literals_is_both() {
        // Written the way an abandoned fold leaves it, which is where a constant predicate over no
        // column actually comes from once the rewriter has been over the plan.
        let plan = filtered("(1::INTEGER > 2::INTEGER)::BOOLEAN");
        let predicate = predicate(&plan);
        assert!(elementwise(&plan, predicate));
        assert!(constant(&plan, predicate));
    }

    #[test]
    fn a_volatile_call_is_elementwise_and_is_not_constant() {
        // The two questions come apart here. `random()` is decided by nothing, which is what makes
        // it not constant, and it is still one call per row, which is what makes it elementwise.
        let plan = filtered("(random()::DOUBLE > 0.5::DOUBLE)::BOOLEAN");
        let predicate = predicate(&plan);
        assert!(volatile(&plan, predicate));
        assert!(elementwise(&plan, predicate));
        assert!(!constant(&plan, predicate));
    }

    #[test]
    fn an_aggregate_is_neither_however_constant_its_arguments_are() {
        let text = concat!(
            "Aggregate #1 groups=[] aggregates=[sum(1::INTEGER)::HUGEINT]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
        );
        let plan = Plan::parse(text).expect("an aggregate");
        let Node::Aggregate { aggregates, .. } = *plan.node(plan.root()) else {
            panic!("the root is not an aggregate");
        };
        let call = plan.expr_list(aggregates)[0];
        assert!(matches!(plan.expr(call), Expr::Aggregate { .. }));
        assert!(!elementwise(&plan, call));
        assert!(!constant(&plan, call));
    }

    #[test]
    fn a_case_answers_for_the_whole_of_itself_and_not_for_the_branch_that_runs() {
        let both =
            filtered("CASE WHEN TRUE::BOOLEAN THEN TRUE::BOOLEAN ELSE FALSE::BOOLEAN END::BOOLEAN");
        assert!(constant(&both, predicate(&both)));
        // One branch reading a column is enough, because which branch runs is a per row answer.
        let one = filtered(
            "CASE WHEN TRUE::BOOLEAN THEN TRUE::BOOLEAN ELSE (#0.0::INTEGER > 1::INTEGER)::BOOLEAN END::BOOLEAN",
        );
        assert!(!constant(&one, predicate(&one)));
        assert!(elementwise(&one, predicate(&one)));
    }
}

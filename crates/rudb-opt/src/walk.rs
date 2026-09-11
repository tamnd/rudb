//! Walking a plan, which every pass does before it changes anything.
//!
//! Two walks, and they are separate on purpose. [`nodes`] hands back the nodes reachable from the
//! root, because the arena holds everything that was ever built and a pass that read an unreachable
//! node would be reading a plan nobody is going to run. [`exprs`] hands back the expressions
//! reachable from a node, because an expression is a tree and the thing a pass wants is usually
//! every leaf of it.
//!
//! Both are iterative rather than recursive. A plan comes from a query and a query comes from a
//! person or from a generator, so the depth is whatever was written, and a deeply nested `CASE` is
//! not a reason for the optimizer to run out of stack.

use rudb_plan::{Expr, ExprRef, Node, NodeRef, Plan};

/// Every node reachable from the root, in no particular order.
///
/// A node appears once however many times it is referred to, which matters the day a plan stops
/// being a tree.
#[must_use]
pub(crate) fn nodes(plan: &Plan) -> Vec<NodeRef> {
    let mut seen = vec![false; plan.node_count()];
    let mut stack = vec![plan.root()];
    let mut out = Vec::new();
    while let Some(reference) = stack.pop() {
        let at = reference as usize;
        if at >= seen.len() || seen[at] {
            continue;
        }
        seen[at] = true;
        out.push(reference);
        stack.extend(plan.node(reference).children().into_iter().flatten());
    }
    out
}

/// Every expression a node holds directly, which is where an expression walk starts.
///
/// The arms of this match are the whole reason a pass does not have to know every node kind: a node
/// added to the plan without being added here has no expressions as far as every pass is concerned,
/// which is a wrong answer, so the match has no wildcard.
#[must_use]
pub(crate) fn roots(plan: &Plan, reference: NodeRef) -> Vec<ExprRef> {
    match *plan.node(reference) {
        Node::Get { .. } | Node::Dummy | Node::Limit { .. } | Node::CrossProduct { .. } => {
            Vec::new()
        }
        Node::SetOp { .. } => Vec::new(),
        Node::Values { rows, .. } => {
            plan.row_list(rows).iter().flat_map(|&row| plan.expr_list(row).to_vec()).collect()
        }
        Node::TableFunction { args, .. } => plan.expr_list(args).to_vec(),
        Node::Filter { predicate, .. } => vec![predicate],
        Node::Project { exprs, .. } => plan.expr_list(exprs).to_vec(),
        Node::Aggregate { groups, aggregates, .. } => {
            let mut out = plan.expr_list(groups).to_vec();
            out.extend(plan.expr_list(aggregates));
            out
        }
        Node::Sort { keys, .. } => plan.sort_key_list(keys).iter().map(|key| key.expr).collect(),
        Node::Distinct { on, .. } => plan.expr_list(on).to_vec(),
        Node::Join { conditions, .. } => plan.expr_list(conditions).to_vec(),
    }
}

/// Every expression in the trees rooted at `roots`, including the roots themselves.
#[must_use]
pub(crate) fn exprs(plan: &Plan, roots: &[ExprRef]) -> Vec<ExprRef> {
    let mut seen = vec![false; plan.expr_count()];
    let mut stack = roots.to_vec();
    let mut out = Vec::new();
    while let Some(reference) = stack.pop() {
        let at = reference as usize;
        if at >= seen.len() || seen[at] {
            continue;
        }
        seen[at] = true;
        out.push(reference);
        stack.extend(children(plan, reference));
    }
    out
}

/// The operands of one expression.
///
/// No wildcard arm here either, and for the same reason as [`roots`]: an expression kind whose
/// operands are not listed is a subtree every pass is blind to.
fn children(plan: &Plan, reference: ExprRef) -> Vec<ExprRef> {
    match *plan.expr(reference) {
        Expr::Column(_) | Expr::Constant(_) => Vec::new(),
        Expr::Cast { input, .. } => vec![input],
        Expr::Compare { left, right, .. } => vec![left, right],
        Expr::Conjunction { children, .. } => plan.expr_list(children).to_vec(),
        Expr::Function { args, .. } => plan.expr_list(args).to_vec(),
        Expr::Aggregate { args, filter, .. } => {
            let mut out = plan.expr_list(args).to_vec();
            out.extend(filter);
            out
        }
        Expr::Case { arms, otherwise } => {
            let mut out = Vec::new();
            for arm in plan.arm_list(arms) {
                out.push(arm.when);
                out.push(arm.then);
            }
            out.extend(otherwise);
            out
        }
    }
}

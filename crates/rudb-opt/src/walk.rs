//! Rewriting the operands of one expression, without deciding what the rewrite is.
//!
//! Two passes take an expression apart and put it back with different operands. Folding replaces an
//! operand with its value where it has one, and filter pushdown replaces a column reference with
//! whatever the operator below computes that column from. Everything except that one sentence is the
//! same code in both: visit each operand, and if any of them moved, append a new expression carrying
//! the operands that moved and the type the plan already recorded.
//!
//! Appending rather than editing in place is not a style choice. An expression may only refer to an
//! expression behind it in the arena, which [`Plan::validate`] checks and which is what makes a plan
//! acyclic by construction, so a rewritten operand has to be appended before the operator that reads
//! it can be.
//!
//! [`Plan::validate`]: rudb_plan::Plan::validate

use rudb_plan::{Arm, Expr, ExprRef, Plan, Slice};

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
    match *plan.expr(expr) {
        Expr::Column(_) | Expr::Constant(_) => expr,
        Expr::Cast { input, try_cast } => {
            let rewritten = child(plan, input);
            if rewritten == input {
                expr
            } else {
                plan.add_expr(Expr::Cast { input: rewritten, try_cast }, ty)
            }
        }
        Expr::Compare { op, left, right } => {
            let rewritten_left = child(plan, left);
            let rewritten_right = child(plan, right);
            if rewritten_left == left && rewritten_right == right {
                expr
            } else {
                plan.add_expr(
                    Expr::Compare { op, left: rewritten_left, right: rewritten_right },
                    ty,
                )
            }
        }
        Expr::Conjunction { op, children } => match list(plan, children, child) {
            None => expr,
            Some(children) => plan.add_expr(Expr::Conjunction { op, children }, ty),
        },
        Expr::Function { name, args } => match list(plan, args, child) {
            None => expr,
            Some(args) => plan.add_expr(Expr::Function { name, args }, ty),
        },
        Expr::Aggregate { name, args, distinct, filter } => {
            let rewritten_args = list(plan, args, child);
            let rewritten_filter = filter.map(|inner| child(plan, inner));
            if rewritten_args.is_none() && rewritten_filter == filter {
                expr
            } else {
                let args = rewritten_args.unwrap_or(args);
                plan.add_expr(
                    Expr::Aggregate { name, args, distinct, filter: rewritten_filter },
                    ty,
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
                plan.add_expr(Expr::Case { arms, otherwise: rewritten_otherwise }, ty)
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

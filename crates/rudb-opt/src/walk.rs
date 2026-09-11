//! Walking one expression, without deciding what the walk is for.
//!
//! Four things every pass here ends up needing: rewrite the operands, list the columns, ask whether
//! asking twice can give two answers, and ask whether two expressions are the same expression. None
//! of them is interesting and all of them have a match arm per variant, so a variant added to `Expr`
//! and forgotten about is a compile error in one file instead of four.
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

use rudb_plan::{Arm, ColumnBinding, Expr, ExprRef, Plan, Slice};

use crate::fold::VOLATILE;

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

/// Calls `found` for every column `expr` reads.
pub(crate) fn columns(plan: &Plan, expr: ExprRef, found: &mut impl FnMut(ColumnBinding)) {
    match *plan.expr(expr) {
        Expr::Column(binding) => found(binding),
        Expr::Constant(_) => {}
        Expr::Cast { input, .. } => columns(plan, input, found),
        Expr::Compare { left, right, .. } => {
            columns(plan, left, found);
            columns(plan, right, found);
        }
        Expr::Conjunction { children, .. } | Expr::Function { args: children, .. } => {
            for &child in plan.expr_list(children) {
                columns(plan, child, found);
            }
        }
        Expr::Aggregate { args, filter, .. } => {
            for &arg in plan.expr_list(args) {
                columns(plan, arg, found);
            }
            if let Some(inner) = filter {
                columns(plan, inner, found);
            }
        }
        Expr::Case { arms, otherwise } => {
            for arm in plan.arm_list(arms) {
                columns(plan, arm.when, found);
                columns(plan, arm.then, found);
            }
            if let Some(inner) = otherwise {
                columns(plan, inner, found);
            }
        }
    }
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

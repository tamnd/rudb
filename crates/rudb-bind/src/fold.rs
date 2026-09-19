//! Working out what an expression comes to, for the places that need the value and not the plan.
//!
//! `LIMIT` is why this exists. `LIMIT 3` is a literal and `LIMIT 1 + 1` is not, and the plan node
//! holds a row count rather than an expression, so something has to turn the second into the first
//! before there is a plan to hold. The pinned binary does the same thing in the same place, which is
//! its binder evaluating the expression and writing the number down.
//!
//! The optimizer's folding pass calls this too, which is the reason it is here rather than there.
//! The binder is below the optimizer in the layer rule and the optimizer is below the executor, so a
//! thing both of them do belongs at the bottom of the three. What the pass adds on top is the plan
//! rewriting: finding the expressions, walking them bottom up, sharing the results and putting the
//! constants back. None of that is evaluation and none of it is here.
//!
//! # Raising and abandoning
//!
//! Evaluating can fail, and the two callers want opposite things when it does. The pass abandons the
//! fold and leaves the expression alone, so that `CAST('abc' AS INTEGER)` still raises from running
//! the query rather than from planning it, and so that a query whose unreachable branch would have
//! failed still runs. The binder has nothing to leave alone, because a `LIMIT 1 // 0` has no row
//! count for the node to hold, so there the error is the answer. That is what the pin does with it
//! as well. So the failure comes back as an error and the pass is the one that throws it away.
//!
//! # What has no value
//!
//! A column, an aggregate and a window function each answer per row or per group, so there is no one
//! value and the answer is `None`. A volatile call is `None` for a different reason: asking twice
//! can give two answers, so writing one of them down is a choice this has no business making. A
//! `TIMESTAMPTZ` printed as text is `None` for a third: the text depends on the session zone, and
//! the optimizer has no session by design.

use rudb_common::{LogicalType, Result, Value};
use rudb_kernels::{Comparison, Connective, call_values, cast_value, combine, compare_values};
use rudb_plan::{CompareOp, ConjunctionOp, Expr, ExprRef, Plan, Slice};
use rudb_vector::Vector;

/// The functions whose value is not decided by their arguments.
///
/// `SELECT DISTINCT function_name FROM duckdb_functions() WHERE has_side_effects` on the pinned
/// binary, which is the list at the commit the grammar is vendored from. rudb implements none of
/// them today and the list is here anyway, so that the first one to land is refused by code that
/// already knew about it rather than folded by code that had never heard of it.
pub const VOLATILE: [&str; 17] = [
    "current_connection_id",
    "current_query",
    "current_query_id",
    "current_transaction_id",
    "currval",
    "error",
    "gen_random_uuid",
    "nextval",
    "random",
    "setseed",
    "setval",
    "sleep_ms",
    "stats",
    "uuid",
    "uuidv4",
    "uuidv7",
    "write_log",
];

/// What an expression comes to, or `None` if it does not come to one thing.
///
/// # Errors
///
/// If evaluating it raises. A caller that would rather have the expression than the error throws
/// this away, which is what the folding pass does and what the module documentation explains.
pub fn value_of(plan: &Plan, expr: ExprRef) -> Result<Option<Value>> {
    let value = match *plan.expr(expr) {
        Expr::Constant(value) => plan.value(value).clone(),
        Expr::Column(_) | Expr::Aggregate { .. } | Expr::Window { .. } => return Ok(None),
        Expr::Cast { input, try_cast } => {
            if plan.expr_type(input) == &LogicalType::TimestampTz
                && plan.expr_type(expr) == &LogicalType::Varchar
            {
                return Ok(None);
            }
            let Some(inner) = value_of(plan, input)? else { return Ok(None) };
            cast_value(&inner, plan.expr_type(expr), try_cast)?
        }
        Expr::Compare { op, left, right } => {
            let (Some(left), Some(right)) = (value_of(plan, left)?, value_of(plan, right)?) else {
                return Ok(None);
            };
            compare_values(comparison(op), &left, &right)?
        }
        Expr::Conjunction { op, children } => {
            let Some(values) = values_of(plan, children)? else { return Ok(None) };
            let vectors: Vec<Vector> = values
                .into_iter()
                .map(|value| Vector::constant(LogicalType::Boolean, value, 1))
                .collect();
            combine(connective(op), &vectors)?.value_at(0)
        }
        Expr::Function { name, args } => {
            let name = plan.string(name);
            if VOLATILE.contains(&name) {
                return Ok(None);
            }
            let Some(values) = values_of(plan, args)? else { return Ok(None) };
            // No expression to name, because there is no expression to keep. A caller that wanted
            // the expression rather than the error is throwing the error away anyway, and the one
            // that wanted the value reports what was written around it instead.
            call_values(name, &values, plan.expr_type(expr), None)?
        }
        Expr::Case { arms, otherwise } => return case(plan, arms, otherwise),
    };
    Ok(Some(value))
}

/// What a run of expressions comes to, or `None` if any one of them does not come to one thing.
fn values_of(plan: &Plan, slice: Slice) -> Result<Option<Vec<Value>>> {
    let mut values = Vec::with_capacity(plan.expr_list(slice).len());
    for &expr in plan.expr_list(slice) {
        let Some(value) = value_of(plan, expr)? else { return Ok(None) };
        values.push(value);
    }
    Ok(Some(values))
}

/// What a searched `CASE` comes to, which is the first arm that fires and no arm after it.
///
/// One arm at a time and not every arm at once, because an arm that does not fire is an arm that
/// was written not to be evaluated. `CASE WHEN false THEN 1 // 0 ELSE 4 END` is 4 and not a division
/// by zero, and it is 4 for the same reason at run time, where the executor evaluates an arm only on
/// the rows that reached it.
fn case(plan: &Plan, arms: Slice, otherwise: Option<ExprRef>) -> Result<Option<Value>> {
    for arm in plan.arm_list(arms) {
        let Some(when) = value_of(plan, arm.when)? else { return Ok(None) };
        // A null condition is not a condition that fired, which is the one place this differs from
        // reading it as a boolean.
        if when.as_bool() == Some(true) {
            return value_of(plan, arm.then);
        }
    }
    match otherwise {
        Some(otherwise) => value_of(plan, otherwise),
        None => Ok(Some(Value::Null)),
    }
}

/// The kernels' comparison for the plan's.
///
/// A translation rather than one shared enum, because the kernels are rank 3 and the plan is rank 9.
/// There is a second copy in `rudb-exec`, which does not depend on this crate. The match is
/// exhaustive in both, so a ninth comparison stops both of them compiling rather than quietly
/// folding to the wrong answer in one.
pub fn comparison(op: CompareOp) -> Comparison {
    match op {
        CompareOp::Equal => Comparison::Equal,
        CompareOp::NotEqual => Comparison::NotEqual,
        CompareOp::Less => Comparison::Less,
        CompareOp::LessOrEqual => Comparison::LessOrEqual,
        CompareOp::Greater => Comparison::Greater,
        CompareOp::GreaterOrEqual => Comparison::GreaterOrEqual,
        CompareOp::DistinctFrom => Comparison::DistinctFrom,
        CompareOp::NotDistinctFrom => Comparison::NotDistinctFrom,
    }
}

/// The kernels' connective for the plan's.
pub fn connective(op: ConjunctionOp) -> Connective {
    match op {
        ConjunctionOp::And => Connective::And,
        ConjunctionOp::Or => Connective::Or,
    }
}

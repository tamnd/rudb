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
//! cast into or out of `TIMESTAMPTZ` is `None` for a third: the answer depends on the session zone,
//! and the optimizer has no session by design.

use rudb_common::{ErrorCode, LogicalType, Result, Value};
use rudb_kernels::cast::reads_time_zone;
use rudb_kernels::{Comparison, Connective, call_values, cast_value, combine, compare_values};
use rudb_plan::{CompareOp, ConjunctionOp, Expr, ExprRef, Plan, Slice};
use rudb_vector::Vector;

/// The functions whose value is not decided by their arguments.
///
/// `SELECT DISTINCT function_name FROM duckdb_functions() WHERE has_side_effects` on the pinned
/// binary, which is the list at the commit the grammar is vendored from. rudb has the sequence
/// functions, `random` and `setseed` so far, and the rest are listed anyway so that the next one to
/// land is refused by code that already knew about it rather than folded by code that had never
/// heard of it. `TRY` refuses an operand that calls any of them.
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
    evaluate(plan, expr, &mut Lambdas { enabled: false, frames: Vec::new() })
}

/// What an expression comes to, the way [`value_of`] works it out, except that a `list_filter` or a
/// `list_transform` over a constant list is run element by element rather than left alone.
///
/// The argument of a `COLUMNS` is the one place that needs it, because the pin evaluates a lambda
/// over the column names there before there is a plan to run it in. The folding pass does not ask
/// for it, so a lambda in a query still runs where it was written.
///
/// # Errors
///
/// If evaluating it raises.
pub fn value_with_lambdas(plan: &Plan, expr: ExprRef) -> Result<Option<Value>> {
    evaluate(plan, expr, &mut Lambdas { enabled: true, frames: Vec::new() })
}

/// Whether lambdas are run at all, and the parameter values of the ones being run, innermost last.
struct Lambdas {
    enabled: bool,
    frames: Vec<(u32, [Value; 2])>,
}

fn evaluate(plan: &Plan, expr: ExprRef, lambdas: &mut Lambdas) -> Result<Option<Value>> {
    let value = match *plan.expr(expr) {
        Expr::Constant(value) => plan.value(value).clone(),
        Expr::LambdaParam(binding) => {
            let found = lambdas.frames.iter().rev().find(|(table, _)| *table == binding.table);
            match found.and_then(|(_, values)| values.get(binding.column as usize)) {
                Some(value) => value.clone(),
                None => return Ok(None),
            }
        }
        Expr::Column(_) | Expr::Aggregate { .. } | Expr::Window { .. } | Expr::Lambda { .. } => {
            return Ok(None);
        }
        Expr::Cast { input, try_cast } => {
            if reads_time_zone(plan.expr_type(input), plan.expr_type(expr)) {
                return Ok(None);
            }
            let Some(inner) = evaluate(plan, input, lambdas)? else { return Ok(None) };
            cast_value(&inner, plan.expr_type(expr), try_cast)?
        }
        Expr::Compare { op, left, right } => {
            let (Some(left), Some(right)) =
                (evaluate(plan, left, lambdas)?, evaluate(plan, right, lambdas)?)
            else {
                return Ok(None);
            };
            compare_values(comparison(op), &left, &right)?
        }
        Expr::Conjunction { op, children } => {
            let Some(values) = values_of(plan, children, lambdas)? else { return Ok(None) };
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
            if let ("try", [only]) = (name, plan.expr_list(args)) {
                return match evaluate(plan, *only, lambdas) {
                    Err(error) if caught(&error) => Ok(Some(Value::Null)),
                    answer => answer,
                };
            }
            if lambdas.enabled
                && let [list, lambda] = plan.expr_list(args)
                && let Expr::Lambda { table, body, .. } = *plan.expr(*lambda)
            {
                return run_lambda(plan, name, expr, *list, table, body, lambdas);
            }
            let Some(values) = values_of(plan, args, lambdas)? else { return Ok(None) };
            // The one call the values alone cannot answer, since a value of an enum is its string
            // and the position is in the type.
            if let ("enum_code", [only], [arg]) = (name, values.as_slice(), plan.expr_list(args)) {
                return rudb_vector::enum_position(plan.expr_type(*arg), only).map(Some);
            }
            // No expression to name, because there is no expression to keep. A caller that wanted
            // the expression rather than the error is throwing the error away anyway, and the one
            // that wanted the value reports what was written around it instead.
            call_values(name, &values, plan.expr_type(expr), None)?
        }
        Expr::Case { arms, otherwise } => return case(plan, arms, otherwise, lambdas),
    };
    Ok(Some(value))
}

/// A `list_filter` or a `list_transform` over a list that folds, run once per element with the
/// element and its position, from 1, as the parameters.
fn run_lambda(
    plan: &Plan,
    name: &str,
    expr: ExprRef,
    list: ExprRef,
    table: u32,
    body: ExprRef,
    lambdas: &mut Lambdas,
) -> Result<Option<Value>> {
    let filter = match name {
        "list_filter" => true,
        "list_transform" => false,
        _ => return Ok(None),
    };
    let Some(list) = evaluate(plan, list, lambdas)? else { return Ok(None) };
    let Value::List { values, .. } = list else { return Ok(Some(Value::Null)) };
    let LogicalType::List(element) = plan.expr_type(expr) else { return Ok(None) };
    let mut out = Vec::with_capacity(values.len());
    for (at, value) in values.into_iter().enumerate() {
        lambdas.frames.push((table, [value.clone(), Value::BigInt(at as i64 + 1)]));
        let answer = evaluate(plan, body, lambdas);
        lambdas.frames.pop();
        let Some(answer) = answer? else { return Ok(None) };
        if !filter {
            out.push(answer);
        } else if answer.as_bool() == Some(true) {
            out.push(value);
        }
    }
    Ok(Some(Value::List { element: (**element).clone(), values: out }))
}

/// Whether `TRY` answers null for this error rather than passing it on, which is the pin's three
/// kinds of error a value can cause.
#[must_use]
pub fn caught(error: &rudb_common::Error) -> bool {
    matches!(error.code(), ErrorCode::Conversion | ErrorCode::OutOfRange | ErrorCode::InvalidInput)
}

/// What a run of expressions comes to, or `None` if any one of them does not come to one thing.
fn values_of(plan: &Plan, slice: Slice, lambdas: &mut Lambdas) -> Result<Option<Vec<Value>>> {
    let mut values = Vec::with_capacity(plan.expr_list(slice).len());
    for &expr in plan.expr_list(slice) {
        let Some(value) = evaluate(plan, expr, lambdas)? else { return Ok(None) };
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
fn case(
    plan: &Plan,
    arms: Slice,
    otherwise: Option<ExprRef>,
    lambdas: &mut Lambdas,
) -> Result<Option<Value>> {
    for arm in plan.arm_list(arms) {
        let Some(when) = evaluate(plan, arm.when, lambdas)? else { return Ok(None) };
        // A null condition is not a condition that fired, which is the one place this differs from
        // reading it as a boolean.
        if when.as_bool() == Some(true) {
            return evaluate(plan, arm.then, lambdas);
        }
    }
    match otherwise {
        Some(otherwise) => evaluate(plan, otherwise, lambdas),
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

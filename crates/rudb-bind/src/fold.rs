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
//! cast into or out of `TIMESTAMPTZ`, and a call on one, is `None` for a third: the answer depends on
//! the session zone, and the optimizer has no session by design.

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
/// heard of it. `TRY` refuses an operand that calls any of them. The advisory lock functions of a
/// PostgreSQL session are here too, because each call takes or releases a lock.
pub const VOLATILE: [&str; 30] = [
    "clock_timestamp",
    "current_connection_id",
    "current_query",
    "current_query_id",
    "current_transaction_id",
    "currval",
    "error",
    "gen_random_uuid",
    "nextval",
    "pg_advisory_lock",
    "pg_advisory_lock_shared",
    "pg_advisory_unlock",
    "pg_advisory_unlock_all",
    "pg_advisory_unlock_shared",
    "pg_advisory_xact_lock",
    "pg_advisory_xact_lock_shared",
    "pg_sleep",
    "pg_try_advisory_lock",
    "pg_try_advisory_lock_shared",
    "pg_try_advisory_xact_lock",
    "pg_try_advisory_xact_lock_shared",
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

/// Raises the first error of a part of the plan that PostgreSQL works out when it plans the query.
///
/// The planner of PostgreSQL folds each call whose arguments are all constants, in every expression
/// of the plan, and an error there fails the statement before it makes a row. So
/// `SELECT 1/0 + x FROM t WHERE false` is a division by zero there, and on the extended protocol the
/// error comes at `Bind` and not at `Execute`. It does not fold an arm of a `CASE` after a condition
/// that is a constant true or in place of one that is a constant false or null, nor an operand of
/// `AND` after a constant false, of `OR` after a constant true or of `COALESCE` after a constant
/// that is not null. This does the same.
///
/// # Errors
///
/// The first error that folding raises.
pub fn planned(plan: &Plan) -> Result<()> {
    let mut seen = vec![false; plan.node_count()];
    let mut stack = vec![plan.root()];
    while let Some(node) = stack.pop() {
        let Some(slot) = seen.get_mut(node as usize) else { continue };
        if std::mem::replace(slot, true) {
            continue;
        }
        let held = plan.node(node);
        for (expr, _, _) in plan.top_level_exprs(held) {
            constant_parts(plan, expr)?;
        }
        stack.extend(held.children().into_iter().flatten());
    }
    Ok(())
}

/// Folds the constant parts of one expression for [`planned`], the whole of it if it is constant.
fn constant_parts(plan: &Plan, expr: ExprRef) -> Result<()> {
    // The value of an `AND`, an `OR` or a `COALESCE` reads every operand, and the operands after
    // the one that decides it are not folded.
    let stops = match *plan.expr(expr) {
        Expr::Conjunction { .. } => true,
        Expr::Function { name, .. } => plan.string(name) == "coalesce",
        _ => false,
    };
    if !stops && value_of(plan, expr)?.is_some() {
        return Ok(());
    }
    match *plan.expr(expr) {
        Expr::Column(_) | Expr::Constant(_) | Expr::Lambda { .. } | Expr::LambdaParam(_) => Ok(()),
        Expr::Cast { input, .. } => constant_parts(plan, input),
        Expr::Compare { left, right, .. } => {
            constant_parts(plan, left)?;
            constant_parts(plan, right)
        }
        Expr::Conjunction { op, children } => {
            let decides = Value::Boolean(op == ConjunctionOp::Or);
            for &child in plan.expr_list(children) {
                constant_parts(plan, child)?;
                if value_of(plan, child)? == Some(decides.clone()) {
                    break;
                }
            }
            Ok(())
        }
        Expr::Function { name, args } => {
            let coalesce = plan.string(name) == "coalesce";
            for &arg in plan.expr_list(args) {
                constant_parts(plan, arg)?;
                if coalesce && value_of(plan, arg)?.is_some_and(|value| !value.is_null()) {
                    break;
                }
            }
            Ok(())
        }
        Expr::Aggregate { args, filter, .. } => {
            for &arg in plan.expr_list(args).iter().chain(filter.iter()) {
                constant_parts(plan, arg)?;
            }
            Ok(())
        }
        Expr::Window { args, filter, order, .. } => {
            for &arg in plan.expr_list(args).iter().chain(filter.iter()) {
                constant_parts(plan, arg)?;
            }
            for key in plan.sort_key_list(order) {
                constant_parts(plan, key.expr)?;
            }
            Ok(())
        }
        Expr::Case { arms, otherwise } => {
            for arm in plan.arm_list(arms) {
                constant_parts(plan, arm.when)?;
                match value_of(plan, arm.when)? {
                    Some(when) if when.as_bool() == Some(true) => {
                        return constant_parts(plan, arm.then);
                    }
                    Some(_) => {}
                    None => constant_parts(plan, arm.then)?,
                }
            }
            otherwise.map_or(Ok(()), |otherwise| constant_parts(plan, otherwise))
        }
    }
}

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
            let (from, target) = (plan.expr_type(input), plan.expr_type(expr));
            // A union cast needs the type a null came from, which the value alone cannot say.
            if matches!(target, LogicalType::Union(_)) {
                return rudb_kernels::cast::cast_to_union(&inner, from, target, try_cast).map(Some);
            }
            match rudb_kernels::json::cast_typed(&inner, from, target, try_cast, None) {
                Some(cast) => cast?,
                None => cast_value(&inner, target, try_cast)?,
            }
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
            // A call on a `TIMESTAMPTZ`, or one that makes one, reads the session zone the same way
            // a cast does, and `timezone` reads a zone it is given from a plain timestamp too.
            let zoned = |arg: &ExprRef| plan.expr_type(*arg) == &LogicalType::TimestampTz;
            if name == "timezone"
                || plan.expr_type(expr) == &LogicalType::TimestampTz
                || plan.expr_list(args).iter().any(zoned)
            {
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
            // The `JSON` builders write a string and a `JSON` differently, and both are held as
            // text, so they are handed the types as well.
            if rudb_kernels::json::BUILDERS.contains(&name) {
                let types: Vec<LogicalType> =
                    plan.expr_list(args).iter().map(|arg| plan.expr_type(*arg).clone()).collect();
                return rudb_kernels::json::build(name, &values, &types, None);
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

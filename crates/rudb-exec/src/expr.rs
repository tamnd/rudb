//! Evaluating a bound expression over a chunk.
//!
//! One function, recursive, one vector out per call. That is section 8.2's description of tier 0
//! word for word: "a tree of expression nodes, each evaluating its children into intermediate
//! vectors and then applying a kernel". The intermediate vectors are the cost and they are the
//! thing tiers 1 and 2 exist to remove, by fusing a chain of them into one loop and by compiling
//! that loop respectively. Neither of those can be checked against anything until this exists, so
//! this exists first and stays.
//!
//! What runs in a pipeline is [`Prepared`](crate::Prepared), which does once per pipeline the four
//! things this does once per chunk. This stays as the reference the prepared form is checked
//! against, for the reason `spec/engine/04-expressions.md` gives for keeping every slow path that
//! a fast path replaced: a fast path with nothing to disagree with is a fast path nobody can tell
//! is wrong. It is also still what the operators that evaluate an expression exactly once use,
//! since preparing a tree to run it on one chunk is more work than walking it.
//!
//! Nothing here decides a type. Every expression in a bound plan carries the type it evaluates to,
//! the binder put the casts in, and a kernel is told what it returns rather than working it out.
//! An evaluator that inferred anything would be a second type system that has to agree with the
//! first one, and the interesting bugs in a database are exactly the places where two such things
//! disagree.

use rudb_common::{Error, Result, Value};
use rudb_kernels::{cast, combine, compare, is_true};
use rudb_plan::{Expr, ExprRef, Plan};
use rudb_vector::{Chunk, Vector};

use crate::prepared::{comparison, connective, narrow};
use crate::schema::Schema;

/// Evaluates one expression over a chunk, producing one vector as long as the chunk.
///
/// `schema` describes `chunk`, and it is what a column reference resolves against.
///
/// # Errors
///
/// If a column reference names a binding the schema does not have, if an aggregate appears outside
/// an aggregate operator, or anything a kernel reports.
pub fn evaluate(plan: &Plan, expr: ExprRef, schema: &Schema, chunk: &Chunk) -> Result<Vector> {
    let ty = plan.expr_type(expr).clone();
    match *plan.expr(expr) {
        Expr::Column(binding) => {
            let position = schema.position_of(binding).ok_or_else(|| {
                Error::internal(format!(
                    "column #{}.{} is not in the schema this operator was given",
                    binding.table, binding.column
                ))
            })?;
            Ok(chunk.column(position)?.clone())
        }
        Expr::Constant(reference) => {
            Ok(Vector::constant(ty, plan.value(reference).clone(), chunk.len()))
        }
        Expr::Cast { input, try_cast } => {
            let inner = evaluate(plan, input, schema, chunk)?;
            cast(&inner, &ty, try_cast)
        }
        Expr::Compare { op, left, right } => {
            let left = evaluate(plan, left, schema, chunk)?;
            let right = evaluate(plan, right, schema, chunk)?;
            compare(comparison(op), &left, &right)
        }
        Expr::Conjunction { op, children } => {
            let children = evaluate_all(plan, plan.expr_list(children), schema, chunk)?;
            combine(connective(op), &children)
        }
        Expr::Function { name, args } => {
            let args = evaluate_all(plan, plan.expr_list(args), schema, chunk)?;
            rudb_kernels::call(plan.string(name), &args, &ty)
        }
        Expr::Aggregate { name, .. } => Err(Error::internal(format!(
            "the {} aggregate was evaluated as an ordinary expression",
            plan.string(name)
        ))),
        Expr::Case { arms, otherwise } => {
            let arms = plan.arm_list(arms).to_vec();
            let mut answers = vec![Value::Null; chunk.len()];
            let mut pending: Vec<usize> = (0..chunk.len()).collect();
            for arm in arms {
                if pending.is_empty() {
                    break;
                }
                let narrowed = narrow(chunk, &pending)?;
                let flags = evaluate(plan, arm.when, schema, &narrowed)?;
                let mut taken = Vec::new();
                let mut still = Vec::new();
                // row at a time: 2c (#57) replaces this whole arm with a selection threaded through
                // the arms and a scatter kernel writing the results back, which is the change that
                // removes all three of these loops at once.
                for (at, &row) in pending.iter().enumerate() {
                    if is_true(&flags.value_at(at)) {
                        taken.push((at, row));
                    } else {
                        still.push(row);
                    }
                }
                if !taken.is_empty() {
                    let positions: Vec<usize> = taken.iter().map(|&(at, _)| at).collect();
                    let matched = narrow(&narrowed, &positions)?;
                    let results = evaluate(plan, arm.then, schema, &matched)?;
                    // row at a time: the scatter this wants is 2c (#57), same as the loop above.
                    for (slot, &(_, row)) in taken.iter().enumerate() {
                        answers[row] = results.value_at(slot);
                    }
                }
                pending = still;
            }
            if let Some(otherwise) = otherwise {
                if !pending.is_empty() {
                    let narrowed = narrow(chunk, &pending)?;
                    let results = evaluate(plan, otherwise, schema, &narrowed)?;
                    // row at a time: the scatter this wants is 2c (#57), same as the two above.
                    for (slot, &row) in pending.iter().enumerate() {
                        answers[row] = results.value_at(slot);
                    }
                }
            }
            Vector::from_values(ty, &answers)
        }
    }
}

/// Evaluates a list of expressions over one chunk.
///
/// # Errors
///
/// Anything [`evaluate`] reports, on the first expression that reports it.
pub fn evaluate_all(
    plan: &Plan,
    exprs: &[ExprRef],
    schema: &Schema,
    chunk: &Chunk,
) -> Result<Vec<Vector>> {
    exprs.iter().map(|&expr| evaluate(plan, expr, schema, chunk)).collect()
}

//! Writing a bound expression back out the way an error message quotes it.
//!
//! One message in the engine names the expression it failed in rather than the values it failed on,
//! which is division by zero, and the expression it names is the bound one. That means the casts the
//! binder inserted are in the text, a column is the name its operator produces rather than the name
//! the query wrote, and a literal is the value it was folded to. `SELECT a // 0 FROM t` says
//! `(a // 0)` and `SELECT a::DOUBLE // 0.0 FROM t` says `(CAST(a AS DOUBLE) // 0.0)`, both measured.
//!
//! This is the third way an expression is written in this engine and the three are not
//! interchangeable. The plan dump in `rudb-plan` annotates every node with its type, because a dump
//! that cannot be read back without a catalog is not a dump. The column namer in `rudb-parse` writes
//! the text as the user typed it, because that is what goes in the result header. This one writes
//! what DuckDB's `ToString` writes, because the reader is somebody comparing two engines' error
//! messages.

use std::fmt::{self, Write};

use rudb_common::Value;
use rudb_plan::{Expr, ExprRef, Plan};

use crate::schema::Schema;

/// How this bound expression is written in an error message.
#[must_use]
pub fn written(plan: &Plan, expr: ExprRef, schema: &Schema) -> String {
    let mut out = String::new();
    // A `String` is not a writer that can fail, so the result of writing into one says nothing.
    // Dropped rather than unwrapped, because the caller is an error message and an error message is
    // not worth a panic.
    let _ = form(plan, &mut out, expr, schema);
    out
}

fn form<W: Write>(plan: &Plan, out: &mut W, expr: ExprRef, schema: &Schema) -> fmt::Result {
    match *plan.expr(expr) {
        Expr::Column(binding) => match schema.position_of(binding) {
            Some(position) => out.write_str(&schema.fields()[position].name),
            // Unreachable from a message, since an expression over a column the schema does not
            // have fails before it computes anything. Written the way the plan dump writes it
            // rather than panicked on, because no error message is worth a panic.
            None => write!(out, "#{}.{}", binding.table, binding.column),
        },
        // An interval is the one constant that is quoted and cast rather than written plain, which
        // is `'1 day'::INTERVAL`. It is also the only constant of its kind that can reach a
        // message at all: every other non numeric type is folded away before the division that
        // would name it, and dividing an interval by zero is the one division of a non number
        // there is. Measured for #393.
        Expr::Constant(value) => match plan.value(value) {
            held @ Value::Interval { .. } => write!(out, "'{held}'::INTERVAL"),
            held => write!(out, "{held}"),
        },
        Expr::Cast { input, try_cast } => {
            out.write_str(if try_cast { "TRY_CAST(" } else { "CAST(" })?;
            form(plan, out, input, schema)?;
            write!(out, " AS {})", plan.expr_type(expr))
        }
        Expr::Compare { op, left, right } => {
            out.write_char('(')?;
            form(plan, out, left, schema)?;
            write!(out, " {} ", op.symbol())?;
            form(plan, out, right, schema)?;
            out.write_char(')')
        }
        Expr::Conjunction { op, children } => {
            out.write_char('(')?;
            for (position, &child) in plan.expr_list(children).iter().enumerate() {
                if position > 0 {
                    write!(out, " {} ", op.keyword())?;
                }
                form(plan, out, child, schema)?;
            }
            out.write_char(')')
        }
        Expr::Function { name, args } | Expr::Aggregate { name, args, .. } => {
            call(plan, out, plan.string(name), args, schema)
        }
        Expr::Case { arms, otherwise } => {
            out.write_str("CASE")?;
            for arm in plan.arm_list(arms) {
                out.write_str(" WHEN ")?;
                form(plan, out, arm.when, schema)?;
                out.write_str(" THEN ")?;
                form(plan, out, arm.then, schema)?;
            }
            if let Some(otherwise) = otherwise {
                out.write_str(" ELSE ")?;
                form(plan, out, otherwise, schema)?;
            }
            out.write_str(" END")
        }
    }
}

/// A binary operator goes between its operands and everything else goes in front of them.
///
/// Whether a name is an operator is the first character and nothing else: every operator in the
/// catalog is punctuation and every named function starts with a letter, so there is no table to
/// consult. Only the binary case is written between, which is measured rather than assumed: `a // 0`
/// comes out as `(a // 0)` and unary minus comes out as `-(a)`, brackets and all, the same as a call
/// to a function whose name happens to be a dash.
fn call<W: Write>(
    plan: &Plan,
    out: &mut W,
    name: &str,
    args: rudb_plan::Slice,
    schema: &Schema,
) -> fmt::Result {
    let operator = !name.starts_with(|first: char| first.is_alphabetic() || first == '_');
    let args = plan.expr_list(args);
    match (operator, args) {
        (true, [left, right]) => {
            out.write_char('(')?;
            form(plan, out, *left, schema)?;
            write!(out, " {name} ")?;
            form(plan, out, *right, schema)?;
            out.write_char(')')
        }
        _ => {
            write!(out, "{name}(")?;
            for (position, &arg) in args.iter().enumerate() {
                if position > 0 {
                    out.write_str(", ")?;
                }
                form(plan, out, arg, schema)?;
            }
            out.write_char(')')
        }
    }
}

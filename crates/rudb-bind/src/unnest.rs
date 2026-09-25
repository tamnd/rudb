//! `unnest` in a select list, which makes a row per element of a list rather than a value per row.
//!
//! A call binds as a column of a lateral `unnest` placed over everything the block computes and
//! under its projection, which is where the pin runs it: after the grouping and the windows, so
//! `SELECT unnest(list(k)) FROM t` takes apart the list the aggregate made and `count(*) OVER ()`
//! beside an unnest counts the rows before they were multiplied. Every call in one block is taken
//! apart by the same operator, side by side, which is how the pin lines them up: the row makes as
//! many rows as its longest list has elements and a shorter one is null for the rest.
//!
//! `recursive := true` takes apart every level of a nested list and `max_depth := n` the first n,
//! and a call that takes apart several levels is several operators, one per level. The levels of
//! all the calls in a block are lined up at the deepest one, the way the pin lines them up, so a
//! call that takes apart one level while another takes apart two joins in at the second. That is
//! why `unnest([[1, 2], [3]], recursive := true), unnest([10, 20])` pairs 3 with 10 and then null
//! with 20, rather than making its rows before the other list has made any.
//!
//! A struct is the one argument the pin takes that this does not yet. It is turned down rather than
//! guessed at.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_parse::{Ast, ast};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef};

use crate::binder::Binder;
use crate::fold;
use crate::scope::Scope;

/// One `unnest` call a select block wrote, waiting to be planned under the block's projection.
#[derive(Debug, Clone)]
pub(crate) struct UnnestCall {
    /// The list, bound against what the block computes, with an array cast to a list.
    arg: ExprRef,
    /// How many levels the call takes apart.
    depth: usize,
}

/// The type one level down, for a list or an array, and `None` for anything else.
fn element(ty: &LogicalType) -> Option<&LogicalType> {
    match ty {
        LogicalType::List(element) | LogicalType::Array(element, _) => Some(element),
        _ => None,
    }
}

/// How many levels of list or array a type has.
fn nesting(ty: &LogicalType) -> usize {
    element(ty).map_or(0, |inner| 1 + nesting(inner))
}

/// The type `depth` levels down, which for the untyped null is the null again.
fn element_at(ty: &LogicalType, depth: usize) -> LogicalType {
    let mut at = ty;
    for _ in 0..depth {
        match element(at) {
            Some(inner) => at = inner,
            None => break,
        }
    }
    at.clone()
}

impl Binder<'_> {
    /// A call to `unnest` written in an expression, which is a column of the block's unnest.
    pub(crate) fn bind_unnest(
        &mut self,
        ast: &Ast,
        call: ast::ExprRef,
        args: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<ExprRef> {
        if self.in_lambda() {
            return Err(Error::binder("UNNEST in lambda expressions is not supported"));
        }
        if self.in_unnest {
            return Err(Error::binder(
                "Nested UNNEST calls are not supported - use UNNEST(x, recursive := true) to \
                 unnest multiple levels",
            ));
        }
        if !self.unnest_here || self.in_aggregate || self.in_window {
            return Err(Error::binder("UNNEST not supported here"));
        }
        let [arg] = args else {
            if args.is_empty() {
                return Err(Error::binder("UNNEST() requires at least one argument"));
            }
            return Err(Error::binder(
                "UNNEST - unsupported extra argument, unnest only supports recursive := \
                 [true/false], max_depth := # or keep_parent_names := [true/false]",
            ));
        };
        let mut recursive = false;
        let mut max_depth = None;
        for target in ast.named_args(call).to_vec() {
            let name = ast.string(target.alias).to_ascii_lowercase();
            let wanted = match name.as_str() {
                "recursive" | "keep_parent_names" => LogicalType::Boolean,
                "max_depth" => LogicalType::BigInt,
                _ => {
                    return Err(Error::binder(format!(
                        "Unsupported parameter \"{}\" for unnest",
                        ast.string(target.alias)
                    )));
                }
            };
            let bound = self.bind_expr(ast, target.expr, scope)?;
            let bound = self.checked_cast_to(bound, &wanted, false)?;
            let value = match fold::value_of(self.plan(), bound)? {
                Some(Value::Null) => {
                    return Err(Error::binder(format!(
                        "UNNEST parameter \"{name}\" cannot be NULL"
                    )));
                }
                Some(value) => value,
                None => {
                    return Err(Error::binder(format!(
                        "UNNEST parameter \"{name}\" has to be a constant"
                    )));
                }
            };
            match (name.as_str(), value) {
                ("recursive", Value::Boolean(on)) => recursive = on,
                ("max_depth", Value::BigInt(0)) => {
                    return Err(Error::binder("UNNEST cannot have a max depth of 0"));
                }
                ("max_depth", Value::BigInt(depth)) => {
                    max_depth = Some(usize::try_from(depth).map_err(|_| {
                        Error::binder(format!("UNNEST cannot have a max depth of {depth}"))
                    })?);
                }
                _ => {}
            }
        }
        let outer = std::mem::replace(&mut self.in_unnest, true);
        let bound = self.bind_expr(ast, *arg, scope);
        self.in_unnest = outer;
        let bound = self.over_aggregate(bound?, scope)?;
        let ty = self.plan().expr_type(bound).clone();
        let (arg, depth) = match &ty {
            LogicalType::List(_) => (bound, 1),
            LogicalType::Array(inner, _) => {
                let list = LogicalType::List(inner.clone());
                (self.checked_cast_to(bound, &list, false)?, 1)
            }
            LogicalType::Null => (bound, 1),
            LogicalType::Struct(_) => {
                return Err(Error::not_implemented("UNNEST of a STRUCT is not supported yet"));
            }
            other => {
                return Err(Error::binder(format!(
                    "UNNEST() can only be applied to lists, structs and NULL, not {other}"
                )));
            }
        };
        let depth = if let Some(most) = max_depth {
            most.min(nesting(&ty)).max(1)
        } else if recursive {
            nesting(&ty).max(1)
        } else {
            depth
        };
        let index = match self.unnest_index {
            Some(index) => index,
            None => {
                let index = self.fresh_index();
                self.unnest_index = Some(index);
                index
            }
        };
        let position = self.unnests.len();
        self.unnests.push(UnnestCall { arg, depth });
        let produced = element_at(&ty, depth);
        let binding = ColumnBinding::new(index, position as u32);
        Ok(self.add_expr(Expr::Column(binding), produced))
    }

    /// Whether a column is one an `unnest` of the block being bound produces.
    pub(crate) fn is_unnest_output(&self, binding: ColumnBinding) -> bool {
        self.unnest_index == Some(binding.table)
    }

    /// The block's unnests over `node`, one lateral call per level, the last of them producing the
    /// columns the calls were bound to.
    ///
    /// A call that takes apart `d` levels of a block whose deepest call takes apart `deepest` joins
    /// in at level `deepest - d + 1`, reading the list it was written with, and every level after
    /// that reads what the level before made of it.
    pub(crate) fn plan_unnests(
        &mut self,
        mut node: NodeRef,
        index: u32,
        calls: &[UnnestCall],
    ) -> Result<NodeRef> {
        let deepest = calls.iter().map(|call| call.depth).max().unwrap_or(1);
        let function = self.plan_mut().intern("unnest");
        let options = self.plan_mut().add_name_list(&[]);
        let settings = self.plan_mut().add_expr_list(&[]);
        let mut current: Vec<Option<ExprRef>> = vec![None; calls.len()];
        for level in 1..=deepest {
            let at = if level == deepest { index } else { self.fresh_index() };
            let mut args = Vec::new();
            let mut fields = Vec::new();
            let mut taking = Vec::new();
            for (which, call) in calls.iter().enumerate() {
                if deepest - call.depth + 1 > level {
                    continue;
                }
                let mut arg = current[which].unwrap_or(call.arg);
                if let LogicalType::Array(inner, _) = self.plan().expr_type(arg).clone() {
                    arg = self.checked_cast_to(arg, &LogicalType::List(inner), false)?;
                }
                let produced =
                    element(self.plan().expr_type(arg)).cloned().unwrap_or(LogicalType::Null);
                args.push(arg);
                fields.push(Field::new("unnest", produced));
                taking.push(which);
            }
            for (position, (&which, field)) in taking.iter().zip(&fields).enumerate() {
                let binding = ColumnBinding::new(at, position as u32);
                current[which] = Some(self.add_expr(Expr::Column(binding), field.ty.clone()));
            }
            let args = self.plan_mut().add_expr_list(&args);
            let columns = self.plan_mut().add_fields(&fields);
            node = self.add_node(Node::LateralFunction {
                input: node,
                index: at,
                function,
                args,
                options,
                settings,
                columns,
            });
        }
        Ok(node)
    }
}

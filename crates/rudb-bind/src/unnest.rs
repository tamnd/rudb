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
//! A struct is taken apart into columns rather than rows, one per field, and only where the call is
//! the whole of a select target, which is the one place a single expression can stand for several
//! columns. The depth a call is allowed counts the struct levels after the list levels, so
//! `recursive := true` over a list of structs makes a row per element and a column per field, and
//! goes on into a struct inside a struct but not into a list inside one. A struct level makes no
//! rows, so a call that takes apart only a struct is a `struct_extract` per field and no operator at
//! all. `keep_parent_names := true` names a field inside a field by the path to it, `a.x`, and the
//! fields of the struct the call was given by their own names.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_parse::{Ast, ast};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef};

use crate::binder::Binder;
use crate::fold;
use crate::scope::Scope;
use crate::structs::STRUCT_EXTRACT;

/// A struct a root `unnest` left to be taken apart into columns by the target it is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UnnestStruct {
    /// How many levels of struct to take apart.
    pub(crate) depth: usize,
    /// Whether a field inside a field is named by the path to it.
    pub(crate) keep_parent_names: bool,
}

/// An `unnest` a block's `GROUP BY` wrote, which runs under the grouping.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GroupedUnnest {
    /// The list as it was written, before anything was done to it.
    arg: ExprRef,
    depth: usize,
    /// The column the call was bound to.
    column: ExprRef,
}

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
        let root = std::mem::take(&mut self.unnest_root);
        let mut recursive = false;
        let mut keep_parent_names = false;
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
                ("keep_parent_names", Value::Boolean(on)) => keep_parent_names = on,
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
        let bound = bound?;
        let ty = self.plan().expr_type(bound).clone();
        if !matches!(
            ty,
            LogicalType::List(_)
                | LogicalType::Array(..)
                | LogicalType::Struct(_)
                | LogicalType::Null
        ) {
            return Err(Error::binder(format!(
                "UNNEST() can only be applied to lists, structs and NULL, not {ty}"
            )));
        }
        // The depth the call is allowed, spent on the list levels first and then on the struct
        // levels under them.
        let allowed = match max_depth {
            Some(most) => most,
            None if recursive => usize::MAX,
            None => 1,
        };
        let lists = allowed.min(nesting(&ty));
        let produced = element_at(&ty, lists);
        let structs = allowed - lists;
        let expands = structs > 0 && matches!(produced, LogicalType::Struct(_));
        match self.unnest_grouping {
            Some(true) => return Err(Error::binder("Cannot group on an UNNEST or UNLIST clause")),
            Some(false) if expands => {
                return Err(Error::binder("UNNEST of struct cannot be used in GROUP BY clause"));
            }
            _ => {}
        }
        if expands {
            if !root {
                return Err(Error::binder(
                    "UNNEST() on a struct column can only be applied as the root element of a \
                     SELECT expression",
                ));
            }
            self.unnest_struct = Some(UnnestStruct { depth: structs, keep_parent_names });
        }
        if lists == 0 && !matches!(ty, LogicalType::Null) {
            // Only a struct, which makes no rows, so the call is the struct itself for the target
            // to take apart.
            return Ok(bound);
        }
        let depth = lists.max(1);
        if self.unnest_grouping.is_none() {
            // The same call as one the block groups on is the grouped column, which the grouping
            // rule then finds among the keys.
            let grouped = self.grouped_unnests.clone();
            if let Some(found) =
                grouped.iter().find(|held| held.depth == depth && self.same_expr(held.arg, bound))
            {
                return Ok(found.column);
            }
        }
        let written = bound;
        let bound = self.over_aggregate(bound, scope)?;
        let arg = match &ty {
            LogicalType::Array(inner, _) => {
                self.checked_cast_to(bound, &LogicalType::List(inner.clone()), false)?
            }
            _ => bound,
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
        let binding = ColumnBinding::new(index, position as u32);
        let column = self.add_expr(Expr::Column(binding), produced);
        if self.unnest_grouping.is_some() {
            self.grouped_unnests.push(GroupedUnnest { arg: written, depth, column });
        }
        Ok(column)
    }

    /// The columns a root `unnest` of a struct stands for, each a `struct_extract` of `input`, and
    /// their names, taking apart `depth` levels of struct.
    pub(crate) fn unnest_fields(
        &mut self,
        input: ExprRef,
        taking: UnnestStruct,
        prefix: Option<&str>,
        exprs: &mut Vec<ExprRef>,
        names: &mut Vec<String>,
    ) -> Result<()> {
        let LogicalType::Struct(fields) = self.plan().expr_type(input).clone() else {
            return Err(Error::internal("a struct unnest of something that is not a struct"));
        };
        let recorded = self.plan_mut().intern(STRUCT_EXTRACT);
        for (at, field) in fields.iter().enumerate() {
            let key = self.add_constant(Value::BigInt(at as i64 + 1));
            let args = self.plan_mut().add_expr_list(&[input, key]);
            let expr = self.add_expr(Expr::Function { name: recorded, args }, field.ty.clone());
            // The fields of an unnamed struct, `row(1, 2)`, are named by their place.
            let own = if field.name.is_empty() {
                format!("element{}", at + 1)
            } else {
                field.name.clone()
            };
            let name = match prefix {
                Some(prefix) if taking.keep_parent_names => format!("{prefix}.{own}"),
                _ => own,
            };
            if taking.depth > 1 && matches!(field.ty, LogicalType::Struct(_)) {
                let deeper = UnnestStruct { depth: taking.depth - 1, ..taking };
                // An unnamed field has no name to put in front of its own fields'.
                let prefix = (!field.name.is_empty()).then_some(name.as_str());
                self.unnest_fields(expr, deeper, prefix, exprs, names)?;
            } else {
                exprs.push(expr);
                names.push(name);
            }
        }
        Ok(())
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

//! Binding a function that takes a lambda.
//!
//! `list_transform(l, lambda x: x + 1)` is not an ordinary call, because its second argument is not
//! a value. It is an expression over names that only mean something per element, so it cannot be
//! bound before the function has looked at the list and said what the element is. The function
//! binds it, then: the list first, then the body with the parameters in scope, typed as the element
//! and as a `BIGINT` position that counts from one.
//!
//! The parameters are bound as the columns of a table index of their own, which is what the pin
//! does too, and a body reads them with [`Expr::LambdaParam`]. Everything else in the body is bound
//! the ordinary way against the scope the call was written in, so an outer column is captured and
//! is one value per row, while the parameters are one value per element. An aggregate or a window
//! in the body is computed over the rows, and its arguments are bound with the parameters hidden,
//! which is what makes `lambda x: x + sum(k)` fine and `lambda x: sum(x)` a column that does not
//! resolve. The pin raises an internal error for the second one, which is tamnd/duckdb#12.

use rudb_common::{Error, LogicalType, Result};
use rudb_parse::Ast;
use rudb_parse::ast::{self, BinaryOp};
use rudb_plan::{ColumnBinding, Expr, ExprRef};

use crate::binder::Binder;
use crate::scope::Scope;

/// Every name a function that takes a lambda answers to, with the name the plan records.
///
/// The recorded name is the one the executor dispatches on and the one a wrong call is reported
/// under, which is how the aliases work everywhere else in the catalog.
const LAMBDA_FUNCTIONS: &[(&str, &str)] = &[
    ("list_transform", TRANSFORM),
    ("list_apply", TRANSFORM),
    ("apply", TRANSFORM),
    ("array_transform", TRANSFORM),
    ("array_apply", TRANSFORM),
    ("list_filter", FILTER),
    ("filter", FILTER),
    ("array_filter", FILTER),
    ("list_reduce", REDUCE),
    ("array_reduce", REDUCE),
    ("reduce", REDUCE),
];

/// Every element through the body, in order.
pub(crate) const TRANSFORM: &str = "list_transform";

/// The elements the body holds for, in order.
pub(crate) const FILTER: &str = "list_filter";

/// Every element folded into one value, left to right.
pub(crate) const REDUCE: &str = "list_reduce";

/// The name the plan records for a name written in a call, if it is a function that takes a lambda.
pub(crate) fn lambda_function(written: &str) -> Option<&'static str> {
    LAMBDA_FUNCTIONS
        .iter()
        .find(|(name, _)| rudb_catalog::same_name(written, name))
        .map(|&(_, recorded)| recorded)
}

/// One lambda whose body is being bound.
#[derive(Debug, Clone)]
pub(crate) struct Frame {
    /// The table index its parameters are bound under.
    table: u32,
    /// The parameters as written, which is the case the column heading keeps.
    names: Vec<String>,
    /// What each one is.
    types: Vec<LogicalType>,
}

/// Whether an argument is a lambda written with the arrow the pin no longer accepts, `x -> x + 1`
/// or `(x, i) -> x * i`.
///
/// It is an arrow over a bare name or a row of bare names, which is what the pin tests before it
/// decides the arrow is not the JSON operator. Anything else on the left of an arrow is the JSON
/// operator and is bound as one.
fn arrow_lambda(ast: &Ast, arg: ast::ExprRef) -> bool {
    let bare = |expr: ast::ExprRef| matches!(ast.expr(expr), ast::Expr::Column { name } if ast.name(name).count() == 1);
    match ast.expr(arg) {
        ast::Expr::Binary { op: BinaryOp::Arrow, left, .. } => match ast.expr(left) {
            ast::Expr::Row { items } => ast.expr_list(items).iter().all(|&item| bare(item)),
            _ => bare(left),
        },
        _ => false,
    }
}

/// Whether an argument is a lambda in either spelling.
fn any_lambda(ast: &Ast, arg: ast::ExprRef) -> bool {
    matches!(ast.expr(arg), ast::Expr::Lambda { .. }) || arrow_lambda(ast, arg)
}

impl Binder<'_> {
    /// Whether a lambda's body is being bound.
    pub(crate) fn in_lambda(&self) -> bool {
        !self.lambda_frames.is_empty()
    }

    /// The parameter a bare name means, from the innermost lambda that has it.
    pub(crate) fn lambda_parameter(&mut self, word: &str) -> Option<ExprRef> {
        let (binding, ty) = self.lambda_frames.iter().rev().find_map(|frame| {
            let at = frame.names.iter().position(|name| rudb_catalog::same_name(name, word))?;
            Some((ColumnBinding::new(frame.table, at as u32), frame.types[at].clone()))
        })?;
        Some(self.add_expr(Expr::LambdaParam(binding), ty))
    }

    /// Binds a call to `list_transform`, `list_filter` or `list_reduce`, by any of their names.
    ///
    /// The list is bound first, and its type is what the parameters are typed from. A list that is
    /// the untyped null is the untyped null out, before the body is looked at, which is the pin's
    /// answer and the type it gives it. The call is recorded under `recorded` with the list and the
    /// lambda as its arguments, and `list_reduce`'s initial value after them when it has one. It
    /// bypasses the signature table, because there is no type for the lambda to resolve against and
    /// the return type is the body's, which the table cannot say.
    pub(crate) fn bind_lambda_call(
        &mut self,
        ast: &Ast,
        recorded: &'static str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<ExprRef> {
        let reduce = recorded == REDUCE;
        let (list, lambda, initial) = match *arguments {
            [list, lambda] => (list, lambda, None),
            [list, lambda, initial] if reduce => (list, lambda, Some(initial)),
            _ => return Err(self.no_lambda_match(ast, recorded, arguments, scope)),
        };
        if any_lambda(ast, list)
            || !any_lambda(ast, lambda)
            || initial.is_some_and(|initial| any_lambda(ast, initial))
        {
            return Err(self.no_lambda_match(ast, recorded, arguments, scope));
        }
        let ast::Expr::Lambda { params, body } = ast.expr(lambda) else {
            return Err(Error::binder(
                "Deprecated lambda arrow (->) detected. Please transition to the new lambda \
                 syntax, i.e.., lambda x, i: x + i, before DuckDB's next release.\nUse SET \
                 lambda_syntax='ENABLE_SINGLE_ARROW' to revert to the deprecated behavior.\nFor \
                 more information, see https://duckdb.org/docs/current/sql/functions/lambda.html.",
            ));
        };
        let names: Vec<String> = ast.name(params).map(str::to_string).collect();
        if names.len() > 3 || (names.len() > 2 && !reduce) {
            return Err(Error::binder(format!(
                "This lambda function only supports up to {} lambda parameters!",
                if reduce { "three" } else { "two" }
            )));
        }
        // The pin binds the parameters as a table it names after them, and a repeated name is the
        // error that table raises, in its words.
        for (at, name) in names.iter().enumerate() {
            if names[..at].iter().any(|earlier| rudb_catalog::same_name(earlier, name)) {
                return Err(Error::binder(format!(
                    "table \"0_macro_parameters({})\" has duplicate column name \"{name}\"",
                    names.join(", ")
                )));
            }
        }
        let mut list = self.bind_expr(ast, list, scope)?;
        let element = match self.plan().expr_type(list).clone() {
            LogicalType::List(element) => *element,
            LogicalType::Array(element, _) => {
                let as_list = LogicalType::List(element.clone());
                list = self.cast_to(list, &as_list);
                *element
            }
            LogicalType::Null => return Ok(self.add_constant(rudb_common::Value::Null)),
            _ => {
                return Err(Error::binder("Invalid LIST argument during lambda function binding!"));
            }
        };
        let list_type = self.plan().expr_type(list).clone();
        let initial = match initial {
            Some(initial) => Some(self.bind_expr(ast, initial, scope)?),
            None => None,
        };
        let table = self.fresh_index();
        let interned: Vec<_> = names.iter().map(|name| self.plan_mut().intern(name)).collect();
        let params = self.plan_mut().add_name_list(&interned);
        let (body, returns, initial) = if reduce {
            self.bind_reduce(ast, body, scope, table, &names, element, initial)?
        } else {
            let types = [element, LogicalType::BigInt][..names.len()].to_vec();
            let mut body = self.bind_lambda_body(ast, body, scope, table, &names, types)?;
            // A filter's body is a condition, and one that is not a boolean is cast to one the way
            // a `WHERE` would be. `lambda x: x % 2` keeps the odd elements and a null drops one.
            if recorded == FILTER && self.plan().expr_type(body) != &LogicalType::Boolean {
                body = self.checked_cast_to(body, &LogicalType::Boolean, false)?;
            }
            let returns = if recorded == FILTER {
                list_type
            } else {
                LogicalType::List(Box::new(self.plan().expr_type(body).clone()))
            };
            (body, returns, None)
        };
        let body_type = self.plan().expr_type(body).clone();
        let lambda = self.add_expr(Expr::Lambda { table, params, body }, body_type);
        let args = match initial {
            Some(initial) => self.plan_mut().add_expr_list(&[list, lambda, initial]),
            None => self.plan_mut().add_expr_list(&[list, lambda]),
        };
        let name = self.plan_mut().intern(recorded);
        Ok(self.add_expr(Expr::Function { name, args }, returns))
    }

    /// Binds a lambda's body with its parameters in scope as `types`.
    fn bind_lambda_body(
        &mut self,
        ast: &Ast,
        body: ast::ExprRef,
        scope: &Scope,
        table: u32,
        names: &[String],
        types: Vec<LogicalType>,
    ) -> Result<ExprRef> {
        self.lambda_frames.push(Frame { table, names: names.to_vec(), types });
        let body = self.bind_expr(ast, body, scope);
        self.lambda_frames.pop();
        body
    }

    /// Binds `list_reduce`'s body, and settles the type the accumulator is carried in.
    ///
    /// The parameters are the accumulator, the element and a `BIGINT` position. The accumulator
    /// starts as the initial value's type, or the element's when there is none, and what the body
    /// makes of it may be wider, so the pin binds the body again with the accumulator widened to
    /// the two's common type. It does that once and not until nothing changes, because a decimal
    /// grows a digit every time it is added to and would never settle. That is why the answer to
    /// `list_reduce([1.5, 2, 3], lambda x, y: x + y)` is a `DECIMAL(13,1)` over a list of
    /// `DECIMAL(11,1)`: two additions' worth of width, from two bindings. With an initial value the
    /// second binding's decimal is cast back to the first one's instead, so the same sum starting
    /// from `1.5` is a `DECIMAL(12,1)`. Both are measured, and this follows the pin's
    /// `MaybeRebindListReduceLambda` and `ListReduceBind` step for step.
    ///
    /// Returns the body cast to the accumulator's type, that type, and the initial value cast to it.
    #[allow(clippy::too_many_arguments)]
    fn bind_reduce(
        &mut self,
        ast: &Ast,
        body: ast::ExprRef,
        scope: &Scope,
        table: u32,
        names: &[String],
        element: LogicalType,
        initial: Option<ExprRef>,
    ) -> Result<(ExprRef, LogicalType, Option<ExprRef>)> {
        let count = names.len();
        let typed = |accumulator: &LogicalType| {
            [accumulator.clone(), element.clone(), LogicalType::BigInt][..count].to_vec()
        };
        let initial_type = initial.map(|initial| self.plan().expr_type(initial).clone());
        let start = initial_type.clone().unwrap_or_else(|| element.clone());
        let first = self.bind_lambda_body(ast, body, scope, table, names, typed(&start))?;
        let returned = self.plan().expr_type(first).clone();
        let widened = common_type(&start, &returned)
            .ok_or_else(|| no_common_type(&start, &returned, initial.is_some()))?;
        let (mut body, accumulator) = if initial.is_some() {
            let mut body =
                self.bind_lambda_body(ast, body, scope, table, names, typed(&widened))?;
            let returned = self.plan().expr_type(body).clone();
            if (has_decimal(&widened) || has_decimal(&returned)) && returned != widened {
                body = self.cast_to(body, &widened);
            }
            let returned = self.plan().expr_type(body).clone();
            let accumulator = common_type(&widened, &returned)
                .ok_or_else(|| no_common_type(&widened, &returned, true))?;
            (body, accumulator)
        } else if widened != element {
            let body = self.bind_lambda_body(ast, body, scope, table, names, typed(&widened))?;
            let returned = self.plan().expr_type(body).clone();
            let accumulator = common_type(&element, &returned)
                .ok_or_else(|| no_common_type(&element, &returned, false))?;
            (body, accumulator)
        } else {
            (first, widened)
        };
        if !(2..=3).contains(&count) {
            return Err(Error::binder("list_reduce expects a function with 2 or 3 arguments"));
        }
        if self.plan().expr_type(body) != &accumulator {
            body = self.cast_to(body, &accumulator);
        }
        let initial = initial.map(|initial| {
            if self.plan().expr_type(initial) == &accumulator {
                initial
            } else {
                self.cast_to(initial, &accumulator)
            }
        });
        Ok((body, accumulator, initial))
    }

    /// The pin's refusal of a call to a lambda function with the wrong arguments, which names a
    /// lambda's type as `LAMBDA`.
    fn no_lambda_match(
        &mut self,
        ast: &Ast,
        recorded: &str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Error {
        let mut types = Vec::with_capacity(arguments.len());
        for &arg in arguments {
            if any_lambda(ast, arg) {
                types.push("LAMBDA".to_string());
                continue;
            }
            match self.bind_expr(ast, arg, scope) {
                Ok(bound) => types.push(self.plan().expr_type(bound).to_string()),
                Err(error) => return error,
            }
        }
        let candidates = if recorded == REDUCE {
            "\tlist_reduce(col0 ANY[], col1 LAMBDA) -> ANY\n\tlist_reduce(col0 ANY[], col1 LAMBDA, \
             col2 ANY) -> ANY\n"
                .to_string()
        } else {
            format!("\t{recorded}(col0 ANY[], col1 LAMBDA) -> ANY[]\n")
        };
        Error::binder(format!(
            "No function matches the given name and argument types '{recorded}({})'. You might \
             need to add explicit type casts.\n\tCandidate functions:\n{candidates}",
            types.join(", ")
        ))
    }
}

/// The type two types meet at, which is the pin's `TryGetMaxLogicalType` for what a lambda body
/// produces.
///
/// It is the promotion every other operator uses, plus a boolean meeting a number at the number,
/// which the pin allows here because a boolean casts to any number implicitly. That is what makes
/// `list_reduce([1, 2, 3], lambda x, y: x > y)` an `INTEGER` of `0` rather than a refusal.
fn common_type(left: &LogicalType, right: &LogicalType) -> Option<LogicalType> {
    match (left, right) {
        (LogicalType::Boolean, number) | (number, LogicalType::Boolean) if number.is_numeric() => {
            Some(number.clone())
        }
        _ => left.promote(right),
    }
}

/// The pin's refusal when the body makes something the accumulator cannot hold.
fn no_common_type(start: &LogicalType, returned: &LogicalType, initial: bool) -> Error {
    let what = if initial { "initial value type" } else { "list element type" };
    Error::binder(format!(
        "No common super type between {what} {start} and lambda return type {returned}"
    ))
}

/// Whether a decimal is anywhere in a type, nested or not.
fn has_decimal(ty: &LogicalType) -> bool {
    matches!(ty, LogicalType::Decimal { .. }) || ty.children().iter().any(has_decimal)
}

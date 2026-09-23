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
];

/// Every element through the body, in order.
pub(crate) const TRANSFORM: &str = "list_transform";

/// The elements the body holds for, in order.
pub(crate) const FILTER: &str = "list_filter";

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

    /// Binds a call to `list_transform` or `list_filter`, by any of their names.
    ///
    /// The list is bound first, and its type is what the parameters are typed from. A list that is
    /// the untyped null is the untyped null out, before the body is looked at, which is the pin's
    /// answer and the type it gives it. The call is recorded under `recorded` with the list and the
    /// lambda as its two arguments, and it bypasses the signature table, because there is no type
    /// for the lambda to resolve against and the return type is the body's, which the table cannot
    /// say.
    pub(crate) fn bind_lambda_call(
        &mut self,
        ast: &Ast,
        recorded: &'static str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<ExprRef> {
        let [list, lambda] = *arguments else {
            return Err(self.no_lambda_match(ast, recorded, arguments, scope));
        };
        if any_lambda(ast, list) || !any_lambda(ast, lambda) {
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
        if names.len() > 2 {
            return Err(Error::binder(
                "This lambda function only supports up to two lambda parameters!",
            ));
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
        let table = self.fresh_index();
        let types = [element, LogicalType::BigInt][..names.len()].to_vec();
        let interned: Vec<_> = names.iter().map(|name| self.plan_mut().intern(name)).collect();
        let params = self.plan_mut().add_name_list(&interned);
        self.lambda_frames.push(Frame { table, names, types });
        let body = self.bind_expr(ast, body, scope);
        self.lambda_frames.pop();
        let mut body = body?;
        // A filter's body is a condition, and one that is not a boolean is cast to one the way a
        // `WHERE` would be. `lambda x: x % 2` keeps the odd elements and a null drops one.
        if recorded == FILTER && self.plan().expr_type(body) != &LogicalType::Boolean {
            body = self.checked_cast_to(body, &LogicalType::Boolean, false)?;
        }
        let body_type = self.plan().expr_type(body).clone();
        let returns = if recorded == FILTER {
            list_type
        } else {
            LogicalType::List(Box::new(body_type.clone()))
        };
        let lambda = self.add_expr(Expr::Lambda { table, params, body }, body_type);
        let args = self.plan_mut().add_expr_list(&[list, lambda]);
        let name = self.plan_mut().intern(recorded);
        Ok(self.add_expr(Expr::Function { name, args }, returns))
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
        Error::binder(format!(
            "No function matches the given name and argument types '{recorded}({})'. You might \
             need to add explicit type casts.\n\tCandidate functions:\n\t{recorded}(col0 ANY[], \
             col1 LAMBDA) -> ANY[]\n",
            types.join(", ")
        ))
    }
}

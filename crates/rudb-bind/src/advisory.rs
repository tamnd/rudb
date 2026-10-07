//! The advisory lock functions of a PostgreSQL session, such as `pg_advisory_lock(1)`.
//!
//! These functions are not in the function list of the pin, so the binder makes the calls itself,
//! as it does for `nextval`. It adds the backend number of the session, the OID of its database
//! and its `lock_timeout` before the keys, which is all that the kernel in
//! [`rudb_kernels::advisory`] needs to know about the session.

use rudb_common::{Error, ErrorCode, LogicalType, Result, SqlState, Value};
use rudb_parse::ast::{self, Ast, LiteralKind};
use rudb_plan::{Expr, ExprRef};

use crate::binder::Binder;
use crate::scope::Scope;

fn advisory_name(written: &str) -> Option<&'static str> {
    rudb_kernels::advisory::FUNCTIONS
        .into_iter()
        .find(|name| rudb_catalog::same_name(written, name))
}

/// Whether a target of the select list is a call of an advisory lock function or of `pg_sleep`
/// that gives `void`.
pub(crate) fn gives_void(ast: &Ast, expr: ast::ExprRef) -> bool {
    match ast.expr(expr) {
        ast::Expr::Function { name, .. } => ast.name(name).last().is_some_and(|written| {
            rudb_catalog::same_name(written, "pg_sleep")
                || advisory_name(written).is_some_and(rudb_kernels::advisory::gives_void)
        }),
        _ => false,
    }
}

impl Binder<'_> {
    /// The call of an advisory lock function in a PostgreSQL session, or `None` for another name
    /// or another session.
    pub(crate) fn advisory_call(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        let Some(name) = advisory_name(written) else {
            return Ok(None);
        };
        let Some(postgres) = self.session.postgres() else {
            return Ok(None);
        };
        let backend = i64::from(postgres.backend);
        let database = i64::from(postgres.database);
        let timeout = match rudb_common::guc::find("lock_timeout")
            .map(|parameter| postgres.settings.setting(parameter))
        {
            Some(rudb_common::guc::Setting::Int(ms)) => i64::from(ms),
            _ => 0,
        };
        let mut bound = Vec::with_capacity(arguments.len());
        let mut unknown = Vec::with_capacity(arguments.len());
        // A string literal is read as the key type, as in PostgreSQL, so that a bad key gives the
        // error of the input function.
        let key = match arguments.len() {
            1 => Some(rudb_pgtypes::oid::INT8),
            2 => Some(rudb_pgtypes::oid::INT4),
            _ => None,
        };
        for &argument in arguments {
            let literal =
                matches!(ast.expr(argument), ast::Expr::Literal { kind: LiteralKind::String, .. });
            let read = key.and_then(|oid| self.read_literal(ast, argument, oid));
            bound.push(match read {
                Some(value) => value?,
                None => self.bind_expr(ast, argument, scope)?,
            });
            unknown.push(literal);
        }
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        // `pg_advisory_unlock_all()` takes no key, the others take one `bigint` key or two
        // `integer` keys. An argument of another type has no implicit cast to the key.
        let wanted = match (name, bound.len()) {
            ("pg_advisory_unlock_all", 0) => Some(LogicalType::BigInt),
            ("pg_advisory_unlock_all", _) => None,
            (_, 1) => Some(LogicalType::BigInt),
            (_, 2) => Some(LogicalType::Integer),
            _ => None,
        };
        let long = wanted == Some(LogicalType::BigInt);
        let fits = |ty: &LogicalType, literal: bool| {
            use LogicalType as L;
            let integer = [L::TinyInt, L::SmallInt, L::Integer, L::UTinyInt, L::USmallInt];
            literal
                || *ty == L::Null
                || integer.contains(ty)
                || (long && matches!(ty, L::BigInt | L::UInteger))
        };
        let fit = types.iter().zip(&unknown).all(|(ty, &literal)| fits(ty, literal));
        let Some(wanted) = wanted.filter(|_| fit) else {
            return Err(no_such_function(name, &types, &unknown));
        };
        let mut args = vec![
            self.add_constant(Value::BigInt(backend)),
            self.add_constant(Value::BigInt(database)),
            self.add_constant(Value::BigInt(timeout)),
        ];
        for arg in bound {
            args.push(self.checked_cast_to(arg, &wanted, false)?);
        }
        let returns = match rudb_kernels::advisory::gives_void(name) {
            true => LogicalType::Varchar,
            false => LogicalType::Boolean,
        };
        let args = self.plan_mut().add_expr_list(&args);
        let name = self.plan_mut().intern(name);
        Ok(Some(self.add_expr(Expr::Function { name, args }, returns)))
    }
}

impl Binder<'_> {
    /// The call `pg_sleep(seconds)` in a PostgreSQL session, or `None` for another name or another
    /// session. The kernel reads the backend number to find the cancel flag of the session.
    pub(crate) fn sleep_call(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        if !rudb_catalog::same_name(written, "pg_sleep") {
            return Ok(None);
        }
        let Some(postgres) = self.session.postgres() else {
            return Ok(None);
        };
        let backend = i64::from(postgres.backend);
        let [seconds] = arguments[..] else {
            let unknown: Vec<bool> = arguments
                .iter()
                .map(|&argument| {
                    matches!(
                        ast.expr(argument),
                        ast::Expr::Literal { kind: LiteralKind::String, .. }
                    )
                })
                .collect();
            let mut types = Vec::with_capacity(arguments.len());
            for &argument in arguments {
                let bound = self.bind_expr(ast, argument, scope)?;
                types.push(self.plan().expr_type(bound).clone());
            }
            return Err(no_such_function("pg_sleep", &types, &unknown));
        };
        let bound = match self.read_literal(ast, seconds, rudb_pgtypes::oid::FLOAT8) {
            Some(value) => value?,
            None => self.bind_expr(ast, seconds, scope)?,
        };
        let seconds = self.checked_cast_to(bound, &LogicalType::Double, false)?;
        let args = [self.add_constant(Value::BigInt(backend)), seconds];
        let args = self.plan_mut().add_expr_list(&args);
        let name = self.plan_mut().intern("pg_sleep");
        Ok(Some(self.add_expr(Expr::Function { name, args }, LogicalType::Varchar)))
    }
}

/// The error of PostgreSQL for a call of an advisory lock function that no form of it takes.
fn no_such_function(name: &str, types: &[LogicalType], unknown: &[bool]) -> Error {
    let spelled = types
        .iter()
        .zip(unknown)
        .map(|(ty, &unknown)| match unknown || *ty == LogicalType::Null {
            true => "unknown".into(),
            false => rudb_pgtypes::format_type(rudb_pgtypes::pg_type(ty).oid),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!("function {name}({spelled}) does not exist");
    Error::new(ErrorCode::Binder, message.clone())
        .state(SqlState::UNDEFINED_FUNCTION)
        .pg(message)
        .detail("No function of that name accepts the given argument types.")
        .hint("You might need to add explicit type casts.")
}

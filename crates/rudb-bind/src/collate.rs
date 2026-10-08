//! The collations of a PostgreSQL session.
//!
//! rudb compares and sorts text by its bytes, which is the order of the `C` collation. A `COLLATE`
//! names a collation of `pg_collation` and is checked as `transformCollateClause` checks it: the
//! type must be one that has a collation, and the name must be a collation. Two different
//! collations written with `COLLATE` must not meet in one operator or function, which is the rule
//! of `assign_collations_walker` in `parse_collate.c`.

use std::collections::HashMap;

use rudb_common::{Error, LogicalType, Result, Span, SqlState, Value};
use rudb_parse::{Ast, ast};
use rudb_plan::{Expr, ExprRef};

use crate::binder::Binder;
use crate::expr::{postgres_oid, written_oid};
use crate::scope::Scope;

/// The OID of the collation `default`, which stands for the collation of the database.
const DEFAULT_COLLATION: u32 = 100;

/// The collation of every database of rudb, as `datcollate` and `datctype` report it.
const DATABASE_COLLATION: &str = "C";

/// A collation written with `COLLATE`, and where the `COLLATE` was written.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Explicit {
    oid: u32,
    span: Span,
}

/// The collations a statement wrote and the ones the expressions above them took.
#[derive(Debug, Default)]
pub(crate) struct Collated {
    /// The bound expressions a `COLLATE` was written on, with the collation it named.
    written: HashMap<ExprRef, Explicit>,
    /// The collation each expression took from its inputs, kept so that each one is found once.
    derived: HashMap<ExprRef, Option<Explicit>>,
}

/// Whether a value of `ty` has a collation, which a null of no type also has, as an `unknown`
/// does in PostgreSQL.
fn collatable(ty: &LogicalType) -> bool {
    match ty {
        LogicalType::Null => true,
        ty => postgres_oid(ty)
            .and_then(rudb_pgtypes::TypeInfo::get)
            .is_some_and(|info| info.collation != 0),
    }
}

/// The name of a collation for an error.
fn collation_name(oid: u32) -> &'static str {
    rudb_pgtypes::collation_by_oid(oid).map_or("", |collation| collation.name)
}

impl Binder<'_> {
    /// Binds `left COLLATE right`, where `right` is the name of the collation.
    pub(crate) fn bind_collate(
        &mut self,
        ast: &Ast,
        left: ast::ExprRef,
        right: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let span = self.current_span;
        let bound = self.bind_expr(ast, left, scope)?;
        let ty = self.plan().expr_type(bound).clone();
        if !collatable(&ty) {
            let name = written_oid(ast, left, &ty)
                .map_or_else(|| ty.to_string(), |oid| rudb_pgtypes::format_type(oid).into_owned());
            return Err(Error::binder(format!("collations are not supported by type {name}"))
                .state(SqlState::DATATYPE_MISMATCH)
                .with_span(span));
        }
        let parts: Vec<&str> = match ast.expr(right) {
            ast::Expr::Column { name } => ast.name(name).collect(),
            _ => Vec::new(),
        };
        let Some(found) = self.find_collation(&parts).map_err(|error| error.with_span(span))?
        else {
            let name = parts.join(".");
            return Err(Error::binder(format!(
                "collation \"{name}\" for encoding \"UTF8\" does not exist"
            ))
            .state(SqlState::UNDEFINED_OBJECT)
            .with_span(span));
        };
        // A second `COLLATE` replaces the first, as in `x COLLATE "C" COLLATE "POSIX"`.
        self.collated.derived.remove(&bound);
        self.collated.written.insert(bound, Explicit { oid: found.oid, span });
        Ok(bound)
    }

    /// The collation that the parts of a name written after `COLLATE` name, as `LookupCollation`
    /// finds it. The built-in collations are in `pg_catalog`, and a name of three parts names the
    /// database first.
    fn find_collation(&self, parts: &[&str]) -> Result<Option<&'static rudb_pgtypes::Collation>> {
        let database = self.catalog().default_catalog();
        let (schema, name) = match parts {
            [name] => (None, *name),
            [schema, name] => (Some(*schema), *name),
            [catalog, schema, name] if *catalog == database => (Some(*schema), *name),
            [_, _, _] => {
                return Err(Error::binder(format!(
                    "cross-database references are not implemented: {}",
                    parts.join(".")
                ))
                .state(SqlState::FEATURE_NOT_SUPPORTED));
            }
            _ => {
                return Err(Error::binder(format!(
                    "improper qualified name (too many dotted names): {}",
                    parts.join(".")
                ))
                .state(SqlState::SYNTAX_ERROR));
            }
        };
        match schema {
            None | Some("pg_catalog") => Ok(rudb_pgtypes::collation(name)),
            Some(schema) if self.catalog().has_schema(database, schema) => Ok(None),
            Some(schema) => Err(Error::binder(format!("schema \"{schema}\" does not exist"))
                .state(SqlState::INVALID_SCHEMA_NAME)),
        }
    }

    /// Checks that the collations written under `expr` agree, once a statement has written one.
    pub(crate) fn check_collations(&mut self, expr: ExprRef) -> Result<()> {
        if !self.collated.written.is_empty() {
            self.derive_collation(expr)?;
        }
        Ok(())
    }

    /// Gives `to` the collation that `from` takes from its inputs, for the column that an
    /// aggregate or a window call is read through, so the expression above the call sees it.
    pub(crate) fn carry_collation(&mut self, from: ExprRef, to: ExprRef) -> Result<()> {
        if self.collated.written.is_empty() {
            return Ok(());
        }
        if let Some(found) = self.derive_collation(from)? {
            self.collated.written.insert(to, found);
        }
        Ok(())
    }

    /// The collation written with `COLLATE` that `expr` takes from its inputs, if any.
    ///
    /// A comparison or a function takes the collation of its inputs, and two different ones are
    /// an error at the second. The result keeps the collation only when its type has one, so
    /// `length(a COLLATE "C") = length(b COLLATE "POSIX")` is allowed. The conditions of a `CASE`
    /// are expressions of their own, and the result takes the collation of the branches.
    fn derive_collation(&mut self, expr: ExprRef) -> Result<Option<Explicit>> {
        if let Some(&written) = self.collated.written.get(&expr) {
            return Ok(Some(written));
        }
        if let Some(&derived) = self.collated.derived.get(&expr) {
            return Ok(derived);
        }
        let plan = self.plan();
        let inputs: Vec<ExprRef> = match plan.expr(expr) {
            Expr::Cast { input, .. } => vec![*input],
            Expr::Compare { left, right, .. } => vec![*left, *right],
            Expr::Conjunction { children: args, .. }
            | Expr::Function { args, .. }
            | Expr::Aggregate { args, .. }
            | Expr::Window { args, .. } => plan.expr_list(*args).to_vec(),
            Expr::Case { arms, otherwise } => {
                let arms = plan.arm_list(*arms).to_vec();
                let otherwise = *otherwise;
                for arm in &arms {
                    self.derive_collation(arm.when)?;
                }
                arms.iter().map(|arm| arm.then).chain(otherwise).collect()
            }
            Expr::Column(_) | Expr::Constant(_) | Expr::Lambda { .. } | Expr::LambdaParam(_) => {
                Vec::new()
            }
        };
        let merged = self.merge_collations(&inputs)?;
        let result = merged.filter(|_| collatable(self.plan().expr_type(expr)));
        self.collated.derived.insert(expr, result);
        Ok(result)
    }

    /// The collation written with `COLLATE` that the inputs of an operator or a function take
    /// together. Two different ones are an error at the second.
    fn merge_collations(&mut self, inputs: &[ExprRef]) -> Result<Option<Explicit>> {
        let mut merged: Option<Explicit> = None;
        for &input in inputs {
            let Some(found) = self.derive_collation(input)? else { continue };
            match merged {
                None => merged = Some(found),
                Some(first) if first.oid != found.oid => {
                    return Err(Error::binder(format!(
                        "collation mismatch between explicit collations \"{}\" and \"{}\"",
                        collation_name(first.oid),
                        collation_name(found.oid)
                    ))
                    .state(SqlState::COLLATION_MISMATCH)
                    .with_span(found.span));
                }
                Some(_) => {}
            }
        }
        Ok(merged)
    }

    /// The OID of the collation of a call over `args`, as `PG_GET_COLLATION` gives it to the
    /// function: the collation written with `COLLATE` that the arguments take, or else the
    /// default collation of the database, which is the collation of the database itself.
    pub(crate) fn call_collation(&mut self, args: &[ExprRef]) -> Result<u32> {
        let explicit = match self.collated.written.is_empty() {
            true => None,
            false => self.merge_collations(args)?,
        };
        let oid = explicit.map_or(DEFAULT_COLLATION, |explicit| explicit.oid);
        Ok(match oid {
            DEFAULT_COLLATION => rudb_pgtypes::collation(DATABASE_COLLATION)
                .map_or(DEFAULT_COLLATION, |collation| collation.oid),
            oid => oid,
        })
    }

    /// `ILIKE` or `NOT ILIKE` of two strings in PostgreSQL. `Generic_Text_IC_like` matches the
    /// lower case of the text against the lower case of the pattern with `LIKE`, where `lower`
    /// maps the case by the collation of the call. So the collation `C` folds only the ASCII
    /// letters, and `pg_c_utf8` folds each letter that has a lower case.
    pub(crate) fn pg_ilike(
        &mut self,
        negated: bool,
        left: ExprRef,
        right: ExprRef,
    ) -> Result<ExprRef> {
        let oid = self.call_collation(&[left, right])?;
        let oid = self.add_constant(Value::BigInt(i64::from(oid)));
        let mut sides = Vec::with_capacity(2);
        for side in [left, right] {
            let side = self.cast_to(side, &LogicalType::Varchar);
            sides.push(self.pgproc_kernel("lower", &[side, oid], LogicalType::Varchar));
        }
        self.call(if negated { "!~~" } else { "~~" }, sides)
    }
}

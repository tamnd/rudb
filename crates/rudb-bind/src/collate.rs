//! The collations of a PostgreSQL session.
//!
//! rudb compares and sorts text by its bytes, which is the order of the `C` collation. A `COLLATE`
//! names a collation of `pg_collation` and is checked as `transformCollateClause` checks it: the
//! type must be one that has a collation, and the name must be a collation.
//!
//! Each expression takes a collation from its inputs by the rules of `assign_collations_walker` in
//! `parse_collate.c`. A collation written with `COLLATE` is explicit. A column of a subquery, of a
//! `WITH` query, of a `VALUES` list or of a set operation holds the collation of the expressions
//! that make it, and that collation is implicit. An explicit collation wins over an implicit one,
//! and two different explicit collations are an error. Two different implicit collations are a
//! conflict, which is an error only where a collation is necessary: in a function that reads the
//! collation, in a comparison of text, and in a column that a set operation, `ORDER BY`, `GROUP BY`
//! or `DISTINCT` sorts on. The collation `default` is the weakest implicit collation, and here it
//! is no collation at all.

use std::collections::HashMap;

use rudb_common::{Error, LogicalType, Result, Span, SqlState, Value};
use rudb_parse::{Ast, ast};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node};

use crate::binder::Binder;
use crate::expr::{postgres_oid, written_oid};
use crate::scope::Scope;

/// The OID of the collation `default`, which stands for the collation of the database.
const DEFAULT_COLLATION: u32 = 100;

/// The collation of every database of rudb, as `datcollate` and `datctype` report it.
const DATABASE_COLLATION: &str = "C";

/// How an expression holds its collation, the `CollateStrength` of `parse_collate.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strength {
    /// A column whose expressions did not agree on a collation, which has none. A function that
    /// reads the collation of such a column alone has no collation to use.
    Indeterminate,
    /// The collation of a column.
    Implicit,
    /// Two different implicit collations, the second of them and where it was. The first is the
    /// collation of the [`Derived`].
    Conflict { other: u32, at: Span },
    /// A collation written with `COLLATE`.
    Explicit,
}

impl Strength {
    /// The order in which the strengths win, the weakest first.
    fn rank(self) -> u8 {
        match self {
            Self::Indeterminate => 0,
            Self::Implicit => 1,
            Self::Conflict { .. } => 2,
            Self::Explicit => 3,
        }
    }
}

/// The collation that an expression takes, how it holds it, and where it comes from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Derived {
    oid: u32,
    span: Span,
    strength: Strength,
}

/// The collations a statement wrote and the ones the expressions above them took.
#[derive(Debug, Default)]
pub(crate) struct Collated {
    /// The bound expressions a `COLLATE` was written on, with the collation it named.
    written: HashMap<ExprRef, Derived>,
    /// The collation each expression took from its inputs, kept so that each one is found once.
    derived: HashMap<ExprRef, Option<Derived>>,
    /// The collation of each column of a set operation, as the operation found it from its two
    /// sides.
    outputs: HashMap<ColumnBinding, Option<Derived>>,
    /// The table index of the output of the definition that each read of a `WITH` query reads.
    pub(crate) reads: HashMap<u32, u32>,
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

/// The error of a conflict of two implicit collations where a collation is necessary.
fn implicit_mismatch(first: u32, other: u32, at: Span) -> Error {
    Error::binder(format!(
        "collation mismatch between implicit collations \"{}\" and \"{}\"",
        collation_name(first),
        collation_name(other)
    ))
    .state(SqlState::COLLATION_MISMATCH)
    .hint("You can choose the collation by applying the COLLATE clause to one or both expressions.")
    .with_span(at)
}

/// The error of a function or a comparison whose inputs give it no collation, where `what` names
/// it as PostgreSQL does, for example `lower() function` or `string comparison`. PostgreSQL finds
/// it when the statement runs, so it has no position.
pub(crate) fn indeterminate(what: &str) -> Error {
    Error::binder(format!("could not determine which collation to use for {what}"))
        .state(SqlState::INDETERMINATE_COLLATION)
        .hint("Use the COLLATE clause to set the collation explicitly.")
        .unplaced()
}

/// What a function needs a collation for, by the name of its kernel, as its error says it.
fn needs_collation(name: &str) -> Option<&'static str> {
    match name {
        "~~" | "!~~" => Some("LIKE"),
        _ => None,
    }
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
        let derived = Derived { oid: found.oid, span, strength: Strength::Explicit };
        self.collated.written.insert(bound, derived);
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

    /// Checks that a column that is sorted or grouped on has one collation, as
    /// `assign_collations_walker` checks a target entry with a `ressortgroupref`. Two different
    /// implicit collations there are an error at the second.
    pub(crate) fn check_sort_collation(&mut self, expr: ExprRef) -> Result<()> {
        if self.collated.written.is_empty() {
            return Ok(());
        }
        match self.derive_collation(expr)? {
            Some(Derived { oid, strength: Strength::Conflict { other, at }, .. }) => {
                Err(implicit_mismatch(oid, other, at))
            }
            _ => Ok(()),
        }
    }

    /// Gives `to` the collation that `from` takes from its inputs, for the column that an
    /// aggregate or a window call is read through, so the expression above the call sees it.
    pub(crate) fn carry_collation(&mut self, from: ExprRef, to: ExprRef) -> Result<()> {
        if self.collated.written.is_empty() {
            return Ok(());
        }
        let found = self.derive_collation(from)?;
        self.collated.derived.insert(to, found);
        Ok(())
    }

    /// Finds the collation of each column of a set operation from the columns of its two sides,
    /// the rule of `transformSetOperationTree`. `sides` holds the column of each side with its
    /// type, if the side has one. Two different implicit collations are an error, except under
    /// `UNION ALL`, where the column has no collation.
    pub(crate) fn set_op_collations(
        &mut self,
        index: u32,
        sides: &[[Option<(ColumnBinding, LogicalType)>; 2]],
        union_all: bool,
    ) -> Result<()> {
        if self.collated.written.is_empty() {
            return Ok(());
        }
        for (at, columns) in sides.iter().enumerate() {
            let mut merged = Merging::default();
            for (column, ty) in columns.iter().flatten() {
                let found = self.produced(*column)?;
                merged.add(found, collatable(ty))?;
            }
            let found = match merged.finish() {
                Some(Derived { oid, strength: Strength::Conflict { other, at }, .. }) => {
                    if !union_all {
                        return Err(implicit_mismatch(oid, other, at));
                    }
                    Some(Derived { oid: 0, span: at, strength: Strength::Indeterminate })
                }
                // The column of the operation is not a `COLLATE` clause, so an operation that holds
                // it takes the collation as implicit, as the placeholder of `transformSetOperationTree`.
                Some(Derived { oid, span, strength: Strength::Explicit }) => {
                    Some(Derived { oid, span, strength: Strength::Implicit })
                }
                found => found,
            };
            self.collated.outputs.insert(ColumnBinding::new(index, at as u32), found);
        }
        Ok(())
    }

    /// The collation of the expression that makes the column `binding`, as that expression holds
    /// it. A column of a set operation has the collation the operation found for it, and a read of
    /// a `WITH` query has the collation of the column of the definition.
    fn produced(&mut self, binding: ColumnBinding) -> Result<Option<Derived>> {
        if let Some(&found) = self.collated.outputs.get(&binding) {
            return Ok(found);
        }
        if let Some(&output) = self.collated.reads.get(&binding.table) {
            return self.produced(ColumnBinding::new(output, binding.column));
        }
        let column = binding.column as usize;
        // The grouping of the block that is bound now, whose node is added after its select list.
        if let Some(aggregation) = &self.aggregation
            && aggregation.index == binding.table
        {
            let made =
                aggregation.groups.iter().chain(&aggregation.aggregates).nth(column).copied();
            return made.map_or(Ok(None), |made| self.derive_collation(made));
        }
        let plan = self.plan();
        let node = (0..plan.node_count())
            .map(|node| node as u32)
            .find(|&node| plan.node(node).table_index() == Some(binding.table));
        let Some(node) = node else { return Ok(None) };
        match *plan.node(node) {
            Node::Project { exprs, .. } => {
                let made = plan.expr_list(exprs).get(column).copied();
                made.map_or(Ok(None), |made| self.derive_collation(made))
            }
            Node::Aggregate { groups, aggregates, .. } => {
                let made =
                    plan.expr_list(groups).iter().chain(plan.expr_list(aggregates)).nth(column);
                made.copied().map_or(Ok(None), |made| self.derive_collation(made))
            }
            // The rows of a `VALUES` list take a collation as the sides of `UNION ALL` do.
            Node::Values { rows, .. } => {
                let made: Vec<ExprRef> = plan
                    .row_list(rows)
                    .iter()
                    .filter_map(|&row| plan.expr_list(row).get(column).copied())
                    .collect();
                Ok(match self.merge_collations(&made)? {
                    Some(Derived { strength: Strength::Conflict { .. }, span, .. }) => {
                        Some(Derived { oid: 0, span, strength: Strength::Indeterminate })
                    }
                    found => found,
                })
            }
            // A recursive query has the collations of the query that starts it.
            Node::RecursiveCte { anchor, .. } => match self.output_index(anchor) {
                Some(output) => self.produced(ColumnBinding::new(output, binding.column)),
                None => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// The table index that the columns `node` produces bind against, under the nodes that only
    /// pass their input through.
    pub(crate) fn output_index(&self, mut node: u32) -> Option<u32> {
        loop {
            let held = self.plan().node(node);
            if let Some(index) = held.table_index() {
                return Some(index);
            }
            node = match *held {
                Node::Filter { input, .. }
                | Node::Sort { input, .. }
                | Node::Limit { input, .. }
                | Node::LimitPercent { input, .. }
                | Node::TopN { input, .. }
                | Node::Distinct { input, .. } => input,
                Node::MaterializedCte { body, .. } => body,
                _ => return None,
            };
        }
    }

    /// The collation that `expr` takes from its inputs, if any.
    ///
    /// A comparison or a function takes the collation of its inputs. The result keeps the
    /// collation only when its type has one, so `length(a COLLATE "C") = length(b COLLATE
    /// "POSIX")` is allowed. The conditions of a `CASE` are expressions of their own, and the
    /// result takes the collation of the branches. A column takes the collation of the expression
    /// that makes it as an implicit one.
    fn derive_collation(&mut self, expr: ExprRef) -> Result<Option<Derived>> {
        if let Some(&written) = self.collated.written.get(&expr) {
            return Ok(Some(written));
        }
        if let Some(&derived) = self.collated.derived.get(&expr) {
            return Ok(derived);
        }
        let plan = self.plan();
        let inputs: Vec<ExprRef> = match plan.expr(expr) {
            Expr::Column(binding) => {
                let binding = *binding;
                let span = plan.expr_span(expr);
                let collated = collatable(plan.expr_type(expr));
                let result = match self.produced(binding)? {
                    _ if !collated => None,
                    None => None,
                    Some(found) => match found.strength {
                        Strength::Indeterminate | Strength::Conflict { .. } => {
                            Some(Derived { oid: 0, span, strength: Strength::Indeterminate })
                        }
                        _ if found.oid == DEFAULT_COLLATION => None,
                        _ => Some(Derived { oid: found.oid, span, strength: Strength::Implicit }),
                    },
                };
                self.collated.derived.insert(expr, result);
                return Ok(result);
            }
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
            Expr::Constant(_) | Expr::Lambda { .. } | Expr::LambdaParam(_) => Vec::new(),
        };
        let merged = self.merge_collations(&inputs)?;
        let unresolved = matches!(
            merged,
            Some(Derived { strength: Strength::Indeterminate | Strength::Conflict { .. }, .. })
        );
        if unresolved {
            let plan = self.plan();
            let what = match plan.expr(expr) {
                Expr::Compare { left, .. } if collatable(plan.expr_type(*left)) => {
                    Some("string comparison")
                }
                Expr::Function { name, .. } => needs_collation(plan.string(*name)),
                _ => None,
            };
            if let Some(what) = what {
                return Err(indeterminate(what));
            }
        }
        // An expression over columns of no collation alone takes the collation of its type.
        let result = merged
            .filter(|_| collatable(self.plan().expr_type(expr)))
            .filter(|found| found.strength != Strength::Indeterminate);
        self.collated.derived.insert(expr, result);
        Ok(result)
    }

    /// The collation that the inputs of an operator or a function take together, the rule of
    /// `merge_collation_state`. Two different explicit collations are an error at the second.
    fn merge_collations(&mut self, inputs: &[ExprRef]) -> Result<Option<Derived>> {
        let mut merged = Merging::default();
        for &input in inputs {
            let found = self.derive_collation(input)?;
            merged.add(found, collatable(self.plan().expr_type(input)))?;
        }
        Ok(merged.finish())
    }

    /// The OID of the collation of a call over `args`, as `PG_GET_COLLATION` gives it to the
    /// function: the collation that the arguments take, or else the default collation of the
    /// database, which is the collation of the database itself. Arguments that give the call no
    /// collation are an error, where `what` names the function as the error does.
    ///
    /// PostgreSQL finds that error when the function runs, and rudb finds it when the call is
    /// bound, so here a call that runs on no rows is an error too.
    pub(crate) fn call_collation(&mut self, args: &[ExprRef], what: &str) -> Result<u32> {
        let found = match self.collated.written.is_empty() {
            true => None,
            false => self.merge_collations(args)?,
        };
        let oid = match found {
            Some(Derived {
                strength: Strength::Indeterminate | Strength::Conflict { .. }, ..
            }) => {
                return Err(indeterminate(what));
            }
            Some(found) => found.oid,
            None => DEFAULT_COLLATION,
        };
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
        let oid = self.call_collation(&[left, right], "ILIKE")?;
        let oid = self.add_constant(Value::BigInt(i64::from(oid)));
        let mut sides = Vec::with_capacity(2);
        for side in [left, right] {
            let side = self.cast_to(side, &LogicalType::Varchar);
            sides.push(self.pgproc_kernel("lower", &[side, oid], LogicalType::Varchar));
        }
        self.call(if negated { "!~~" } else { "~~" }, sides)
    }
}

/// The collation that a list of inputs takes so far, as `merge_collation_state` keeps it.
#[derive(Debug, Default)]
struct Merging {
    /// The strongest collation so far.
    found: Option<Derived>,
    /// Whether an input of a type that has a collation took the collation `default`, which wins
    /// over a column of no collation.
    defaulted: bool,
}

impl Merging {
    /// Adds the collation of one more input, where `collated` says whether the type of the input
    /// has a collation.
    fn add(&mut self, found: Option<Derived>, collated: bool) -> Result<()> {
        let Some(found) = found else {
            self.defaulted |= collated;
            return Ok(());
        };
        let Some(first) = self.found else {
            self.found = Some(found);
            return Ok(());
        };
        if found.strength.rank() > first.strength.rank() {
            self.found = Some(found);
            return Ok(());
        }
        if found.strength.rank() < first.strength.rank() || found.oid == first.oid {
            return Ok(());
        }
        match first.strength {
            Strength::Implicit => {
                let strength = Strength::Conflict { other: found.oid, at: found.span };
                self.found = Some(Derived { strength, ..first });
            }
            Strength::Explicit => {
                return Err(Error::binder(format!(
                    "collation mismatch between explicit collations \"{}\" and \"{}\"",
                    collation_name(first.oid),
                    collation_name(found.oid)
                ))
                .state(SqlState::COLLATION_MISMATCH)
                .with_span(found.span));
            }
            Strength::Indeterminate | Strength::Conflict { .. } => {}
        }
        Ok(())
    }

    /// The collation the inputs take together.
    fn finish(self) -> Option<Derived> {
        match self.found {
            Some(Derived { strength: Strength::Indeterminate, .. }) if self.defaulted => None,
            found => found,
        }
    }
}

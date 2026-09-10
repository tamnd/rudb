//! From an `Ast` to a `Plan`.
//!
//! The binder walks the written query once, in the order the operators end up in rather than the
//! order the clauses are written in, which is `FROM`, `WHERE`, `GROUP BY`, `HAVING`, `SELECT`,
//! `DISTINCT`, `ORDER BY`, `LIMIT`. That order is not a stylistic choice: it is the reason `WHERE`
//! cannot see an output alias and `HAVING` cannot see a column that was not grouped, and doing it
//! in any other order means special casing both of those instead of getting them for free.
//!
//! Two things leave here settled that nothing downstream reconsiders. Every column is a table index
//! and a position rather than a name, so the optimizer never has to ask which `id` a name meant.
//! And every expression has a type, with the casts that make the types line up already written into
//! the plan as [`Expr::Cast`] nodes, so an executor never has to decide what a comparison between
//! an `INTEGER` and a `BIGINT` does.

use rudb_catalog::{Catalog, same_name};
use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_functions::resolve;
use rudb_parse::ast::{self, Ast, Distinct, LiteralKind, Nulls, Order, Quantifier, SetOp};
use rudb_parse::{NONE, parse_ast};
use rudb_plan::{ColumnBinding, Expr, ExprRef, JoinKind, Node, NodeRef, Plan, SetOpKind, SortKey};

use crate::expr::{describe, has_aggregate};
use crate::scope::{Scope, Visible};

/// Binds a parsed statement against a catalog.
///
/// # Errors
///
/// If the script does not hold exactly one statement, if a name does not resolve, if a type does
/// not work out, or if the query uses something M0 does not bind yet.
pub fn bind(ast: &Ast, catalog: &Catalog) -> Result<Plan> {
    let query = match ast.statements.as_slice() {
        [ast::Statement::Query(query)] => *query,
        [] => return Err(Error::binder("no statement to bind")),
        _ => return Err(Error::not_implemented("a script of more than one statement")),
    };
    let mut binder = Binder::new(catalog);
    let (root, _) = binder.bind_query(ast, query)?;
    let mut plan = binder.into_plan();
    plan.set_root(root);
    plan.validate()?;
    Ok(plan)
}

/// Parses and binds one query, which is the whole front end in one call.
///
/// # Errors
///
/// Anything the parser or the binder reports.
pub fn bind_sql(query: &str, catalog: &Catalog) -> Result<Plan> {
    let ast = parse_ast(query)?;
    bind(&ast, catalog)
}

/// What an aggregating select block has decided so far.
#[derive(Debug)]
pub(crate) struct Aggregation {
    /// The table index the aggregate's output binds against.
    pub(crate) index: u32,
    /// The group expressions, over the input, which are the first output columns.
    pub(crate) groups: Vec<ExprRef>,
    /// The aggregate calls found so far, which follow the groups in the output.
    pub(crate) aggregates: Vec<ExprRef>,
}

/// The state one binding run carries.
#[derive(Debug)]
pub(crate) struct Binder<'a> {
    catalog: &'a Catalog,
    plan: Plan,
    next_index: u32,
    /// Set while a select block aggregates, which changes what a bare column means.
    pub(crate) aggregation: Option<Aggregation>,
    /// Set while an aggregate's own arguments are being bound, so nesting is caught.
    pub(crate) in_aggregate: bool,
    /// Where we are, for an error message that says which clause the writer should look at.
    pub(crate) clause: &'static str,
}

impl<'a> Binder<'a> {
    pub(crate) fn new(catalog: &'a Catalog) -> Self {
        Self {
            catalog,
            plan: Plan::new(),
            next_index: 0,
            aggregation: None,
            in_aggregate: false,
            clause: "SELECT clause",
        }
    }

    pub(crate) fn plan(&self) -> &Plan {
        &self.plan
    }

    pub(crate) fn plan_mut(&mut self) -> &mut Plan {
        &mut self.plan
    }

    pub(crate) fn into_plan(self) -> Plan {
        self.plan
    }

    /// A table index nothing else has.
    fn fresh_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    /// A reference to one column of an operator's output.
    fn column(&mut self, index: u32, position: usize, ty: LogicalType) -> ExprRef {
        let binding = ColumnBinding::new(index, position as u32);
        self.plan.add_expr(Expr::Column(binding), ty)
    }

    // ---------------------------------------------------------------- queries

    pub(crate) fn bind_query(
        &mut self,
        ast: &Ast,
        query: ast::QueryRef,
    ) -> Result<(NodeRef, Scope)> {
        let written = ast.query(query);
        match written.body {
            ast::QueryBody::Select(select) => self.bind_select(ast, select, &written),
            ast::QueryBody::SetOp { op, quantifier, by_name, left, right } => {
                if by_name {
                    return Err(Error::not_implemented("UNION BY NAME"));
                }
                self.bind_set_op(ast, &written, op, quantifier, left, right)
            }
        }
    }

    fn bind_set_op(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        op: SetOp,
        quantifier: Quantifier,
        left: ast::QueryRef,
        right: ast::QueryRef,
    ) -> Result<(NodeRef, Scope)> {
        let (left_node, left_scope) = self.bind_query(ast, left)?;
        let (right_node, right_scope) = self.bind_query(ast, right)?;
        if left_scope.len() != right_scope.len() {
            return Err(Error::binder(format!(
                "Set operations can only apply to expressions with the same number of result columns, but left side has {} and right side has {}",
                left_scope.len(),
                right_scope.len()
            )));
        }
        // Both sides have to hand back one set of types, so each column meets the other side's.
        let mut types = Vec::with_capacity(left_scope.len());
        for (left, right) in left_scope.columns.iter().zip(&right_scope.columns) {
            let common = left.ty.promote(&right.ty).ok_or_else(|| {
                Error::binder(format!(
                    "Cannot combine a column of type {} with a column of type {} in a set operation",
                    left.ty, right.ty
                ))
            })?;
            types.push(common);
        }
        let left_node = self.conform(left_node, &left_scope, &types);
        let right_node = self.conform(right_node, &right_scope, &types);
        let index = self.fresh_index();
        let kind = match op {
            SetOp::Union => SetOpKind::Union,
            SetOp::Except => SetOpKind::Except,
            SetOp::Intersect => SetOpKind::Intersect,
        };
        // UNION alone removes duplicates and UNION ALL keeps them, which is the one place the
        // unwritten quantifier and ALL disagree.
        let all = quantifier == Quantifier::All;
        let mut node = self.plan.add_node(Node::SetOp {
            left: left_node,
            right: right_node,
            kind,
            all,
            index,
        });
        let mut scope = Scope::empty();
        for (at, (column, ty)) in left_scope.columns.iter().zip(&types).enumerate() {
            scope.push(Visible {
                table: String::new(),
                name: column.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: ty.clone(),
            });
        }
        // Above a set operation there is nothing but the output columns, so an ORDER BY term is
        // either a position, an output name, or an expression over the output, and never needs a
        // column projected for it that the query did not ask for.
        let keys = self.sort_keys(ast, query, &scope, &[])?;
        if !keys.is_empty() {
            let keys = self.plan.add_sort_keys(&keys);
            node = self.plan.add_node(Node::Sort { input: node, keys });
        }
        node = self.apply_limit(ast, query, node)?;
        Ok((node, scope))
    }

    /// Projects one side of a set operation so that its columns have the agreed types.
    fn conform(&mut self, node: NodeRef, scope: &Scope, types: &[LogicalType]) -> NodeRef {
        if scope.columns.iter().zip(types).all(|(column, ty)| &column.ty == ty) {
            return node;
        }
        let index = self.fresh_index();
        let mut exprs = Vec::with_capacity(types.len());
        let mut names = Vec::with_capacity(types.len());
        for (column, ty) in scope.columns.iter().zip(types) {
            let expr = self.plan.add_expr(Expr::Column(column.binding), column.ty.clone());
            exprs.push(self.cast_to(expr, ty));
            names.push(self.plan.intern(&column.name));
        }
        let exprs = self.plan.add_expr_list(&exprs);
        let names = self.plan.add_name_list(&names);
        self.plan.add_node(Node::Project { input: node, index, exprs, names })
    }

    // ----------------------------------------------------------------- select

    fn bind_select(
        &mut self,
        ast: &Ast,
        select: ast::SelectRef,
        query: &ast::Query,
    ) -> Result<(NodeRef, Scope)> {
        let written = ast.select(select);
        let (mut node, input) = self.bind_from(ast, written.from)?;

        if written.filter != NONE {
            self.clause = "WHERE clause";
            let predicate = self.bind_expr(ast, written.filter, &input)?;
            let predicate = self.as_boolean(predicate, "WHERE")?;
            node = self.plan.add_node(Node::Filter { input: node, predicate });
        }

        let targets = ast.target_list(written.targets).to_vec();
        if targets.is_empty() {
            return Err(Error::binder("a SELECT needs at least one expression to select"));
        }

        let group_items = self.group_items(ast, &written, &targets)?;
        let aggregating = !group_items.is_empty()
            || written.having != NONE
            || targets.iter().any(|target| has_aggregate(ast, target.expr));
        if aggregating {
            self.clause = "GROUP BY clause";
            let mut groups = Vec::with_capacity(group_items.len());
            for item in &group_items {
                groups.push(self.bind_expr(ast, *item, &input)?);
            }
            let index = self.fresh_index();
            self.aggregation = Some(Aggregation { index, groups, aggregates: Vec::new() });
        }

        self.clause = "SELECT clause";
        let (mut exprs, mut names) = self.bind_targets(ast, &targets, &input)?;
        let visible = exprs.len();

        let mut having = None;
        if written.having != NONE {
            self.clause = "HAVING clause";
            let predicate = self.bind_expr(ast, written.having, &input)?;
            let predicate = self.over_aggregate(predicate, &input)?;
            having = Some(self.as_boolean(predicate, "HAVING")?);
        }

        // The projection's index has to exist before the sort keys are built, because a key is a
        // reference to a projected column even when the expression it sorts on is not selected.
        let project = self.fresh_index();
        let mut output = Scope::empty();
        for (at, (expr, name)) in exprs.iter().zip(&names).enumerate() {
            output.push(Visible {
                table: String::new(),
                name: name.clone(),
                binding: ColumnBinding::new(project, at as u32),
                ty: self.plan.expr_type(*expr).clone(),
            });
        }

        self.clause = "ORDER BY clause";
        let mut extra = Vec::new();
        let keys = self.select_sort_keys(
            ast, query, &input, &output, project, &mut exprs, &mut names, &mut extra,
        )?;
        if !extra.is_empty() && written.distinct != Distinct::No {
            return Err(Error::binder(
                "For SELECT DISTINCT, ORDER BY expressions must appear in the select list",
            ));
        }
        let on = self.distinct_on(ast, written.distinct, &output)?;

        if let Some(aggregation) = self.aggregation.take() {
            let index = aggregation.index;
            let groups = self.plan.add_expr_list(&aggregation.groups);
            let aggregates = self.plan.add_expr_list(&aggregation.aggregates);
            node = self.plan.add_node(Node::Aggregate { input: node, index, groups, aggregates });
        }
        if let Some(predicate) = having {
            node = self.plan.add_node(Node::Filter { input: node, predicate });
        }

        let interned: Vec<u32> = names.iter().map(|name| self.plan.intern(name)).collect();
        let exprs_slice = self.plan.add_expr_list(&exprs);
        let names_slice = self.plan.add_name_list(&interned);
        node = self.plan.add_node(Node::Project {
            input: node,
            index: project,
            exprs: exprs_slice,
            names: names_slice,
        });

        if written.distinct != Distinct::No {
            let on = self.plan.add_expr_list(&on);
            node = self.plan.add_node(Node::Distinct { input: node, on });
        }
        if !keys.is_empty() {
            let keys = self.plan.add_sort_keys(&keys);
            node = self.plan.add_node(Node::Sort { input: node, keys });
        }
        node = self.apply_limit(ast, query, node)?;

        if extra.is_empty() {
            output.columns.truncate(visible);
            return Ok((node, output));
        }
        // An expression sorted on but not selected was carried this far to make the sort possible,
        // and now it goes, because the query did not ask for it.
        let index = self.fresh_index();
        let mut kept = Vec::with_capacity(visible);
        let mut kept_names = Vec::with_capacity(visible);
        let mut scope = Scope::empty();
        for (at, name) in names.iter().enumerate().take(visible) {
            let ty = output.columns[at].ty.clone();
            kept.push(self.column(project, at, ty.clone()));
            kept_names.push(self.plan.intern(name));
            scope.push(Visible {
                table: String::new(),
                name: name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty,
            });
        }
        let exprs = self.plan.add_expr_list(&kept);
        let names = self.plan.add_name_list(&kept_names);
        node = self.plan.add_node(Node::Project { input: node, index, exprs, names });
        Ok((node, scope))
    }

    /// Binds the target list, expanding every star into the columns it stands for.
    fn bind_targets(
        &mut self,
        ast: &Ast,
        targets: &[ast::Target],
        input: &Scope,
    ) -> Result<(Vec<ExprRef>, Vec<String>)> {
        let mut exprs = Vec::with_capacity(targets.len());
        let mut names = Vec::with_capacity(targets.len());
        for target in targets {
            if let ast::Expr::Star { qualifier } = ast.expr(target.expr) {
                let table = ast.name(qualifier).last().map(str::to_string);
                let expanded: Vec<Visible> =
                    input.star(table.as_deref())?.into_iter().cloned().collect();
                for column in expanded {
                    let expr = self.plan.add_expr(Expr::Column(column.binding), column.ty);
                    exprs.push(self.over_aggregate(expr, input)?);
                    names.push(column.name);
                }
                continue;
            }
            let expr = self.bind_expr(ast, target.expr, input)?;
            exprs.push(self.over_aggregate(expr, input)?);
            names.push(if target.alias == NONE {
                self.output_name(ast, target.expr, input)
            } else {
                ast.string(target.alias).to_string()
            });
        }
        Ok((exprs, names))
    }

    /// The name an unaliased target gets.
    ///
    /// A bare column keeps the spelling the table was created with rather than the spelling the
    /// query used, so `SELECT USERID FROM hits` has a column called `UserID`. Identifiers match
    /// without regard to case and the catalog is the one that holds the case.
    fn output_name(&self, ast: &Ast, target: ast::ExprRef, input: &Scope) -> String {
        if let ast::Expr::Column { name } = ast.expr(target) {
            let parts: Vec<&str> = ast.name(name).collect();
            if let Ok(found) = input.resolve(&parts) {
                return found.name.clone();
            }
        }
        describe(ast, target)
    }

    /// The expressions a `GROUP BY` clause names, with positions and output aliases followed.
    fn group_items(
        &self,
        ast: &Ast,
        select: &ast::Select,
        targets: &[ast::Target],
    ) -> Result<Vec<ast::ExprRef>> {
        if select.group_by_all {
            // GROUP BY ALL means every target that is not itself an aggregate, which is the set
            // that would otherwise have to be written out again by hand.
            return Ok(targets
                .iter()
                .filter(|target| !has_aggregate(ast, target.expr))
                .map(|target| target.expr)
                .collect());
        }
        let mut items = Vec::new();
        for &item in ast.expr_list(select.group_by) {
            items.push(self.output_reference(ast, item, targets, "GROUP BY")?.unwrap_or(item));
        }
        Ok(items)
    }

    /// The target a `GROUP BY` or `ORDER BY` term names, when it names one by position or alias.
    fn output_reference(
        &self,
        ast: &Ast,
        item: ast::ExprRef,
        targets: &[ast::Target],
        clause: &str,
    ) -> Result<Option<ast::ExprRef>> {
        match ast.expr(item) {
            ast::Expr::Literal { kind: LiteralKind::Number, text } => {
                let written = ast.string(text);
                let position: usize = written.parse().map_err(|_| {
                    Error::binder(format!("{clause} term {written} is not a column"))
                })?;
                if position == 0 || position > targets.len() {
                    return Err(Error::binder(format!(
                        "{clause} term out of range - should be between 1 and {}",
                        targets.len()
                    )));
                }
                Ok(Some(targets[position - 1].expr))
            }
            ast::Expr::Column { name } => {
                let parts: Vec<&str> = ast.name(name).collect();
                let [written] = parts.as_slice() else { return Ok(None) };
                let mut found = None;
                for target in targets {
                    if target.alias != NONE && same_name(ast.string(target.alias), written) {
                        if found.is_some() {
                            return Ok(None);
                        }
                        found = Some(target.expr);
                    }
                }
                Ok(found)
            }
            _ => Ok(None),
        }
    }

    // -------------------------------------------------------------- modifiers

    /// Sort keys for a select, projecting anything sorted on that is not already selected.
    #[allow(clippy::too_many_arguments)]
    fn select_sort_keys(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        input: &Scope,
        output: &Scope,
        project: u32,
        exprs: &mut Vec<ExprRef>,
        names: &mut Vec<String>,
        extra: &mut Vec<usize>,
    ) -> Result<Vec<SortKey>> {
        if query.order_by_all {
            return Ok(self.every_column(output));
        }
        let items = ast.order_list(query.order_by).to_vec();
        let mut keys = Vec::with_capacity(items.len());
        for item in items {
            let position = match self.output_position(ast, item.expr, output)? {
                Some(position) => position,
                None => {
                    let bound = self.bind_expr(ast, item.expr, input)?;
                    let bound = self.over_aggregate(bound, input)?;
                    match exprs.iter().position(|&held| self.same_expr(held, bound)) {
                        Some(position) => position,
                        None => {
                            exprs.push(bound);
                            names.push(describe(ast, item.expr));
                            extra.push(exprs.len() - 1);
                            exprs.len() - 1
                        }
                    }
                }
            };
            let ty = self.plan.expr_type(exprs[position]).clone();
            let expr = self.column(project, position, ty);
            keys.push(sort_key(expr, item));
        }
        Ok(keys)
    }

    /// Sort keys over an output that has nothing behind it to project, which is a set operation.
    fn sort_keys(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        output: &Scope,
        targets: &[ast::Target],
    ) -> Result<Vec<SortKey>> {
        if query.order_by_all {
            return Ok(self.every_column(output));
        }
        let items = ast.order_list(query.order_by).to_vec();
        let mut keys = Vec::with_capacity(items.len());
        for item in items {
            let expr = match self.output_position(ast, item.expr, output)? {
                Some(position) => {
                    let column = &output.columns[position];
                    let (binding, ty) = (column.binding, column.ty.clone());
                    self.plan.add_expr(Expr::Column(binding), ty)
                }
                None => {
                    let _ = targets;
                    self.bind_expr(ast, item.expr, output)?
                }
            };
            keys.push(sort_key(expr, item));
        }
        Ok(keys)
    }

    fn every_column(&mut self, output: &Scope) -> Vec<SortKey> {
        let columns: Vec<(ColumnBinding, LogicalType)> =
            output.columns.iter().map(|column| (column.binding, column.ty.clone())).collect();
        columns
            .into_iter()
            .map(|(binding, ty)| {
                let expr = self.plan.add_expr(Expr::Column(binding), ty);
                SortKey { expr, descending: false, nulls_first: false }
            })
            .collect()
    }

    /// Which output column a term names, by position or by name.
    fn output_position(
        &self,
        ast: &Ast,
        item: ast::ExprRef,
        output: &Scope,
    ) -> Result<Option<usize>> {
        match ast.expr(item) {
            ast::Expr::Literal { kind: LiteralKind::Number, text } => {
                let written = ast.string(text);
                if written.contains(['.', 'e', 'E']) {
                    return Ok(None);
                }
                let position: usize = written.parse().map_err(|_| {
                    Error::binder(format!("ORDER BY term {written} is not a column"))
                })?;
                if position == 0 || position > output.len() {
                    return Err(Error::binder(format!(
                        "ORDER BY term out of range - should be between 1 and {}",
                        output.len()
                    )));
                }
                Ok(Some(position - 1))
            }
            ast::Expr::Column { name } => {
                let parts: Vec<&str> = ast.name(name).collect();
                let [written] = parts.as_slice() else { return Ok(None) };
                Ok(output.position_of(None, written))
            }
            _ => Ok(None),
        }
    }

    /// The expressions a `DISTINCT ON` names, which have to be columns of the output.
    fn distinct_on(
        &mut self,
        ast: &Ast,
        distinct: Distinct,
        output: &Scope,
    ) -> Result<Vec<ExprRef>> {
        let Distinct::On(items) = distinct else {
            return Ok(Vec::new());
        };
        let items = ast.expr_list(items).to_vec();
        let mut on = Vec::with_capacity(items.len());
        for item in items {
            let Some(position) = self.output_position(ast, item, output)? else {
                return Err(Error::not_implemented(
                    "DISTINCT ON an expression that is not in the select list",
                ));
            };
            let column = &output.columns[position];
            let (binding, ty) = (column.binding, column.ty.clone());
            on.push(self.plan.add_expr(Expr::Column(binding), ty));
        }
        Ok(on)
    }

    fn apply_limit(&mut self, ast: &Ast, query: &ast::Query, input: NodeRef) -> Result<NodeRef> {
        if query.limit_percent {
            return Err(Error::not_implemented("LIMIT with a percentage"));
        }
        let count = self.constant_count(ast, query.limit, "LIMIT")?;
        let offset = self.constant_count(ast, query.offset, "OFFSET")?.unwrap_or(0);
        if count.is_none() && offset == 0 {
            return Ok(input);
        }
        Ok(self.plan.add_node(Node::Limit { input, count, offset }))
    }

    /// The row count a `LIMIT` or an `OFFSET` names, which has to be a constant.
    fn constant_count(
        &mut self,
        ast: &Ast,
        written: ast::ExprRef,
        clause: &str,
    ) -> Result<Option<u64>> {
        if written == NONE {
            return Ok(None);
        }
        self.clause = "LIMIT clause";
        let scope = Scope::empty();
        let bound = self.bind_expr(ast, written, &scope)?;
        let Expr::Constant(value) = *self.plan.expr(bound) else {
            return Err(Error::not_implemented(format!("a {clause} that is not a constant")));
        };
        let count = match self.plan.value(value) {
            Value::Null => return Ok(None),
            Value::TinyInt(count) => i128::from(*count),
            Value::SmallInt(count) => i128::from(*count),
            Value::Integer(count) => i128::from(*count),
            Value::BigInt(count) => i128::from(*count),
            Value::HugeInt(count) => *count,
            other => {
                return Err(Error::binder(format!(
                    "{clause} takes a whole number of rows, not a value of type {}",
                    other.logical_type()
                )));
            }
        };
        u64::try_from(count)
            .map(Some)
            .map_err(|_| Error::binder(format!("{clause} must not be negative")))
    }

    // ------------------------------------------------------------------- from

    fn bind_from(&mut self, ast: &Ast, from: ast::Slice) -> Result<(NodeRef, Scope)> {
        let sources = ast.source_list(from).to_vec();
        let Some((first, rest)) = sources.split_first() else {
            // No FROM clause is one row of no columns, which is what SELECT 1 sits on. Not an
            // empty table: an empty table would make SELECT 1 return nothing.
            return Ok((self.plan.add_node(Node::Dummy), Scope::empty()));
        };
        let (mut node, mut scope) = self.bind_source(ast, *first)?;
        for source in rest {
            let (right, right_scope) = self.bind_source(ast, *source)?;
            node = self.plan.add_node(Node::CrossProduct { left: node, right });
            scope = scope.concat(right_scope);
        }
        Ok((node, scope))
    }

    fn bind_source(&mut self, ast: &Ast, source: ast::SourceRef) -> Result<(NodeRef, Scope)> {
        match ast.source(source) {
            ast::Source::Table { name, alias, columns } => {
                self.bind_table(ast, name, alias, columns)
            }
            ast::Source::Subquery { query, alias, columns } => {
                let (node, mut scope) = self.bind_query(ast, query)?;
                let label = if alias == NONE {
                    "unnamed_subquery".to_string()
                } else {
                    ast.string(alias).to_string()
                };
                scope.relabel(&label);
                if !columns.is_empty() {
                    let names: Vec<&str> = ast.name(columns).collect();
                    scope.rename(&names, &label)?;
                }
                Ok((node, scope))
            }
            ast::Source::Join { left, right, kind, natural, on, using } => {
                self.bind_join(ast, left, right, kind, natural, on, using)
            }
        }
    }

    fn bind_table(
        &mut self,
        ast: &Ast,
        name: ast::Slice,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let parts: Vec<&str> = ast.name(name).collect();
        let catalog = self.catalog;
        let resolved = catalog.resolve(&parts)?;
        let table = catalog.table(&resolved)?;
        let fields: Vec<Field> = table.columns().to_vec();
        let label =
            if alias == NONE { resolved.table.clone() } else { ast.string(alias).to_string() };
        let index = self.fresh_index();
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: label.clone(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
            });
        }
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, &label)?;
        }
        let catalog_name = self.plan.intern(&resolved.catalog);
        let schema = self.plan.intern(&resolved.schema);
        let table_name = self.plan.intern(&resolved.table);
        let alias = self.plan.intern(&label);
        let columns = self.plan.add_fields(&fields);
        let node = self.plan.add_node(Node::Get {
            catalog: catalog_name,
            schema,
            table: table_name,
            alias,
            index,
            columns,
        });
        Ok((node, scope))
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_join(
        &mut self,
        ast: &Ast,
        left: ast::SourceRef,
        right: ast::SourceRef,
        kind: ast::JoinKind,
        natural: bool,
        on: ast::ExprRef,
        using: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let (left_node, left_scope) = self.bind_source(ast, left)?;
        let (right_node, right_scope) = self.bind_source(ast, right)?;
        let split = left_scope.len();
        let mut scope = left_scope.concat(right_scope);

        // NATURAL is USING over whatever both sides happen to call the same thing, which is why it
        // is resolved here and never reaches the plan as its own idea.
        let merged: Vec<String> = if natural {
            let mut names = Vec::new();
            for (at, column) in scope.columns.iter().enumerate().take(split) {
                if scope.columns[split..].iter().any(|right| same_name(&right.name, &column.name))
                    && !names.iter().any(|held: &String| same_name(held, &column.name))
                {
                    let _ = at;
                    names.push(column.name.clone());
                }
            }
            names
        } else {
            ast.name(using).map(str::to_string).collect()
        };

        let mut conditions = Vec::new();
        let mut dropped = Vec::new();
        for name in &merged {
            let left_at = scope.columns[..split]
                .iter()
                .position(|column| same_name(&column.name, name))
                .ok_or_else(|| {
                    Error::binder(format!(
                        "column \"{name}\" specified in USING clause does not exist in left table"
                    ))
                })?;
            let right_at = scope.columns[split..]
                .iter()
                .position(|column| same_name(&column.name, name))
                .map(|at| at + split)
                .ok_or_else(|| {
                    Error::binder(format!(
                        "column \"{name}\" specified in USING clause does not exist in right table"
                    ))
                })?;
            let left_column = &scope.columns[left_at];
            let (left_binding, left_type) = (left_column.binding, left_column.ty.clone());
            let right_column = &scope.columns[right_at];
            let (right_binding, right_type) = (right_column.binding, right_column.ty.clone());
            let left_expr = self.plan.add_expr(Expr::Column(left_binding), left_type);
            let right_expr = self.plan.add_expr(Expr::Column(right_binding), right_type);
            conditions.push(self.compare(rudb_plan::CompareOp::Equal, left_expr, right_expr)?);
            dropped.push(right_at);
        }
        // A joined-on column appears once, so the right side's copy goes. Dropping from the back
        // keeps the positions of the ones still to drop correct.
        dropped.sort_unstable();
        for at in dropped.into_iter().rev() {
            scope.remove(at);
        }

        if on != NONE {
            if !merged.is_empty() {
                return Err(Error::binder("a join cannot have both ON and USING"));
            }
            self.clause = "JOIN condition";
            let predicate = self.bind_expr(ast, on, &scope)?;
            conditions.push(self.as_boolean(predicate, "JOIN")?);
        }

        if kind == ast::JoinKind::Cross {
            if !conditions.is_empty() {
                return Err(Error::binder("a CROSS JOIN cannot have a condition"));
            }
            let node =
                self.plan.add_node(Node::CrossProduct { left: left_node, right: right_node });
            return Ok((node, scope));
        }
        if conditions.is_empty() && kind == ast::JoinKind::Inner {
            let node =
                self.plan.add_node(Node::CrossProduct { left: left_node, right: right_node });
            return Ok((node, scope));
        }
        let kind = match kind {
            ast::JoinKind::Inner | ast::JoinKind::Cross => JoinKind::Inner,
            ast::JoinKind::Left => JoinKind::Left,
            ast::JoinKind::Right => JoinKind::Right,
            ast::JoinKind::Full => JoinKind::Full,
            ast::JoinKind::Semi => JoinKind::Semi,
            ast::JoinKind::Anti => JoinKind::Anti,
            ast::JoinKind::Positional => JoinKind::Positional,
        };
        let conditions = self.plan.add_expr_list(&conditions);
        let node =
            self.plan.add_node(Node::Join { left: left_node, right: right_node, kind, conditions });
        Ok((node, scope))
    }

    // -------------------------------------------------------------- aggregates

    /// Binds an aggregate call, records it, and hands back a reference to where its result lands.
    pub(crate) fn bind_aggregate(
        &mut self,
        ast: &Ast,
        name: &str,
        args: &[ast::ExprRef],
        distinct: bool,
        scope: &Scope,
    ) -> Result<ExprRef> {
        if self.in_aggregate {
            return Err(Error::binder(format!(
                "aggregate function calls cannot be nested, and {name}() is inside one"
            )));
        }
        if self.aggregation.is_none() {
            return Err(Error::binder(format!(
                "aggregate function calls cannot be used in the {}",
                self.clause
            )));
        }
        self.in_aggregate = true;
        let mut bound = Vec::with_capacity(args.len());
        let mut failure = None;
        for &arg in args {
            match self.bind_expr(ast, arg, scope) {
                Ok(expr) => bound.push(expr),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        self.in_aggregate = false;
        if let Some(error) = failure {
            return Err(error);
        }

        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan.expr_type(arg).clone()).collect();
        let resolved = resolve(name, &types)?;
        let mut cast = Vec::with_capacity(bound.len());
        for (arg, wanted) in bound.iter().zip(&resolved.arguments) {
            cast.push(self.cast_to(*arg, wanted));
        }
        let args = self.plan.add_expr_list(&cast);
        let name = self.plan.intern(resolved.name);
        let ty = resolved.returns;
        let call =
            self.plan.add_expr(Expr::Aggregate { name, args, distinct, filter: None }, ty.clone());

        // Two identical aggregates are one column of the aggregate's output. `SELECT sum(x),
        // sum(x) / count(*)` computes one sum, not two.
        let existing = self.aggregation.as_ref().map(|held| held.aggregates.clone());
        let existing = existing.unwrap_or_default();
        let at = match existing.iter().position(|&held| self.same_expr(held, call)) {
            Some(at) => at,
            None => {
                let aggregation = self.aggregation.as_mut().expect("checked above");
                aggregation.aggregates.push(call);
                aggregation.aggregates.len() - 1
            }
        };
        let aggregation = self.aggregation.as_ref().expect("checked above");
        let (index, groups) = (aggregation.index, aggregation.groups.len());
        Ok(self.column(index, groups + at, ty))
    }

    /// Rewrites a bound expression into one the aggregate's output can answer.
    ///
    /// A subexpression that is one of the group expressions becomes a reference to that group. A
    /// column that is neither grouped nor inside an aggregate is the error every SQL user has seen,
    /// and it is reported here because this is the first point where it is knowable.
    pub(crate) fn over_aggregate(&mut self, expr: ExprRef, scope: &Scope) -> Result<ExprRef> {
        let Some(aggregation) = self.aggregation.as_ref() else {
            return Ok(expr);
        };
        let index = aggregation.index;
        let groups = aggregation.groups.clone();
        for (at, group) in groups.iter().enumerate() {
            if self.same_expr(expr, *group) {
                let ty = self.plan.expr_type(*group).clone();
                return Ok(self.column(index, at, ty));
            }
        }
        let ty = self.plan.expr_type(expr).clone();
        match self.plan.expr(expr).clone() {
            Expr::Column(binding) if binding.table == index => Ok(expr),
            Expr::Column(binding) => {
                let name =
                    scope.columns.iter().find(|column| column.binding == binding).map_or_else(
                        || "a column".to_string(),
                        |column| format!("\"{}\"", column.name),
                    );
                Err(Error::binder(format!(
                    "column {name} must appear in the GROUP BY clause or must be part of an aggregate function"
                )))
            }
            Expr::Constant(_) | Expr::Aggregate { .. } => Ok(expr),
            Expr::Cast { input, try_cast } => {
                let input = self.over_aggregate(input, scope)?;
                Ok(self.plan.add_expr(Expr::Cast { input, try_cast }, ty))
            }
            Expr::Compare { op, left, right } => {
                let left = self.over_aggregate(left, scope)?;
                let right = self.over_aggregate(right, scope)?;
                Ok(self.plan.add_expr(Expr::Compare { op, left, right }, ty))
            }
            Expr::Conjunction { op, children } => {
                let written = self.plan.expr_list(children).to_vec();
                let mut rewritten = Vec::with_capacity(written.len());
                for child in written {
                    rewritten.push(self.over_aggregate(child, scope)?);
                }
                let children = self.plan.add_expr_list(&rewritten);
                Ok(self.plan.add_expr(Expr::Conjunction { op, children }, ty))
            }
            Expr::Function { name, args } => {
                let written = self.plan.expr_list(args).to_vec();
                let mut rewritten = Vec::with_capacity(written.len());
                for arg in written {
                    rewritten.push(self.over_aggregate(arg, scope)?);
                }
                let args = self.plan.add_expr_list(&rewritten);
                Ok(self.plan.add_expr(Expr::Function { name, args }, ty))
            }
            Expr::Case { arms, otherwise } => {
                let written = self.plan.arm_list(arms).to_vec();
                let mut rewritten = Vec::with_capacity(written.len());
                for arm in written {
                    let when = self.over_aggregate(arm.when, scope)?;
                    let then = self.over_aggregate(arm.then, scope)?;
                    rewritten.push(rudb_plan::Arm { when, then });
                }
                let otherwise = match otherwise {
                    Some(expr) => Some(self.over_aggregate(expr, scope)?),
                    None => None,
                };
                let arms = self.plan.add_arms(&rewritten);
                Ok(self.plan.add_expr(Expr::Case { arms, otherwise }, ty))
            }
        }
    }

    /// Whether two bound expressions are the same expression, by shape rather than by reference.
    pub(crate) fn same_expr(&self, left: ExprRef, right: ExprRef) -> bool {
        same_expr(&self.plan, left, right)
    }
}

/// A sort key with SQL's defaults filled in.
///
/// Unstated is ascending, and unstated nulls go where the direction puts them, which is last for
/// ascending and first for descending. That is DuckDB's rule and it is the one that makes
/// `ORDER BY x DESC` the exact reverse of `ORDER BY x`.
fn sort_key(expr: ExprRef, item: ast::OrderItem) -> SortKey {
    let descending = item.order == Order::Descending;
    let nulls_first = match item.nulls {
        Nulls::First => true,
        Nulls::Last => false,
        Nulls::Unstated => descending,
    };
    SortKey { expr, descending, nulls_first }
}

/// Structural equality over two expressions of one plan.
fn same_expr(plan: &Plan, left: ExprRef, right: ExprRef) -> bool {
    if left == right {
        return true;
    }
    if plan.expr_type(left) != plan.expr_type(right) {
        return false;
    }
    let lists = |left, right| {
        let left: &[ExprRef] = plan.expr_list(left);
        let right: &[ExprRef] = plan.expr_list(right);
        left.len() == right.len()
            && left.iter().zip(right).all(|(&left, &right)| same_expr(plan, left, right))
    };
    match (plan.expr(left), plan.expr(right)) {
        (Expr::Column(left), Expr::Column(right)) => left == right,
        (Expr::Constant(left), Expr::Constant(right)) => plan.value(*left) == plan.value(*right),
        (
            Expr::Cast { input: left, try_cast: left_try },
            Expr::Cast { input: right, try_cast: right_try },
        ) => left_try == right_try && same_expr(plan, *left, *right),
        (
            Expr::Compare { op: left_op, left: left_a, right: left_b },
            Expr::Compare { op: right_op, left: right_a, right: right_b },
        ) => {
            left_op == right_op
                && same_expr(plan, *left_a, *right_a)
                && same_expr(plan, *left_b, *right_b)
        }
        (
            Expr::Conjunction { op: left_op, children: left_children },
            Expr::Conjunction { op: right_op, children: right_children },
        ) => left_op == right_op && lists(*left_children, *right_children),
        (
            Expr::Function { name: left_name, args: left_args },
            Expr::Function { name: right_name, args: right_args },
        ) => plan.string(*left_name) == plan.string(*right_name) && lists(*left_args, *right_args),
        (
            Expr::Aggregate {
                name: left_name,
                args: left_args,
                distinct: left_distinct,
                filter: left_filter,
            },
            Expr::Aggregate {
                name: right_name,
                args: right_args,
                distinct: right_distinct,
                filter: right_filter,
            },
        ) => {
            plan.string(*left_name) == plan.string(*right_name)
                && left_distinct == right_distinct
                && match (left_filter, right_filter) {
                    (None, None) => true,
                    (Some(left), Some(right)) => same_expr(plan, *left, *right),
                    _ => false,
                }
                && lists(*left_args, *right_args)
        }
        (
            Expr::Case { arms: left_arms, otherwise: left_otherwise },
            Expr::Case { arms: right_arms, otherwise: right_otherwise },
        ) => {
            let left_arms = plan.arm_list(*left_arms);
            let right_arms = plan.arm_list(*right_arms);
            left_arms.len() == right_arms.len()
                && left_arms.iter().zip(right_arms).all(|(left, right)| {
                    same_expr(plan, left.when, right.when) && same_expr(plan, left.then, right.then)
                })
                && match (left_otherwise, right_otherwise) {
                    (None, None) => true,
                    (Some(left), Some(right)) => same_expr(plan, *left, *right),
                    _ => false,
                }
        }
        _ => false,
    }
}

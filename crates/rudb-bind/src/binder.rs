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

use std::sync::Arc;

use rudb_catalog::{Catalog, Entry, QualifiedName, same_name};
use rudb_common::bounds::Zones;
use rudb_common::{
    Error, Field, LogicalType, Result, Semantics, Session, ShowBehavior, Span, Stat, Value,
};
use rudb_functions::{
    Columns, FILE_ROW_NUMBER, FunctionKind, Given, Resolved, TableFunction, csv_fields, csv_given,
    files, is_file, is_pattern, kind_of, parquet_footers, resolve, resolve_pragma, resolve_table,
};
use rudb_kernels::{cast_value, row_count};
use rudb_parse::ast::{self, Ast, Distinct, LiteralKind, Nulls, Order, Quantifier, SetOp};
use rudb_parse::{NONE, identifier_parts, parse_ast_with_case};
use rudb_plan::{
    Bound, BuildSide, ColumnBinding, ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan,
    SetOpKind, SortKey, WindowBound, WindowExclude, WindowFrame, WindowUnit,
};

use crate::expr::{describe, has_aggregate};
use crate::fold;
use crate::parameters::Parameters;
use crate::scope::{Scope, Visible};

/// Binds a parsed statement against a catalog.
///
/// # Errors
///
/// If the script does not hold exactly one statement, if a name does not resolve, if a type does
/// not work out, or if the query uses something M0 does not bind yet.
pub fn bind(ast: &Ast, catalog: &Catalog) -> Result<Plan> {
    bind_with(ast, catalog, &Parameters::new(), &Session::new())
}

/// Binds a parsed query against a catalog, with values for its parameters and its settings.
///
/// The session is what `current_setting()` reads, and a caller with no database behind it passes an
/// empty one, which makes every setting name unrecognized rather than making up an answer.
///
/// # Errors
///
/// Everything [`bind`] reports, plus an error for a parameter that was given no value.
pub fn bind_with(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
) -> Result<Plan> {
    let query = match ast.statements.as_slice() {
        [ast::Statement::Query(query)] => *query,
        [] => return Err(Error::binder("no statement to bind")),
        // One statement that is not a query is its own answer. Reporting it as a script of several
        // reads as a count being wrong, and the count is right.
        [_] => return Err(Error::not_implemented("a statement that is not a query")),
        _ => return Err(Error::not_implemented("a script of more than one statement")),
    };
    let mut binder = Binder::with(catalog, parameters, session);
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
    bind_sql_with(query, catalog, &Session::new())
}

/// Parses and binds one query, with the settings a call to `current_setting()` reads.
///
/// # Errors
///
/// Anything the parser or the binder reports.
pub fn bind_sql_with(query: &str, catalog: &Catalog, session: &Session) -> Result<Plan> {
    let ast = parse_ast_with_case(query, session.semantics().identifier_case())?;
    bind_with(&ast, catalog, &Parameters::new(), session)
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

/// One run of window calls that agree on where the rows come from and in what order.
///
/// The run is the unit the plan has an operator for, so two calls that write the same partition,
/// the same order and the same frame are one operator and one sort, and a third that writes a
/// different order is a second operator stacked on the first. Nothing here merges runs that only
/// look compatible, because a window is evaluated over the rows the operator below it produced and
/// deciding two runs are the same is the optimizer's job rather than the binder's.
#[derive(Debug)]
pub(crate) struct WindowRun {
    /// The table index the run's result columns bind against.
    index: u32,
    /// What divides the input into independent partitions.
    partition: Vec<ExprRef>,
    /// The order within a partition.
    order: Vec<SortKey>,
    /// The frame every call in the run shares.
    frame: WindowFrame,
    /// The calls, in the order their columns are appended.
    calls: Vec<ExprRef>,
}

/// One window call as it was written, before any of it has been bound.
///
/// These six travel together from the parser all the way to the run they end up filed under, and
/// carrying them as one thing keeps the call that binds them readable.
pub(crate) struct WindowCall<'a> {
    /// The function name, as written and not yet resolved.
    pub(crate) name: &'a str,
    /// The arguments, which may include a star that only `count` is allowed to be given.
    pub(crate) args: &'a [ast::ExprRef],
    /// Whether `DISTINCT` was written inside the parens.
    pub(crate) distinct: bool,
    /// The `FILTER (WHERE ...)` predicate, which is written before the `OVER`, or `NONE`.
    pub(crate) filter: ast::ExprRef,
    /// Whether `IGNORE NULLS` was written inside the parens, which is where DuckDB puts it.
    pub(crate) ignore_nulls: bool,
    /// The `OVER`, which the parser has already resolved against any `WINDOW` clause.
    pub(crate) spec: ast::WindowRef,
}

/// Everything inside one window call once it is bound, which is what decides its run.
struct WindowParts {
    /// The arguments, before the casts the resolved signature asks for.
    args: Vec<ExprRef>,
    /// What divides the input into independent partitions.
    partition: Vec<ExprRef>,
    /// The order within a partition.
    order: Vec<SortKey>,
    /// The frame, with both ends and the exclusion.
    frame: WindowFrame,
}

/// What opening the files behind a table function call said about them.
///
/// The answers travel together because they come out of the same footer. A Parquet file states its
/// columns, its row count and its statistics in the same few kilobytes at the end of it, so a
/// binder that has read one has read all of them, and splitting them into four arguments would
/// mean four ways to forget one.
#[derive(Debug)]
struct Read {
    /// The columns the call produces, in the order the file stores them.
    fields: Vec<Field>,
    /// How many rows all of the files hold, where anybody counted.
    rows: Stat<u64>,
    /// How many distinct values a column holds, by name, for the columns anybody counted.
    distincts: Vec<(String, Stat<u64>)>,
    /// The bounds the files keep per part of themselves, where anything can answer for them.
    zones: Option<Arc<dyn Zones>>,
}

impl Read {
    /// Columns that came from somewhere other than a file, so nothing counted anything.
    fn uncounted(fields: Vec<Field>) -> Self {
        Self { fields, rows: Stat::Unknown, distincts: Vec::new(), zones: None }
    }
}

/// A materialised `WITH` definition that has been bound and can be read by name.
#[derive(Debug)]
struct Materialized {
    /// Which written definition this is, as an index into `Ast::ctes`.
    written: u32,
    /// The number the plan uses to pair a read with what it reads.
    cte: u32,
    /// The name it was written with, which is the table name a read is reachable through.
    name: String,
    /// What it produces, in order, under the declared names when a column list was written.
    fields: Vec<Field>,
}

#[derive(Debug)]
pub(crate) struct PendingSubquery {
    pub(crate) node: NodeRef,
    pub(crate) kind: JoinKind,
    pub(crate) conditions: Vec<ExprRef>,
    pub(crate) dependent: bool,
    /// The outer columns the query's body read, which is what `dependent` counts.
    ///
    /// Kept rather than reduced to the flag because a join's `ON` has to decide which of its two
    /// inputs the query is joined into, and the answer is the side those columns come from. A
    /// query that reads neither side can go on either.
    pub(crate) reads: Vec<ColumnBinding>,
    /// The table index this query's join adds to the rows it is joined into.
    ///
    /// Kept so that a `HAVING` which reads one of these can say which columns came from a query
    /// joined above the grouping rather than from the table underneath it. Those columns are not
    /// the table's and the grouping rule has nothing to say about them.
    pub(crate) index: u32,
    /// Whether the query was written inside an aggregate call's argument or its `FILTER`.
    ///
    /// One written there is read once per row going into the aggregate, so it has to be joined in
    /// underneath the grouping however uncorrelated it is. Every other query a grouped block writes
    /// is one row for the whole block and is lifted over the grouping instead, which is what
    /// [`Binder::lift_over_aggregate`] decides.
    pub(crate) inside_aggregate: bool,
}

/// Which input of a join a query written in that join's `ON` is joined into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// The state one binding run carries.
#[derive(Debug)]
pub(crate) struct Binder<'a> {
    catalog: &'a Catalog,
    /// What the parameters were given, empty for a statement that is not prepared.
    pub(crate) parameters: &'a Parameters,
    /// What the settings are now, which is what `current_setting()` folds to.
    pub(crate) session: &'a Session,
    /// Meaning-changing choices copied once and resolved into the plan above execution.
    pub(crate) semantics: Semantics,
    plan: Plan,
    next_index: u32,
    /// Source range inherited by plan objects built for the current AST expression or query.
    pub(crate) current_span: Span,
    /// Set while a select block aggregates, which changes what a bare column means.
    pub(crate) aggregation: Option<Aggregation>,
    /// Set while an aggregate's own arguments are being bound, so nesting is caught.
    pub(crate) in_aggregate: bool,
    /// Set while an aggregate's `FILTER` is being bound, which is refused its own aggregate.
    pub(crate) in_filter: bool,
    /// The window runs this select block has collected, in the order they were first written.
    pub(crate) windows: Vec<WindowRun>,
    /// Set while a window call's own arguments and keys are being bound, so nesting is caught.
    pub(crate) in_window: bool,
    /// Uncorrelated scalar queries waiting to be joined into the select block that uses them.
    pub(crate) scalar_subqueries: Vec<PendingSubquery>,
    /// Table indices of the queries this block will join in above its grouping, not below it.
    ///
    /// Only ever set while a `HAVING` is being rewritten over the aggregate. A column from one of
    /// these is not a column of the grouped table, so the rule about grouping every column does not
    /// reach it, and the join that produces it goes on top of the `Aggregate` rather than under it.
    pub(crate) joined_above: Vec<u32>,
    pub(crate) outer_scopes: Vec<Scope>,
    /// Which of the outer scopes are a FROM entry's left neighbours rather than an enclosing query.
    ///
    /// The two are resolved the same way and refused differently. An aggregate may read a column of
    /// the query it is written in and may not read one a LATERAL brought in from the left, so the
    /// check needs to know which scope the name came out of. Each entry is a position in
    /// `outer_scopes`.
    pub(crate) lateral_scopes: Vec<usize>,
    pub(crate) correlations: Vec<Vec<ColumnBinding>>,
    /// Where we are, for an error message that says which clause the writer should look at.
    pub(crate) clause: &'static str,
    /// The views whose bodies are open on the stack, which is what catches a cycle.
    expanding: Vec<String>,
    /// The materialised `WITH` definitions whose bodies are being bound, innermost last.
    ///
    /// A stack rather than a map from what was written, because a plain `WITH` is put into every
    /// place it is named, so a materialised one written inside a plain one is bound once per use
    /// and each of those is a materialisation of its own with a number of its own.
    materialized: Vec<Materialized>,
    /// How many materialisations have been numbered, which is where the next number comes from.
    next_cte: u32,
    /// When this statement started, read once and kept, which is what `now()` folds to.
    started: Option<i64>,
}

impl<'a> Binder<'a> {
    pub(crate) fn with(
        catalog: &'a Catalog,
        parameters: &'a Parameters,
        session: &'a Session,
    ) -> Self {
        Self {
            catalog,
            parameters,
            session,
            semantics: session.semantics(),
            plan: Plan::new(),
            next_index: 0,
            current_span: Span::new(0, 0),
            aggregation: None,
            in_aggregate: false,
            in_filter: false,
            windows: Vec::new(),
            in_window: false,
            scalar_subqueries: Vec::new(),
            joined_above: Vec::new(),
            outer_scopes: Vec::new(),
            lateral_scopes: Vec::new(),
            correlations: Vec::new(),
            clause: "SELECT clause",
            expanding: Vec::new(),
            materialized: Vec::new(),
            next_cte: 0,
            started: None,
        }
    }

    pub(crate) fn catalog(&self) -> &Catalog {
        self.catalog
    }

    /// When this statement started, in microseconds since the epoch.
    ///
    /// Read from the clock the first time something asks and kept after that, so a query that
    /// writes `now()` twice gets one answer for both. That is what the pin does and what it reports
    /// in the `stability` column of `duckdb_functions()`, where every one of these is
    /// `CONSISTENT_WITHIN_QUERY`. A query that never asks never reads the clock.
    pub(crate) fn instant(&mut self) -> i64 {
        *self.started.get_or_insert_with(crate::context::micros_now)
    }

    pub(crate) fn plan(&self) -> &Plan {
        &self.plan
    }

    pub(crate) fn plan_mut(&mut self) -> &mut Plan {
        &mut self.plan
    }

    pub(crate) fn add_expr(&mut self, expr: Expr, ty: LogicalType) -> ExprRef {
        self.plan.add_expr_at(expr, ty, self.current_span)
    }

    pub(crate) fn add_constant(&mut self, value: Value) -> ExprRef {
        let ty = value.logical_type();
        let reference = self.plan.add_value(value);
        self.plan.add_expr_at(Expr::Constant(reference), ty, self.current_span)
    }

    pub(crate) fn add_node(&mut self, node: Node) -> NodeRef {
        self.plan.add_node_at(node, self.current_span)
    }

    pub(crate) fn into_plan(self) -> Plan {
        self.plan
    }

    /// A table index nothing else has.
    pub(crate) fn fresh_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    /// A reference to one column of an operator's output.
    fn column(&mut self, index: u32, position: usize, ty: LogicalType) -> ExprRef {
        let binding = ColumnBinding::new(index, position as u32);
        self.plan.add_expr(Expr::Column(binding), ty)
    }

    /// Joins scalar query results into the row stream that contains their expressions.
    fn attach_scalar_subqueries(&mut self, mut input: NodeRef) -> NodeRef {
        let subqueries = std::mem::take(&mut self.scalar_subqueries);
        for pending in subqueries {
            input = self.attach_subquery(input, pending);
        }
        input
    }

    /// Joins one query's result into a row stream, which is where its columns come from.
    ///
    /// Split out from [`Self::attach_scalar_subqueries`] because a join's `ON` does not attach its
    /// queries to the rows the whole `FROM` produced. It attaches them to one of the join's two
    /// inputs, since a join condition is evaluated by the join and can only read what the join was
    /// given.
    fn attach_subquery(&mut self, input: NodeRef, pending: PendingSubquery) -> NodeRef {
        let PendingSubquery {
            node: mut right,
            kind,
            conditions,
            dependent,
            reads: _,
            index: _,
            inside_aggregate: _,
        } = pending;
        if kind == JoinKind::Single && !self.semantics.scalar_subquery_error_on_multiple_rows() {
            right = self.add_node(Node::Limit {
                input: right,
                count: Bound::Rows(1),
                offset: Bound::Rows(0),
            });
        }
        let conditions = self.plan.add_expr_list(&conditions);
        if dependent {
            self.add_node(Node::DependentJoin { left: input, right, kind, conditions })
        } else {
            self.add_node(Node::Join {
                left: input,
                right,
                kind,
                conditions,
                build: BuildSide::default(),
            })
        }
    }

    // ---------------------------------------------------------------- queries

    pub(crate) fn bind_query(
        &mut self,
        ast: &Ast,
        query: ast::QueryRef,
    ) -> Result<(NodeRef, Scope)> {
        let span = ast.query_span(query);
        let outer = std::mem::replace(&mut self.current_span, span);
        let result =
            self.bind_query_inner(ast, query).map_err(|error| error.with_fallback_span(span));
        self.current_span = outer;
        result
    }

    fn bind_query_inner(&mut self, ast: &Ast, query: ast::QueryRef) -> Result<(NodeRef, Scope)> {
        let written = ast.query(query);
        if written.ctes.is_empty() {
            return self.bind_body(ast, &written);
        }
        // The names a query introduces are gone again once it is bound, and they go whether the
        // binding worked or not, which is why the stack is cut back here rather than at the end of
        // the call that pushed onto it.
        let depth = self.materialized.len();
        let result = self.bind_materialized(ast, &written);
        self.materialized.truncate(depth);
        result
    }

    /// A query with materialised `WITH` definitions in front of it.
    ///
    /// The definitions are bound first and in the order they were written, so that a later one can
    /// read an earlier one, and then the body. The wrapping runs backwards so that the first
    /// definition ends up outermost, which is the order they have to be filled in.
    fn bind_materialized(&mut self, ast: &Ast, written: &ast::Query) -> Result<(NodeRef, Scope)> {
        let depth = self.materialized.len();
        let held = ast.cte_list(written.ctes).to_vec();
        let mut definitions = Vec::with_capacity(held.len());
        for &index in &held {
            definitions.push(self.bind_definition(ast, index)?);
        }
        let (mut node, scope) = self.bind_body(ast, written)?;
        for (at, definition) in definitions.into_iter().enumerate().rev() {
            let entry = &self.materialized[depth + at];
            let cte = entry.cte;
            let name = entry.name.clone();
            let fields = entry.fields.clone();
            let name = self.plan.intern(&name);
            let columns = self.plan.add_fields(&fields);
            node =
                self.add_node(Node::MaterializedCte { definition, body: node, name, cte, columns });
        }
        Ok((node, scope))
    }

    /// Binds one materialised `WITH` definition and makes its name readable from there on.
    ///
    /// The definition is projected onto exactly the columns a read of it sees, under the names the
    /// column list declared when there was one. That projection is not decoration: what is held is
    /// what a read gets back, so the held rows have to be the rows of the definition's own select
    /// list and nothing it happened to carry along underneath.
    ///
    /// A column list with more names in it than the definition has columns is not an error here,
    /// which is the pinned build's rule and is written out on [`Scope::rename_prefix`].
    fn bind_definition(&mut self, ast: &Ast, index: u32) -> Result<NodeRef> {
        let held = ast.cte(index);
        let name = ast.string(held.name).to_string();
        let (node, mut scope) = self.bind_query(ast, held.query)?;
        if !held.columns.is_empty() {
            let names: Vec<&str> = ast.name(held.columns).collect();
            scope.rename_prefix(&names);
        }
        let table = self.fresh_index();
        let mut exprs = Vec::with_capacity(scope.len());
        let mut names = Vec::with_capacity(scope.len());
        for column in &scope.columns {
            exprs.push(self.plan.add_expr(Expr::Column(column.binding), column.ty.clone()));
            names.push(self.plan.intern(&column.name));
        }
        let exprs = self.plan.add_expr_list(&exprs);
        let names = self.plan.add_name_list(&names);
        let node = self.add_node(Node::Project { input: node, index: table, exprs, names });
        let cte = self.next_cte;
        self.next_cte += 1;
        self.materialized.push(Materialized { written: index, cte, name, fields: scope.fields() });
        Ok(node)
    }

    fn bind_body(&mut self, ast: &Ast, written: &ast::Query) -> Result<(NodeRef, Scope)> {
        match written.body {
            ast::QueryBody::Select(select) => self.bind_select(ast, select, written),
            ast::QueryBody::SetOp { op, quantifier, by_name, left, right } => {
                let operator = Operator { op, quantifier, by_name };
                self.bind_set_op(ast, written, operator, left, right)
            }
            ast::QueryBody::Values(rows) => self.bind_values(ast, written, rows),
            ast::QueryBody::Describe(inner) => self.bind_describe(ast, written, inner),
            ast::QueryBody::Show { name, relation } => self.bind_show(ast, written, name, relation),
        }
    }

    /// `SHOW name`, resolved while binding so execution receives an ordinary constant plan.
    fn bind_show(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        name: ast::Slice,
        relation: ast::QueryRef,
    ) -> Result<(NodeRef, Scope)> {
        let text = ast.name_text(name);
        let parts: Vec<&str> = ast.name(name).collect();
        let table_exists = self.catalog.resolve(&parts).is_ok();
        let as_table = match self.semantics.show_behavior() {
            ShowBehavior::Auto => table_exists,
            ShowBehavior::Setting => false,
            ShowBehavior::Table => true,
        };
        if as_table {
            return self.bind_describe(ast, query, relation);
        }
        let Some((_, value)) =
            self.session.iter().find(|(name, _)| name.eq_ignore_ascii_case(&text))
        else {
            return Err(Error::catalog(format!("Setting with name \"{text}\" does not exist")));
        };
        let field = Field::new(text, LogicalType::Varchar);
        let expr = self.plan.add_constant(Value::Varchar(value.to_string()));
        let row = self.plan.add_expr_list(&[expr]);
        let rows = self.plan.add_rows(&[row]);
        let columns = self.plan.add_fields(std::slice::from_ref(&field));
        let index = self.fresh_index();
        let node = self.add_node(Node::Values { index, columns, rows });
        let mut scope = Scope::empty();
        scope.push(Visible {
            table: String::new(),
            name: field.name,
            binding: ColumnBinding::new(index, 0),
            ty: LogicalType::Varchar,
            not_null: false,
        });
        Ok((node, scope))
    }

    /// `DESCRIBE <query>`, which is six VARCHAR columns saying what the query returns.
    ///
    /// The query is bound and never run, because binding is the whole of the answer: the names and
    /// the types of a query's columns are settled by the time the binder is done with it, so the
    /// rows of a describe are a constant from there on. That is why this comes out as a `VALUES`
    /// whose rows were computed here rather than as an operator of its own, and it is what makes
    /// `SELECT column_name FROM (DESCRIBE ...) WHERE ...` an ordinary query over an ordinary
    /// relation with no special case above it.
    ///
    /// The six columns, their order and their types are the reference binary's. `key`, `default`
    /// and `extra` are null for everything this engine can declare, since `PRIMARY KEY`, `UNIQUE`
    /// and `DEFAULT` are all refused by `CREATE TABLE` today and there is nothing for the first two
    /// to hold, and `extra` is empty upstream as well on every table it was asked about. They are
    /// here rather than left out because the width of a result is part of the result, and a program
    /// that reads the fifth column has to find one.
    fn bind_describe(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        inner: ast::QueryRef,
    ) -> Result<(NodeRef, Scope)> {
        let (_, described) = self.bind_query(ast, inner)?;
        let fields: Vec<Field> = ["column_name", "column_type", "null", "key", "default", "extra"]
            .iter()
            .map(|name| Field::new(*name, LogicalType::Varchar))
            .collect();
        let mut slices = Vec::with_capacity(described.columns.len());
        for column in described.columns.clone() {
            // `NO` and `YES` and not a boolean, because the column is VARCHAR upstream and a
            // client that prints the result has to get the same four or three characters.
            let written = [
                column.name.clone(),
                column.ty.to_string(),
                if column.not_null { "NO" } else { "YES" }.to_owned(),
            ];
            let mut items: Vec<ExprRef> = written
                .into_iter()
                .map(|text| self.plan.add_constant(Value::Varchar(text)))
                .collect();
            for _ in 0..3 {
                let empty = self.plan.add_constant(Value::Null);
                items.push(self.cast_to(empty, &LogicalType::Varchar));
            }
            slices.push(self.plan.add_expr_list(&items));
        }
        let rows = self.plan.add_rows(&slices);
        let columns = self.plan.add_fields(&fields);
        let index = self.fresh_index();
        let mut node = self.add_node(Node::Values { index, columns, rows });
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: String::new(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
                not_null: false,
            });
        }
        let keys = self.sort_keys(ast, query, &scope, &[])?;
        if !keys.is_empty() {
            let keys = self.plan.add_sort_keys(&keys);
            node = self.add_node(Node::Sort { input: node, keys });
        }
        node = self.apply_limit(ast, query, node, &mut scope)?;
        Ok((node, scope))
    }

    /// Whether a projected expression is a column passed straight through from below.
    ///
    /// Only `DESCRIBE` asks, and only to decide whether the `null` column says `NO`. Anything that
    /// is computed is nullable however strict its inputs were, which is both the safe reading and
    /// the one the reference binary gives.
    fn passes_through(&self, expr: ExprRef, input: &Scope) -> bool {
        let Expr::Column(binding) = *self.plan.expr(expr) else { return false };
        input.columns.iter().any(|column| column.binding == binding && column.not_null)
    }

    /// `VALUES (1, 'a'), (2, 'b')`, as a query in its own right.
    ///
    /// The column names are `col0`, `col1` and so on, which is what DuckDB calls them, and the
    /// column types are what every row in that position promotes to. Promotion is the same rule a
    /// set operation uses, and for the same reason: a column has one type and the rows have to
    /// agree on it before anything downstream can read the column.
    fn bind_values(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        rows: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let written = ast.rows(rows).to_vec();
        let Some(first) = written.first() else {
            return Err(Error::binder("VALUES needs at least one row"));
        };
        let width = first.len as usize;
        for (at, row) in written.iter().enumerate() {
            if row.len as usize != width {
                return Err(Error::binder(format!(
                    "VALUES lists must all be the same length, expected {width} columns but row {} has {}",
                    at + 1,
                    row.len
                )));
            }
        }
        // A row of a `VALUES` cannot see a column, because there is nothing under it to see.
        let empty = Scope::empty();
        let previous = std::mem::replace(&mut self.clause, "VALUES clause");
        let mut bound: Vec<Vec<ExprRef>> = Vec::with_capacity(written.len());
        for row in &written {
            let mut items = Vec::with_capacity(width);
            for &expr in ast.expr_list(*row) {
                items.push(self.bind_expr(ast, expr, &empty)?);
            }
            bound.push(items);
        }
        self.clause = previous;
        let mut types = Vec::with_capacity(width);
        for at in 0..width {
            let mut ty = self.plan.expr_type(bound[0][at]).clone();
            for row in &bound[1..] {
                let other = self.plan.expr_type(row[at]).clone();
                ty = ty.promote(&other).ok_or_else(|| {
                    Error::binder(format!(
                        "Cannot combine a value of type {ty} with a value of type {other} in column {} of a VALUES",
                        at + 1
                    ))
                })?;
            }
            types.push(ty);
        }
        let mut slices = Vec::with_capacity(bound.len());
        for row in &bound {
            let items: Vec<ExprRef> = row
                .iter()
                .zip(&types)
                .map(|(&expr, ty)| self.checked_cast_to(expr, ty, false))
                .collect::<Result<_>>()?;
            slices.push(self.plan.add_expr_list(&items));
        }
        let rows = self.plan.add_rows(&slices);
        let fields: Vec<Field> = types
            .iter()
            .enumerate()
            .map(|(at, ty)| Field::new(format!("col{at}"), ty.clone()))
            .collect();
        let columns = self.plan.add_fields(&fields);
        let index = self.fresh_index();
        let mut node = self.add_node(Node::Values { index, columns, rows });
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: String::new(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
                not_null: false,
            });
        }
        let keys = self.sort_keys(ast, query, &scope, &[])?;
        if !keys.is_empty() {
            let keys = self.plan.add_sort_keys(&keys);
            node = self.add_node(Node::Sort { input: node, keys });
        }
        node = self.apply_limit(ast, query, node, &mut scope)?;
        Ok((node, scope))
    }

    fn bind_set_op(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        operator: Operator,
        left: ast::QueryRef,
        right: ast::QueryRef,
    ) -> Result<(NodeRef, Scope)> {
        let (left_node, left_scope) = self.bind_query(ast, left)?;
        let (right_node, right_scope) = self.bind_query(ast, right)?;
        let merged = if operator.by_name {
            match_by_name(&left_scope, &right_scope)?
        } else {
            match_by_position(&left_scope, &right_scope)?
        };
        let left_node = self.conform(left_node, &left_scope, &merged, |column| column.left)?;
        let right_node = self.conform(right_node, &right_scope, &merged, |column| column.right)?;
        let index = self.fresh_index();
        let kind = match operator.op {
            SetOp::Union => SetOpKind::Union,
            SetOp::Except => SetOpKind::Except,
            SetOp::Intersect => SetOpKind::Intersect,
        };
        // UNION alone removes duplicates and UNION ALL keeps them, which is the one place the
        // unwritten quantifier and ALL disagree.
        let all = operator.quantifier == Quantifier::All;
        let mut node =
            self.add_node(Node::SetOp { left: left_node, right: right_node, kind, all, index });
        let mut scope = Scope::empty();
        for (at, column) in merged.iter().enumerate() {
            scope.push(Visible {
                table: String::new(),
                name: column.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: column.ty.clone(),
                // A column of a set operation is nullable whatever the two sides were, because a
                // column that refuses nulls on one side and takes them on the other takes them.
                not_null: false,
            });
        }
        // Above a set operation there is nothing but the output columns, so an ORDER BY term is
        // either a position, an output name, or an expression over the output, and never needs a
        // column projected for it that the query did not ask for.
        let keys = self.sort_keys(ast, query, &scope, &[])?;
        if !keys.is_empty() {
            let keys = self.plan.add_sort_keys(&keys);
            node = self.add_node(Node::Sort { input: node, keys });
        }
        node = self.apply_limit(ast, query, node, &mut scope)?;
        Ok((node, scope))
    }

    /// Projects one side of a set operation onto the columns the operation comes out with.
    ///
    /// `pick` says which column of this side each output column is. It answers nothing for a
    /// column only the other side wrote, which happens under `BY NAME` and which this side fills
    /// with a null, since that is the row it would have written if it had written the column.
    fn conform(
        &mut self,
        node: NodeRef,
        scope: &Scope,
        merged: &[Merged],
        pick: impl Fn(&Merged) -> Option<usize>,
    ) -> Result<NodeRef> {
        let unchanged = merged.len() == scope.len()
            && merged
                .iter()
                .enumerate()
                .all(|(at, column)| pick(column) == Some(at) && column.ty == scope.columns[at].ty);
        if unchanged {
            return Ok(node);
        }
        let index = self.fresh_index();
        let mut exprs = Vec::with_capacity(merged.len());
        let mut names = Vec::with_capacity(merged.len());
        for column in merged {
            let expr = match pick(column) {
                Some(at) => {
                    let held = &scope.columns[at];
                    self.plan.add_expr(Expr::Column(held.binding), held.ty.clone())
                }
                None => self.plan.add_constant(Value::Null),
            };
            exprs.push(self.checked_cast_to(expr, &column.ty, false)?);
            names.push(self.plan.intern(&column.name));
        }
        let exprs = self.plan.add_expr_list(&exprs);
        let names = self.plan.add_name_list(&names);
        Ok(self.add_node(Node::Project { input: node, index, exprs, names }))
    }

    // ----------------------------------------------------------------- select

    fn bind_select(
        &mut self,
        ast: &Ast,
        select: ast::SelectRef,
        query: &ast::Query,
    ) -> Result<(NodeRef, Scope)> {
        let written = ast.select(select);
        // A window belongs to the block that wrote it, and a block can be bound inside another one
        // without a subquery in between, so the outer block's runs are put aside for the duration
        // rather than left where a nested block would append to them.
        let outer_windows = std::mem::take(&mut self.windows);
        // Same argument for the queries lifted over this block's grouping. They are recorded while
        // the select list is being bound and read until the sort keys are done, and a block bound
        // inside that stretch has its own set, so the outer block's is put aside rather than left
        // where the inner one would clear it.
        let outer_joined_above = std::mem::take(&mut self.joined_above);
        let (mut node, input) = self.bind_from(ast, written.from)?;
        node = self.attach_scalar_subqueries(node);

        if written.filter != NONE {
            self.clause = "WHERE clause";
            let predicate = self.bind_expr(ast, written.filter, &input)?;
            let predicate = self.as_boolean(predicate, "WHERE")?;
            node = self.attach_scalar_subqueries(node);
            node = self.add_node(Node::Filter { input: node, predicate });
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

        // The queries this block's clauses wrote that are joined in above the grouping rather than
        // below it. TPC-H q11 is the case in a `HAVING`: `HAVING sum(ps_supplycost * ps_availqty) >
        // (SELECT sum(...))` compares one group's total against a total over the whole table, and
        // the second total is one row that has nothing to do with the groups. Joined underneath the
        // grouping it would be a column of every input row and the grouping rule would ask for it in
        // the GROUP BY, which is the complaint this used to make.
        let mut above = Vec::new();

        self.clause = "SELECT clause";
        let (mut exprs, mut names) = self.bind_targets(ast, &targets, &input, &mut above)?;
        let visible = exprs.len();

        let mut having = None;
        if written.having != NONE {
            self.clause = "HAVING clause";
            let before = self.scalar_subqueries.len();
            let predicate = self.bind_expr(ast, written.having, &input)?;
            self.lift_over_aggregate(before, &mut above, &input)?;
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
                not_null: self.passes_through(*expr, &input),
            });
        }

        self.clause = "ORDER BY clause";
        let mut extra = Vec::new();
        let keys = self.select_sort_keys(
            ast, query, &input, &output, project, &mut exprs, &mut names, &mut extra, &mut above,
        )?;
        self.joined_above = outer_joined_above;
        if !extra.is_empty() && written.distinct != Distinct::No {
            return Err(Error::binder(
                "For SELECT DISTINCT, ORDER BY expressions must appear in the select list",
            ));
        }
        let on = self.distinct_on(ast, written.distinct, &output)?;

        node = self.attach_scalar_subqueries(node);

        if let Some(aggregation) = self.aggregation.take() {
            let index = aggregation.index;
            let groups = self.plan.add_expr_list(&aggregation.groups);
            let aggregates = self.plan.add_expr_list(&aggregation.aggregates);
            node = self.add_node(Node::Aggregate { input: node, index, groups, aggregates });
        }
        if !above.is_empty() {
            debug_assert!(self.scalar_subqueries.is_empty(), "a query is waiting to be joined");
            self.scalar_subqueries = above;
            node = self.attach_scalar_subqueries(node);
        }
        if let Some(predicate) = having {
            node = self.add_node(Node::Filter { input: node, predicate });
        }

        // After the grouping and after `HAVING`, which is where the reference binary puts it:
        // `SELECT j, sum(count(i)) OVER () FROM t GROUP BY j HAVING count(i) > 1` totals only the
        // groups that survived the filter.
        for run in std::mem::replace(&mut self.windows, outer_windows) {
            let partition = self.plan.add_expr_list(&run.partition);
            let order = self.plan.add_sort_keys(&run.order);
            let expressions = self.plan.add_expr_list(&run.calls);
            node = self.add_node(Node::Window {
                input: node,
                index: run.index,
                partition,
                order,
                frame: run.frame,
                expressions,
            });
        }

        let interned: Vec<u32> = names.iter().map(|name| self.plan.intern(name)).collect();
        let exprs_slice = self.plan.add_expr_list(&exprs);
        let names_slice = self.plan.add_name_list(&interned);
        node = self.add_node(Node::Project {
            input: node,
            index: project,
            exprs: exprs_slice,
            names: names_slice,
        });

        if written.distinct != Distinct::No {
            let on = self.plan.add_expr_list(&on);
            node = self.add_node(Node::Distinct { input: node, on });
        }
        if !keys.is_empty() {
            let keys = self.plan.add_sort_keys(&keys);
            node = self.add_node(Node::Sort { input: node, keys });
        }
        node = self.apply_limit(ast, query, node, &mut output)?;

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
            // Through the scope rather than through `project`, because a limit that had a query
            // joined in under it put a projection of its own over the top and these columns are
            // that projection's now.
            let binding = output.columns[at].binding;
            kept.push(self.plan.add_expr(Expr::Column(binding), ty.clone()));
            kept_names.push(self.plan.intern(name));
            scope.push(Visible {
                table: String::new(),
                name: name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty,
                not_null: output.columns[at].not_null,
            });
        }
        let exprs = self.plan.add_expr_list(&kept);
        let names = self.plan.add_name_list(&kept_names);
        node = self.add_node(Node::Project { input: node, index, exprs, names });
        Ok((node, scope))
    }

    /// Binds the target list, expanding every star into the columns it stands for.
    /// Moves the queries a clause just wrote from under this block's grouping to over it.
    ///
    /// A query written in a select list, a `HAVING` or an `ORDER BY` is one row that has nothing to
    /// do with the groups, so it belongs on top of the grouping and not underneath it. Underneath,
    /// its column is a column of every row going into the aggregate, which the grouping rule then
    /// asks for in the `GROUP BY`, and the aggregate carries nothing but its groups and its
    /// aggregates upward, so the projection could not read the column even if the rule let it
    /// through. That is both halves of #1027.
    ///
    /// A correlated one still goes underneath, because what it correlates to is a column of the
    /// rows going into the grouping and there is nothing above the grouping to read. So does one
    /// written inside an aggregate call, since that is read once per row going into the aggregate
    /// and lifting it over would put it where the aggregate that reads it cannot.
    ///
    /// `before` is what [`Self::scalar_subqueries`] held before the clause was bound, so only the
    /// queries that clause wrote are considered.
    fn lift_over_aggregate(
        &mut self,
        before: usize,
        above: &mut Vec<PendingSubquery>,
        scope: &Scope,
    ) -> Result<()> {
        if self.aggregation.is_none() {
            return Ok(());
        }
        let mut lifted = Vec::new();
        for pending in self.scalar_subqueries.split_off(before) {
            if pending.dependent || pending.inside_aggregate {
                self.scalar_subqueries.push(pending);
            } else {
                self.joined_above.push(pending.index);
                lifted.push(pending);
            }
        }
        // A mark join carries its comparison rather than the expression carrying it, and that
        // comparison is written over the outer rows, so it needs the same rewrite the expression
        // gets. It is done in a second pass so that a comparison reading another query lifted by
        // the same clause finds that query's index already recorded.
        for pending in &mut lifted {
            let conditions = std::mem::take(&mut pending.conditions);
            let mut over = Vec::with_capacity(conditions.len());
            for condition in conditions {
                over.push(self.over_aggregate(condition, scope)?);
            }
            pending.conditions = over;
        }
        above.append(&mut lifted);
        Ok(())
    }

    fn bind_targets(
        &mut self,
        ast: &Ast,
        targets: &[ast::Target],
        input: &Scope,
        above: &mut Vec<PendingSubquery>,
    ) -> Result<(Vec<ExprRef>, Vec<String>)> {
        let mut exprs = Vec::with_capacity(targets.len());
        let mut names = Vec::with_capacity(targets.len());
        for target in targets {
            if let ast::Expr::Star { qualifier, replacements } = ast.expr(target.expr) {
                let table = ast.name(qualifier).last().map(str::to_string);
                let expanded: Vec<Visible> =
                    input.star(table.as_deref())?.into_iter().cloned().collect();
                let replacements = ast.target_list(replacements).to_vec();
                let mut used = vec![false; replacements.len()];
                for column in expanded {
                    let found = replacements.iter().zip(&mut used).find(|(replacement, _)| {
                        same_name(ast.string(replacement.alias), &column.name)
                    });
                    // The replacement takes the column's place and its position, and it is named the
                    // way the replace list spells it rather than the way the table does. That only
                    // shows when the two differ in case, and `AS EventDate` over a column called
                    // `eventdate` is exactly the case that shows it.
                    let before = self.scalar_subqueries.len();
                    let (expr, name) = match found {
                        Some((replacement, used)) => {
                            *used = true;
                            let expr = self.bind_expr(ast, replacement.expr, input)?;
                            (expr, ast.string(replacement.alias).to_string())
                        }
                        None => (
                            self.plan.add_expr(Expr::Column(column.binding), column.ty),
                            column.name,
                        ),
                    };
                    self.lift_over_aggregate(before, above, input)?;
                    exprs.push(self.over_aggregate(expr, input)?);
                    names.push(name);
                }
                // A replace list that named something the star did not stand for is a mistake and
                // not a no op, and it is caught here because this is the first point at which the
                // set of names the star stands for is known.
                if let Some((replacement, _)) =
                    replacements.iter().zip(&used).find(|(_, used)| !**used)
                {
                    return Err(missing_replacement(ast.string(replacement.alias), input));
                }
                continue;
            }
            let before = self.scalar_subqueries.len();
            let expr = self.bind_expr(ast, target.expr, input)?;
            self.lift_over_aggregate(before, above, input)?;
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
        describe(ast, target, self.semantics)
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
        above: &mut Vec<PendingSubquery>,
    ) -> Result<Vec<SortKey>> {
        if query.order_by_all {
            return Ok(self.every_column(output));
        }
        let items = ast.order_list(query.order_by).to_vec();
        let mut keys = Vec::with_capacity(items.len());
        for item in items {
            self.check_order_literal(ast, item.expr)?;
            let position = match self.output_position(ast, item.expr, output)? {
                Some(position) => position,
                None => {
                    let before = self.scalar_subqueries.len();
                    let bound = self.bind_expr(ast, item.expr, input)?;
                    self.lift_over_aggregate(before, above, input)?;
                    let bound = self.over_aggregate(bound, input)?;
                    match exprs.iter().position(|&held| self.same_expr(held, bound)) {
                        Some(position) => position,
                        None => {
                            exprs.push(bound);
                            names.push(describe(ast, item.expr, self.semantics));
                            extra.push(exprs.len() - 1);
                            exprs.len() - 1
                        }
                    }
                }
            };
            let ty = self.plan.expr_type(exprs[position]).clone();
            let expr = self.column(project, position, ty);
            keys.push(self.sort_key(expr, item));
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
            self.check_order_literal(ast, item.expr)?;
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
            keys.push(self.sort_key(expr, item));
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
                let descending = self.semantics.default_descending();
                SortKey { expr, descending, nulls_first: self.semantics.nulls_first(descending) }
            })
            .collect()
    }

    /// A sort key with the session defaults filled in.
    fn sort_key(&self, expr: ExprRef, item: ast::OrderItem) -> SortKey {
        let descending = match item.order {
            Order::Unstated => self.semantics.default_descending(),
            Order::Ascending => false,
            Order::Descending => true,
        };
        let nulls_first = match item.nulls {
            Nulls::First => true,
            Nulls::Last => false,
            Nulls::Unstated => self.semantics.nulls_first(descending),
        };
        SortKey { expr, descending, nulls_first }
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

    /// Refuses a literal sort key unless the session explicitly accepts its no-op behavior.
    fn check_order_literal(&self, ast: &Ast, item: ast::ExprRef) -> Result<()> {
        if !self.semantics.order_by_non_integer_literal()
            && matches!(
                ast.expr(item),
                ast::Expr::Literal { kind, text }
                    if kind != LiteralKind::Number
                        || ast.string(text).contains(['.', 'e', 'E'])
            )
        {
            return Err(Error::binder(
                "ORDER BY non-integer literal has no effect.\n* SET order_by_non_integer_literal=true to allow this behavior.",
            ));
        }
        Ok(())
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

    /// The `LIMIT` and the `OFFSET`, over the rows everything else in the query produced.
    ///
    /// The scope is taken by reference because a limit the binder could not work out reads its
    /// number off a query joined in underneath, and that join puts a column in the rows which the
    /// query did not ask for. A projection over the limit drops it again, and the scope has to say
    /// so, since its bindings are what anything above this reads.
    fn apply_limit(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        input: NodeRef,
        scope: &mut Scope,
    ) -> Result<NodeRef> {
        let waiting = self.scalar_subqueries.len();
        if query.limit_percent {
            let percent = self.constant_percent(ast, query.limit)?;
            let offset = self.count_bound(ast, query.offset, "OFFSET")?;
            let offset = self.settled(offset, "OFFSET")?;
            return Ok(match percent {
                Some(percent) => self.add_node(Node::LimitPercent { input, percent, offset }),
                // A null share is no limit at all, the same as a null row count, so what is left
                // is whatever the offset asked for.
                None => self.limited(input, Bound::All, Bound::Rows(offset)),
            });
        }
        let count = self.count_bound(ast, query.limit, "LIMIT")?;
        // An offset the query left off is nought rows skipped, where a limit it left off is every
        // row emitted, so the two clauses read the same word differently.
        let offset = match self.count_bound(ast, query.offset, "OFFSET")? {
            Bound::All => Bound::Rows(0),
            named => named,
        };
        let joined = self.scalar_subqueries.split_off(waiting);
        if joined.is_empty() {
            return Ok(self.limited(input, count, offset));
        }
        let mut input = input;
        for pending in joined {
            input = self.attach_subquery(input, pending);
        }
        let limit = self.add_node(Node::Limit { input, count, offset });
        Ok(self.reproject(limit, scope))
    }

    /// A row count limit over `input`, or `input` itself when neither half of the clause asks for
    /// anything.
    fn limited(&mut self, input: NodeRef, count: Bound, offset: Bound) -> NodeRef {
        if count == Bound::All && offset == Bound::Rows(0) {
            return input;
        }
        self.add_node(Node::Limit { input, count, offset })
    }

    /// The number a bound holds, for the one caller that has nowhere to put a column.
    ///
    /// A share of the input reads its offset through this, because `LIMIT 30 PERCENT` builds a
    /// node that takes a number and not a [`Bound`], and a share written as a subquery is refused
    /// a few lines above this anyway.
    fn settled(&self, bound: Bound, clause: &str) -> Result<u64> {
        match bound {
            Bound::Rows(rows) => Ok(rows),
            Bound::All => Ok(0),
            Bound::Read(_) => Err(Error::not_implemented(format!(
                "{clause} holding a subquery beside a LIMIT written as a percentage"
            ))),
        }
    }

    /// A projection over `node` handing back exactly the columns `scope` names.
    ///
    /// The scope's bindings are rewritten to this projection's, because its columns are the ones
    /// anything above reads. Only a limit that had a query joined in under it wants this, and only
    /// because there is not always a projection above to drop the column that join added.
    fn reproject(&mut self, node: NodeRef, scope: &mut Scope) -> NodeRef {
        let index = self.fresh_index();
        let mut exprs = Vec::with_capacity(scope.columns.len());
        let mut names = Vec::with_capacity(scope.columns.len());
        for column in &scope.columns {
            exprs.push(self.plan.add_expr(Expr::Column(column.binding), column.ty.clone()));
            names.push(self.plan.intern(&column.name));
        }
        for (at, column) in scope.columns.iter_mut().enumerate() {
            column.binding = ColumnBinding::new(index, at as u32);
        }
        let exprs = self.plan.add_expr_list(&exprs);
        let names = self.plan.add_name_list(&names);
        self.add_node(Node::Project { input: node, index, exprs, names })
    }

    /// The share of the input a `LIMIT n PERCENT` names.
    ///
    /// The same evaluation as a row count and a different type at the end of it: the value is cast
    /// to `DOUBLE` rather than to `BIGINT`, so `LIMIT '30'%` is thirty percent and `LIMIT true%` is
    /// one percent, which is what the pin answers. A null is no limit at all.
    ///
    /// The range is checked here because the pin checks it here. `LIMIT 101 PERCENT` fails an
    /// `EXPLAIN` on the pinned binary, so it is refused while the query is planned and not when it
    /// is run, and a `NAN` is outside the range like any other value that is not between nought and
    /// a hundred.
    fn constant_percent(&mut self, ast: &Ast, written: ast::ExprRef) -> Result<Option<f64>> {
        if written == NONE {
            return Ok(None);
        }
        self.clause = "LIMIT clause";
        let scope = Scope::empty();
        let bound = self.bind_expr(ast, written, &scope)?;
        let Some(value) = fold::value_of(&self.plan, bound)? else {
            return Err(Error::not_implemented("a LIMIT holding a subquery"));
        };
        if value.is_null() {
            return Ok(None);
        }
        let cast = cast_value(&value, &LogicalType::Double, false)?;
        let Value::Double(percent) = cast else {
            return Err(Error::binder(format!(
                "LIMIT takes a percentage, not a value of type {}",
                value.logical_type()
            )));
        };
        if !(0.0..=100.0).contains(&percent) {
            return Err(Error::out_of_range(
                "Limit percent out of range, should be between 0% and 100%",
            ));
        }
        Ok(Some(percent))
    }

    /// The row count a `LIMIT` or an `OFFSET` names.
    ///
    /// It does not have to be a literal. Anything whose value is settled before the first row is
    /// read will do, so `LIMIT 1 + 1` and `LIMIT CAST(3 AS BIGINT)` are both two, and that is what
    /// the pin does with them: its binder evaluates the expression and writes the number down.
    ///
    /// What is left over is an expression the binder cannot settle, which is a subquery, because it
    /// has to run first, and a call that answers differently every time it is made, such as
    /// `RANDOM()` or `nextval`. Those become a [`Bound::Read`] holding the expression, and the
    /// number comes off the first chunk that reaches the limit. The pin takes both and answers them
    /// the same way.
    ///
    /// The value is cast to `BIGINT` whatever it was written as, which is the whole of the type
    /// rule. `LIMIT '3'` is three rows because the string converts, `LIMIT 2.5` is three rows
    /// because the conversion rounds, `LIMIT true` is one row, and `LIMIT DATE '2020-01-01'` is the
    /// cast refusing a date. Every one of those messages is the cast's own, which is why there is
    /// no type check here to write a worse one. A limit that is read while the query runs is cast
    /// the same way by the operator that reads it, so the two paths answer alike.
    fn count_bound(&mut self, ast: &Ast, written: ast::ExprRef, clause: &str) -> Result<Bound> {
        if written == NONE {
            return Ok(Bound::All);
        }
        self.clause = "LIMIT clause";
        let scope = Scope::empty();
        let bound = self.bind_expr(ast, written, &scope)?;
        let Some(value) = fold::value_of(&self.plan, bound)? else {
            return Ok(Bound::Read(bound));
        };
        // A null is no limit at all, the same as leaving the clause off, and the pin agrees:
        // `LIMIT NULL` and `LIMIT CAST(NULL AS INTEGER)` both answer every row.
        if value.is_null() {
            return Ok(Bound::All);
        }
        row_count(&value, clause).map(Bound::Rows)
    }

    // ------------------------------------------------------------------- from

    fn bind_from(&mut self, ast: &Ast, from: ast::Slice) -> Result<(NodeRef, Scope)> {
        let sources = ast.source_list(from).to_vec();
        let Some((first, rest)) = sources.split_first() else {
            // No FROM clause is one row of no columns, which is what SELECT 1 sits on. Not an
            // empty table: an empty table would make SELECT 1 return nothing.
            return Ok((self.add_node(Node::Dummy), Scope::empty()));
        };
        let (mut node, mut scope) = self.bind_source(ast, *first)?;
        for source in rest {
            let (right, right_scope, correlations) = self.bind_lateral(ast, *source, &scope)?;
            node = if correlations.is_empty() {
                self.add_node(Node::CrossProduct { left: node, right })
            } else {
                let conditions = self.plan.add_expr_list(&[]);
                self.add_node(Node::DependentJoin {
                    left: node,
                    right,
                    kind: JoinKind::Inner,
                    conditions,
                })
            };
            scope = scope.concat(right_scope);
        }
        Ok((node, scope))
    }

    /// Binds one FROM entry with everything written to its left already visible.
    ///
    /// That is what LATERAL means, and it is what a comma separated FROM does here whether the word
    /// was written or not, because the pinned build resolves `FROM o, (SELECT o.k + 1)` without it.
    /// The keyword therefore changes nothing and is accepted rather than acted on.
    ///
    /// The columns of the left that the entry read come back with it, and an entry that read none
    /// is an ordinary product. The rest are somebody else's: a name that resolved past the left
    /// neighbours belongs to an enclosing query, so it is handed up to whichever frame is waiting
    /// for it rather than counted here, or the subquery this FROM sits in would lose track of its
    /// own correlation.
    fn bind_lateral(
        &mut self,
        ast: &Ast,
        source: ast::SourceRef,
        left: &Scope,
    ) -> Result<(NodeRef, Scope, Vec<ColumnBinding>)> {
        self.lateral_scopes.push(self.outer_scopes.len());
        self.outer_scopes.push(left.clone());
        self.correlations.push(Vec::new());
        let bound = self.bind_source(ast, source);
        let read = self.correlations.pop().expect("correlation frame");
        self.outer_scopes.pop();
        self.lateral_scopes.pop();
        let (node, scope) = bound?;

        let mut here = Vec::new();
        for binding in read {
            if left.columns.iter().any(|column| column.binding == binding) {
                here.push(binding);
            } else if let Some(enclosing) = self.correlations.last_mut() {
                if !enclosing.contains(&binding) {
                    enclosing.push(binding);
                }
            }
        }
        // A table function is allowed to read the left the same as anything else here. There is
        // nothing underneath one for the domain to be pushed into, since its arguments are what
        // produce its rows, so the unnesting pass turns it into a `LateralFunction` and the call is
        // made once per domain value. That is `domain.rs`.
        //
        // Nothing has to be turned down here for the functions that would not survive it. The only
        // table functions taking an argument that is not a name are the series family, which is the
        // family that operator answers, and a name that is not a constant is refused where the
        // columns are settled, because settling them means opening the file or reading the catalog.
        Ok((node, scope, here))
    }

    fn bind_source(&mut self, ast: &Ast, source: ast::SourceRef) -> Result<(NodeRef, Scope)> {
        match ast.source(source) {
            ast::Source::Table { name, alias, columns } => {
                self.bind_table(ast, name, alias, columns)
            }
            ast::Source::Function { name, args, alias, columns, pragma } => {
                self.bind_table_function(ast, name, args, alias, columns, pragma)
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
            ast::Source::Values { rows, alias, columns } => {
                let bare = ast::Query::bare(ast::QueryBody::Values(rows));
                let (node, mut scope) = self.bind_values(ast, &bare, rows)?;
                let label =
                    if alias == NONE { String::new() } else { ast.string(alias).to_string() };
                scope.relabel(&label);
                if !columns.is_empty() {
                    let names: Vec<&str> = ast.name(columns).collect();
                    scope.rename(&names, &label)?;
                }
                Ok((node, scope))
            }
            ast::Source::Cte { cte, alias, columns } => {
                self.bind_cte_scan(ast, cte, alias, columns)
            }
            ast::Source::Join { left, right, kind, natural, on, using } => {
                self.bind_join(ast, left, right, kind, natural, on, using)
            }
        }
    }

    /// A read of a materialised `WITH`, which is a leaf the same way a table scan is.
    ///
    /// Which definition it reads was settled by the parser, so there is no name to look up here and
    /// no shadowing left to think about. What is looked up is the materialisation that definition
    /// turned into, and the search runs backwards because the same definition is bound again for
    /// each use of a plain `WITH` it sits inside, and a read means the innermost of those.
    fn bind_cte_scan(
        &mut self,
        ast: &Ast,
        written: u32,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let Some(held) = self.materialized.iter().rev().find(|held| held.written == written) else {
            let name = ast.string(ast.cte(written).name);
            return Err(Error::binder(format!("Table with name {name} does not exist!")));
        };
        let cte = held.cte;
        let fields = held.fields.clone();
        let text = held.name.clone();
        let label = if alias == NONE { text.clone() } else { ast.string(alias).to_string() };
        let name = self.plan.intern(&text);
        let index = self.fresh_index();
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: label.clone(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
                not_null: field.not_null,
            });
        }
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, &label)?;
        }
        let columns = self.plan.add_fields(&fields);
        let node = self.add_node(Node::CteScan { index, cte, name, columns });
        Ok((node, scope))
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
        // The catalog is asked first and the file is the fallback, which is the order DuckDB uses:
        // a table really called `mixed.parquet` wins over a file of that name sitting next to it.
        let resolved = match catalog.resolve(&parts) {
            Ok(resolved) => resolved,
            Err(missing) => {
                return self.bind_replacement_scan(ast, &parts, alias, columns, missing);
            }
        };
        if catalog.entry(&resolved)? == Entry::View {
            return self.bind_view(ast, &resolved, alias, columns);
        }
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
                not_null: field.not_null,
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
        // What the store wrote down about itself, against the table index the same way a Parquet
        // footer is. A table with nothing to say records nothing and the estimate falls back to the
        // constants it used before, which is what every table did until the file had a directory
        // worth asking.
        if let Some(zones) = table.rows().zones() {
            self.plan.set_zones(index, zones);
        }
        for (column, distinct) in table.rows().distincts() {
            self.plan.measure_distinct(index, &column, distinct);
        }
        let node = self.add_node(Node::Get {
            catalog: catalog_name,
            schema,
            table: table_name,
            alias,
            index,
            columns,
        });
        Ok((node, scope))
    }

    /// A view where a table goes, which is the body bound again right here.
    ///
    /// Inline and not behind a node. The view is gone by the time the plan exists, so everything
    /// downstream sees the query somebody would have written by hand, and the column pruning that
    /// makes `SELECT COUNT(*) FROM 'hits.parquet'` read no columns at all keeps working through
    /// `FROM hits`. A `Node::View` would be a barrier with nothing on the other side of it.
    ///
    /// The scope this builds is a subquery's, right down to the name in the error message. duckdb
    /// v1.5.1 reports a view whose column list has gone stale as `table "unnamed_subquery" has 1
    /// columns available but 2 columns specified`, which is the sentence its subquery alias rule
    /// produces, so a view there is a subquery with the view's name written over it afterwards.
    fn bind_view(
        &mut self,
        ast: &Ast,
        name: &QualifiedName,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let view = self.catalog.view(name)?;
        let full = name.to_string();
        if self.expanding.contains(&full) {
            // Two quotes each side, which is what the binary prints. It quotes the name on the way
            // in and then formats the quoted name into a quoted slot, so a view called `a` comes
            // back as `""a""`. That is upstream's wart and copying it is the whole job here.
            return Err(Error::binder(format!(
                "infinite recursion detected: attempting to recursively bind view \"\"{}\"\"",
                name.table
            )));
        }
        let body = parse_ast_with_case(view.sql(), self.semantics.identifier_case())?;
        let query = match body.statements.as_slice() {
            [ast::Statement::Query(query)] => *query,
            // Only a query can have got past the binder at creation, so this is a view the catalog
            // was handed some other way rather than anything a statement can produce.
            _ => return Err(Error::binder(format!("view \"{}\" is not a query", name.table))),
        };
        self.expanding.push(full);
        let bound = self.bind_query(&body, query);
        self.expanding.pop();
        let (node, mut scope) = bound?;

        let aliases: Vec<&str> = view.aliases().iter().map(String::as_str).collect();
        if !aliases.is_empty() {
            scope.rename(&aliases, "unnamed_subquery")?;
        }
        // What the catalog tables report as this view's columns, written down here because this is
        // the moment they are known. Upstream refreshes the same cache at the same point, which was
        // measured: both `duckdb_columns()` and `duckdb_views().column_count` keep reporting the old
        // list after an `ALTER TABLE` underneath until something reads the view, and then both move.
        // It is written before the label and before the `AS t(a, b)` list below, because those two
        // rename the view for one query and not for everyone.
        view.remember(scope.fields());
        let label = if alias == NONE { name.table.clone() } else { ast.string(alias).to_string() };
        scope.relabel(&label);
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, &label)?;
        }
        Ok((node, scope))
    }

    /// A function call where a table goes, such as `range(10)`.
    ///
    /// The arguments are bound against an empty scope. A table function that can see the row on its
    /// left is `LATERAL`, and this is not it, so a column name in here is not resolved against
    /// whatever happens to be to the left in the `FROM` list. Letting it would mean `FROM t,
    /// range(t.n)` quietly binding to something whose meaning depends on the order the sources were
    /// written in.
    fn bind_table_function(
        &mut self,
        ast: &Ast,
        name: ast::Slice,
        args: ast::Slice,
        alias: ast::StrRef,
        columns: ast::Slice,
        pragma: bool,
    ) -> Result<(NodeRef, Scope)> {
        let parts: Vec<&str> = ast.name(name).collect();
        // A qualified call names a schema, and the two schemas that exist are the ones every
        // built-in lives in. Anything else is a name that has to fail rather than fall through to
        // the unqualified lookup and be found somewhere it was not asked for.
        let function_name = *parts.last().unwrap_or(&"");
        if let Some(schema) = parts.iter().rev().nth(1) {
            if !schema.eq_ignore_ascii_case("main") && !schema.eq_ignore_ascii_case("system") {
                return Err(Error::catalog(format!(
                    "Table Function with name {} does not exist!",
                    parts.join(".")
                )));
            }
        }
        // The name is looked up before the arguments are bound so that a call of something that is
        // not a table function says that, rather than reporting whatever is wrong with the
        // arguments of a function that was never going to exist.
        let Some(called) = TableFunction::lookup(function_name) else {
            if pragma {
                // `PRAGMA database_list` is a view upstream and not a function, and the pragma
                // namespace holds both, so a name that is not a function gets one more look in the
                // catalog before it is turned down. It has to be the no argument form: a view
                // takes none, and `pragma_database_list()` with parentheses is a missing function
                // on the pin too.
                if args.is_empty() && self.catalog.resolve(&parts).is_ok() {
                    return self.bind_table(ast, name, alias, columns);
                }
                let spelled = function_name.strip_prefix("pragma_").unwrap_or(function_name);
                return Err(Error::catalog(format!(
                    "Pragma Function with name {spelled} does not exist!"
                )));
            }
            return Err(Error::catalog(format!(
                "Table Function with name {function_name} does not exist!"
            )));
        };
        let written = ast.target_list(args).to_vec();
        let empty = Scope::empty();
        let previous = std::mem::replace(&mut self.clause, "table function arguments");
        let mut bound = Vec::new();
        let mut written_options = Vec::new();
        for argument in written {
            let expr = self.bind_expr(ast, argument.expr, &empty)?;
            if argument.alias == NONE {
                bound.push(expr);
            } else {
                let name = ast.string(argument.alias).to_string();
                let (parameter, value) = self.named_argument(called, &name, expr)?;
                written_options.push((parameter, value, expr));
            }
        }
        self.clause = previous;
        let options = Options::of(&written_options)?;

        // The types are what resolve the call, not the count, because `read_parquet(3)` is a
        // different answer from `read_parquet('3')` and only the types tell them apart.
        let given: Vec<LogicalType> =
            bound.iter().map(|&expr| self.plan.expr_type(expr).clone()).collect();
        let resolved = if pragma {
            resolve_pragma(function_name, &given)?
        } else {
            resolve_table(function_name, &given)?
        };
        let mut cast: Vec<ExprRef> = bound
            .iter()
            .zip(&resolved.arguments)
            .map(|(&expr, ty)| self.checked_cast_to(expr, ty, false))
            .collect::<Result<_>>()?;

        if resolved.function.takes_a_name() {
            let Columns::Fixed(fields) = resolved.columns else {
                return Err(Error::internal("a pragma that resolved to a file"));
            };
            let [argument] = cast[..] else {
                return Err(Error::internal("a pragma that resolved to more than one name"));
            };
            return self.bind_pragma(ast, resolved.function, &fields, argument, alias, columns);
        }
        // Filled in by the arm below that has the file names, and left alone by a function whose
        // columns are fixed, because none of those reads a file to find out how tall it is.
        let mut measured = Stat::Unknown;
        let mut counted: Vec<(String, Stat<u64>)> = Vec::new();
        let mut bounded: Option<Arc<dyn Zones>> = None;
        let fields = match resolved.columns {
            Columns::Fixed(fields) => fields,
            columns => {
                // The one argument is a pattern, and what replaces it is one constant per file it
                // matched. The executor is handed names rather than a pattern, so it never walks a
                // directory and the answer cannot change between binding a prepared statement and
                // running it, which is the same reason the schema is settled here.
                let paths = self.file_paths(cast[0], resolved.function.name())?;
                let mut fields = match columns {
                    // Parquet takes the first file's footer as the answer and CSV sniffs all of
                    // them, which is not a choice made here. See `csv_fields`.
                    Columns::Csv => csv_fields(&paths, options.given)?,
                    _ => {
                        let footers = parquet_footers(&paths)?;
                        measured = footers.rows;
                        counted = footers.distincts;
                        bounded = footers.zones;
                        footers.fields
                    }
                };
                if options.all_varchar {
                    // The sniffer still ran, because the names come out of the same pass over the
                    // front of the file and only the types are being overruled. The executor reads
                    // the text as VARCHAR because this is the schema it is told to read into, which
                    // is the same road a file in a glob takes when the set is wider than the file.
                    for field in &mut fields {
                        field.ty = LogicalType::Varchar;
                    }
                }
                if options.binary_as_string {
                    // A byte array column with no annotation on it is a BLOB, and this is the caller
                    // saying that the file's writer meant text. The reader already holds both in the
                    // same string column and already validates the bytes, so the whole of the option
                    // is what the column is called from here on.
                    for field in &mut fields {
                        if field.ty == LogicalType::Blob {
                            field.ty = LogicalType::Varchar;
                        }
                    }
                }
                if options.file_row_number {
                    // Not a column of the file, so it goes on the end where a projection cannot be
                    // confused about which one it is, and the executor counts it as the rows come
                    // out. A file that already has a column of that name is the one case where the
                    // option cannot be honoured, and saying so is better than handing back two
                    // columns with the same name and letting a reference to it pick one.
                    if fields.iter().any(|field| field.name == FILE_ROW_NUMBER) {
                        return Err(Error::binder(format!(
                            "Duplicate column name \"{FILE_ROW_NUMBER}\": the file already has a \
                             column of that name, so file_row_number cannot add one"
                        )));
                    }
                    fields.push(Field::required(FILE_ROW_NUMBER.to_string(), LogicalType::BigInt));
                }
                cast = paths.iter().map(|path| self.path_constant(path)).collect();
                fields
            }
        };
        let label = if alias == NONE {
            resolved.function.name().to_string()
        } else {
            ast.string(alias).to_string()
        };
        let names: Vec<&str> = ast.name(columns).collect();
        self.table_function_source(
            resolved.function,
            &cast,
            &written_options,
            Read { fields, rows: measured, distincts: counted, zones: bounded },
            &label,
            &names,
        )
    }

    /// `pragma_table_info('t')` or `pragma_show('t')`, answered while it is bound.
    ///
    /// The same trick `DESCRIBE` uses and for the same reason: the columns of a table are settled by
    /// the time the name has resolved, so the rows are a constant from there on and this comes out
    /// as a `VALUES` rather than as an operator that reads a catalog while the query runs. It also
    /// means `SELECT name FROM pragma_table_info('t') WHERE notnull` is an ordinary query over an
    /// ordinary relation, which is the whole reason these exist as functions rather than only as
    /// statements.
    ///
    /// The name arrives as a string rather than as something the parser read, so it is split here
    /// under the identifier rule and then resolved like any other name. A name that is not there
    /// comes back as the catalog's own complaint, which is what the pin answers with too.
    fn bind_pragma(
        &mut self,
        ast: &Ast,
        function: TableFunction,
        fields: &[Field],
        argument: ExprRef,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let written = self.pragma_name(argument, function)?;
        let parts = identifier_parts(&written);
        let spelled: Vec<&str> = parts.iter().map(String::as_str).collect();
        let name = self.catalog.resolve(&spelled)?;
        let described = self.described(ast, &name)?;
        let mut rows = Vec::with_capacity(described.len());
        for (at, field) in described.iter().enumerate() {
            let items = if matches!(function, TableFunction::PragmaShow) {
                self.describing(field)
            } else {
                self.table_info(at, field)
            };
            rows.push(self.plan.add_expr_list(&items));
        }
        let rows = self.plan.add_rows(&rows);
        let held = self.plan.add_fields(fields);
        let index = self.fresh_index();
        let node = self.add_node(Node::Values { index, columns: held, rows });
        let label =
            if alias == NONE { function.name().to_string() } else { ast.string(alias).to_string() };
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: label.clone(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
                not_null: false,
            });
        }
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, &label)?;
        }
        Ok((node, scope))
    }

    /// The name a pragma was called with, which has to be a constant.
    ///
    /// A null is a name spelled `NULL` rather than an error about nulls, because the pin turns
    /// whatever it was handed into text before it goes looking and then says a table of that name
    /// does not exist. Writing `pragma_table_info(NULL)` is a mistake either way and this is the
    /// sentence the mistake already has.
    ///
    /// `pragma_table_info('t' || 'x')` is the pin's `tx` and is turned away here, which is the same
    /// missing constant folding [`Binder::named_argument`] writes about and closes the same day.
    fn pragma_name(&self, argument: ExprRef, function: TableFunction) -> Result<String> {
        let Expr::Constant(reference) = *self.plan.expr(argument) else {
            return Err(Error::not_implemented(format!(
                "{}() given a name that is not a constant",
                function.name()
            )));
        };
        match self.plan.value(reference) {
            Value::Varchar(name) => Ok(name.clone()),
            Value::Null => Ok("NULL".to_string()),
            other => {
                Err(Error::internal(format!("a pragma name bound as VARCHAR arrived as {other}")))
            }
        }
    }

    /// The columns of whatever a pragma was pointed at.
    ///
    /// A view is bound here, which is how it comes to have columns at all. Reading a view is what
    /// binds it and describing one counts as reading it, so a view the engine ships with reports a
    /// column count from this point on, the same as it would after a select. The node that binding
    /// produces is thrown away, because the answer is the scope and not the query.
    ///
    /// Every column of a view is nullable whatever the column underneath was declared as, which is
    /// the pin's answer through `pragma_table_info()`, `pragma_show()` and `duckdb_columns()` alike.
    /// [`Scope::fields`] drops the flag on its own, so there is nothing to clear here.
    fn described(&mut self, ast: &Ast, name: &QualifiedName) -> Result<Vec<Field>> {
        if self.catalog.entry(name)? == Entry::Table {
            return Ok(self.catalog.table(name)?.columns().to_vec());
        }
        let (_, scope) = self.bind_view(ast, name, NONE, ast::Slice::default())?;
        Ok(scope.fields())
    }

    /// One row of `pragma_show()`, which is one row of `DESCRIBE` written by the other caller.
    fn describing(&mut self, field: &Field) -> Vec<ExprRef> {
        let written = [
            field.name.clone(),
            field.ty.to_string(),
            if field.not_null { "NO" } else { "YES" }.to_owned(),
        ];
        let mut items: Vec<ExprRef> =
            written.into_iter().map(|text| self.plan.add_constant(Value::Varchar(text))).collect();
        for _ in 0..3 {
            let empty = self.plan.add_constant(Value::Null);
            items.push(self.cast_to(empty, &LogicalType::Varchar));
        }
        items
    }

    /// One row of `pragma_table_info()`, which is SQLite's six columns about the same column.
    ///
    /// `cid` counts from zero, which is SQLite's numbering and not the one based `ordinal_position`
    /// the standard views report. `dflt_value` and `pk` are the two nothings rudb has to report
    /// until `CREATE TABLE` takes a `DEFAULT` or a key.
    fn table_info(&mut self, at: usize, field: &Field) -> Vec<ExprRef> {
        let cid = self.plan.add_constant(Value::Integer(i32::try_from(at).unwrap_or(i32::MAX)));
        let name = self.plan.add_constant(Value::Varchar(field.name.clone()));
        let ty = self.plan.add_constant(Value::Varchar(field.ty.to_string()));
        let not_null = self.plan.add_constant(Value::Boolean(field.not_null));
        let default = self.plan.add_constant(Value::Null);
        let default = self.cast_to(default, &LogicalType::Varchar);
        let key = self.plan.add_constant(Value::Boolean(false));
        vec![cid, name, ty, not_null, default, key]
    }

    /// One named parameter of a table function call, folded into what the call was given.
    ///
    /// The value has to be a constant of the type the parameter wants. It has to be constant
    /// because an option can decide what the columns are and the columns are settled here, and it
    /// has to be already of the type because there is no constant folding in front of the binder
    /// yet. DuckDB folds first, so `binary_as_string=1` and `binary_as_string='yes'` are both true
    /// there and both are turned away here, which is a gap that closes on its own the day the
    /// optimizer runs before the plan is finished. `binary_as_string=True` is what the ClickBench
    /// entry writes and is what has to work.
    ///
    /// A name that is not a parameter of this function is the binary's sentence followed by what it
    /// could have been. The binary puts the candidates on their own indented lines and this puts
    /// them on the same line, because an error is one line here.
    fn named_argument(
        &mut self,
        function: TableFunction,
        name: &str,
        expr: ExprRef,
    ) -> Result<(&'static str, Value)> {
        let known = function
            .parameters()
            .iter()
            .find(|(parameter, _)| parameter.eq_ignore_ascii_case(name));
        let Some((parameter, wanted)) = known else {
            let candidates: Vec<String> = function
                .parameters()
                .iter()
                .map(|(parameter, ty)| format!("    {parameter} {ty}"))
                .collect();
            return Err(Error::binder(format!(
                "Invalid named parameter \"{name}\" for function {}\nCandidates:\n{}\n",
                function.name(),
                candidates.join("\n")
            )));
        };
        let Expr::Constant(reference) = *self.plan.expr(expr) else {
            return Err(Error::not_implemented(format!(
                "the named parameter {parameter} with a value that is not a constant"
            )));
        };
        let value = self.plan.value(reference).clone();
        if value == Value::Null {
            return Err(Error::binder(null_parameter(function, parameter)));
        }
        let given = self.plan.expr_type(expr).clone();
        if given != *wanted {
            return Err(Error::not_implemented(format!(
                "the named parameter {parameter} given a {given} where a {wanted} was wanted"
            )));
        }
        Ok((parameter, value))
    }

    /// A file where a table name goes, which is what DuckDB calls a replacement scan.
    ///
    /// `SELECT * FROM 'hits.parquet'` is how most DuckDB queries in the wild are written, ClickBench
    /// among them, so this is not sugar over `read_parquet` so much as the spelling people use. The
    /// catalog has already been asked and has already said no, and `missing` is what it said, so a
    /// name that is not a file comes back with the catalog's own answer rather than with a complaint
    /// about files.
    ///
    /// Only a single unqualified name is a candidate. A qualified one names a schema and a schema
    /// that does not exist is not a path.
    fn bind_replacement_scan(
        &mut self,
        ast: &Ast,
        parts: &[&str],
        alias: ast::StrRef,
        columns: ast::Slice,
        missing: Error,
    ) -> Result<(NodeRef, Scope)> {
        let [path] = parts else { return Err(missing) };
        let path = *path;
        let extension = path.rsplit_once('.').map(|(_, after)| after).unwrap_or_default();
        let Some(function) = Self::reader_for(extension) else {
            if is_file(path) {
                // A file that is really there and that nothing here can read is a different mistake
                // from a name that is not a file, and DuckDB says so with both lines, the second of
                // which is the way out. A file with no dot in it lands here too, which is why the
                // test is on the extension having a reader rather than on there being an extension.
                return Err(Error::binder(format!(
                    "No extension found that is capable of reading the file \"{path}\"\n* If this \
                     file is a supported file format you can explicitly use the reader functions, \
                     such as read_csv, read_json or read_parquet"
                )));
            }
            return Err(missing);
        };
        // The pattern is expanded before it is known to match anything, so a name that ends in .csv
        // and is not there gives the reader's own message rather than the catalog's. That is
        // DuckDB's order and it is the helpful one: somebody who wrote a file name wants to hear
        // about the file.
        let paths = files(path)?;
        let read = match function {
            TableFunction::ReadParquet => {
                let footers = parquet_footers(&paths)?;
                Read {
                    fields: footers.fields,
                    rows: footers.rows,
                    distincts: footers.distincts,
                    zones: footers.zones,
                }
            }
            _ => Read::uncounted(csv_fields(&paths, Given::default())?),
        };
        // The name the columns answer to is the file's stem, so `SELECT mixed.a FROM
        // 'data/mixed.parquet'` works. That is DuckDB's choice and it is the useful one, since the
        // alternative is a table name with a dot and a slash in it that nothing can write. A pattern
        // keeps the whole of what was written instead, which is DuckDB's choice too and was
        // measured: there is no stem to take when the name stands for a directory full of files.
        let label = if alias == NONE {
            if is_pattern(path) {
                path.to_string()
            } else {
                let file = path.rsplit_once('/').map_or(path, |(_, file)| file);
                file.rsplit_once('.').map_or(file, |(stem, _)| stem).to_string()
            }
        } else {
            ast.string(alias).to_string()
        };
        let arguments: Vec<ExprRef> = paths.iter().map(|path| self.path_constant(path)).collect();
        let names: Vec<&str> = ast.name(columns).collect();
        self.table_function_source(function, &arguments, &[], read, &label, &names)
    }

    /// One file name, as a constant expression in the plan.
    fn path_constant(&mut self, path: &str) -> ExprRef {
        let value = self.plan.add_value(Value::Varchar(path.to_string()));
        self.plan.add_expr(Expr::Constant(value), LogicalType::Varchar)
    }

    /// The table function a file with this extension is read by, and `None` for one nothing reads.
    ///
    /// Both spellings of a tab separated file go to the CSV reader, which is not a shortcut: the
    /// extension picks the reader and the reader sniffs the punctuation, so a `.tsv` file that holds
    /// commas is read as commas. That was measured rather than assumed. The comparison ignores case
    /// because `UP.CSV` reads in duckdb v1.4.1.
    fn reader_for(extension: &str) -> Option<TableFunction> {
        if extension.eq_ignore_ascii_case("parquet") {
            return Some(TableFunction::ReadParquet);
        }
        if extension.eq_ignore_ascii_case("csv") || extension.eq_ignore_ascii_case("tsv") {
            return Some(TableFunction::ReadCsv);
        }
        None
    }

    /// The node and the scope of a table function call whose arguments and columns are settled.
    ///
    /// The half a written out call shares with a replacement scan, which is everything after the
    /// question of what the file is called has been answered one way or the other.
    ///
    /// `read` is what the caller found out about the files, which comes in here rather than being
    /// read here because this function has the names and not the files: a replacement scan has
    /// already expanded its pattern and a written out call has already cast its argument, and
    /// neither of them wants to do it twice.
    fn table_function_source(
        &mut self,
        function: TableFunction,
        args: &[ExprRef],
        written: &[(&'static str, Value, ExprRef)],
        read: Read,
        label: &str,
        names: &[&str],
    ) -> Result<(NodeRef, Scope)> {
        let Read { fields, rows, distincts, zones } = read;
        let index = self.fresh_index();
        // Against the table index rather than against the node, because a pass is free to move the
        // node and none of them can move an index: an index is what a column reference names and
        // rewriting one would mean rewriting every expression above it. Nothing is recorded for a
        // function nobody measured, since an absent entry already reads back as unknown.
        if rows.is_known() {
            self.plan.measure(index, rows);
        }
        for (column, distinct) in distincts {
            self.plan.measure_distinct(index, &column, distinct);
        }
        if let Some(zones) = zones {
            self.plan.set_zones(index, zones);
        }
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: label.to_string(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
                // A reader takes what the file has, and no file format this reads says a column
                // cannot be null. The reference binary answers YES for every column of a Parquet.
                not_null: false,
            });
        }
        if !names.is_empty() {
            scope.rename(names, label)?;
        }
        let function = self.plan.intern(function.name());
        let args = self.plan.add_expr_list(args);
        let named: Vec<u32> =
            written.iter().map(|(parameter, _, _)| self.plan.intern(parameter)).collect();
        let settings: Vec<ExprRef> = written.iter().map(|(_, _, expr)| *expr).collect();
        let options = self.plan.add_name_list(&named);
        let settings = self.plan.add_expr_list(&settings);
        let columns = self.plan.add_fields(&fields);
        let node = self.add_node(Node::TableFunction {
            index,
            function,
            args,
            options,
            settings,
            columns,
        });
        Ok((node, scope))
    }

    /// Every file a table function's file argument names, in the order they were written.
    ///
    /// Each pattern has to find at least one file of its own, which is DuckDB's rule and is why
    /// this expands one at a time rather than gathering everything and looking at the total. A
    /// list keeps its written order and its duplicates, so a file named twice is read twice, which
    /// was measured: the sort and the dedup belong to one pattern rather than to the list.
    fn file_paths(&self, expr: ExprRef, name: &str) -> Result<Vec<String>> {
        let mut paths = Vec::new();
        for pattern in self.file_patterns(expr, name)? {
            paths.extend(files(&pattern)?);
        }
        Ok(paths)
    }

    /// The patterns a table function argument names, which have to be constants.
    ///
    /// A table function that reads a file is resolved by opening the file, and that happens here
    /// rather than when the query runs, because the rest of the statement cannot bind until the
    /// column names are known. So the path has to be something this binder can work out without
    /// running anything, and a literal is that. DuckDB folds a constant expression first, so
    /// `read_parquet('a' || '.parquet')` works there, and folding is M1 work that this will pick up
    /// for free once the optimizer runs before the plan is finished rather than after.
    ///
    /// One string is one pattern and a list is one pattern an item, which is DuckDB's pair of
    /// overloads. A null is a different sentence in each of them, both of them measured.
    fn file_patterns(&self, expr: ExprRef, name: &str) -> Result<Vec<String>> {
        let Expr::Constant(reference) = *self.plan.expr(expr) else {
            return Err(Error::not_implemented(
                "a table function file name that is not a constant",
            ));
        };
        match self.plan.value(reference) {
            Value::Varchar(path) => Ok(vec![path.clone()]),
            // DuckDB's own wording, which says list because its other overload takes one.
            Value::Null => Err(Error::parser(format!("{name} cannot take NULL list as parameter"))),
            Value::List { values, .. } => values
                .iter()
                .map(|value| match value {
                    Value::Varchar(path) => Ok(path.clone()),
                    _ => Err(Error::parser(format!(
                        "{name} reader cannot take NULL input as parameter"
                    ))),
                })
                .collect(),
            other => {
                Err(Error::internal(format!("a file name bound as VARCHAR arrived as {other}")))
            }
        }
    }

    /// Which input of a join a query written in its `ON` has to be joined into.
    ///
    /// A join condition is evaluated by the join, over the rows its two inputs handed it, so a
    /// column the condition reads has to be produced by one of those two. A query written in the
    /// `ON` produces columns the condition reads, which means the query cannot be joined in above
    /// the join the way one written in a `WHERE` or a `SELECT` is. It has to go underneath, into
    /// one input or the other.
    ///
    /// Which input is decided by what the query reads. A query whose body reads the right side can
    /// only be evaluated where those rows are, so it goes into the right input, and the same for
    /// the left. A query that reads neither could go into either and goes into the left, which is
    /// also where an `IN` puts one whose left hand side reads the left and whose body reads
    /// nothing.
    ///
    /// The one that has no answer is a query that reads both sides. There is no single input that
    /// produces what it needs, and the shape upstream calls a pair dependent join is what handles
    /// it. `None` is that case, and the caller turns it into a refusal rather than a plan.
    fn side_of(
        &self,
        pending: &PendingSubquery,
        left_tables: &[u32],
        right_tables: &[u32],
    ) -> Option<Side> {
        let mut needs_left = false;
        let mut needs_right = false;
        let mut note = |binding: ColumnBinding| {
            needs_left |= left_tables.contains(&binding.table);
            needs_right |= right_tables.contains(&binding.table);
        };
        for &binding in &pending.reads {
            note(binding);
        }
        // A mark join carries the comparison rather than the condition carrying it, and that
        // comparison is written over the join's own rows. `l.a IN (SELECT ...)` reads the left side
        // there and nowhere else, so leaving it out would put the query on whichever side its body
        // happened to name and let the comparison ask a join for a column it was not given.
        for &condition in &pending.conditions {
            self.plan.read_columns(condition, &mut |_, binding| note(binding));
        }
        match (needs_left, needs_right) {
            (true, true) => None,
            (_, true) => Some(Side::Right),
            _ => Some(Side::Left),
        }
    }

    /// A join whose condition holds a query that reads rows from both of its inputs.
    ///
    /// This is the one [`Binder::side_of`] has no side for. The query has to be evaluated once per
    /// pair of rows, and there is no input that produces a pair, so it cannot go into either input
    /// the way the other two cases do. What produces a pair is the join itself, so the join becomes
    /// a product, the query is joined into the product's rows the way a query in a `WHERE` is joined
    /// into the rows the whole `FROM` produced, and the condition becomes a filter above that.
    ///
    /// That rewrite is only the same query for an inner join. An inner join keeps the pairs its
    /// condition holds and drops the rest, which is what a product and a filter do. Every other kind
    /// does something with the pairs it dropped, a left join pads them, a semi join counts them, and
    /// a filter above a product has already thrown away which left row a dropped pair came from, so
    /// those are refused by name. Upstream plans them as a pair dependent join and rudb does not
    /// have one yet, which is what tamnd/rudb#913 stays open for.
    ///
    /// The product is not the plan that runs. The condition goes back into the join as a condition
    /// when filter pushdown looks at it, which is the pass that already turns a filter over an inner
    /// join into a join condition, so an equality in the `ON` is still an equality the hash join can
    /// build on. What cannot be pushed back down is the part that reads the query's output, and that
    /// part could not have been a join condition in the first place.
    #[allow(clippy::too_many_arguments)]
    fn bind_pair_dependent_join(
        &mut self,
        kind: ast::JoinKind,
        independent: bool,
        left: NodeRef,
        right: NodeRef,
        pair: Vec<PendingSubquery>,
        conditions: Vec<ExprRef>,
        scope: Scope,
    ) -> Result<(NodeRef, Scope)> {
        if kind != ast::JoinKind::Inner {
            return Err(Error::not_implemented(
                "a subquery that reads both sides of that join, written in the condition of a join \
                 that is not an inner join"
                    .to_string(),
            ));
        }
        // A lateral right side is already evaluated per left row, so the product this would build is
        // not the product the query means.
        if !independent {
            return Err(Error::not_implemented(
                "a subquery that reads both sides of that join, written in the condition of a join \
                 whose right side is lateral"
                    .to_string(),
            ));
        }
        let mut node = self.add_node(Node::CrossProduct { left, right });
        for pending in pair {
            node = self.attach_subquery(node, pending);
        }
        // `ON` and `USING` cannot both be written, and this is only reached from the `ON` path, so
        // the list is the one bound condition. The fold is here so that it stays right if that stops
        // being true rather than for a case that exists today.
        let mut conditions = conditions.into_iter();
        let mut predicate = conditions.next().expect("a join condition was bound");
        for next in conditions {
            let children = self.plan.add_expr_list(&[predicate, next]);
            let conjunction = Expr::Conjunction { op: ConjunctionOp::And, children };
            predicate = self.plan.add_expr(conjunction, LogicalType::Boolean);
        }
        let node = self.add_node(Node::Filter { input: node, predicate });
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
        let (right_node, right_scope, correlated) = self.bind_lateral(ast, right, &left_scope)?;
        // A row of the right side exists only for the left row it was evaluated against, so a kind
        // that has to produce right rows with no left row has nothing to produce them from. The
        // pinned build says this and names only the two kinds that work.
        if !correlated.is_empty()
            && !matches!(kind, ast::JoinKind::Inner | ast::JoinKind::Cross | ast::JoinKind::Left)
        {
            return Err(Error::binder(
                "The combining JOIN type must be INNER or LEFT for a LATERAL reference",
            ));
        }
        let split = left_scope.len();
        // Which table index came from which side, kept before the two scopes become one. A query
        // written in the `ON` is joined into one of the inputs rather than above the join, and this
        // is what says which. A `USING` drops the right side's copy of a joined-on column out of
        // the scope below, and dropping a column does not change the index it came from, so the
        // answer this gives is still right afterwards.
        let left_tables: Vec<u32> =
            left_scope.columns.iter().map(|column| column.binding.table).collect();
        let right_tables: Vec<u32> =
            right_scope.columns.iter().map(|column| column.binding.table).collect();
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
            // A name written twice is one column, not two. `USING (id, id)` is legal and means what
            // `USING (id)` means, and the reference binary agrees. Taking it twice would build the
            // same equality twice and, worse, drop the right side's copy twice, which takes a
            // column out of the answer that nobody named and runs off the end of the scope when the
            // copy was the last column in it.
            let mut names: Vec<String> = Vec::new();
            for name in ast.name(using) {
                if !names.iter().any(|held| same_name(held, name)) {
                    names.push(name.to_string());
                }
            }
            names
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

        let mut left_node = left_node;
        let mut right_node = right_node;
        let mut pair = Vec::new();
        if on != NONE {
            if !merged.is_empty() {
                return Err(Error::binder("a join cannot have both ON and USING"));
            }
            self.clause = "JOIN condition";
            let waiting = self.scalar_subqueries.len();
            let predicate = self.bind_expr(ast, on, &scope)?;
            conditions.push(self.as_boolean(predicate, "JOIN")?);
            for pending in self.scalar_subqueries.split_off(waiting) {
                match self.side_of(&pending, &left_tables, &right_tables) {
                    Some(Side::Right) => right_node = self.attach_subquery(right_node, pending),
                    Some(Side::Left) => left_node = self.attach_subquery(left_node, pending),
                    None => pair.push(pending),
                }
            }
        }

        if kind == ast::JoinKind::Cross && !conditions.is_empty() {
            return Err(Error::binder("a CROSS JOIN cannot have a condition"));
        }
        if !pair.is_empty() {
            return self.bind_pair_dependent_join(
                kind,
                correlated.is_empty(),
                left_node,
                right_node,
                pair,
                conditions,
                scope,
            );
        }
        // A product is the join with nothing to join on, and it is not one when the right side has
        // to be evaluated per left row, because then there is a dependency to lower even though
        // there is no condition to test.
        if correlated.is_empty()
            && conditions.is_empty()
            && matches!(kind, ast::JoinKind::Cross | ast::JoinKind::Inner)
        {
            let node = self.add_node(Node::CrossProduct { left: left_node, right: right_node });
            return Ok((node, scope));
        }
        // A semi join and an anti join ask a question about the right side rather than producing
        // any of it, so what is in scope after one is the left side alone. The condition is bound
        // above and is the last thing that can name the right side. Without this, `SELECT *` over
        // one expanded to both sides and the projection asked a join whose output is the left side
        // for columns it does not have, which came out as an internal error about a column not
        // being in the schema. That is tamnd/rudb#847. The reference binary refuses `b.w` here with
        // a binder error naming `a` as the only candidate table, which is the same rule said from
        // the other end.
        if matches!(kind, ast::JoinKind::Semi | ast::JoinKind::Anti) {
            scope.truncate(split);
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
        let node = if correlated.is_empty() {
            self.add_node(Node::Join {
                left: left_node,
                right: right_node,
                kind,
                conditions,
                build: BuildSide::default(),
            })
        } else {
            self.add_node(Node::DependentJoin {
                left: left_node,
                right: right_node,
                kind,
                conditions,
            })
        };
        Ok((node, scope))
    }

    // -------------------------------------------------------------- aggregates

    /// Binds a `FILTER (WHERE ...)` predicate, or says there was none.
    ///
    /// The predicate is a condition over the input rows and not over the answer, so it is bound in
    /// the scope the arguments are bound in, and it is cast to `BOOLEAN` the way a `WHERE` is:
    /// `FILTER (WHERE i)` over an integer column is a filter on whether the integer is not zero.
    fn bind_filter(
        &mut self,
        ast: &Ast,
        filter: ast::ExprRef,
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        if filter == NONE {
            return Ok(None);
        }
        let bound = self.bind_expr(ast, filter, scope)?;
        Ok(Some(self.checked_cast_to(bound, &LogicalType::Boolean, false)?))
    }

    /// Binds an aggregate call, records it, and hands back a reference to where its result lands.
    pub(crate) fn bind_aggregate(
        &mut self,
        ast: &Ast,
        name: &str,
        args: &[ast::ExprRef],
        distinct: bool,
        filter: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        if self.in_filter {
            return Err(Error::binder("aggregate functions are not allowed in FILTER"));
        }
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
        // The predicate goes first, which is the order the messages come out in upstream: a call
        // whose argument and whose filter both name columns that are not there is refused over the
        // filter. It is bound as if it were inside the call, so an aggregate in it is caught, and a
        // window in it is refused with the words a window inside an aggregate is refused with.
        self.in_aggregate = true;
        self.in_filter = true;
        let filter = self.bind_filter(ast, filter, scope);
        self.in_filter = false;
        self.in_aggregate = false;
        let filter = filter?;

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
            cast.push(self.checked_cast_to(*arg, wanted, false)?);
        }
        let args = self.plan.add_expr_list(&cast);
        let name = self.plan.intern(resolved.name);
        let ty = resolved.returns;
        let call = self.plan.add_expr(Expr::Aggregate { name, args, distinct, filter }, ty.clone());

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

    // ----------------------------------------------------------------- windows

    /// Binds a window call, files it under the run it belongs to, and hands back its column.
    ///
    /// The result is a column of a [`Node::Window`] rather than the call itself, for the reason the
    /// aggregate path returns a column too: the operator produces the value and everything above it
    /// reads the value, so a target that wraps a window in arithmetic is arithmetic over a column.
    pub(crate) fn bind_window(
        &mut self,
        ast: &Ast,
        written: &WindowCall<'_>,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let WindowCall { name, args, distinct, filter, ignore_nulls, spec } = *written;
        if self.in_aggregate {
            return Err(Error::binder(
                "aggregate function calls cannot contain window function calls",
            ));
        }
        if self.in_window {
            return Err(Error::binder("window function calls cannot be nested"));
        }
        // A join condition is part of the `WHERE` clause as far as this one sentence is concerned,
        // which is upstream's wording and not a simplification: `ON sum(a.i) OVER () = b.i` is
        // refused there with the words a window in a `WHERE` is refused with.
        let clause = if self.clause == "JOIN condition" { "WHERE clause" } else { self.clause };
        if clause != "SELECT clause" && clause != "ORDER BY clause" {
            return Err(Error::binder(format!("{clause} cannot contain window functions!")));
        }

        // `count(*)` is a different function from `count(x)` here for the reason it is a different
        // function in an ordinary call: one counts rows and the other counts the rows where its
        // argument is not null. A star is not an expression and nothing below this binds one.
        let starred = args.iter().any(|&arg| {
            matches!(ast.expr(arg), ast::Expr::Star { qualifier, replacements }
                if qualifier.is_empty() && replacements.is_empty())
        });
        let (name, args): (&str, &[ast::ExprRef]) = if starred {
            if !same_name(name, "count") || args.len() != 1 {
                return Err(Error::binder(format!("* is not allowed in {name}()")));
            }
            ("count_star", &[])
        } else if same_name(name, "count") && args.is_empty() {
            // `count()` with nothing in it is upstream's other spelling of `count(*)`. It counts
            // rows the same way and it is not an arity mistake.
            ("count_star", &[])
        } else {
            (name, args)
        };

        let held = ast.window(spec);
        self.in_window = true;
        let parts = self.window_parts(ast, args, held, scope);
        // The predicate goes last here, which is the other way round from an ordinary aggregate and
        // is again the order the messages come out in upstream. It is still inside the window, so a
        // window in it is a nested window, while an aggregate in it is an ordinary aggregate over
        // the same rows and is answered.
        let filter = if parts.is_ok() { self.bind_filter(ast, filter, scope) } else { Ok(None) };
        self.in_window = false;
        let parts = parts?;
        let filter = filter?;
        // Upstream's rule, in its words. A `RANGE` offset is a distance from the current row's sort
        // key, so there has to be exactly one sort key for it to be a distance from.
        let offsets = [parts.frame.start, parts.frame.end]
            .iter()
            .any(|end| matches!(end, WindowBound::Preceding(_) | WindowBound::Following(_)));
        if parts.frame.unit == WindowUnit::Range && offsets && parts.order.len() != 1 {
            return Err(Error::binder("RANGE frames must have only one ORDER BY expression"));
        }

        let types: Vec<LogicalType> =
            parts.args.iter().map(|&arg| self.plan.expr_type(arg).clone()).collect();
        let resolved = window_signature(name, &types)?;
        // `fill` reads the sort key rather than the frame, so what it needs from the query is not
        // what any other window needs and it is refused on its own terms.
        if resolved.name == "fill" {
            let keys: Vec<LogicalType> =
                parts.order.iter().map(|key| self.plan.expr_type(key.expr).clone()).collect();
            refuse_fill(&types[0], &keys, distinct, ignore_nulls)?;
        }
        // Upstream's sentence, doubled quotes and all. A DISTINCT over an aggregate inside an OVER
        // is ordinary and answered, and a DISTINCT over a ranking window is refused there, because
        // there is nothing for it to collapse when the call reads no values in the first place.
        if distinct && kind_of(resolved.name) == Some(FunctionKind::Window) {
            return Err(Error::binder(format!(
                "DISTINCT is not implemented for the window function \"\"{name}\"\""
            )));
        }
        // The same sentence for the same reason. A ranking window reads no values, so there is
        // nothing for a predicate over the values to keep or drop.
        if filter.is_some() && kind_of(resolved.name) == Some(FunctionKind::Window) {
            return Err(Error::binder(format!(
                "FILTER is not implemented for the window function \"\"{name}\"\""
            )));
        }
        let mut cast = Vec::with_capacity(parts.args.len());
        for (arg, wanted) in parts.args.iter().zip(&resolved.arguments) {
            cast.push(self.checked_cast_to(*arg, wanted, false)?);
        }
        let args = self.plan.add_expr_list(&cast);
        let name = self.plan.intern(resolved.name);
        let ty = resolved.returns;
        let call = self
            .plan
            .add_expr(Expr::Window { name, args, distinct, filter, ignore_nulls }, ty.clone());

        let at = self.window_run(parts.partition, parts.order, parts.frame, call);
        let index = self.windows.last().expect("the run was just filed").index;
        Ok(self.column(index, at, ty))
    }

    /// Files a call under the run that matches it, or opens a new run, and says which column it is.
    ///
    /// The run that matches is only ever the last one, because a query that goes back to an earlier
    /// partitioning after using a different one in between wants the operators in the order it wrote
    /// them. Merging the two would be a rewrite, and a rewrite over a window is the optimizer's to
    /// make once it knows what the sort below each one costs.
    fn window_run(
        &mut self,
        partition: Vec<ExprRef>,
        order: Vec<SortKey>,
        frame: WindowFrame,
        call: ExprRef,
    ) -> usize {
        let matches = self.windows.last().is_some_and(|run| {
            run.frame == frame
                && run.partition.len() == partition.len()
                && run.order.len() == order.len()
                && run.partition.iter().zip(&partition).all(|(&l, &r)| self.same_expr(l, r))
                && run.order.iter().zip(&order).all(|(l, r)| {
                    l.descending == r.descending
                        && l.nulls_first == r.nulls_first
                        && self.same_expr(l.expr, r.expr)
                })
        });
        if !matches {
            let index = self.fresh_index();
            self.windows.push(WindowRun { index, partition, order, frame, calls: Vec::new() });
        }
        // Two identical calls over one run are one column, the same way two identical aggregates
        // over one grouping are. `SELECT sum(i) OVER (), sum(i) OVER () + 1` totals once.
        let calls = self.windows.last().expect("a run is open").calls.clone();
        if let Some(at) = calls.iter().position(|&held| self.same_expr(held, call)) {
            return at;
        }
        let run = self.windows.last_mut().expect("a run is open");
        run.calls.push(call);
        run.calls.len() - 1
    }

    /// Binds the arguments and everything inside the `OVER`, with the aggregate rule applied.
    ///
    /// The aggregate rule applies to all of it, which is measured rather than assumed: over a
    /// grouped block `sum(count(i)) OVER ()` binds and `sum(i) OVER ()` is the ungrouped column
    /// complaint, and the same pair of answers comes back for a partition key and for an order key.
    fn window_parts(
        &mut self,
        ast: &Ast,
        args: &[ast::ExprRef],
        held: ast::WindowSpec,
        scope: &Scope,
    ) -> Result<WindowParts> {
        let mut bound = Vec::with_capacity(args.len());
        for &arg in args {
            let expr = self.bind_expr(ast, arg, scope)?;
            bound.push(self.over_aggregate(expr, scope)?);
        }
        let mut partition = Vec::new();
        for &key in ast.expr_list(held.partition) {
            let expr = self.bind_expr(ast, key, scope)?;
            partition.push(self.over_aggregate(expr, scope)?);
        }
        let mut order = Vec::new();
        for item in ast.order_list(held.order).to_vec() {
            let expr = self.bind_expr(ast, item.expr, scope)?;
            let expr = self.over_aggregate(expr, scope)?;
            order.push(self.sort_key(expr, item));
        }
        let frame = WindowFrame {
            unit: match held.unit {
                ast::WindowUnit::Rows => WindowUnit::Rows,
                ast::WindowUnit::Range => WindowUnit::Range,
                ast::WindowUnit::Groups => WindowUnit::Groups,
            },
            start: self.window_bound(ast, held.start, scope)?,
            end: self.window_bound(ast, held.end, scope)?,
            exclude: match held.exclude {
                ast::WindowExclude::NoOthers => WindowExclude::NoOthers,
                ast::WindowExclude::CurrentRow => WindowExclude::CurrentRow,
                ast::WindowExclude::Group => WindowExclude::Group,
                ast::WindowExclude::Ties => WindowExclude::Ties,
            },
        };
        Ok(WindowParts { args: bound, partition, order, frame })
    }

    /// One end of a frame, with its offset bound where it has one.
    fn window_bound(
        &mut self,
        ast: &Ast,
        bound: ast::WindowBound,
        scope: &Scope,
    ) -> Result<WindowBound> {
        let offset = |binder: &mut Self, written| {
            let expr = binder.bind_expr(ast, written, scope)?;
            binder.over_aggregate(expr, scope)
        };
        Ok(match bound {
            ast::WindowBound::UnboundedPreceding => WindowBound::UnboundedPreceding,
            ast::WindowBound::CurrentRow => WindowBound::CurrentRow,
            ast::WindowBound::UnboundedFollowing => WindowBound::UnboundedFollowing,
            ast::WindowBound::Preceding(written) => WindowBound::Preceding(offset(self, written)?),
            ast::WindowBound::Following(written) => WindowBound::Following(offset(self, written)?),
        })
    }

    /// Whether a column is the result of a query this block wrote and has not joined in yet.
    fn is_pending_subquery(&self, binding: ColumnBinding) -> bool {
        self.scalar_subqueries.iter().any(|pending| pending.index == binding.table)
    }

    /// Whether a column is the result of a window this block is building.
    fn is_window_output(&self, binding: ColumnBinding) -> bool {
        self.windows.iter().any(|run| run.index == binding.table)
    }

    /// Whether a column was resolved in an enclosing query rather than in this one.
    ///
    /// Every such read is written into the frame of the query being bound as it is resolved, and
    /// the frame is only handed up once that query's body is done, so while a select list or a
    /// `HAVING` is being bound the frame still holds everything this query read from outside it.
    fn is_correlation(&self, binding: ColumnBinding) -> bool {
        self.correlations.last().is_some_and(|frame| frame.contains(&binding))
    }

    /// The name a column is written under, for an error message to say which one it means.
    ///
    /// A column of an enclosing query is not in this query's scope, so the outer scopes are searched
    /// as well. Without that the message names no column at all, which is how `column a column must
    /// appear in the GROUP BY clause` came to be a sentence this engine printed.
    fn name_of(&self, binding: ColumnBinding, scope: &Scope) -> String {
        std::iter::once(scope)
            .chain(self.outer_scopes.iter().rev())
            .flat_map(|visible| visible.columns.iter())
            .find(|column| column.binding == binding)
            .map_or_else(|| "a column".to_string(), |column| format!("\"{}\"", column.name))
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
            // A window result is not a column of the input and the grouping rule has nothing to say
            // about it. It reads the aggregate's output rather than the table's, which is why
            // `SELECT sum(count(i)) OVER () FROM t GROUP BY j` binds and `sum(i) OVER ()` over the
            // same block does not.
            Expr::Column(binding) if self.is_window_output(binding) => Ok(expr),
            // The same argument for a query joined in above the grouping. `HAVING sum(x) > (SELECT
            // ...)` reads one row out of a query that has nothing to do with the groups, and the
            // join that produces it sits on top of the `Aggregate`, so what it produces is not one
            // of the grouped table's columns either.
            Expr::Column(binding) if self.joined_above.contains(&binding.table) => Ok(expr),
            // A column of an enclosing query is one value for the whole of this one, because this
            // query is evaluated once per outer row. It is a constant here in the sense the grouping
            // rule cares about, so it is allowed wherever a grouped column is and needs no group of
            // its own. The grouping rule is about columns of this query's own `FROM`, and a name
            // that resolved past it is not one of those. That is #995.
            Expr::Column(binding) if self.is_correlation(binding) => Ok(expr),
            // A query this block wrote that is still waiting to be joined in underneath the
            // grouping, which is a correlated one, since an uncorrelated one was lifted over the
            // grouping by [`Self::lift_over_aggregate`] and is not here. It has to stay underneath,
            // because what it correlates to is a column of the rows going into the aggregate, and
            // underneath is where the aggregate cannot carry its column upward. That is a thing
            // this engine cannot plan rather than a GROUP BY the query is missing, and it is worth
            // saying so, because the column belongs to no table anybody wrote and the sentence
            // below could not name it. That is #1032.
            Expr::Column(binding) if self.is_pending_subquery(binding) => Err(Error::binder(
                "a correlated subquery over a grouped query is not supported here yet",
            )),
            Expr::Column(binding) => {
                let name = self.name_of(binding, scope);
                Err(Error::binder(format!(
                    "column {name} must appear in the GROUP BY clause or must be part of an aggregate function"
                )))
            }
            Expr::Constant(_) | Expr::Aggregate { .. } | Expr::Window { .. } => Ok(expr),
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

/// The named parameters a table function call was written with.
///
/// A struct rather than the fields loose, because the seventeen DuckDB has on `read_parquet` and the
/// thirty on `read_csv` are all going to want somewhere to go, and because a call with none of them
/// written should read as the default of this rather than as a bare false somewhere.
///
/// The CSV half goes on to the reader and is opened with, here and again in the executor. The
/// Parquet half is answered here and nothing downstream sees it, which is what `binary_as_string`
/// turning a BLOB column into a VARCHAR one is.
#[derive(Debug, Default)]
struct Options {
    /// `binary_as_string`, which says an unannotated byte array column in a Parquet file holds
    /// text. The ClickBench file has twenty eight of those and every query reads them as strings.
    binary_as_string: bool,
    /// `all_varchar`, which reads every column of a CSV file as text rather than sniffing a type.
    all_varchar: bool,
    /// `file_row_number`, which adds a column holding each row's ordinal inside its own file.
    ///
    /// The one Parquet option here that the executor has to act on rather than the binder, since
    /// the column is not in the file and has to be counted as the rows come out of it.
    file_row_number: bool,
    /// `delim`, `sep`, `quote`, `escape` and `header`, which are what the sniffer would decide.
    given: Given,
}

impl Options {
    /// What these named parameters add up to.
    ///
    /// Each one was already checked against the function's list, so a name in here is a name that
    /// function takes and the value is already the type it wants. What is left is reading them, and
    /// the last one written wins, which is DuckDB's answer to `delim='|', delim=','` and was
    /// measured rather than assumed.
    fn of(written: &[(&'static str, Value, ExprRef)]) -> Result<Self> {
        let mut options = Self::default();
        for (parameter, value, _) in written {
            match (*parameter, value) {
                ("binary_as_string", Value::Boolean(on)) => options.binary_as_string = *on,
                ("all_varchar", Value::Boolean(on)) => options.all_varchar = *on,
                ("file_row_number", Value::Boolean(on)) => options.file_row_number = *on,
                _ => {}
            }
        }
        let named: Vec<(&str, Value)> =
            written.iter().map(|(parameter, value, _)| (*parameter, value.clone())).collect();
        options.given = csv_given(&named)?;
        Ok(options)
    }
}

/// What was written between the two sides of a set operation.
#[derive(Clone, Copy)]
struct Operator {
    /// `UNION`, `EXCEPT` or `INTERSECT`.
    op: SetOp,
    /// `ALL`, `DISTINCT`, or neither, which means `DISTINCT` everywhere it is allowed.
    quantifier: Quantifier,
    /// Whether `BY NAME` was written, which only `UNION` takes.
    by_name: bool,
}

/// One column of the result of a set operation, and where each side keeps it.
struct Merged {
    /// The name it comes out under, which is the left side's when both sides wrote it.
    name: String,
    /// What it is, after the two sides' types have met.
    ty: LogicalType,
    /// Which column of the left side it is, absent when only the right side wrote it.
    left: Option<usize>,
    /// Which column of the right side it is, absent when only the left side wrote it.
    right: Option<usize>,
}

/// Matches the two sides of an ordinary set operation, which is first column to first column.
///
/// The names are the left side's, so `SELECT a FROM t UNION SELECT b FROM u` comes out as `a`.
fn match_by_position(left: &Scope, right: &Scope) -> Result<Vec<Merged>> {
    if left.len() != right.len() {
        return Err(Error::binder(format!(
            "Set operations can only apply to expressions with the same number of result columns, but left side has {} and right side has {}",
            left.len(),
            right.len()
        )));
    }
    let mut merged = Vec::with_capacity(left.len());
    for (at, (held, other)) in left.columns.iter().zip(&right.columns).enumerate() {
        merged.push(Merged {
            name: held.name.clone(),
            ty: meet(&held.ty, &other.ty)?,
            left: Some(at),
            right: Some(at),
        });
    }
    Ok(merged)
}

/// Matches the two sides of a `UNION BY NAME`, which is by column name and not by position.
///
/// The result has the left side's columns in the order the left side wrote them, then the right
/// side's columns the left side did not write, in the order the right side wrote them. A column
/// only one side wrote is that side's type and the other side fills it with a null, which is why
/// nothing here needs the two sides to be the same width. Names match without regard to case, and
/// the spelling that comes out is the left side's, both of which follow the rest of the engine.
fn match_by_name(left: &Scope, right: &Scope) -> Result<Vec<Merged>> {
    named_once(left)?;
    named_once(right)?;
    let mut merged = Vec::with_capacity(left.len() + right.len());
    for (at, held) in left.columns.iter().enumerate() {
        let other = right.columns.iter().position(|column| same_name(&column.name, &held.name));
        let ty = match other {
            Some(other) => meet(&held.ty, &right.columns[other].ty)?,
            None => held.ty.clone(),
        };
        merged.push(Merged { name: held.name.clone(), ty, left: Some(at), right: other });
    }
    for (at, held) in right.columns.iter().enumerate() {
        if left.columns.iter().any(|column| same_name(&column.name, &held.name)) {
            continue;
        }
        merged.push(Merged {
            name: held.name.clone(),
            ty: held.ty.clone(),
            left: None,
            right: Some(at),
        });
    }
    Ok(merged)
}

/// Refuses a side of a `UNION BY NAME` that wrote one name twice.
///
/// Matching by name needs the name to say which column, and a side that wrote `a` twice has no
/// answer to give. An ordinary union does not care, because there the position says which column.
/// The doubled quotes around the name are the reference binary's and not a mistake here.
fn named_once(scope: &Scope) -> Result<()> {
    for (at, held) in scope.columns.iter().enumerate() {
        if scope.columns[..at].iter().any(|column| same_name(&column.name, &held.name)) {
            return Err(Error::binder(format!(
                "UNION (ALL) BY NAME operation doesn't support duplicate names in the SELECT list - the name \"\"{}\"\" occurs multiple times",
                held.name
            )));
        }
    }
    Ok(())
}

/// The one type a column of a set operation comes out as, given what each side wrote.
fn meet(left: &LogicalType, right: &LogicalType) -> Result<LogicalType> {
    left.promote(right).ok_or_else(|| {
        Error::binder(format!(
            "Cannot combine a column of type {left} with a column of type {right} in a set operation"
        ))
    })
}

/// DuckDB's complaint about a named parameter that was given a null, which is a different sentence
/// for almost every parameter.
///
/// Three of them were measured on `v2.0.0-dev84237` and no two agree: `binary_as_string` is the
/// first, `all_varchar` is the second and `header` is the third. They read like three people each
/// writing the message in front of them, which is what they are, and a harness that compares error
/// text compares all of it. Anything not measured gets the first one, which is the most general of
/// the three.
fn null_parameter(function: TableFunction, parameter: &str) -> String {
    match parameter {
        "header" => format!("\"{parameter}\" expects a non-null boolean value (e.g. TRUE or 1)"),
        "all_varchar" => format!("{} \"{parameter}\" cannot be NULL", function.name()),
        _ => format!("Cannot use NULL as argument to \"{parameter}\""),
    }
}

/// The complaint about a `REPLACE` entry that named a column the star did not stand for.
///
/// It reads like the complaint about any other name that is not there, down to the list of names
/// that are, because from the writer's side it is the same mistake.
fn missing_replacement(name: &str, input: &Scope) -> Error {
    Error::binder(format!(
        "Column \"{name}\" in REPLACE list not found in FROM clause{}",
        input.candidates()
    ))
}

/// Whether a type is one `fill` can interpolate over, which is the pin's phrase for it.
///
/// The pin refuses `fill` with `FILL argument must support subtraction` and its sort key with
/// `FILL ordering must support subtraction`, and the two lists are not the same list, which is why
/// this takes a flag rather than answering one question. Every number is on both, so are `DATE`,
/// `TIME` and the two timestamps, and `TIME WITH TIME ZONE` is a sort key there but not an
/// argument. `INTERVAL` is on neither, which is worth saying out loud because an interval does
/// subtract: the sentence names subtraction and the rule is narrower than the sentence.
fn subtractable(ty: &LogicalType, ordering: bool) -> bool {
    if ty.is_numeric() {
        return true;
    }
    match ty {
        LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampS
        | LogicalType::TimestampMs
        | LogicalType::TimestampNs
        | LogicalType::TimestampTz => true,
        LogicalType::TimeTz => ordering,
        _ => false,
    }
}

/// Refuses a `fill` call the way the pin refuses one, in the pin's order.
///
/// The order was measured and it is not the order the clauses are written in. A `fill` over a
/// `VARCHAR` with no `ORDER BY` at all complains about the argument, so the argument is looked at
/// before the sort key is counted, and a `fill` with `DISTINCT` and no `ORDER BY` complains about
/// the `ORDER BY`, so the count comes before the clauses. `IGNORE NULLS` is refused here rather
/// than being answered as a no-op, since there is nothing for it to skip: `fill` is the one window
/// whose whole job is the nulls.
fn refuse_fill(
    argument: &LogicalType,
    order: &[LogicalType],
    distinct: bool,
    ignore_nulls: bool,
) -> Result<()> {
    if !subtractable(argument, false) {
        return Err(Error::binder("FILL argument must support subtraction"));
    }
    let [key] = order else {
        return Err(Error::binder("FILL functions must have only one ORDER BY expression"));
    };
    if !subtractable(key, true) {
        return Err(Error::binder("FILL ordering must support subtraction"));
    }
    if distinct {
        return Err(Error::binder(
            "DISTINCT is not implemented for the window function \"\"fill\"\"",
        ));
    }
    if ignore_nulls {
        return Err(Error::binder(
            "RESPECT/IGNORE NULLS is not supported for the window function \"fill\"",
        ));
    }
    Ok(())
}

/// Resolves the call written inside an `OVER`.
///
/// Every aggregate is also a window, which is why this goes through the same signature table the
/// aggregate path uses, and the ranking windows go through it too because they are rows in the same
/// table. Everything else is one of three refusals, and all three are the reference binary's: a name
/// it knows as a scalar and a name it does not know at all each get their own sentence there.
fn window_signature(name: &str, types: &[LogicalType]) -> Result<Resolved> {
    match kind_of(name) {
        Some(FunctionKind::Aggregate | FunctionKind::Window) => resolve(name, types),
        Some(FunctionKind::Scalar) => {
            Err(Error::catalog(format!("{name} is not an aggregate function")))
        }
        None => Err(Error::catalog(format!("Aggregate Function with name {name} does not exist!"))),
    }
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
        // The partition, the order and the frame are not compared here and do not need to be. Two
        // window calls are only ever asked about when they are already in the same run, which is
        // what agreeing on all three means.
        (
            Expr::Window {
                name: left_name,
                args: left_args,
                distinct: left_distinct,
                filter: left_filter,
                ignore_nulls: left_nulls,
            },
            Expr::Window {
                name: right_name,
                args: right_args,
                distinct: right_distinct,
                filter: right_filter,
                ignore_nulls: right_nulls,
            },
        ) => {
            plan.string(*left_name) == plan.string(*right_name)
                && left_distinct == right_distinct
                && left_nulls == right_nulls
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

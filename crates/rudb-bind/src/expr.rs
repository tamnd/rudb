//! Turning a written expression into a bound one.
//!
//! Every method here takes a scope and produces an [`ExprRef`] into the plan under construction,
//! with a type already decided. Nothing downstream re-derives a type, which is the point: if the
//! binder says the sum of an `INTEGER` and a `BIGINT` is a `BIGINT` then that is what it is, and
//! the executor reads the answer rather than working it out again and possibly differently.
//!
//! Two rewrites happen here rather than later, because carrying two shapes for one meaning through
//! the optimizer costs a case in every pass that touches either. `BETWEEN` becomes a pair of
//! comparisons, `IN` becomes a disjunction of equalities, a simple `CASE` becomes a searched one,
//! and `IS NULL` becomes a null safe comparison against a null.

use rudb_common::{
    DeclaredType, Error, Field, LogicalType, MAX_DECIMAL_WIDTH, Result, Semantics, Session,
    SqlState, StateKey, Value, is_clustering_setting, looks_like_rule, rule_names,
};
use rudb_functions::{FunctionKind, kind_of, part_type, resolve};
use rudb_parse::ast::{self, BinaryOp, LiteralKind, UnaryOp};
use rudb_parse::{Ast, NONE};
use rudb_plan::{Arm, CompareOp, ConjunctionOp, Expr, ExprRef, Node, NodeRef, Plan};

use crate::binder::{AliasClause, Binder, PendingSubquery, WindowCall};
use crate::fold;
use crate::scope::Scope;

/// The pin's list macros over `list_concat`: the name, its parameters as the pin prints them, which
/// argument is the value to be wrapped in a list, and whether it goes in front of the list.
///
/// Read off `duckdb_functions()` on `v2.0.0-dev84237`. `array_push_front` is the odd one, because
/// it takes the list first and still puts the value in front, so `array_push_front([1], 0)` is
/// `[0, 1]` there, and it is copied here as it is rather than put in the order its name suggests.
const LIST_MACROS: &[(&str, &str, usize, bool)] = &[
    ("list_append", "l, e", 1, false),
    ("array_append", "arr, el", 1, false),
    ("array_push_back", "arr, e", 1, false),
    ("list_prepend", "e, l", 0, true),
    ("array_prepend", "el, arr", 0, true),
    ("array_push_front", "arr, e", 1, true),
];

impl Binder<'_> {
    /// Binds the value of a `SET`, which is an expression over nothing.
    ///
    /// A bare word is the text of its last name part rather than a column, the way the pin reads
    /// `SET schema = s1` and `SET disabled_optimizers = expression_rewriter`.
    pub(crate) fn bind_setting_value(&mut self, ast: &Ast, expr: ast::ExprRef) -> Result<ExprRef> {
        self.clause = "SET statement";
        if let ast::Expr::Column { name } = ast.expr(expr)
            && let Some(last) = ast.name(name).last()
        {
            return Ok(self.add_constant(Value::Varchar(last.to_string())));
        }
        self.bind_expr(ast, expr, &Scope::empty())
    }

    /// Binds the value of a `SET VARIABLE` as a query of one row with one column.
    ///
    /// A bare word is text here too, which is how the pin reads `SET VARIABLE a = x`. A query
    /// inside the value is joined in under the projection the way one in a `SELECT` with no `FROM`
    /// is, so a value that is a query that finds several rows is refused in the same words.
    pub(crate) fn bind_variable_value(&mut self, ast: &Ast, expr: ast::ExprRef) -> Result<NodeRef> {
        let value = self.bind_setting_value(ast, expr)?;
        let dummy = self.add_node(Node::Dummy);
        let input = self.attach_scalar_subqueries(dummy);
        let index = self.fresh_index();
        let exprs = self.plan_mut().add_expr_list(&[value]);
        let name = self.plan_mut().intern("value");
        let names = self.plan_mut().add_name_list(&[name]);
        Ok(self.add_node(Node::Project { input, index, exprs, names }))
    }

    /// Binds one written expression against `scope`.
    pub(crate) fn bind_expr(
        &mut self,
        ast: &Ast,
        expr: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let span = self.pinned_span.unwrap_or_else(|| ast.expr_span(expr));
        let outer = std::mem::replace(&mut self.current_span, span);
        let result =
            self.bind_expr_inner(ast, expr, scope).map_err(|error| error.with_fallback_span(span));
        self.current_span = outer;
        result
    }

    fn bind_expr_inner(&mut self, ast: &Ast, expr: ast::ExprRef, scope: &Scope) -> Result<ExprRef> {
        let written = ast.expr(expr);
        // A lambda's body runs once per element and not once per row, and a query inside it would
        // have to be joined in per element, which the pin does not do either. Its sentence.
        if self.in_lambda()
            && matches!(
                written,
                ast::Expr::Subquery { .. }
                    | ast::Expr::Exists { .. }
                    | ast::Expr::InSubquery { .. }
                    | ast::Expr::QuantifiedSubquery { .. }
            )
        {
            return Err(Error::binder("subqueries in lambda expressions are not supported"));
        }
        if self.trying && matches!(written, ast::Expr::Subquery { .. } | ast::Expr::Exists { .. }) {
            return Err(Error::binder("TRY can not be used in combination with a scalar subquery"));
        }
        match written {
            ast::Expr::Star { .. } if self.star_name.is_some() => {
                let name = self.star_name.clone().unwrap_or_default();
                Ok(self.add_constant(Value::Varchar(name)))
            }
            ast::Expr::Star { .. } => match self.star_names(ast, expr)? {
                Some(names) => Ok(names),
                None if self.folding => Err(Error::binder("STAR expression is not supported here")),
                None => Err(Error::binder(format!("* is not allowed in the {}", self.clause))),
            },
            ast::Expr::Columns { unpacked: true, .. } => {
                Err(Error::binder("*COLUMNS() can not be used in this place"))
            }
            ast::Expr::Columns { .. } => self.bind_picked(ast, scope),
            ast::Expr::Column { name } => self.bind_column(ast, name, scope),
            ast::Expr::Positional { index } => {
                let found = scope.positional(index).map_err(|total| {
                    Error::binder(format!(
                        "Positional reference {index} out of range (total {total} columns)"
                    ))
                })?;
                let (table, column) = (found.table.clone(), found.name.clone());
                if table.is_empty() {
                    self.bind_column_parts(Some(ast), &[&column], scope)
                } else {
                    self.bind_column_parts(Some(ast), &[&table, &column], scope)
                }
            }
            ast::Expr::Literal { kind, text } => self.bind_literal(ast, kind, text),
            ast::Expr::Unary { op, operand } => self.bind_unary(ast, op, operand, scope),
            ast::Expr::Binary { op, left, right } => self.bind_binary(ast, op, left, right, scope),
            // A set-returning function in the select list of a PostgreSQL session makes a row per
            // value. See `Binder::bind_series`.
            ast::Expr::Function { name, args, distinct: false, filter }
                if filter == NONE
                    && name.len == 1
                    && self.unnest_here
                    && !self.in_unnest
                    && !self.in_aggregate
                    && !self.in_window
                    && !self.in_lambda()
                    && self.session.postgres().is_some()
                    && rudb_catalog::same_name(
                        ast.name(name).last().unwrap_or_default(),
                        "generate_series",
                    ) =>
            {
                let args = ast.expr_list(args).to_vec();
                self.bind_series(ast, expr, &args, scope)
            }
            ast::Expr::Function { name, args, distinct, filter }
                if name.len == 1
                    && rudb_catalog::same_name(
                        ast.name(name).last().unwrap_or_default(),
                        "unnest",
                    ) =>
            {
                if distinct || filter != NONE {
                    return Err(Error::binder("UNNEST not supported here"));
                }
                let args = ast.expr_list(args).to_vec();
                self.bind_unnest(ast, expr, &args, scope)
            }
            ast::Expr::Function { name, args, distinct, filter }
                if name.len == 1
                    && !distinct
                    && filter == NONE
                    && ["make_type", "get_type"].iter().any(|called| {
                        rudb_catalog::same_name(ast.name(name).last().unwrap_or_default(), called)
                    }) =>
            {
                let written = ast.name(name).last().unwrap_or_default().to_string();
                match self.type_call(ast, expr, &written, args, scope)? {
                    Some(folded) => Ok(folded),
                    None => self.bind_call(ast, name, args, distinct, filter, &[], scope),
                }
            }
            ast::Expr::Function { name, args, distinct, filter }
                if name.len == 1
                    && !distinct
                    && filter == NONE
                    && rudb_catalog::same_name(
                        ast.name(name).last().unwrap_or_default(),
                        crate::structs::UNION_VALUE,
                    ) =>
            {
                let mut names = Vec::new();
                let mut bound = Vec::new();
                for &arg in ast.expr_list(args) {
                    names.push(String::new());
                    bound.push(self.bind_expr(ast, arg, scope)?);
                }
                for target in ast.named_args(expr).to_vec() {
                    names.push(ast.string(target.alias).to_string());
                    bound.push(self.bind_expr(ast, target.expr, scope)?);
                }
                if names.len() == 1 && names[0].is_empty() {
                    return Err(Error::binder(
                        "Need named argument for union tag, e.g. UNION_VALUE(a := b)",
                    ));
                }
                self.union_value(&names, &bound)
            }
            ast::Expr::Function { name, args, distinct, filter }
                if self.catalog().macros().next().is_some()
                    && self
                        .catalog()
                        .resolve_macro(&ast.name(name).collect::<Vec<_>>(), None)
                        .is_some() =>
            {
                let modified = distinct || filter != NONE || !ast.aggregate_order(expr).is_empty();
                let expanded = self.user_macro(ast, expr, name, args, modified, scope)?;
                expanded.ok_or_else(|| Error::internal("a macro that went away while it was bound"))
            }
            ast::Expr::Function { .. } if ast.misnamed(expr).is_some() => {
                let message = ast.misnamed(expr).map_or("", |(message, _)| message);
                Err(Error::binder(message))
            }
            ast::Expr::Function { name, args, .. } if !ast.named_args(expr).is_empty() => {
                let written = ast.name(name).last().unwrap_or_default().to_string();
                let sorted = ast.aggregate_order(expr);
                self.refuse_named(ast, expr, &written, ast.expr_list(args), sorted, scope)
            }
            ast::Expr::Window { name, args, distinct, filter, ignore_nulls, order, spec }
                if self.catalog().macros().next().is_some()
                    && self
                        .catalog()
                        .resolve_macro(&ast.name(name).collect::<Vec<_>>(), Some(false))
                        .is_some() =>
            {
                let written = ast.name(name).last().unwrap_or_default().to_string();
                let listed = ast.expr_list(args).to_vec();
                let call = WindowCall {
                    name: &written,
                    args: &listed,
                    distinct,
                    filter,
                    ignore_nulls,
                    order,
                    spec,
                };
                let expanded = self.user_window_macro(ast, expr, name, args, &call, scope)?;
                expanded.ok_or_else(|| Error::internal("a macro that went away while it was bound"))
            }
            ast::Expr::Window { name, args, .. } if !ast.named_args(expr).is_empty() => {
                let written = ast.name(name).last().unwrap_or_default().to_string();
                self.refuse_named(ast, expr, &written, ast.expr_list(args), &[], scope)
            }
            ast::Expr::Function { name, args, distinct, filter } => {
                let sorted = ast.aggregate_order(expr);
                // Only an aggregate reads this, and the pin lets any other call write it and
                // ignores it, apart from the name of the column.
                self.exporting = ast.exports_state(expr);
                let bound = self.bind_call(ast, name, args, distinct, filter, sorted, scope);
                self.exporting = false;
                match self.session.postgres() {
                    Some(_) => {
                        let written = ast.name(name).last().unwrap_or_default();
                        Ok(self.postgres_result(written, bound?))
                    }
                    None => bound,
                }
            }
            ast::Expr::Window { name, args, distinct, filter, ignore_nulls, order, spec } => {
                let written = ast.name(name).last().unwrap_or_default().to_string();
                let args = ast.expr_list(args).to_vec();
                let call = WindowCall {
                    name: &written,
                    args: &args,
                    distinct,
                    filter,
                    ignore_nulls,
                    order,
                    spec,
                };
                match self.builtin_window_macro(ast, &call, scope)? {
                    Some(expanded) => Ok(expanded),
                    None => self.bind_window(ast, &call, scope),
                }
            }
            ast::Expr::Cast { operand, ty, try_cast } => {
                let session = self.session;
                let written = ast.string(ty);
                let target = crate::statement::session_type(self.catalog(), session, written)?;
                let declared = session.postgres().and(rudb_pgtypes::declared_type(written));
                if !try_cast
                    && let Some(declared) = declared
                    && let Some(value) = self.read_literal(ast, operand, declared.oid)
                {
                    let cast = self.checked_cast_to(value?, &target, false)?;
                    return self.pg_length(cast, declared, true);
                }
                let input = self.bind_expr(ast, operand, scope)?;
                let cast = self.checked_cast_to(input, &target, try_cast)?;
                match declared {
                    Some(declared) => self.pg_length(cast, declared, true),
                    None => Ok(cast),
                }
            }
            ast::Expr::Case { operand, arms, otherwise } => {
                self.bind_case(ast, operand, arms, otherwise, scope)
            }
            ast::Expr::Between { operand, low, high, negated } => {
                self.bind_between(ast, operand, low, high, negated, scope)
            }
            ast::Expr::In { operand, list, negated } => {
                self.bind_in(ast, operand, list, negated, scope)
            }
            ast::Expr::InSubquery { operand, query, negated } => {
                self.bind_in_subquery(ast, operand, query, negated, scope)
            }
            ast::Expr::QuantifiedSubquery { operand, op, query, all } => {
                self.bind_quantified_subquery(ast, operand, op, query, all, scope)
            }
            ast::Expr::QuantifiedArray { operand, op, array, all } => {
                self.bind_quantified_array(ast, operand, op, array, all, scope)
            }
            // A row is a struct whose fields have no names, which the pin calls a TUPLE.
            ast::Expr::Row { items } => {
                let written = ast.expr_list(items).to_vec();
                let mut bound = Vec::with_capacity(written.len());
                for value in written {
                    bound.push(self.bind_expr(ast, value, scope)?);
                }
                self.pack_struct(&vec![String::new(); bound.len()], &bound)
            }
            // A lambda that got here is not the argument of a function that takes one, since that
            // function binds it itself. See `crate::lambda`.
            ast::Expr::Lambda { .. } => Err(Error::binder("invalid lambda expression")),
            ast::Expr::List { items } => self.bind_list(ast, items, scope),
            ast::Expr::Struct { names, values } => {
                let names: Vec<String> = ast.name(names).map(str::to_string).collect();
                // A field has to have a name, and the one way to write one without is `{'': 1}`.
                if names.iter().any(String::is_empty) {
                    return Err(Error::binder(
                        "Need named argument for struct pack, e.g. STRUCT_PACK(a := b)",
                    ));
                }
                let written = ast.expr_list(values).to_vec();
                let mut bound = Vec::with_capacity(written.len());
                for value in written {
                    bound.push(self.bind_expr(ast, value, scope)?);
                }
                self.pack_struct(&names, &bound)
            }
            ast::Expr::Parameter { name } => self.bind_parameter(ast, name),
            // A whole item of an insert's `VALUES` row is taken care of before it gets here, so a
            // `DEFAULT` that reaches this is one written anywhere else.
            ast::Expr::Default if self.default_as_null => Ok(self.add_constant(Value::Null)),
            ast::Expr::Default => Err(Error::binder("DEFAULT is not allowed here!")),
            ast::Expr::Subquery { query, array } => {
                self.bind_scalar_subquery(ast, query, array, scope)
            }
            ast::Expr::Exists { query, negated } => {
                self.bind_exists_subquery(ast, query, negated, scope)
            }
        }
    }

    /// Binds an uncorrelated scalar query and returns its one output as a column expression.
    ///
    /// `ARRAY(SELECT ...)` is the same subquery with its column gathered into one list, which is
    /// empty rather than null when the query has no rows. The pin writes it as `array_agg` ordered
    /// by the query's own `ORDER BY`, and so does this: a sort at the top of the query, with or
    /// without the projection that drops keys it did not select, becomes the order of the
    /// aggregate, because the rows reach an aggregate in no particular order once there are
    /// several chunks of them. A sort under a limit stays where it is, since it decides which rows
    /// there are.
    fn bind_scalar_subquery(
        &mut self,
        ast: &Ast,
        query: ast::QueryRef,
        array: bool,
        outer: &Scope,
    ) -> Result<ExprRef> {
        let (mut node, scope, correlations) = self.bind_isolated_subquery(ast, query, outer)?;
        let [column] = scope.columns.as_slice() else {
            return Err(Error::binder(format!(
                "Subquery returns {} columns - expected 1",
                scope.len()
            )));
        };
        let mut binding = column.binding;
        let mut ty = column.ty.clone();
        if array {
            let mut element = self.add_expr(Expr::Column(binding), ty.clone());
            let resolved = resolve("array_agg", std::slice::from_ref(&ty))?;
            let mut args = vec![element];
            let mut name = resolved.name.to_string();
            if let Some((input, first, keys)) = self.sorted_top(node) {
                node = input;
                element = first.unwrap_or(element);
                let keys = self.plan().sort_key_list(keys).to_vec();
                args = vec![element];
                args.extend(keys.iter().map(|key| key.expr));
                let flags: Vec<StateKey> = keys
                    .iter()
                    .zip(1..)
                    .map(|(key, column)| StateKey {
                        descending: key.descending,
                        nulls_first: key.nulls_first,
                        column,
                    })
                    .collect();
                name = rudb_kernels::ordered_name(&name, 1, &flags);
            }
            let args = self.plan_mut().add_expr_list(&args);
            let name = self.plan_mut().intern(&name);
            ty = resolved.returns;
            let gathered = self.add_expr(
                Expr::Aggregate { name, args, distinct: false, filter: None },
                ty.clone(),
            );
            let aggregates = self.plan_mut().add_expr_list(&[gathered]);
            let groups = self.plan_mut().add_expr_list(&[]);
            let index = self.fresh_index();
            node = self.add_node(Node::Aggregate { input: node, index, groups, aggregates });
            binding = rudb_plan::ColumnBinding::new(index, 0);
        }
        let expr = self.add_expr(Expr::Column(binding), ty.clone());
        self.scalar_subqueries.push(PendingSubquery {
            node,
            kind: rudb_plan::JoinKind::Single,
            conditions: Vec::new(),
            dependent: !correlations.is_empty(),
            reads: correlations,
            index: binding.table,
            inside_aggregate: self.in_aggregate,
        });
        if array {
            let LogicalType::List(element) = &ty else {
                return Err(Error::internal(format!("array_agg returning {ty}")));
            };
            let empty = Value::List { element: (**element).clone(), values: Vec::new() };
            let empty = self.add_constant(empty);
            return self.call("coalesce", vec![expr, empty]);
        }
        Ok(expr)
    }

    /// The input of the sort at the top of a query, the expression the query's one column is over
    /// that input when a projection sits on the sort, and the sort's keys.
    fn sorted_top(&self, node: NodeRef) -> Option<(NodeRef, Option<ExprRef>, rudb_plan::Slice)> {
        match *self.plan().node(node) {
            Node::Sort { input, keys } => Some((input, None, keys)),
            Node::Project { input, exprs, .. } => {
                let Node::Sort { input, keys } = *self.plan().node(input) else {
                    return None;
                };
                let &[first] = self.plan().expr_list(exprs) else {
                    return None;
                };
                Some((input, Some(first), keys))
            }
            _ => None,
        }
    }

    /// Binds an uncorrelated existence test as a nullable marker joined once into the outer rows.
    fn bind_exists_subquery(
        &mut self,
        ast: &Ast,
        query: ast::QueryRef,
        negated: bool,
        outer: &Scope,
    ) -> Result<ExprRef> {
        let (node, _, correlations) = self.bind_isolated_subquery(ast, query, outer)?;
        let node = self.add_node(Node::Limit {
            input: node,
            count: rudb_plan::Bound::Rows(1),
            offset: rudb_plan::Bound::Rows(0),
        });
        let index = self.fresh_index();
        let marker = self.add_constant(Value::Boolean(true));
        let exprs = self.plan_mut().add_expr_list(&[marker]);
        let name = self.plan_mut().intern("exists");
        let names = self.plan_mut().add_name_list(&[name]);
        let node = self.add_node(Node::Project { input: node, index, exprs, names });
        let marker = self
            .plan_mut()
            .add_expr(Expr::Column(rudb_plan::ColumnBinding::new(index, 0)), LogicalType::Boolean);
        self.scalar_subqueries.push(PendingSubquery {
            node,
            kind: rudb_plan::JoinKind::Single,
            conditions: Vec::new(),
            dependent: !correlations.is_empty(),
            reads: correlations,
            index,
            inside_aggregate: self.in_aggregate,
        });
        self.against_null(
            if negated { CompareOp::NotDistinctFrom } else { CompareOp::DistinctFrom },
            marker,
        )
    }

    /// Binds a query in its own aggregation and pending-subquery state.
    ///
    /// The columns it read from the query it sits in come back with it, and an uncorrelated query
    /// read none. The rest are somebody else's: a name that resolved past the enclosing query
    /// belongs to one further out still, so it is handed up to whichever frame is waiting for it
    /// rather than counted here. Without that the query two levels out looks uncorrelated, is
    /// planned as though it were, and the column it was supposed to feed down is asked for from an
    /// operator that was never given it. [`Self::bind_lateral`] splits the same way for the same
    /// reason.
    ///
    /// The query in between stays uncorrelated, which is right. It reads nothing of its own from
    /// the outer row, and the dependent join the outer query gets pushes the domain down through
    /// it to wherever the column is actually read.
    fn bind_isolated_subquery(
        &mut self,
        ast: &Ast,
        query: ast::QueryRef,
        outer_scope: &Scope,
    ) -> Result<(NodeRef, Scope, Vec<rudb_plan::ColumnBinding>)> {
        let outer_aggregation = self.aggregation.take();
        let outer_in_aggregate = std::mem::replace(&mut self.in_aggregate, false);
        let outer_in_filter = std::mem::replace(&mut self.in_filter, false);
        let outer_subqueries = std::mem::take(&mut self.scalar_subqueries);
        let outer_clause = std::mem::replace(&mut self.clause, "SELECT clause");

        self.outer_scopes.push(outer_scope.clone());
        self.correlations.push(Vec::new());
        let bound = self.bind_query(ast, query);
        let read = self.correlations.pop().expect("correlation frame");
        self.outer_scopes.pop();
        let mut correlations = Vec::new();
        for binding in read {
            if outer_scope.columns.iter().any(|column| column.binding == binding) {
                correlations.push(binding);
            } else if let Some(enclosing) = self.correlations.last_mut()
                && !enclosing.contains(&binding)
            {
                enclosing.push(binding);
            }
        }
        let nested_subqueries = std::mem::take(&mut self.scalar_subqueries);
        self.aggregation = outer_aggregation;
        self.in_aggregate = outer_in_aggregate;
        self.in_filter = outer_in_filter;
        self.scalar_subqueries = outer_subqueries;
        self.clause = outer_clause;

        let (node, scope) = bound?;
        debug_assert!(
            nested_subqueries.is_empty(),
            "a nested select left scalar queries unattached"
        );
        Ok((node, scope, correlations))
    }

    /// `?`, `?1`, `$1` or `$name`, which is the value the statement was prepared with.
    ///
    /// A constant, because the value is known by the time this runs. A statement is parsed once and
    /// bound once per set of values, so the plan a prepared statement runs is an ordinary plan and
    /// nothing after the binder knows a parameter was ever written. The cost of that is binding
    /// again for each execution, which is the front end and not the query, and the benefit is that
    /// the optimizer gets to fold and prune with the values in hand.
    fn bind_parameter(&mut self, ast: &Ast, name: ast::StrRef) -> Result<ExprRef> {
        let name = ast.string(name);
        if self.parameters.get(name).is_none()
            && let Some(placeholders) = self.parameters.placeholders()
            && let Some(declared) = placeholders.declared(name)
        {
            let null = self.add_constant(Value::Null);
            return Ok(match declared {
                Some(ty) => {
                    placeholders.resolve(name, &ty);
                    self.cast_to(null, &ty)
                }
                None => {
                    self.placeholders.push((null, name.to_string()));
                    null
                }
            });
        }
        let Some(value) = self.parameters.get(name) else {
            // A numbered parameter in a query that has no values for it is an undefined parameter
            // in PostgreSQL.
            if self.session.postgres().is_some() {
                let number = name.trim_start_matches('$');
                return Err(Error::binder(format!("there is no parameter ${number}"))
                    .state(SqlState::UNDEFINED_PARAMETER));
            }
            return Err(Error::invalid_input(
                "Prepared statement parameters cannot be used directly\nTo use prepared statement \
                 parameters, use PREPARE to prepare a statement, followed by EXECUTE",
            ));
        };
        let constant = self.add_constant(value.clone());
        // A null has no type, and in PostgreSQL it is a null of the type of its parameter.
        if value.is_null()
            && self.session.postgres().is_some()
            && let Some(ty) = self.parameters.declared_type(name)
            && let Some(ty) = rudb_pgtypes::logical_type(ty.oid)
        {
            return Ok(self.cast_to(constant, &ty));
        }
        Ok(constant)
    }

    fn bind_column(&mut self, ast: &Ast, name: ast::Slice, scope: &Scope) -> Result<ExprRef> {
        let parts: Vec<&str> = ast.name(name).collect();
        self.bind_column_parts(Some(ast), &parts, scope)
    }

    /// The alias of this block's select list that `parts` may name in the clause being bound.
    ///
    /// Only a bare name is one, since the pin reads `t.y` as a column of `t` whatever the select
    /// list calls things. Inside an aggregate's arguments only the select list reads aliases, which
    /// is the pin's: `SELECT x AS y, sum(y)` sums `x` and `HAVING sum(y)` is a missing column.
    fn alias_for(&self, parts: &[&str]) -> Option<(usize, ast::ExprRef, usize)> {
        let [word] = parts else { return None };
        let aliases = self.aliases.as_ref()?;
        if self.in_aggregate && aliases.clause != AliasClause::Select {
            return None;
        }
        aliases.find(word)
    }

    /// Whether `binding` is a column this block groups by as it is, with nothing around it.
    fn grouped(&self, binding: rudb_plan::ColumnBinding) -> bool {
        self.aggregation.as_ref().is_some_and(|aggregation| {
            aggregation.groups.iter().any(|&group| {
                matches!(self.plan().expr(group), Expr::Column(column) if *column == binding)
            })
        })
    }

    /// A column named by its written parts, or a field of a struct column when no column is.
    ///
    /// With the statement's tree in hand, a bare name may also be one of the block's aliases, in
    /// the order [`Self::alias_for`] and the clause being bound decide. Without it, which is the
    /// front of a struct path, it is never one, since the pin does not look into an alias's fields.
    fn bind_column_parts(
        &mut self,
        ast: Option<&Ast>,
        parts: &[&str],
        scope: &Scope,
    ) -> Result<ExprRef> {
        // A lambda parameter beats a column of the same name, so `lambda l: l + 1` over a table with
        // a column `l` reads the element. The innermost lambda that has the name is the one meant.
        if let [word] = parts
            && let Some(parameter) = self.lambda_parameter(word)
        {
            return Ok(parameter);
        }
        // A bare `current_date` is one of the ten session context keywords, and a column of that
        // name beats it. The scope is asked whether anything answers to the word before the fold
        // rather than the fold happening when resolution fails, so that two tables carrying the name
        // is still the ambiguity error. Both halves were measured against the pin. See
        // `crate::context`.
        let alias = ast.and_then(|ast| Some((ast, self.alias_for(parts)?)));
        if let [word] = parts
            && alias.is_none()
            && !scope.names(word)
            && !self.outer_scopes.iter().any(|outer| outer.names(word))
            && let Some(folded) = self.context_keyword(word)
        {
            return Ok(folded);
        }
        let clause = self.aliases.as_ref().map(|aliases| aliases.clause);
        if let Some(found) = scope.resolve_optional(parts)? {
            // In a `HAVING` an alias beats a column the block does not group by, so `SELECT sum(x)
            // AS x FROM t HAVING x > 6` compares the sum.
            if let Some((ast, alias)) = alias
                && clause == Some(AliasClause::Having)
                && !self.grouped(found.binding)
            {
                return self.bind_alias(ast, parts[0], alias, scope);
            }
            return Ok(self.add_expr(Expr::Column(found.binding), found.ty.clone()));
        }
        if let Some((ast, alias)) = alias {
            return self.bind_alias(ast, parts[0], alias, scope);
        }
        let mut found = None;
        for (at, outer) in self.outer_scopes.iter().enumerate().rev() {
            if let Some(visible) = outer.resolve_optional(parts)? {
                found = Some((at, visible.binding, visible.ty.clone()));
                break;
            }
        }
        let Some((at, binding, ty)) = found else {
            if self.columns_scope.is_some() {
                return Err(Error::binder(format!(
                    "Failed to bind \"{}\" - COLUMNS expression can only contain lambda parameters",
                    parts.join(".")
                )));
            }
            if let Some(field) = self.struct_path(parts, scope)? {
                return Ok(field);
            }
            // The pin reads `read_csv(data)` as `read_csv('data')`, dots and all, and warns that
            // it will stop doing so. It is the deprecated behaviour, but it is still the answer.
            if self.identifiers_as_strings {
                return Ok(self.add_constant(Value::Varchar(parts.join("."))));
            }
            // The pin's own words for a name that is neither a column nor an alias in these two.
            if let ([word], Some(_)) = (parts, ast) {
                if clause == Some(AliasClause::Qualify) && !self.in_aggregate {
                    return Err(Error::binder(format!(
                        "Referenced column {word} not found in FROM clause and can't find in alias map."
                    )));
                }
                if clause == Some(AliasClause::Having) && !self.in_aggregate {
                    return Err(Error::binder(format!(
                        "column \"{word}\" must appear in the GROUP BY clause or be used in an aggregate function"
                    )));
                }
            }
            // With nothing in scope here or outside, the pin says the `FROM` clause is missing.
            if scope.len() == 0 && self.outer_scopes.iter().all(|outer| outer.len() == 0) {
                return Err(Error::binder(match parts {
                    [word] => format!(
                        "Referenced column \"{word}\" was not found because the FROM clause is \
                         missing"
                    ),
                    _ => format!(
                        "Referenced table \"{}\" not found!",
                        parts[..parts.len() - 1].join(".")
                    ),
                }));
            }
            return scope.resolve(parts).map(|_| unreachable!());
        };
        // A LATERAL entry may not aggregate over what its left neighbour gave it. There is one row
        // of the left per evaluation of the entry, so `sum(o.k)` would be a sum of one value and
        // whoever wrote it meant something else. The pinned build refuses it in these words and a
        // correlated column read anywhere else in the entry, including under its own aggregate's
        // filter or inside a window, is fine.
        if self.in_aggregate && self.lateral_scopes.contains(&at) {
            return Err(Error::binder("LATERAL join cannot contain aggregates!"));
        }
        if let Some(correlations) = self.correlations.last_mut()
            && !correlations.contains(&binding)
        {
            correlations.push(binding);
        }
        Ok(self.add_expr(Expr::Column(binding), ty))
    }

    /// A string literal read by the input function of the PostgreSQL type `oid`, as a PostgreSQL
    /// session reads it in a cast and in a `VALUES` row of an `INSERT`. `None` when `expr` is not
    /// a string literal, when the session is not a PostgreSQL session, or when the type has no
    /// input function here.
    pub(crate) fn read_literal(
        &mut self,
        ast: &Ast,
        expr: ast::ExprRef,
        oid: u32,
    ) -> Option<Result<ExprRef>> {
        let ast::Expr::Literal { kind: LiteralKind::String, text } = ast.expr(expr) else {
            return None;
        };
        let session = self.session;
        let input = session.postgres()?.input.as_ref()?;
        let value = input.read(oid, ast.string(text))?;
        let span = ast.expr_span(expr);
        Some(value.map(|value| self.add_constant(value)).map_err(|e| e.with_fallback_span(span)))
    }

    /// The value of a number literal. PostgreSQL reads a number with an exponent, an integer past
    /// `bigint` and a decimal past 38 digits as a `numeric`, where DuckDB reads a double, a
    /// `HUGEINT` or a `BIGNUM`.
    fn number(&self, text: &str, negative: bool) -> Result<Value> {
        let value = number(text, negative)?;
        if self.session.postgres().is_none()
            || !matches!(
                value,
                Value::Double(_) | Value::HugeInt(_) | Value::UHugeInt(_) | Value::BigNum(_)
            )
        {
            return Ok(value);
        }
        let bare: String = text.chars().filter(|c| *c != '_').collect();
        let sign = if negative { "-" } else { "" };
        match rudb_pgtypes::numeric_in(&format!("{sign}{bare}"), -1) {
            Ok(held) => Ok(Value::Numeric(held.to_bytes())),
            Err(_) => Ok(value),
        }
    }

    fn bind_literal(&mut self, ast: &Ast, kind: LiteralKind, text: ast::StrRef) -> Result<ExprRef> {
        let value = match kind {
            LiteralKind::Null => Value::Null,
            LiteralKind::True => Value::Boolean(true),
            LiteralKind::False => Value::Boolean(false),
            LiteralKind::String | LiteralKind::Blob => Value::Varchar(ast.string(text).to_string()),
            LiteralKind::Number => self.number(ast.string(text), false)?,
        };
        let constant = self.add_constant(value);
        // A blob literal is the text a blob prints as, so the cast that reads that text back is the
        // whole of the conversion, and the one that refuses `x'zz'` is the one that already refuses
        // `'\xzz'::BLOB`, in the same words. Per #329.
        if kind == LiteralKind::Blob {
            return Ok(self.cast_to(constant, &LogicalType::Blob));
        }
        Ok(constant)
    }

    /// `[a, b, c]`, which is a call to `list_value`.
    ///
    /// A bracket is that call on the pin too, which is why a query that writes one gets a column
    /// named `list_value(a, b, c)` back from both engines. Binding it as a call rather than folding
    /// it into a `Value` here is what lets a list hold a column: the items are arguments, they are
    /// cast to the element type by the same resolution every other call goes through, and the
    /// executor evaluates it per row like anything else. A list of constants still comes out as one
    /// value, because the folder answers a call whose arguments are all constants, which is what
    /// `read_parquet(['a', 'b'])` reads.
    ///
    /// The element type is what the items promote to and an empty list is `"NULL"[]`, both of which
    /// are the pin's answers and both of which are decided in `rudb-functions` rather than here.
    fn bind_list(&mut self, ast: &Ast, items: ast::Slice, scope: &Scope) -> Result<ExprRef> {
        let written = ast.expr_list(items).to_vec();
        let mut args = Vec::with_capacity(written.len());
        for item in written {
            args.push(self.bind_expr(ast, item, scope)?);
        }
        self.call("list_value", args)
    }

    /// `s.a` or `t.s.a.b` read as fields of a struct column or keys of a map or JSON column, once no
    /// column answers to the name.
    ///
    /// The longest front of the name that is a column wins, which is the pin's order: a table
    /// called `s` with a column `a` is read as that column before a struct column `s` is looked
    /// into. `None` when no front of the name is a struct column, so the caller says the name is
    /// missing in its own words.
    fn struct_path(&mut self, parts: &[&str], scope: &Scope) -> Result<Option<ExprRef>> {
        for split in (1..parts.len()).rev() {
            let Ok(mut expr) = self.bind_column_parts(None, &parts[..split], scope) else {
                continue;
            };
            if !matches!(
                self.plan().expr_type(expr),
                LogicalType::Struct(_)
                    | LogicalType::Union(_)
                    | LogicalType::Map(..)
                    | LogicalType::Json
                    | LogicalType::Variant
            ) {
                continue;
            }
            // A name after a map is a key, as `m['a']` would be, and a key missing from the map is
            // null. A name after a JSON value is a key of the object it holds, and a name after a
            // union is a member, null in a row holding another one.
            for field in &parts[split..] {
                let key = self.add_constant(Value::Varchar((*field).to_string()));
                let picked = if matches!(self.plan().expr_type(expr), LogicalType::Map(..)) {
                    self.map_call("map_extract_value", &[expr, key])?
                } else if matches!(self.plan().expr_type(expr), LogicalType::Union(_)) {
                    self.union_call("union_extract", &[expr, key])?
                } else if *self.plan().expr_type(expr) == LogicalType::Json {
                    self.json_field("struct_extract", &[expr, key])?
                } else if *self.plan().expr_type(expr) == LogicalType::Variant {
                    self.variant_field("struct_extract", &[expr, key])?
                } else {
                    self.struct_field("struct_extract", &[expr, key])?
                };
                let Some(picked) = picked else {
                    return Ok(None);
                };
                expr = picked;
            }
            return Ok(Some(expr));
        }
        Ok(None)
    }

    fn bind_unary(
        &mut self,
        ast: &Ast,
        op: UnaryOp,
        operand: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        // A negated number literal is one constant rather than a call, so that -2147483648 is an
        // INTEGER the way it is written and not a negation of a BIGINT.
        if op == UnaryOp::Negate
            && let ast::Expr::Literal { kind: LiteralKind::Number, text } = ast.expr(operand)
        {
            let value = self.number(ast.string(text), true)?;
            return Ok(self.add_constant(value));
        }
        let bound = self.bind_expr(ast, operand, scope)?;
        match op {
            UnaryOp::Not => {
                let condition = self.as_boolean(bound, "NOT")?;
                self.call("not", vec![condition])
            }
            UnaryOp::Negate => self.call("-", vec![bound]),
            UnaryOp::Plus => self.call("+", vec![bound]),
            UnaryOp::IsNull => self.against_null(CompareOp::NotDistinctFrom, bound),
            UnaryOp::IsNotNull => self.against_null(CompareOp::DistinctFrom, bound),
            UnaryOp::IsTrue => self.against_boolean(CompareOp::NotDistinctFrom, bound, true),
            UnaryOp::IsNotTrue => self.against_boolean(CompareOp::DistinctFrom, bound, true),
            UnaryOp::IsFalse => self.against_boolean(CompareOp::NotDistinctFrom, bound, false),
            UnaryOp::IsNotFalse => self.against_boolean(CompareOp::DistinctFrom, bound, false),
            UnaryOp::IsUnknown => {
                let condition = self.as_boolean(bound, "IS UNKNOWN")?;
                self.against_null(CompareOp::NotDistinctFrom, condition)
            }
            UnaryOp::IsNotUnknown => {
                let condition = self.as_boolean(bound, "IS UNKNOWN")?;
                self.against_null(CompareOp::DistinctFrom, condition)
            }
            UnaryOp::BitNot => self.call("~", vec![bound]),
            UnaryOp::Factorial => self.call("factorial", vec![bound]),
        }
    }

    fn bind_binary(
        &mut self,
        ast: &Ast,
        op: BinaryOp,
        left: ast::ExprRef,
        right: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let op = if op == BinaryOp::Divide && self.semantics.integer_division() {
            BinaryOp::IntegerDivide
        } else {
            op
        };
        if let BinaryOp::And | BinaryOp::Or = op {
            let connective =
                if op == BinaryOp::And { ConjunctionOp::And } else { ConjunctionOp::Or };
            let word = if op == BinaryOp::And { "AND" } else { "OR" };
            let left = self.bind_expr(ast, left, scope)?;
            let left = self.as_boolean(left, word)?;
            let right = self.bind_expr(ast, right, scope)?;
            let right = self.as_boolean(right, word)?;
            return Ok(self.conjunction(connective, vec![left, right]));
        }
        // The grammar reads `a -> b ->> c` as `a -> (b ->> c)`, since what follows an arrow is a
        // whole expression for the old lambdas. The pin puts it back as `(a -> b) ->> c` when the
        // arrow is not a lambda, which is how `doc -> 'a' ->> 0` reads the first element of `a`.
        if op == BinaryOp::Arrow
            && let ast::Expr::Binary { op: BinaryOp::LongArrow, left: inner, right: last } =
                ast.expr(right)
        {
            let picked = self.bind_binary(ast, BinaryOp::Arrow, left, inner, scope)?;
            let last = self.bind_expr(ast, last, scope)?;
            return self.call("->>", vec![picked, last]);
        }
        let written = [left, right];
        let mut left = self.bind_expr(ast, left, scope)?;
        let mut right = self.bind_expr(ast, right, scope)?;
        let mut op = op;
        if self.session.postgres().is_some() {
            self.unknown_operand(op, &mut left, &mut right);
            // A string literal joined to a `bytea` is a `bytea` too.
            if op == BinaryOp::Concat {
                let literal = |at: usize| {
                    matches!(
                        ast.expr(written[at]),
                        ast::Expr::Literal { kind: LiteralKind::String, .. }
                    )
                };
                let blob = |side: ExprRef| *self.plan().expr_type(side) == LogicalType::Blob;
                if literal(1) && blob(left) {
                    right = self.cast_to(right, &LogicalType::Blob);
                } else if literal(0) && blob(right) {
                    left = self.cast_to(left, &LogicalType::Blob);
                }
            }
            if let Some(done) = self.pg_datetime_operator(op, left, right)? {
                return Ok(done);
            }
            // `/` divides an interval in PostgreSQL. Only two integers make an integer division.
            if op == BinaryOp::IntegerDivide
                && *self.plan().expr_type(left) == LogicalType::Interval
            {
                op = BinaryOp::Divide;
            }
        }
        if let Some(comparison) = comparison_of(op) {
            if self.session.postgres().is_some() {
                self.bpchar_operands(ast, written, [&mut left, &mut right], scope)?;
            }
            return self.compare(comparison, left, right);
        }
        match op {
            BinaryOp::Regex => return self.regex_operator(left, right, false, false, false),
            BinaryOp::NotRegex => return self.regex_operator(left, right, false, true, false),
            BinaryOp::RegexInsensitive => {
                return self.regex_operator(left, right, true, false, false);
            }
            BinaryOp::NotRegexInsensitive => {
                return self.regex_operator(left, right, true, true, false);
            }
            BinaryOp::SimilarTo => return self.regex_operator(left, right, false, false, true),
            BinaryOp::NotSimilarTo => {
                return self.regex_operator(left, right, false, true, true);
            }
            // `^@` is `starts_with` under another name, and the pin refuses it over anything but
            // strings with the sentence a call gets, literals spelled as literals.
            // `x AT TIME ZONE z` is `timezone(z, x)`, the arguments swapped.
            BinaryOp::AtTimeZone => return self.call("timezone", vec![right, left]),
            BinaryOp::StartsWith => {
                let types = [left, right].map(|arg| self.plan().expr_type(arg).clone());
                return self
                    .call("^@", vec![left, right])
                    .map_err(|error| literals_spelled(ast, error, &written, &types));
            }
            // `doc -> path` is `json_extract` and `doc ->> path` is a function of its own name
            // that answers as `json_extract_string` does, which is how the pin's refusals name them.
            BinaryOp::Arrow | BinaryOp::LongArrow => {
                let name = if op == BinaryOp::Arrow { "json_extract" } else { "->>" };
                let types = [left, right].map(|arg| self.plan().expr_type(arg).clone());
                return self
                    .call(name, vec![left, right])
                    .map_err(|error| literals_spelled(ast, error, &written, &types));
            }
            _ => {}
        }
        let checked_slash = op == BinaryOp::Divide
            && !self.semantics.ieee_floating_point_ops()
            && self.plan().expr_type(left).is_numeric();
        let checked_remainder = op == BinaryOp::Modulo
            && !self.semantics.ieee_floating_point_ops()
            && (matches!(self.plan().expr_type(left), LogicalType::Float | LogicalType::Double)
                || matches!(
                    self.plan().expr_type(right),
                    LogicalType::Float | LogicalType::Double
                ));
        if self.semantics.null_on_division_by_zero()
            && (matches!(op, BinaryOp::IntegerDivide | BinaryOp::Modulo)
                || checked_slash
                || (op == BinaryOp::Divide
                    && self.plan().expr_type(left) == &LogicalType::Interval))
        {
            right = self.zero_to_null(right);
        }
        // A division in PostgreSQL over an exact number that is not an integer is a `numeric`
        // division, which picks its own scale, so `1.0 / 3` is `0.33333333333333333333`.
        if self.session.postgres().is_some()
            && matches!(op, BinaryOp::Divide | BinaryOp::IntegerDivide)
        {
            let types = [left, right].map(|arg| self.plan().expr_type(arg).clone());
            let exact = |ty: &LogicalType| {
                ty.is_integer()
                    || matches!(
                        ty,
                        LogicalType::Decimal { .. } | LogicalType::Numeric | LogicalType::Null
                    )
            };
            let fractional =
                |ty: &LogicalType| matches!(ty, LogicalType::Decimal { .. } | LogicalType::Numeric);
            if types.iter().all(exact) && types.iter().any(fractional) {
                let left = self.cast_to(left, &LogicalType::Numeric);
                let right = self.cast_to(right, &LogicalType::Numeric);
                return self.call("//", vec![left, right]);
            }
        }
        match function_of(op) {
            Some(name) if checked_slash && !self.semantics.null_on_division_by_zero() => {
                self.call_as(name, "__rudb_checked_slash", vec![left, right])
            }
            Some(name) if checked_remainder && !self.semantics.null_on_division_by_zero() => {
                self.call_as(name, "__rudb_checked_remainder", vec![left, right])
            }
            Some(name) => self.call(name, vec![left, right]),
            None => Err(Error::not_implemented(format!("the {} operator", spelling(ast, op)))),
        }
    }

    /// A `char(n)` column keeps its values with no trailing spaces, and PostgreSQL compares a
    /// `char(n)` value with no regard to its trailing spaces. So a string that is compared with
    /// such a column as a `bpchar` loses its trailing spaces too, and `c = 'ab  '` finds the row of
    /// `'ab'`. That is a string literal, a parameter, and a `varchar` or `char` column or cast. A
    /// `text` value, such as `c::text` or the result of a function, compares as `text`, and then
    /// the column is the text with no trailing spaces, which is what it holds.
    fn bpchar_operands(
        &mut self,
        ast: &Ast,
        written: [ast::ExprRef; 2],
        [left, right]: [&mut ExprRef; 2],
        scope: &Scope,
    ) -> Result<()> {
        use rudb_pgtypes::oid::{BPCHAR, VARCHAR};
        let declared = |binder: &Self, written: ast::ExprRef, expr: ExprRef| match ast.expr(written)
        {
            ast::Expr::Column { .. } => binder
                .through(expr, scope)
                .and_then(|column| column.origin)
                .and_then(|origin| origin.ty),
            ast::Expr::Cast { ty, .. } => rudb_pgtypes::declared_type(ast.string(ty)),
            _ => None,
        };
        let sides = [(written[0], *left), (written[1], *right)].map(|(written, expr)| {
            let ty = declared(self, written, expr);
            let column = matches!(ast.expr(written), ast::Expr::Column { .. })
                && ty.is_some_and(|ty| ty.oid == BPCHAR && ty.typmod >= 4);
            let untyped = matches!(
                ast.expr(written),
                ast::Expr::Literal { kind: LiteralKind::String, .. } | ast::Expr::Parameter { .. }
            );
            let bpchar = untyped || ty.is_some_and(|ty| matches!(ty.oid, BPCHAR | VARCHAR));
            (column, bpchar)
        });
        // A `char(n)` column on both sides holds no trailing spaces on either.
        for (side, (column, _), (held, bpchar)) in
            [(right, sides[0], sides[1]), (left, sides[1], sides[0])]
        {
            let placeholder = self.is_placeholder(*side);
            let string = *self.plan().expr_type(*side) == LogicalType::Varchar;
            if !column || held || !bpchar || !(string || placeholder) {
                continue;
            }
            if placeholder {
                *side = self.cast_to(*side, &LogicalType::Varchar);
            }
            // A parameter compared with a `char(n)` column is a `bpchar`, as in PostgreSQL.
            if let Some(placeholders) = self.parameters.placeholders() {
                let bpchar = DeclaredType { oid: BPCHAR, typmod: -1 };
                for (name, _) in self.placeholders_under(*side, &LogicalType::Varchar, 1) {
                    placeholders.resolve_written(&name, bpchar);
                }
            }
            *side = self.call("rtrim", vec![*side])?;
        }
        Ok(())
    }

    /// Casts a parameter of no known type on one side of an arithmetic operator to the type that
    /// PostgreSQL picks for it, when the other side is a date, a time or an interval.
    ///
    /// PostgreSQL first tries the operator with the parameter as the type of the other side, so
    /// `now() - $1` subtracts two `timestamptz` values. When there is no such operator, it takes
    /// the one operator that is left, so `now() + $1` adds an `interval`. The function resolution
    /// of rudb finds no overload for a null on these operators, so the cast comes first. The
    /// operators that PostgreSQL finds ambiguous, such as `date + $1`, are left as they are.
    fn unknown_operand(&mut self, op: BinaryOp, left: &mut ExprRef, right: &mut ExprRef) {
        use LogicalType as L;
        let (unknown_left, other) = match (self.is_placeholder(*left), self.is_placeholder(*right))
        {
            (true, false) => (true, self.plan().expr_type(*right).clone()),
            (false, true) => (false, self.plan().expr_type(*left).clone()),
            _ => return,
        };
        let wanted = match (op, &other) {
            (BinaryOp::Add, L::Timestamp | L::TimestampTz | L::Time | L::Interval) => L::Interval,
            (
                BinaryOp::Subtract,
                L::Date | L::Timestamp | L::TimestampTz | L::Time | L::Interval,
            ) => other.clone(),
            (BinaryOp::Subtract, L::TimeTz) if !unknown_left => L::Interval,
            (BinaryOp::Divide | BinaryOp::IntegerDivide, L::Interval) if !unknown_left => L::Double,
            (BinaryOp::Concat, L::Blob) => L::Blob,
            _ => return,
        };
        let side = if unknown_left { left } else { right };
        *side = self.cast_to(*side, &wanted);
    }

    /// The PostgreSQL operators on dates and times that the functions of rudb answer in another
    /// way, or `None` for an operator that the functions answer as PostgreSQL does.
    ///
    /// `date - date` is the number of days as an `int4`. `time - time` is an `interval`, which is
    /// the difference of the two times on the same day.
    fn pg_datetime_operator(
        &mut self,
        op: BinaryOp,
        left: ExprRef,
        right: ExprRef,
    ) -> Result<Option<ExprRef>> {
        if op != BinaryOp::Subtract {
            return Ok(None);
        }
        let types = [left, right].map(|side| self.plan().expr_type(side).clone());
        match types {
            [LogicalType::Date, LogicalType::Date] => {
                let days = self.call("-", vec![left, right])?;
                Ok(Some(self.cast_to(days, &LogicalType::Integer)))
            }
            [LogicalType::Time, LogicalType::Time] => {
                let day = Value::Date(0);
                let mut sides = [left, right];
                for side in &mut sides {
                    let day = self.add_constant(day.clone());
                    *side = self.call("+", vec![day, *side])?;
                }
                Ok(Some(self.call("-", sides.to_vec())?))
            }
            _ => Ok(None),
        }
    }

    /// Turns a zero divisor into null before the ordinary arithmetic kernel sees it.
    pub(crate) fn zero_to_null(&mut self, divisor: ExprRef) -> ExprRef {
        let returns = self.plan().expr_type(divisor).clone();
        let args = self.plan_mut().add_expr_list(&[divisor]);
        let name = self.plan_mut().intern("__rudb_zero_to_null");
        self.add_expr(Expr::Function { name, args }, returns)
    }

    /// Binds one of the regex operators to the ordinary regex function it means.
    fn regex_operator(
        &mut self,
        left: ExprRef,
        right: ExprRef,
        insensitive: bool,
        negated: bool,
        always_full: bool,
    ) -> Result<ExprRef> {
        let full = always_full || self.semantics.regex_match_full();
        let name = if full { "regexp_full_match" } else { "regexp_matches" };
        let mut args = vec![left, right];
        if insensitive {
            args.push(self.add_constant(Value::Varchar("i".to_string())));
        }
        let matched = self.call(name, args)?;
        if negated { self.call("not", vec![matched]) } else { Ok(matched) }
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_call(
        &mut self,
        ast: &Ast,
        name: ast::Slice,
        args: ast::Slice,
        distinct: bool,
        filter: ast::ExprRef,
        sorted: &[ast::OrderItem],
        scope: &Scope,
    ) -> Result<ExprRef> {
        let mut written = ast.name(name).last().unwrap_or_default().to_string();
        // The built-in macros are kept in the system database, so a call that names any other
        // database in front of one finds nothing there, and the pin says where it would have.
        if name.len == 2
            && let Some(database) = ast.name(name).next()
            && !rudb_catalog::same_name(database, "system")
            && self.catalog().attached(database).is_some()
            && (crate::macros::is_macro(&written)
                || ["current_user", "session_user", "user", "current_catalog"]
                    .iter()
                    .any(|held| rudb_catalog::same_name(&written, held)))
        {
            return Err(Error::catalog(format!(
                "Scalar Function with name {written} does not exist!\nDid you mean \
                 \"main.{written}\"?"
            )));
        }
        let arguments = ast.expr_list(args).to_vec();
        // `every` is the SQL standard name of `bool_and`, which PostgreSQL has and the pin does not.
        let postgres = self.session.postgres().is_some();
        if postgres && rudb_catalog::same_name(&written, "every") {
            written = "bool_and".to_string();
        }
        // count(*) is a different function from count(x), because one of them counts rows and the
        // other counts the rows where its argument is not null.
        // A replace list on the star is not this, and not anything: upstream's parser refuses
        // `count(* REPLACE (1 AS a))` outright and the vendored grammar has room for it, so leaving
        // it out of here sends it to the arm that says a star is not allowed where it was written.
        let starred = arguments.iter().any(|&arg| {
            matches!(ast.expr(arg), ast::Expr::Star { qualifier, replacements }
                if qualifier.is_empty() && replacements.is_empty())
        });
        // Inside the argument of a `COLUMNS` a star is the list of names it stands for.
        if starred && self.columns_scope.is_none() && self.star_name.is_none() {
            if !rudb_catalog::same_name(&written, "count") || arguments.len() != 1 {
                return Err(Error::binder(format!("* is not allowed in {written}()")));
            }
            return self.bind_aggregate(ast, "count_star", &[], false, filter, &[], scope);
        }
        // `count()` with nothing in it is upstream's other spelling of `count(*)`. It counts rows
        // the same way and it is not an arity mistake, which is what the signature table would
        // otherwise say about a `count` given no arguments.
        if rudb_catalog::same_name(&written, "count") && arguments.is_empty() {
            return self.bind_aggregate(ast, "count_star", &[], false, filter, &[], scope);
        }
        // `TRY(1, 2)` is not the grammar's `TryExpression`, so it arrives as a call, and the pin's
        // parser is what refuses it there.
        if rudb_catalog::same_name(&written, "try") {
            let [only] = arguments[..] else {
                return Err(Error::parser("Wrong number of arguments provided to TRY expression"));
            };
            return self.bind_try(ast, only, scope);
        }
        let modified = distinct || filter != NONE || !sorted.is_empty();
        if modified && crate::macros::is_macro(&written) {
            return Err(Error::invalid_input(format!(
                "Function \"{written}\" is a Macro Function. \"DISTINCT\", \"FILTER\", and \
                 \"ORDER BY\" are only applicable to window and aggregate functions."
            )));
        }
        // `pg_typeof` names the PostgreSQL type, as `format_type` does with no typmod, so a
        // `numeric(2,1)` literal is `numeric`. A string literal or a null has no type yet.
        if !modified
            && self.session.postgres().is_some()
            && rudb_catalog::same_name(&written, "pg_typeof")
            && let [only] = arguments[..]
        {
            let bound = self.bind_expr(ast, only, scope)?;
            if crate::advisory::gives_void(ast, only) {
                return Ok(self.add_constant(Value::Varchar("void".into())));
            }
            let ty = self.plan().expr_type(bound).clone();
            let untyped = ty == LogicalType::Null
                || matches!(ast.expr(only), ast::Expr::Literal { kind: LiteralKind::String, .. });
            let name = match untyped {
                true => "unknown".to_string(),
                false => rudb_pgtypes::format_type(rudb_pgtypes::pg_type(&ty).oid).into_owned(),
            };
            return Ok(self.add_constant(Value::Varchar(name)));
        }
        if !modified
            && postgres
            && let Some(call) = self.postgres_call(ast, &written, &arguments, scope)?
        {
            return Ok(call);
        }
        if !modified && let Some(call) = self.sleep_call(ast, &written, &arguments, scope)? {
            return Ok(call);
        }
        if !modified && let Some(call) = self.advisory_call(ast, &written, &arguments, scope)? {
            return Ok(call);
        }
        if let Some(expanded) = self.builtin_macro(ast, &written, &arguments, scope)? {
            return Ok(expanded);
        }
        if kind_of(&written) == Some(FunctionKind::Aggregate) {
            return self.bind_aggregate(ast, &written, &arguments, distinct, filter, sorted, scope);
        }
        // A ranking window with no `OVER` after it. Upstream says this and not that the name is
        // missing, because the name is there and it is the place it was written that is wrong:
        // `row_number()` has no answer until something says which rows it is counting through.
        if kind_of(&written) == Some(FunctionKind::Window) {
            return Err(Error::binder("Window functions are not supported here"));
        }
        // Upstream's sentence, which names all three modifiers whichever one was written, and which
        // it reaches only once the name has resolved: `nosuch(DISTINCT x)` is a catalog error there
        // and not this, so a name this does not know falls through and gets the catalog's answer.
        if modified && kind_of(&written) == Some(FunctionKind::Scalar) {
            return Err(Error::invalid_input(format!(
                "Function \"{written}\" is a Scalar Function. \"DISTINCT\", \"FILTER\", and \
                 \"ORDER BY\" are only applicable to window and aggregate functions."
            )));
        }
        if let Some(recorded) = crate::lambda::lambda_function(&written) {
            return self.bind_lambda_call(ast, recorded, &arguments, scope);
        }
        if arguments.iter().any(|&arg| matches!(ast.expr(arg), ast::Expr::Lambda { .. })) {
            // A name nobody has gets the catalog's answer, which is about the name and not about
            // what was passed to it.
            if kind_of(&written).is_none() {
                self.call(&written, Vec::new())?;
            }
            return Err(Error::binder("This scalar function does not support lambdas!"));
        }
        let mut bound = Vec::with_capacity(arguments.len());
        for &arg in &arguments {
            bound.push(self.bind_expr(ast, arg, scope)?);
        }
        // A string literal has no type in PostgreSQL until the call gives it one.
        let untyped: Vec<bool> = arguments
            .iter()
            .map(|&arg| {
                matches!(ast.expr(arg), ast::Expr::Literal { kind: LiteralKind::String, .. })
            })
            .collect();
        if let Some(expanded) = self.list_macro(&written, &bound, &untyped)? {
            return Ok(expanded);
        }
        if rudb_catalog::same_name(&written, "if") {
            return self.bind_if(&bound);
        }
        if let Some(aggregated) = self.list_aggregate(&written, &bound)? {
            return Ok(aggregated);
        }
        // A column passed to `struct_pack` without a name gives the field its own name, which is
        // how `struct_pack(*COLUMNS(*))` packs a whole row.
        if rudb_catalog::same_name(&written, crate::structs::STRUCT_PACK) {
            let mut names = Vec::with_capacity(arguments.len());
            for &arg in &arguments {
                let ast::Expr::Column { name } = ast.expr(arg) else {
                    return Err(Error::binder(
                        "Need named argument for struct pack, e.g. STRUCT_PACK(a := b)",
                    ));
                };
                let parts: Vec<&str> = ast.name(name).collect();
                let found = scope.resolve(&parts).map(|found| found.name.clone());
                let field =
                    found.unwrap_or_else(|_| parts.last().copied().unwrap_or_default().into());
                if names.iter().any(|earlier: &String| earlier.eq_ignore_ascii_case(&field)) {
                    return Err(Error::binder(format!("Duplicate struct entry name \"{field}\"")));
                }
                names.push(field);
            }
            return self.pack_struct(&names, &bound);
        }
        if let Some(field) = self.variant_field(&written, &bound)? {
            return Ok(field);
        }
        if let Some(call) = self.map_call(&written, &bound)? {
            return Ok(call);
        }
        if let Some(call) = self.struct_call(&written, &bound)? {
            return Ok(call);
        }
        if let Some(call) = self.state_call(&written, &bound)? {
            return Ok(call);
        }
        if let Some(call) = self.union_call(&written, &bound)? {
            return Ok(call);
        }
        if let Some(field) = self.struct_field(&written, &bound)? {
            return Ok(field);
        }
        if let Some(field) = self.json_field(&written, &bound)? {
            return Ok(field);
        }
        if let Some(call) = self.sequence_call(&written, &bound)? {
            return Ok(call);
        }
        if let Some(call) = self.enum_call(&written, &bound)? {
            return Ok(call);
        }
        // The pin hashes an enum as the integer it is stored in and not as its label.
        if rudb_catalog::same_name(&written, "hash") {
            for arg in &mut bound {
                *arg = self.by_position(*arg);
            }
        }
        if let Some(&builder) =
            rudb_kernels::json::BUILDERS.iter().find(|name| rudb_catalog::same_name(&written, name))
        {
            self.json_builder(ast, builder, &arguments, &bound, scope)?;
        }
        // `typeof` is answered here rather than by a kernel, because the type is settled the moment
        // its argument is bound and nothing about it changes per row. The argument still has to be
        // a legal expression where it was written, so it goes through the aggregate rules first and
        // is then dropped: upstream refuses `SELECT typeof(x), count(*) FROM t` for the same reason
        // it refuses a bare `x` there, even though neither of them reads a value. Any other number
        // of arguments falls through to the ordinary path and gets the arity error from the table.
        if rudb_catalog::same_name(&written, "typeof") && bound.len() == 1 {
            self.over_aggregate(bound[0], scope)?;
            let named = self.plan().expr_type(bound[0]).to_string();
            return Ok(self.add_constant(Value::Varchar(named)));
        }
        // `can_cast_implicitly` is answered from the two types the same way, which is what the pin
        // does too. A null on either side is answered by the same rule, since a null becomes
        // anything and nothing becomes a null.
        if rudb_catalog::same_name(&written, "can_cast_implicitly") && bound.len() == 2 {
            self.over_aggregate(bound[0], scope)?;
            self.over_aggregate(bound[1], scope)?;
            let (source, target) =
                (self.plan().expr_type(bound[0]), self.plan().expr_type(bound[1]));
            let casts = rudb_functions::implicit::cost(source, target).is_some();
            return Ok(self.add_constant(Value::Boolean(casts)));
        }
        // `current_setting` is the other one the binder answers, and it has to be answered here
        // rather than by a kernel for a reason `typeof` does not have: its declared return type is
        // ANY, so there is no type for a plan to carry until the name is read. Upstream folds it
        // too, which an `EXPLAIN` of a query that calls it shows. A call this cannot fold falls
        // through to the table, which refuses it in upstream's words.
        if rudb_catalog::same_name(&written, "current_setting")
            && self.session.postgres().is_some()
            && let Some(folded) = self.postgres_setting(&bound)?
        {
            return Ok(folded);
        }
        if rudb_catalog::same_name(&written, "current_setting")
            && bound.len() == 1
            && let Some(folded) = self.setting(bound[0])?
        {
            return Ok(folded);
        }
        // `version()` of a PostgreSQL session is the text of PostgreSQL, which clients parse for
        // the major version.
        if bound.is_empty()
            && rudb_catalog::same_name(&written, "version")
            && let Some(postgres) = self.session.postgres()
        {
            return Ok(self.add_constant(Value::Varchar(postgres.version.clone())));
        }
        // Anywhere else it is the version of rudb, written the way `pragma_version()` writes it.
        if bound.is_empty() && rudb_catalog::same_name(&written, "version") {
            let version = format!("v{}", env!("CARGO_PKG_VERSION"));
            return Ok(self.add_constant(Value::Varchar(version)));
        }
        // `current_query()` is the text of the statement from its first word, which is as much of
        // it as the pin gives back, and `txid_current()` is the number of the transaction it runs
        // in. Neither changes from one row to the next, so both fold the way the session context
        // does below.
        if bound.is_empty() && rudb_catalog::same_name(&written, "current_query") {
            let text = self.statement_text(ast);
            return Ok(self.add_constant(Value::Varchar(text.trim_start().to_string())));
        }
        if bound.is_empty() && rudb_catalog::same_name(&written, "txid_current") {
            self.read_per_transaction();
            return Ok(self.add_constant(Value::UBigInt(self.session.transaction())));
        }
        // `getvariable` is folded for the reason `current_setting` is: it is declared to return
        // ANY and the type is the variable's, which is only known once the name is read.
        if rudb_catalog::same_name(&written, "getvariable")
            && bound.len() == 1
            && let Some(folded) = self.variable(bound[0])
        {
            return Ok(folded);
        }
        if rudb_catalog::same_name(&written, "in_search_path")
            && let [catalog, schema] = bound[..]
            && let Some(answered) = self.in_search_path(catalog, schema)?
        {
            self.read_per_transaction();
            return Ok(answered);
        }
        if rudb_catalog::same_name(&written, "current_schemas")
            && bound.len() == 1
            && let Some(folded) = self.current_schemas(bound[0])?
        {
            self.read_per_transaction();
            return Ok(folded);
        }
        // The session context functions are the third group the binder answers, and they fold for
        // the reason the pin marks them `CONSISTENT_WITHIN_QUERY`: the answer is settled when the
        // statement starts and no row changes it. A call with arguments is not one of these and
        // falls through to the table, which has a row per name so that `now(1)` is the arity error
        // rather than a missing function. See `crate::context`.
        if bound.is_empty()
            && let Some(folded) = self.context_call(&written)
        {
            return Ok(folded);
        }
        // The one argument form measures from the session-local date at the start of the
        // statement. Insert that date here so the ordinary two-moment kernel remains free of a
        // session dependency and both spellings use exactly the same calendar arithmetic.
        if rudb_catalog::same_name(&written, "age") && bound.len() == 1 {
            bound.insert(0, self.current_date());
        }
        // `timezone` of one moment is its offset, and of a zone and a moment is a conversion that is
        // a different function, so only the first is the shortcut.
        if let Some(&(name, part)) = PART_SHORTCUTS
            .iter()
            .find(|(name, _)| rudb_catalog::same_name(&written, name))
            .filter(|(name, _)| *name != "timezone" || bound.len() == 1)
        {
            return self.bind_part_shortcut(ast, name, part, &arguments, &bound);
        }
        // The byte formatters are declared over a BIGINT and nothing else, and a string literal
        // reaches that by a cast where a VARCHAR does not, so `format_bytes('1')` is one byte,
        // `format_bytes('x')` is the pin's conversion error and `format_bytes('1'::VARCHAR)` is
        // refused. Only the syntax can tell the first and the last apart.
        // The string builders that take a number somewhere are the same, so `lpad('a', '3', 'x')`
        // pads to three and `repeat('ab', '2')` repeats twice, and so are the calls that take a
        // blob, so `base64('abc')` is the blob of those three bytes.
        let count = bound.len();
        for (at, (arg, bound)) in arguments.iter().zip(bound.iter_mut()).enumerate() {
            if matches!(ast.expr(*arg), ast::Expr::Literal { kind: LiteralKind::String, .. })
                && let Some(ty) = declared_parameter(&written, at, count)
            {
                *bound = self.cast_to(*bound, &ty);
            }
        }
        if let Some(function) = ["coalesce", "greatest", "least"]
            .into_iter()
            .find(|name| rudb_catalog::same_name(&written, name))
        {
            let kinds: Vec<Literal> = arguments
                .iter()
                .map(|&arg| match ast.expr(arg) {
                    ast::Expr::Literal { kind: LiteralKind::String, .. } => Literal::Text,
                    ast::Expr::Literal { kind: LiteralKind::Number, text }
                        if ast.string(text).bytes().all(|byte| byte.is_ascii_digit()) =>
                    {
                        Literal::Integer
                    }
                    _ => Literal::Other,
                })
                .collect();
            self.adopt_literals(function, &kinds, &mut bound)?;
            // Parameters of no type and nothing else are `text` in PostgreSQL.
            if postgres && bound.iter().all(|&arg| self.is_placeholder(arg)) {
                for arg in &mut bound {
                    *arg = self.cast_to(*arg, &LogicalType::Varchar);
                }
            }
        }
        if postgres
            && rudb_catalog::same_name(&written, "nullif")
            && bound.iter().all(|&arg| self.is_placeholder(arg))
        {
            for arg in &mut bound {
                *arg = self.cast_to(*arg, &LogicalType::Varchar);
            }
        }
        if rudb_catalog::same_name(&written, "coalesce") && bound.len() > 1 {
            let call = self.call(&written, bound)?;
            return self.lazy_coalesce(call);
        }
        // `divide` and `mod` are `//` and `%` by another name, and the pin's division by zero message
        // quotes them as called, `divide(7, 0)` rather than `(7 // 0)`. They are stored under a
        // private name so the message can tell, and a zero divisor is null under the setting the
        // same as it is for the operators.
        for (spelled, operator, stored) in
            [("divide", "//", "__rudb_divide"), ("mod", "%", "__rudb_mod")]
        {
            if rudb_catalog::same_name(&written, spelled) && bound.len() == 2 {
                if self.semantics.null_on_division_by_zero() {
                    bound[1] = self.zero_to_null(bound[1]);
                }
                return self.call_as(operator, stored, bound);
            }
        }
        // The pin takes the format apart once when it binds, so a format that is not a constant is
        // refused and one that does not parse is refused before any row is read.
        if rudb_catalog::same_name(&written, "strftime") && bound.len() == 2 {
            let at = usize::from(*self.plan().expr_type(bound[0]) != LogicalType::Varchar);
            match fold::value_of(self.plan(), bound[at]) {
                Ok(Some(Value::Varchar(format))) => {
                    rudb_kernels::strftime::Format::parse(&format)?;
                }
                Ok(Some(_)) => {}
                _ => {
                    return Err(Error::binder(
                        "The \"format\" argument in function \"strftime\" must be a constant \
                         expression",
                    ));
                }
            }
        }
        // `strptime` is the same, and a list of formats is taken apart one by one.
        let reads = ["strptime", "try_strptime"]
            .into_iter()
            .find(|name| rudb_catalog::same_name(&written, name));
        if let (Some(name), [_, format]) = (reads, bound.as_slice()) {
            match fold::value_of(self.plan(), *format) {
                Ok(Some(format)) => {
                    rudb_kernels::strptime::Formats::from_value(&format)?;
                }
                _ => {
                    return Err(Error::binder(format!(
                        "The \"format\" argument in function \"{name}\" must be a constant \
                         expression"
                    )));
                }
            }
        }
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        // A whole number written in the query has no cast to a document, so `json_pretty(42)` is
        // refused while `json_pretty(42::BIGINT)` writes the number.
        let count = arguments.len();
        if arguments.iter().enumerate().any(|(at, &arg)| {
            integer_literal(ast, arg) && rudb_functions::json_text_at(&written, at, count)
        }) {
            let spelled: Vec<String> =
                types.iter().zip(&arguments).map(|(ty, &arg)| spelled_type(ast, arg, ty)).collect();
            let name = written.to_ascii_lowercase();
            return Err(rudb_functions::named_mismatch(&name, &spelled, false));
        }
        let mut bound = self.variant_arguments(ast, &written, &arguments, bound)?;
        if postgres {
            self.postgres_rounding(&written, &mut bound);
        }
        // PostgreSQL counts the bytes of a text with `octet_length` and of a bytea with `length`,
        // and the pin has `strlen` and `octet_length` for these.
        if postgres && let [only] = &types[..] {
            let bytes = match only {
                LogicalType::Varchar | LogicalType::Null
                    if rudb_catalog::same_name(&written, "octet_length") =>
                {
                    Some("strlen")
                }
                LogicalType::Blob if rudb_catalog::same_name(&written, "length") => {
                    Some("octet_length")
                }
                _ => None,
            };
            if let Some(bytes) = bytes {
                return self.call(bytes, bound);
            }
        }
        let call = self.call(&written, bound).map_err(|error| match postgres {
            true => undefined_function(ast, error, &written, &arguments, &types),
            false => literals_spelled(ast, error, &arguments, &types),
        })?;
        match postgres {
            true => Ok(self.postgres_narrowed(&written, &types, call)),
            false => Ok(call),
        }
    }

    /// The arguments of a function over `VARIANT`, read the way the pin reads them.
    ///
    /// A string written in the query has an implicit cast to a variant and a string column does
    /// not, so `variant_keys('a')` is a call on the variant `'a'` while `variant_keys(s)` over a
    /// `VARCHAR` column is refused. The path of the functions that take one has to be a constant,
    /// and a list of paths can have no null in it.
    fn variant_arguments(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        mut bound: Vec<ExprRef>,
    ) -> Result<Vec<ExprRef>> {
        let name = written.to_ascii_lowercase();
        let held = match name.as_str() {
            "variant_contains" => 2,
            "variant_typeof"
            | "variant_comparator"
            | "variant_extract"
            | "variant_normalize"
            | "variant_keys"
            | "variant_type"
            | "variant_exists"
            | "variant_array_length"
            | "variant_extract_string" => 1,
            _ => return Ok(bound),
        };
        let fits = match name.as_str() {
            "variant_keys" | "variant_type" | "variant_array_length" => {
                matches!(bound.len(), 1 | 2)
            }
            "variant_contains"
            | "variant_exists"
            | "variant_extract_string"
            | "variant_extract" => bound.len() == 2,
            _ => bound.len() == 1,
        };
        // A call no overload takes is refused with the literals spelled as they were written.
        if !fits {
            return Ok(bound);
        }
        // A negative position written in the query is a whole number no unsigned position takes.
        if name == "variant_extract"
            && matches!(ast.expr(arguments[1]), ast::Expr::Unary { op: UnaryOp::Negate, .. })
            && integer_literal(ast, arguments[1])
        {
            let spelled: Vec<String> = arguments
                .iter()
                .zip(&bound)
                .map(|(&arg, &expr)| spelled_type(ast, arg, self.plan().expr_type(expr)))
                .collect();
            return Err(rudb_functions::named_mismatch(&name, &spelled, false));
        }
        for at in 0..held {
            if matches!(
                ast.expr(arguments[at]),
                ast::Expr::Literal { kind: LiteralKind::String, .. }
            ) {
                bound[at] = self.cast_to(bound[at], &LogicalType::Variant);
            }
        }
        let pathed = matches!(
            name.as_str(),
            "variant_keys"
                | "variant_type"
                | "variant_exists"
                | "variant_array_length"
                | "variant_extract_string"
        );
        if let (true, [_, path]) = (pathed, bound.as_slice()) {
            match fold::value_of(self.plan(), *path) {
                Ok(Some(Value::List { values, .. })) if values.iter().any(Value::is_null) => {
                    return Err(Error::binder(format!("'{name}' does not accept NULL paths")));
                }
                Ok(Some(_)) => {}
                _ => {
                    return Err(Error::binder(format!(
                        "The \"path\" argument in function \"{name}\" must be a constant \
                         expression"
                    )));
                }
            }
        }
        Ok(bound)
    }

    /// `list_append` and the five names like it, which are macros on the pin and are expanded the
    /// same way here.
    ///
    /// Upstream defines each of them as `list_concat` of the list and a one element list holding
    /// the value, so they are not functions with rules of their own and everything about them is
    /// `list_concat`'s. The element type is what the two promote to, `list_append(NULL, 3)` is `[3]`
    /// because `list_concat` skips a null, and a value that will not go into the list is refused
    /// with `list_concat`'s sentence naming `list_concat`, which is exactly what the pin prints for
    /// `list_append([1], 'a')`. Expanding here rather than giving each one a row and a kernel is
    /// what keeps all of that the same without writing any of it down twice.
    ///
    /// The one thing that is the macro's and not `list_concat`'s is the wrong number of arguments,
    /// which the pin reports as a macro and not as a function, in the words below.
    fn list_macro(
        &mut self,
        written: &str,
        bound: &[ExprRef],
        untyped: &[bool],
    ) -> Result<Option<ExprRef>> {
        let Some(&(name, parameters, element, front)) =
            LIST_MACROS.iter().find(|(name, ..)| rudb_catalog::same_name(written, name))
        else {
            return Ok(None);
        };
        if bound.len() != 2 {
            return Err(Error::binder(format!(
                "Macro {name}() does not support the supplied arguments. You might need to add \
                 explicit type casts.\nCandidate macros:\n\t{name}({parameters})"
            )));
        }
        // In PostgreSQL a value of no type is of the type of the elements of the array.
        let mut value = bound[element];
        let list = self.plan().expr_type(bound[1 - element]).clone();
        if let LogicalType::List(inner) = list
            && self.session.postgres().is_some()
            && (self.is_placeholder(value) || untyped[element])
        {
            value = self.cast_to(value, &inner);
        }
        let wrapped = self.call("list_value", vec![value])?;
        let list = bound[1 - element];
        let args = if front { vec![wrapped, list] } else { vec![list, wrapped] };
        self.call("list_concat", args).map(Some)
    }

    fn bind_case(
        &mut self,
        ast: &Ast,
        operand: ast::ExprRef,
        arms: ast::Slice,
        otherwise: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        // A simple CASE is bound as the searched one it means. The operand is bound once and the
        // resulting expression is shared by every arm, so the arms do not each re-evaluate it.
        let subject =
            if operand == NONE { None } else { Some(self.bind_expr(ast, operand, scope)?) };
        let written = ast.arm_list(arms).to_vec();
        let mut bound = Vec::with_capacity(written.len());
        let mut result = LogicalType::Null;
        for arm in &written {
            let when = self.bind_expr(ast, arm.when, scope)?;
            let when = match subject {
                Some(subject) => self.compare(CompareOp::Equal, subject, when)?,
                None => self.as_boolean(when, "CASE")?,
            };
            let then = self.bind_expr(ast, arm.then, scope)?;
            result = meet(&result, self.plan().expr_type(then))?;
            bound.push(Arm { when, then });
        }
        let fallback = if otherwise == NONE {
            None
        } else {
            let bound = self.bind_expr(ast, otherwise, scope)?;
            result = meet(&result, self.plan().expr_type(bound))?;
            Some(bound)
        };
        // Every arm and the else have to hand back the same type, since a CASE produces one column.
        for arm in &mut bound {
            arm.then = self.checked_cast_to(arm.then, &result, false)?;
        }
        let fallback =
            fallback.map(|expr| self.checked_cast_to(expr, &result, false)).transpose()?;
        let arms = self.plan_mut().add_arms(&bound);
        Ok(self.add_expr(Expr::Case { arms, otherwise: fallback }, result))
    }

    /// `if(a, b, c)`, which the pin has as the macro `CASE WHEN (a) THEN (b) ELSE c END`.
    fn bind_if(&mut self, bound: &[ExprRef]) -> Result<ExprRef> {
        let &[condition, then, otherwise] = bound else {
            return Err(Error::binder(
                "Macro \"if\"() does not support the supplied arguments. You might need to add \
                 explicit type casts.\nCandidate macros:\n\t\"if\"(a, b, c)",
            ));
        };
        let when = self.as_boolean(condition, "CASE")?;
        let result = meet(&LogicalType::Null, self.plan().expr_type(then))?;
        let result = meet(&result, self.plan().expr_type(otherwise))?;
        let then = self.checked_cast_to(then, &result, false)?;
        let otherwise = self.checked_cast_to(otherwise, &result, false)?;
        let arms = self.plan_mut().add_arms(&[Arm { when, then }]);
        Ok(self.add_expr(Expr::Case { arms, otherwise: Some(otherwise) }, result))
    }

    fn bind_between(
        &mut self,
        ast: &Ast,
        operand: ast::ExprRef,
        low: ast::ExprRef,
        high: ast::ExprRef,
        negated: bool,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let subject = self.bind_expr(ast, operand, scope)?;
        let low = self.bind_expr(ast, low, scope)?;
        let high = self.bind_expr(ast, high, scope)?;
        let (lower, upper, op) = if negated {
            (CompareOp::Less, CompareOp::Greater, ConjunctionOp::Or)
        } else {
            (CompareOp::GreaterOrEqual, CompareOp::LessOrEqual, ConjunctionOp::And)
        };
        let lower = self.compare(lower, subject, low)?;
        let upper = self.compare(upper, subject, high)?;
        Ok(self.conjunction(op, vec![lower, upper]))
    }

    fn bind_in(
        &mut self,
        ast: &Ast,
        operand: ast::ExprRef,
        list: ast::Slice,
        negated: bool,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let subject = self.bind_expr(ast, operand, scope)?;
        let written = ast.expr_list(list).to_vec();
        if written.is_empty() {
            return Err(Error::binder("IN over an empty list".to_string()));
        }
        let (op, connective) = if negated {
            (CompareOp::NotEqual, ConjunctionOp::And)
        } else {
            (CompareOp::Equal, ConjunctionOp::Or)
        };
        let mut tests = Vec::with_capacity(written.len());
        let postgres = self.session.postgres().is_some();
        for written in written {
            let mut item = self.bind_expr(ast, written, scope)?;
            let mut subject = subject;
            if postgres {
                self.bpchar_operands(ast, [operand, written], [&mut subject, &mut item], scope)?;
            }
            tests.push(self.compare(op, subject, item)?);
        }
        Ok(self.conjunction(connective, tests))
    }

    /// Binds an uncorrelated `IN` as a mark join. The mark join answers the three-valued `ANY`
    /// question directly, so it neither materialises a cross product nor loses the distinction
    /// between false and an unknown comparison.
    fn bind_in_subquery(
        &mut self,
        ast: &Ast,
        operand: ast::ExprRef,
        query: ast::QueryRef,
        negated: bool,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let subject = self.bind_expr(ast, operand, scope)?;
        self.bind_mark_subquery(ast, subject, query, CompareOp::Equal, negated, scope)
    }

    fn bind_quantified_subquery(
        &mut self,
        ast: &Ast,
        operand: ast::ExprRef,
        op: BinaryOp,
        query: ast::QueryRef,
        all: bool,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let subject = self.bind_expr(ast, operand, scope)?;
        let op = comparison_of(op).ok_or_else(|| {
            Error::binder("Only comparisons can be used before ANY or ALL".to_string())
        })?;
        let op = if all { negate_comparison(op) } else { op };
        self.bind_mark_subquery(ast, subject, query, op, all, scope)
    }

    /// Binds `x op ANY (array)` or `x op ALL (array)`.
    ///
    /// The comparison is made against each element by `list_transform`, and the list of answers
    /// is folded by PostgreSQL's rule. `ANY` is true when one answer is true, null when none is
    /// true and one is null, and false else. `ALL` is false when one answer is false, null when
    /// none is false and one is null, and true else. An empty array is false for `ANY` and true
    /// for `ALL` even when `x` is null, and a null array is null.
    ///
    /// Both sides are cast to the type they compare at before the lambda, so the cast is made
    /// once a row and not once an element. An untyped parameter or string on the right is an
    /// array of the left side's type, which is what PostgreSQL gives `c = ANY($1)`.
    fn bind_quantified_array(
        &mut self,
        ast: &Ast,
        operand: ast::ExprRef,
        op: BinaryOp,
        array: ast::ExprRef,
        all: bool,
        scope: &Scope,
    ) -> Result<ExprRef> {
        // The elements of `ARRAY(SELECT ...)` are the rows of the query, and the array is never
        // null, so the comparison is the one against the query, which is a mark join and not a
        // list built and searched once a row.
        if let ast::Expr::Subquery { query, array: true } = ast.expr(array) {
            return self.bind_quantified_subquery(ast, operand, op, query, all, scope);
        }
        let comparison = comparison_of(op).ok_or_else(|| {
            Error::binder("Only comparisons can be used before ANY or ALL".to_string())
        })?;
        // A string on one side is of no type yet in PostgreSQL, and it is read with the input
        // function of the type the other side gives it. On the right that is an array of the left
        // side's type, so `x = ANY('{1,2}')` is `x = ANY('{1,2}'::int[])` for an `int` x. On the
        // left it is the element type, so `'a' = ANY(array[1])` reads `'a'` as an integer.
        let string =
            |expr| matches!(ast.expr(expr), ast::Expr::Literal { kind: LiteralKind::String, .. });
        let late = string(operand) && !string(array);
        let early = if late { Some(self.bind_expr(ast, array, scope)?) } else { None };
        let mut subject = if let Some(list) = early {
            let element = match self.plan().expr_type(list) {
                LogicalType::List(element) | LogicalType::Array(element, _) => (**element).clone(),
                _ => LogicalType::Varchar,
            };
            match self.read_literal(ast, operand, rudb_pgtypes::pg_type(&element).oid) {
                Some(read) => read?,
                None => self.bind_expr(ast, operand, scope)?,
            }
        } else {
            self.bind_expr(ast, operand, scope)?
        };
        let subject_type = self.plan().expr_type(subject).clone();
        let mut list = match early {
            Some(list) => list,
            None => {
                let ty = LogicalType::List(Box::new(subject_type.clone()));
                match self.read_literal(ast, array, rudb_pgtypes::pg_type(&ty).oid) {
                    Some(read) => read?,
                    None => self.bind_expr(ast, array, scope)?,
                }
            }
        };
        // A multidimensional array is compared element by element, as if it were flat.
        while let LogicalType::List(element) | LogicalType::Array(element, _) =
            self.plan().expr_type(list).clone()
            && matches!(*element, LogicalType::List(_) | LogicalType::Array(..))
        {
            list = self.call("flatten", vec![list])?;
        }
        let list_type = self.plan().expr_type(list).clone();
        let constant = matches!(self.plan().expr(list), Expr::Constant(_));
        let element = match list_type {
            LogicalType::List(element) | LogicalType::Array(element, _) => *element,
            LogicalType::Null => subject_type.clone(),
            LogicalType::Varchar if constant => subject_type.clone(),
            _ => {
                return Err(Error::binder("op ANY/ALL (array) requires array on right side")
                    .state(SqlState::WRONG_OBJECT_TYPE));
            }
        };
        if self.is_placeholder(subject) && element != LogicalType::Null {
            subject = self.cast_to(subject, &element);
        }
        let subject_type = self.plan().expr_type(subject).clone();
        let common = match comparison_type(&subject_type, &element) {
            Some(common) => common,
            None => {
                return Err(Error::binder(format!(
                    "Cannot compare values of type {subject_type} and type {element} - an explicit cast is required"
                )));
            }
        };
        let common = if common == LogicalType::Null { LogicalType::Varchar } else { common };
        let subject = self.checked_cast_to(subject, &common, false)?;
        let as_list = LogicalType::List(Box::new(common.clone()));
        let list = self.checked_cast_to(list, &as_list, false)?;
        // An array known when the statement is bound, which is what a driver's `c = ANY($1)` is
        // once the parameter has its value, is written out as one comparison an element joined
        // by `OR` or `AND`. That is the shape of `IN`, which the optimizer reads as a set of
        // values, and the null rule of the two connectives is the rule of `ANY` and `ALL`.
        if let Ok(Some(Value::List { values, .. })) = fold::value_of(self.plan(), list)
            && values.len() <= MAX_EXPANDED_ARRAY
        {
            let mut tests = Vec::with_capacity(values.len());
            for value in values {
                let value = self.add_constant(value);
                let value = self.cast_to(value, &common);
                tests.push(self.add_expr(
                    Expr::Compare { op: comparison, left: subject, right: value },
                    LogicalType::Boolean,
                ));
            }
            let connective = if all { ConjunctionOp::And } else { ConjunctionOp::Or };
            return Ok(self.conjunction(connective, tests));
        }
        let table = self.fresh_index();
        let name = self.plan_mut().intern("x");
        let params = self.plan_mut().add_name_list(&[name]);
        let candidate =
            self.add_expr(Expr::LambdaParam(rudb_plan::ColumnBinding::new(table, 0)), common);
        let body = self.add_expr(
            Expr::Compare { op: comparison, left: subject, right: candidate },
            LogicalType::Boolean,
        );
        let lambda = self.add_expr(Expr::Lambda { table, params, body }, LogicalType::Boolean);
        let args = self.plan_mut().add_expr_list(&[list, lambda]);
        let transform = self.plan_mut().intern(crate::lambda::TRANSFORM);
        let answers = self.add_expr(
            Expr::Function { name: transform, args },
            LogicalType::List(Box::new(LogicalType::Boolean)),
        );
        let fold = if all { "pg_quantified_all" } else { "pg_quantified_any" };
        let fold = self.plan_mut().intern(fold);
        let args = self.plan_mut().add_expr_list(&[answers]);
        Ok(self.add_expr(Expr::Function { name: fold, args }, LogicalType::Boolean))
    }

    fn bind_mark_subquery(
        &mut self,
        ast: &Ast,
        subject: ExprRef,
        query: ast::QueryRef,
        comparison: CompareOp,
        negate: bool,
        outer: &Scope,
    ) -> Result<ExprRef> {
        let (node, inner, correlations) = self.bind_isolated_subquery(ast, query, outer)?;
        let [column] = inner.columns.as_slice() else {
            return Err(Error::binder(format!(
                "Subquery returns {} columns - expected 1",
                inner.len()
            )));
        };
        let candidate_type = column.ty.clone();
        let candidate_name = column.name.clone();
        let source = self.add_expr(Expr::Column(column.binding), candidate_type.clone());
        let marker_value = self.add_constant(Value::Boolean(true));
        let projected = self.fresh_index();
        let exprs = self.plan_mut().add_expr_list(&[source, marker_value]);
        let candidate_name = self.plan_mut().intern(&candidate_name);
        let marker_name = self.plan_mut().intern("mark");
        let names = self.plan_mut().add_name_list(&[candidate_name, marker_name]);
        let node = self.add_node(Node::Project { input: node, index: projected, exprs, names });
        let candidate = self
            .plan_mut()
            .add_expr(Expr::Column(rudb_plan::ColumnBinding::new(projected, 0)), candidate_type);
        let condition = self.compare(comparison, subject, candidate)?;
        let marker = self.add_expr(
            Expr::Column(rudb_plan::ColumnBinding::new(projected, 1)),
            LogicalType::Boolean,
        );
        self.scalar_subqueries.push(PendingSubquery {
            node,
            kind: rudb_plan::JoinKind::Mark,
            conditions: vec![condition],
            dependent: !correlations.is_empty(),
            reads: correlations,
            index: projected,
            inside_aggregate: self.in_aggregate,
        });
        if negate { self.call("not", vec![marker]) } else { Ok(marker) }
    }

    /// Resolves a scalar call, casts the arguments to what the overload wants, and records it.
    /// `TRY(x)`, which answers null on a row where `x` raises a conversion, range or input error.
    ///
    /// It stays a call to `try` around its one argument, of the argument's type, and it is the
    /// executor and the folding that know what that means. The pin refuses an aggregate, a window
    /// function or a subquery inside it, since none of those can be run again for one row, and a
    /// volatile call, since running it again would not give the row the answer it first got.
    fn bind_try(&mut self, ast: &Ast, operand: ast::ExprRef, scope: &Scope) -> Result<ExprRef> {
        let outer = std::mem::replace(&mut self.trying, true);
        let bound = self.bind_expr(ast, operand, scope);
        self.trying = outer;
        let bound = bound?;
        if volatile(self.plan(), bound) {
            return Err(Error::binder(
                "TRY can not be used in combination with a volatile function",
            ));
        }
        let ty = self.plan().expr_type(bound).clone();
        let args = self.plan_mut().add_expr_list(&[bound]);
        let name = self.plan_mut().intern("try");
        Ok(self.add_expr(Expr::Function { name, args }, ty))
    }

    /// The length rule of a PostgreSQL string type, for a value that a cast or a store into a
    /// column already made text. A `name` holds at most 63 bytes, and a `varchar(n)` or a
    /// `char(n)` at most n characters. An explicit cast cuts a longer value with no error. A store
    /// refuses it with `22001`, unless the extra characters are spaces.
    pub(crate) fn pg_length(
        &mut self,
        expr: ExprRef,
        declared: DeclaredType,
        explicit: bool,
    ) -> Result<ExprRef> {
        use rudb_pgtypes::oid;
        self.resolve_written(expr, declared);
        if *self.plan().expr_type(expr) != LogicalType::Varchar {
            return Ok(expr);
        }
        let stored = match declared.oid {
            oid::NAME => return self.call_as("lower", "__rudb_pg_name", vec![expr]),
            oid::VARCHAR if declared.typmod >= 4 => "__rudb_pg_varchar",
            oid::BPCHAR if declared.typmod >= 4 => "__rudb_pg_bpchar",
            _ => return Ok(expr),
        };
        if explicit && declared.oid == oid::VARCHAR {
            let length = self.add_constant(Value::BigInt(i64::from(declared.typmod - 4)));
            return self.call("left", vec![expr, length]);
        }
        let stored = match explicit {
            true => "__rudb_pg_bpchar_cut",
            false => stored,
        };
        let typmod = self.add_constant(Value::BigInt(i64::from(declared.typmod)));
        self.call_as("left", stored, vec![expr, typmod])
    }

    /// A value stored into a column of a declared PostgreSQL type, with the length rule of the
    /// type in a PostgreSQL session.
    pub(crate) fn stored(
        &mut self,
        expr: ExprRef,
        declared: Option<DeclaredType>,
    ) -> Result<ExprRef> {
        match declared {
            Some(declared) if self.session.postgres().is_some() => {
                self.pg_length(expr, declared, false)
            }
            _ => Ok(expr),
        }
    }

    /// Types each parameter of no type that a call reads as a number, the way PostgreSQL types an
    /// `unknown` argument of a function.
    ///
    /// PostgreSQL prefers `float8` in the numeric category, so `abs($1)` and `round($1)` take a
    /// `float8`. That holds only when the overload over a DOUBLE takes each other argument as it
    /// is, since an argument of a known type that matches exactly decides first, so `mod($1, 2)`
    /// stays an `int4`. Where the overload here takes a BIGINT, PostgreSQL has one over an `int4`,
    /// so `substr($1, $2)` takes an `int4`, unless another argument is a BIGINT already.
    fn unknown_numbers(
        &mut self,
        name: &str,
        args: &mut [ExprRef],
        resolved: &mut rudb_functions::Resolved,
    ) -> Result<()> {
        for at in 0..args.len() {
            let types: Vec<LogicalType> =
                args.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
            if types[at] != LogicalType::Null || !self.is_placeholder(args[at]) {
                continue;
            }
            let Some(wanted) = resolved.arguments.get(at).cloned() else { continue };
            if !wanted.is_numeric() {
                continue;
            }
            let mut doubles = types.clone();
            doubles[at] = LogicalType::Double;
            let known = |other: &rudb_functions::Resolved| {
                types.iter().zip(&other.arguments).enumerate().all(|(position, (given, taken))| {
                    position == at || *given == LogicalType::Null || given == taken
                })
            };
            // A parameter of no type at another position keeps the type that the call chose
            // for it, so `repeat($1, $2)` still repeats text.
            for (position, ty) in doubles.iter_mut().enumerate() {
                if position != at
                    && *ty == LogicalType::Null
                    && let Some(taken) = resolved.arguments.get(position)
                {
                    *ty = taken.clone();
                }
            }
            if let Ok(double) = resolve(name, &doubles)
                && double.arguments.get(at) == Some(&LogicalType::Double)
                && known(&double)
            {
                args[at] = self.cast_to(args[at], &LogicalType::Double);
                *resolved = double;
            } else if wanted == LogicalType::BigInt && !types.contains(&LogicalType::BigInt) {
                args[at] = self.cast_to(args[at], &LogicalType::Integer);
                let mut integers = doubles;
                integers[at] = LogicalType::Integer;
                *resolved = resolve(name, &integers)?;
            }
        }
        Ok(())
    }

    pub(crate) fn call(&mut self, name: &str, args: Vec<ExprRef>) -> Result<ExprRef> {
        self.call_recorded_as(name, None, args)
    }

    /// Resolves one name while recording another internal implementation name.
    fn call_as(
        &mut self,
        resolved_name: &str,
        stored_name: &str,
        args: Vec<ExprRef>,
    ) -> Result<ExprRef> {
        self.call_recorded_as(resolved_name, Some(stored_name), args)
    }

    /// Resolves a scalar call and optionally records a private implementation name.
    fn call_recorded_as(
        &mut self,
        resolved_name: &str,
        stored_name: Option<&str>,
        mut args: Vec<ExprRef>,
    ) -> Result<ExprRef> {
        // The pin's string literal reaches any parameter type by a cast, and the UUID readers are
        // the calls here whose one parameter takes nothing else, so a literal is read as a UUID
        // before the call is resolved rather than refused as a VARCHAR.
        if matches!(resolved_name, "uuid_extract_version" | "uuid_extract_timestamp") {
            for arg in &mut args {
                if let Expr::Constant(value) = *self.plan().expr(*arg)
                    && matches!(self.plan().value(value), Value::Varchar(_))
                {
                    *arg = self.cast_to(*arg, &LogicalType::Uuid);
                }
            }
        }
        // The same holds for `trim_extension` in the three argument `parse_filename`, which is a
        // BOOLEAN and the only overload with three arguments, so `'true'` there is read as true
        // and `'system'` is the pin's conversion error rather than a separator. A third argument
        // that is not a string is the pin's binder error, which comes before any cast.
        if resolved_name == "parse_filename"
            && let [_, trim, separator] = args.as_mut_slice()
            && matches!(self.plan().expr_type(*separator), LogicalType::Varchar | LogicalType::Null)
            && let Expr::Constant(value) = *self.plan().expr(*trim)
            && matches!(self.plan().value(value), Value::Varchar(_))
        {
            *trim = self.checked_cast_to(*trim, &LogicalType::Boolean, false)?;
        }
        let types: Vec<LogicalType> =
            args.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        let mut resolved = resolve(resolved_name, &types)?;
        // A parameter of no type in a PostgreSQL session prefers a string to a blob, as an
        // `unknown` argument prefers the string category there, so `repeat($1, 2)` repeats text.
        if self.session.postgres().is_some() {
            let mut texts = types.clone();
            for (at, arg) in args.iter().enumerate() {
                if resolved.arguments.get(at) == Some(&LogicalType::Blob)
                    && self.placeholders.iter().any(|(held, _)| held == arg)
                {
                    texts[at] = LogicalType::Varchar;
                }
            }
            if texts != types
                && let Ok(text) = resolve(resolved_name, &texts)
            {
                resolved = text;
            }
            // An operator is left as it was. PostgreSQL refuses most operators over two
            // parameters of no type, and the engine gives them an `int4`.
            if resolved_name.starts_with(|first: char| first.is_ascii_alphabetic()) {
                self.unknown_numbers(resolved_name, &mut args, &mut resolved)?;
            }
        }
        // The sort order and the null order of a list sort are read once for the whole call on the
        // pin, which is why it refuses one that could change from row to row.
        let settled: &[&str] = match resolved.name {
            "list_sort" | "list_grade_up" => &["sort_order", "null_order"],
            "list_reverse_sort" => &["null_order"],
            // Rounding a decimal picks the scale of the answer from the count of digits, so the
            // count has to be known before there are any rows.
            "round" | "trunc" | "round_even"
                if matches!(resolved.returns, LogicalType::Decimal { .. }) =>
            {
                &["precision"]
            }
            _ => &[],
        };
        for (&arg, parameter) in args.iter().skip(1).zip(settled) {
            if !matches!(fold::value_of(self.plan(), arg), Ok(Some(_))) {
                return Err(Error::binder(format!(
                    "The \"{parameter}\" argument in function \"{}\" must be a constant expression",
                    resolved.name
                )));
            }
        }
        let mut cast = Vec::with_capacity(args.len());
        for (arg, wanted) in args.iter().zip(&resolved.arguments) {
            cast.push(self.checked_cast_to(*arg, wanted, false)?);
        }
        if resolved.name == "regexp_extract_all" {
            self.settled_options(&cast)?;
        }
        let returns = match resolved.returns {
            LogicalType::Struct(fields) if resolved.name == "date_part" && fields.is_empty() => {
                self.part_list(cast[0])?
            }
            LogicalType::List(element)
                if resolved.name == "regexp_extract_all"
                    && matches!(&*element, LogicalType::Struct(fields) if fields.is_empty()) =>
            {
                self.group_names(&cast)?
            }
            returns => self.narrowed_part(resolved.name, &cast, returns),
        };
        let returns = match cast.as_slice() {
            [_, path] if rudb_kernels::json::NAMES.contains(&resolved.name) => {
                self.json_path(resolved.name, *path, returns)?
            }
            [_, structure] if rudb_kernels::json::TRANSFORMS.contains(&resolved.name) => {
                self.json_shape(resolved.name, *structure)?
            }
            _ => returns,
        };
        let args = self.plan_mut().add_expr_list(&cast);
        // With `ieee_floating_point_ops` off, the math functions raise on a value outside their
        // domain instead of answering a NaN or an infinity, and the kernel is told which reading it
        // is by the name the call is stored under.
        let strict = (stored_name.is_none()
            && !self.semantics.ieee_floating_point_ops()
            && STRICT_MATH.contains(&resolved.name))
        .then(|| format!("__rudb_strict_{}", resolved.name));
        let name =
            self.plan_mut().intern(strict.as_deref().or(stored_name).unwrap_or(resolved.name));
        Ok(self.add_expr(Expr::Function { name, args }, returns))
    }

    /// The type a `JSON` function answers once its path is known, refusing a path the pin refuses
    /// before it reads a row.
    ///
    /// A constant path is read here, so a malformed one is a binder error, and one with a wildcard
    /// answers a list of every value it picks. A list of paths has to be a constant, has no null in
    /// it and no wildcard, since its answer is one value for each path.
    fn json_path(&self, name: &str, path: ExprRef, returns: LogicalType) -> Result<LogicalType> {
        let constant = fold::value_of(self.plan(), path).ok().flatten();
        let listed = matches!(self.plan().expr_type(path), LogicalType::List(_));
        match constant {
            Some(Value::List { values, .. }) if listed => {
                for path in &values {
                    if path.is_null() {
                        return Err(Error::binder("JSON path cannot be NULL"));
                    }
                    if rudb_kernels::json::wild_path(path)? {
                        return Err(Error::binder(
                            "Cannot have wildcards in JSON path when supplying multiple paths",
                        ));
                    }
                }
                Ok(returns)
            }
            Some(_) if listed => Ok(returns),
            None if listed => {
                let parameter =
                    if matches!(name, "json_keys" | "json_array_length") { "path" } else { "col1" };
                Err(Error::binder(format!(
                    "The \"{parameter}\" argument in function \"{name}\" must be a constant expression"
                )))
            }
            Some(path) if rudb_kernels::json::wild_path(&path)? => Ok(LogicalType::list(returns)),
            _ => Ok(returns),
        }
    }

    /// The type `json_transform` answers, which is the one its structure names, so the structure has
    /// to be a constant that is read before there are any rows. A null structure answers a null.
    fn json_shape(&self, name: &str, structure: ExprRef) -> Result<LogicalType> {
        let Some(constant) = fold::value_of(self.plan(), structure)? else {
            let parameter = if name.ends_with("_strict") { "col1" } else { "structure" };
            return Err(Error::binder(format!(
                "The \"{parameter}\" argument in function \"{name}\" must be a constant expression"
            )));
        };
        let Value::Varchar(text) = constant else { return Ok(LogicalType::Null) };
        rudb_kernels::json::structure_type(&text, &mut |written| {
            crate::statement::read_type(self.catalog(), written)
        })
    }

    /// The pin's refusals of a call to one of the `JSON` builders, which it makes when it binds the
    /// call since every one of them takes any number of anything.
    fn json_builder(
        &self,
        ast: &Ast,
        name: &str,
        arguments: &[ast::ExprRef],
        bound: &[ExprRef],
        scope: &Scope,
    ) -> Result<()> {
        let types: Vec<&LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg)).collect();
        let spelled = if name == "json_quote" { "to_json" } else { name };
        match (name, types.as_slice()) {
            ("json_array", _) => Ok(()),
            ("json_object", _) if !types.len().is_multiple_of(2) => {
                Err(Error::binder("json_object() requires an even number of arguments"))
            }
            ("json_object", _) => {
                for (&arg, ty) in arguments.iter().zip(&types).step_by(2) {
                    if **ty != LogicalType::Varchar {
                        let named = self.output_name(ast, arg, scope);
                        return Err(Error::binder(format!(
                            "json_object() keys must be VARCHAR, add an explicit cast to argument \"\"{named}\"\""
                        )));
                    }
                }
                Ok(())
            }
            (_, [ty]) => match (name, ty) {
                (
                    "array_to_json",
                    LogicalType::List(_) | LogicalType::Array(..) | LogicalType::Null,
                )
                | ("row_to_json", LogicalType::Struct(_) | LogicalType::Null)
                | ("to_json" | "json_quote", _) => Ok(()),
                ("array_to_json", _) => {
                    Err(Error::binder("array_to_json() argument type must be LIST or ARRAY"))
                }
                _ => Err(Error::binder("row_to_json() argument type must be STRUCT")),
            },
            _ => Err(Error::binder(format!("{spelled}() takes exactly one argument"))),
        }
    }

    /// `enum_code`, `enum_first`, `enum_last`, `enum_range` and `enum_range_boundary`.
    ///
    /// All but the first are about the type rather than the value, so they fold here into the
    /// strings of the list, which is what the pin does too: `enum_first(NULL::mood)` is the first
    /// string and not a null. `enum_range_boundary` reads its two ends once, as the pin does, and
    /// an end that is null is that end of the list. `enum_code` is the position of each value, and
    /// is left for the kernel.
    fn enum_call(&mut self, written: &str, bound: &[ExprRef]) -> Result<Option<ExprRef>> {
        let Some(name) =
            ["enum_code", "enum_first", "enum_last", "enum_range", "enum_range_boundary"]
                .into_iter()
                .find(|name| rudb_catalog::same_name(written, name))
        else {
            return Ok(None);
        };
        let needs = || Error::binder("This function needs an ENUM as an argument");
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        let listed = |labels: &[String]| Value::List {
            element: LogicalType::Varchar,
            values: labels.iter().cloned().map(Value::Varchar).collect(),
        };
        if name == "enum_range_boundary" {
            let [start, end] = bound else { return Ok(None) };
            // Each end is an enum or a null, and a string is not taken for a label here.
            if !types.iter().all(|ty| ty.labels().is_some() || *ty == LogicalType::Null) {
                return Err(needs());
            }
            let ty = match (types[0].labels(), types[1].labels()) {
                (None, None) => return Err(needs()),
                (Some(_), Some(_)) if types[0] != types[1] => {
                    return Err(Error::binder(
                        "The parameters need to link to ONLY one enum OR be NULL ",
                    ));
                }
                (Some(_), _) => types[0].clone(),
                (None, Some(_)) => types[1].clone(),
            };
            let labels = ty.labels().unwrap_or_default();
            let mut ends = [0, labels.len().saturating_sub(1)];
            for (at, &arg) in [*start, *end].iter().enumerate() {
                match fold::value_of(self.plan(), arg) {
                    Ok(Some(Value::Varchar(label))) => {
                        ends[at] = labels.iter().position(|one| *one == label).unwrap_or(0);
                    }
                    Ok(Some(_)) => {}
                    _ => {
                        return Err(Error::not_implemented(
                            "enum_range_boundary over values that change from row to row",
                        ));
                    }
                }
            }
            let [from, to] = ends;
            let slice = if from <= to && !labels.is_empty() { &labels[from..=to] } else { &[] };
            return Ok(Some(self.add_constant(listed(slice))));
        }
        let [only] = bound else { return Ok(None) };
        let Some(labels) = types[0].labels() else { return Err(needs()) };
        let first =
            |at: Option<&String>| at.map_or(Value::Null, |label| Value::Varchar(label.clone()));
        let value = match name {
            "enum_first" => first(labels.first()),
            "enum_last" => first(labels.last()),
            "enum_range" => listed(labels),
            _ => return Ok(Some(self.enum_code(*only))),
        };
        Ok(Some(self.add_constant(value)))
    }

    /// Where each value of an `ENUM` sits in its list, as the unsigned integer it is stored in.
    ///
    /// Also what an enum is ordered and compared by, which is the reason it is not a cast: the pin
    /// refuses to cast an enum to a number other than through its string.
    pub(crate) fn enum_code(&mut self, expr: ExprRef) -> ExprRef {
        let returns = rudb_vector::enum_code_type(self.plan().expr_type(expr));
        let args = self.plan_mut().add_expr_list(&[expr]);
        let name = self.plan_mut().intern("enum_code");
        self.add_expr(Expr::Function { name, args }, returns)
    }

    /// An expression that orders the way this one should, which is itself unless it is an `ENUM`,
    /// whose order is the order of its list and not of its strings.
    pub(crate) fn by_position(&mut self, expr: ExprRef) -> ExprRef {
        if self.plan().expr_type(expr).labels().is_some() { self.enum_code(expr) } else { expr }
    }

    /// `nextval`, `currval` and `setval`, with the sequence they name looked up here.
    ///
    /// The name has to be a constant, which is the pin's rule too, and it is read as a qualified
    /// name the way the pin reads it. What the kernel gets in its place is the number of the
    /// sequence's counter, so a kernel that knows nothing about catalogs can move it. A null name
    /// is a null answer without a sequence to look for. The sequence is also recorded, which is
    /// what makes a table whose default calls `nextval` depend on it.
    fn sequence_call(&mut self, written: &str, bound: &[ExprRef]) -> Result<Option<ExprRef>> {
        let Some(name) = ["nextval", "currval", "setval"]
            .into_iter()
            .find(|name| rudb_catalog::same_name(written, name))
        else {
            return Ok(None);
        };
        if name == "setval" {
            self.setval_arguments(bound)?;
        }
        let call = self.call(name, bound.to_vec())?;
        let Expr::Function { args, .. } = *self.plan().expr(call) else {
            return Ok(Some(call));
        };
        let mut args = self.plan().expr_list(args).to_vec();
        let text = match fold::value_of(self.plan(), args[0]) {
            Ok(Some(Value::Varchar(text))) => text,
            Ok(Some(Value::Null)) => {
                let null = self.plan_mut().add_value(Value::Null);
                return Ok(Some(self.add_expr(Expr::Constant(null), LogicalType::BigInt)));
            }
            _ => {
                return Err(Error::binder(format!(
                    "The \"sequence_name\" argument in function \"{name}\" must be a constant \
                     expression"
                )));
            }
        };
        let parts = qualified_parts(&text)?;
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        let resolved = self.catalog().resolve_sequence(&parts)?;
        let id = self.catalog().sequence(&resolved)?.counter().id();
        if !self.sequences.contains(&resolved) {
            self.sequences.push(resolved);
        }
        args[0] = self.add_constant(Value::BigInt(id as i64));
        for (arg, wanted) in
            args.iter_mut().skip(1).zip([LogicalType::BigInt, LogicalType::Boolean])
        {
            *arg = self.checked_cast_to(*arg, &wanted, false)?;
        }
        let args = self.plan_mut().add_expr_list(&args);
        let name = self.plan_mut().intern(name);
        Ok(Some(self.add_expr(Expr::Function { name, args }, LogicalType::BigInt)))
    }

    /// Refuses a `setval` whose value or flag no implicit cast reaches, the way the pin's overload
    /// resolution does: an integer or a string literal can be the value and a boolean or a string
    /// literal can be the flag, and `setval('s', 5.6)` or `setval('s', 1, 1)` fits neither
    /// overload.
    fn setval_arguments(&self, bound: &[ExprRef]) -> Result<()> {
        let literal = |at: usize| {
            matches!(self.plan().expr(bound[at]), Expr::Constant(_))
                && *self.plan().expr_type(bound[at]) == LogicalType::Varchar
        };
        let fits = |at: usize, wanted: &LogicalType| {
            let ty = self.plan().expr_type(bound[at]);
            ty == wanted
                || *ty == LogicalType::Null
                || literal(at)
                || (*wanted == LogicalType::BigInt
                    && matches!(
                        ty,
                        LogicalType::TinyInt
                            | LogicalType::SmallInt
                            | LogicalType::Integer
                            | LogicalType::UTinyInt
                            | LogicalType::USmallInt
                            | LogicalType::UInteger
                    ))
        };
        let wanted = [LogicalType::BigInt, LogicalType::Boolean];
        if !(2..=3).contains(&bound.len()) || (1..bound.len()).all(|at| fits(at, &wanted[at - 1])) {
            return Ok(());
        }
        let types: Vec<LogicalType> =
            bound.iter().map(|arg| self.plan().expr_type(*arg).clone()).collect();
        Err(crate::maps::no_match(
            "setval",
            &types,
            &[
                "setval(col0 VARCHAR, col1 BIGINT) -> BIGINT",
                "setval(col0 VARCHAR, col1 BIGINT, col2 BOOLEAN) -> BIGINT",
            ],
        ))
    }

    /// The value of the setting a constant names, folded into the plan.
    ///
    /// `None` for an argument that is not a constant string, which is the one case the fold cannot
    /// cover and the one case upstream refuses. The caller falls through to the signature table for
    /// it, so the sentence about it is written once and next to the declared overload it is about.
    ///
    /// The type is the setting's `input_type` and not the shape of the text that came back, which
    /// is what makes `typeof(current_setting('threads'))` BIGINT on both engines while
    /// `typeof(current_setting('memory_limit'))` is VARCHAR. A session with no answer for a name the
    /// catalog knows is a caller that bound without a database behind it, and that is the same
    /// answer as a name nobody has, since neither one can be read.
    ///
    /// A name the catalog does not know goes to [`Binder::beyond`], because rudb has settings that
    /// are not DuckDB's and the catalog is a list of DuckDB's.
    /// `current_setting(name)` and `current_setting(name, missing_ok)` in a PostgreSQL session,
    /// folded to the text of the parameter, as PostgreSQL returns it. A name that is not a
    /// PostgreSQL parameter and has no dot can still be a rudb setting, so it goes on to
    /// [`Binder::setting`]. Only a name that neither of them knows is the error of PostgreSQL, or
    /// a null when `missing_ok` is true.
    fn postgres_setting(&mut self, bound: &[ExprRef]) -> Result<Option<ExprRef>> {
        let (argument, missing_ok) = match *bound {
            [argument] => (argument, false),
            [argument, missing] => {
                let Expr::Constant(held) = *self.plan().expr(missing) else { return Ok(None) };
                let Value::Boolean(missing_ok) = *self.plan().value(held) else { return Ok(None) };
                (argument, missing_ok)
            }
            _ => return Ok(None),
        };
        let Expr::Constant(held) = *self.plan().expr(argument) else { return Ok(None) };
        let Value::Varchar(name) = self.plan().value(held) else { return Ok(None) };
        let name = name.clone();
        let session = self.session;
        let Some(postgres) = session.postgres() else { return Ok(None) };
        if let Some(value) = postgres.settings.get(&name) {
            return Ok(Some(self.add_constant(Value::Varchar(value))));
        }
        if !name.contains('.')
            && (rudb_functions::setting_named(&name).is_some() || self.beyond(&name)?.is_some())
        {
            return self.setting(argument);
        }
        if missing_ok {
            let null = self.add_constant(Value::Null);
            return Ok(Some(self.cast_to(null, &LogicalType::Varchar)));
        }
        Err(Error::catalog(format!("unrecognized configuration parameter \"{name}\""))
            .state(SqlState::UNDEFINED_OBJECT))
    }

    fn setting(&mut self, argument: ExprRef) -> Result<Option<ExprRef>> {
        let Expr::Constant(held) = *self.plan().expr(argument) else { return Ok(None) };
        let Value::Varchar(name) = self.plan().value(held) else { return Ok(None) };
        let name = name.clone();
        // The search path is on the catalog rather than in the session, because the catalog is
        // what reads it for every name.
        if name.eq_ignore_ascii_case("schema") {
            let schema = self.catalog().default_schema().to_string();
            return Ok(Some(self.add_constant(Value::Varchar(schema))));
        }
        if name.eq_ignore_ascii_case("search_path") {
            let path = self.catalog().search_path();
            return Ok(Some(self.add_constant(Value::Varchar(path))));
        }
        let Some(known) = rudb_functions::setting_named(&name) else {
            let value = self
                .beyond(&name)?
                .ok_or_else(|| Error::catalog(rudb_functions::unknown_setting(&name)))?;
            return Ok(Some(self.add_constant(value)));
        };
        let text = self
            .session
            .get(known.name)
            .ok_or_else(|| Error::catalog(rudb_functions::unknown_setting(&name)))?;
        // A setting that is unset reads as null whatever its type says, which is what the pin
        // answers for the three of them that are unset until something writes one.
        if text == rudb_functions::UNSET {
            return Ok(Some(self.add_constant(Value::Null)));
        }
        let value = match known.input_type {
            "BOOLEAN" => Value::Boolean(text.parse().map_err(|_| {
                Error::internal(format!("{} is set to {text}, which is not a boolean", known.name))
            })?),
            "BIGINT" => Value::BigInt(text.parse().map_err(|_| {
                Error::internal(format!("{} is set to {text}, which is not a number", known.name))
            })?),
            "UBIGINT" => Value::UBigInt(text.parse().map_err(|_| {
                Error::internal(format!("{} is set to {text}, which is not a number", known.name))
            })?),
            "DOUBLE" => Value::Double(text.parse().map_err(|_| {
                Error::internal(format!("{} is set to {text}, which is not a number", known.name))
            })?),
            // `VARCHAR`, and the six `VARCHAR[]` and one `MAP` among them, which come back as the
            // text the pin prints for them rather than as a built list. A caller reading one of
            // those is reading a setting rudb carries and does not act on, so the text is the whole
            // of what there is to say about it.
            _ => Value::Varchar(text.to_string()),
        };
        Ok(Some(self.add_constant(value)))
    }

    /// A call to `getvariable` with the name it reads, which is the variable's value at its type,
    /// or a null with no type for a name nothing has set, which is what the pin answers.
    ///
    /// `None` for a name that is not a constant, which the table then refuses in the pin's words.
    fn variable(&mut self, argument: ExprRef) -> Option<ExprRef> {
        let Expr::Constant(held) = *self.plan().expr(argument) else { return None };
        let (value, ty) = match self.plan().value(held) {
            Value::Varchar(name) => match self.session.variable(name) {
                Some(found) => (found.value.clone(), found.ty.clone()),
                None => (Value::Null, LogicalType::Null),
            },
            Value::Null => (Value::Null, LogicalType::Null),
            _ => return None,
        };
        let reference = self.plan_mut().add_value(value);
        Some(self.add_expr(Expr::Constant(reference), ty))
    }

    /// `in_search_path(catalog, schema)`, which is whether that schema is one a bare name is
    /// looked for in.
    ///
    /// The pin walks its search path, which is `temp.main`, then what `SET search_path` wrote,
    /// then the default database's `main`, `system.main` and `system.pg_catalog`. An entry written
    /// without a database matches its schema under the empty name and under the default
    /// database's, which is why `in_search_path('', 'main')` is true there. Names match without
    /// regard to case.
    ///
    /// Two constants are answered here. Anything else is the same test written as a lookup in the
    /// list of pairs the path comes to, so a null on either side is a null the way it is on the
    /// pin. `None` when either side is not a string, which the table then refuses.
    fn in_search_path(&mut self, catalog: ExprRef, schema: ExprRef) -> Result<Option<ExprRef>> {
        let text = |ty: &LogicalType| matches!(ty, LogicalType::Varchar | LogicalType::Null);
        if !text(self.plan().expr_type(catalog)) || !text(self.plan().expr_type(schema)) {
            return Ok(None);
        }
        let default = self.catalog().default_catalog().to_ascii_lowercase();
        let mut pairs = Vec::new();
        for (database, name) in self.catalog().search_pairs() {
            let name = name.to_ascii_lowercase();
            if database.is_empty() {
                pairs.push(format!("{default}\u{1}{name}"));
            }
            pairs.push(format!("{}\u{1}{name}", database.to_ascii_lowercase()));
        }
        let constant = |binder: &Self, at: ExprRef| match *binder.plan().expr(at) {
            Expr::Constant(held) => Some(binder.plan().value(held).clone()),
            _ => None,
        };
        if let (Some(written), Some(named)) = (constant(self, catalog), constant(self, schema)) {
            let answer = match (written, named) {
                (Value::Varchar(written), Value::Varchar(named)) => {
                    let wanted = format!(
                        "{}\u{1}{}",
                        written.to_ascii_lowercase(),
                        named.to_ascii_lowercase()
                    );
                    Value::Boolean(pairs.contains(&wanted))
                }
                _ => Value::Null,
            };
            let reference = self.plan_mut().add_value(answer);
            return Ok(Some(self.add_expr(Expr::Constant(reference), LogicalType::Boolean)));
        }
        let separator = self.add_constant(Value::Varchar("\u{1}".to_string()));
        let catalog = self.call("lower", vec![catalog])?;
        let schema = self.call("lower", vec![schema])?;
        let key = self.call("||", vec![catalog, separator])?;
        let key = self.call("||", vec![key, schema])?;
        let values = pairs.into_iter().map(Value::Varchar).collect();
        let list = self.add_constant(Value::List { element: LogicalType::Varchar, values });
        self.call("list_contains", vec![list, key]).map(Some)
    }

    /// The value of a setting rudb has and DuckDB does not.
    ///
    /// The settings catalog cannot answer for these because it is the list of DuckDB's settings and
    /// these are not on it, deliberately, so that `duckdb_settings()` does not claim they are.
    /// `SET` already decides which side of that line a name falls on and this is the reading half
    /// of the same decision. Without it a session driving the engine through SQL can write one of
    /// these and then has no way to ask what it says.
    ///
    /// All four are here and each is read off something the binder is already holding: the row order
    /// declarations are on the catalog, and the relationship declarations, the rule switches and the
    /// seam pins are on the session. The seams were left out of this on the grounds that their names
    /// live in a crate the binder does not depend on, and the cost of that was that
    /// `SET seam.chunk.compaction = 'learned-gain'` was accepted and then
    /// `current_setting('seam.chunk.compaction')` said there is no such parameter, so a sweep had no
    /// way to confirm it was measuring what it asked for. `rudb-plan` already depends on `rudb-seam`
    /// and the binder already depends on `rudb-plan`, so naming it here adds no rank edge.
    ///
    /// A rule reads back as a boolean and the other three as the text they were written as, which is
    /// what `Database::setting` answers for all of them. A declaration nobody made reads back
    /// as the empty string rather than as null, because the empty string is what `SET cluster_by =
    /// ''` leaves behind and a setting that does not round trip is one somebody reports as a bug.
    ///
    /// `None` for a name that is not one of these either, which each caller words its own error
    /// for, because the two of them are two statements and upstream says something different about
    /// each.
    ///
    /// # Errors
    ///
    /// For a name that looks like a rule and is not, with the list of rules in it, which is the
    /// message `SET` gives for the same mistake.
    pub(crate) fn beyond(&self, name: &str) -> Result<Option<Value>> {
        if is_clustering_setting(name) {
            return Ok(Some(Value::Varchar(self.catalog().clustering())));
        }
        if Session::is_links_setting(name) {
            return Ok(Some(Value::Varchar(self.session.links().to_string())));
        }
        if let Some(enabled) = self.session.rules().named(name) {
            return Ok(Some(Value::Boolean(enabled)));
        }
        // After the rules, so that a name a rule answers for keeps answering as a rule, and out of
        // the text the session carries rather than out of a `Settings` this rank could hold.
        if let Some(pinned) = rudb_seam::Settings::written_get(self.session.seams(), name)? {
            return Ok(Some(Value::Varchar(pinned)));
        }
        if looks_like_rule(name) {
            return Err(Error::catalog(format!(
                "no rule called {name}, the rules are {}",
                rule_names()
            )));
        }
        Ok(None)
    }

    /// A call whose named arguments the parser could not put in places, because they fit none of
    /// the function's lists of parameters or fit two of them in different orders. It is refused in
    /// the pin's words, with every argument spelled the way the pin spells it. The key of a
    /// `WITHIN GROUP` is the first argument there, since that is the place it fills.
    fn refuse_named(
        &mut self,
        ast: &Ast,
        call: ast::ExprRef,
        written: &str,
        arguments: &[ast::ExprRef],
        sorted: &[ast::OrderItem],
        scope: &Scope,
    ) -> Result<ExprRef> {
        let function = written.to_ascii_lowercase();
        let mut spelled = Vec::new();
        let implicit = match sorted {
            [key] if function.starts_with("quantile_") => {
                let bound = self.bind_expr(ast, key.expr, scope)?;
                spelled.push(self.plan().expr_type(bound).to_string());
                1
            }
            _ => 0,
        };
        for &arg in arguments {
            let bound = self.bind_expr(ast, arg, scope)?;
            spelled.push(spelled_type(ast, arg, self.plan().expr_type(bound)));
        }
        let named = ast.named_args(call);
        for target in named {
            let bound = self.bind_expr(ast, target.expr, scope)?;
            let ty = spelled_type(ast, target.expr, self.plan().expr_type(bound));
            spelled.push(format!("\"{}\" := {ty}", ast.string(target.alias)));
        }
        let names: Vec<&str> = named.iter().map(|target| ast.string(target.alias)).collect();
        let ambiguous = matches!(
            rudb_parse::parameters::arrange(&function, implicit, arguments.len(), &names),
            Ok(rudb_parse::parameters::Arranged::Ambiguous)
        );
        Err(rudb_functions::named_mismatch(&function, &spelled, ambiguous))
    }

    /// `year(x)` and the other names that read one part of a date, bound as the `date_part` call
    /// they are on the pin.
    ///
    /// Each one is declared over a date, a timestamp, a zoned timestamp and an interval, so an
    /// argument that is none of those is refused under the name that was written, with the pin's
    /// list of the four. A string literal or a null could be any of them and the pin will not pick
    /// one. `julian` has no interval reading, since an interval is not a day anywhere.
    fn bind_part_shortcut(
        &mut self,
        ast: &Ast,
        name: &str,
        part: &str,
        arguments: &[ast::ExprRef],
        bound: &[ExprRef],
    ) -> Result<ExprRef> {
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        let spelled: Vec<String> =
            types.iter().zip(arguments).map(|(ty, &arg)| spelled_type(ast, arg, ty)).collect();
        let interval = name != "julian";
        let timed = TIMED_SHORTCUTS.contains(&name);
        let [only] = bound[..] else {
            return Err(part_mismatch(name, &spelled, interval, timed));
        };
        let wanted = match &types[0] {
            LogicalType::Date | LogicalType::Timestamp | LogicalType::TimestampTz => None,
            LogicalType::Interval if interval => None,
            LogicalType::Time | LogicalType::TimeTz | LogicalType::TimeNs if timed => None,
            LogicalType::TimestampS | LogicalType::TimestampMs | LogicalType::TimestampNs => {
                Some(LogicalType::Timestamp)
            }
            _ if matches!(spelled[0].as_str(), "STRING_LITERAL" | "\"NULL\"") => {
                return Err(Error::binder(format!(
                    "Could not choose a best candidate function for the function call \"{name}({})\". In order to select one, please add explicit type casts.",
                    spelled[0]
                )));
            }
            _ => return Err(part_mismatch(name, &spelled, interval, timed)),
        };
        let when = match wanted {
            Some(ty) => self.cast_to(only, &ty),
            None => only,
        };
        let spec = self.add_constant(Value::Varchar(part.to_string()));
        self.call("date_part", vec![spec, when])
    }

    /// The answer type of a `date_part`, which is the one call whose type comes from the value of
    /// an argument rather than from the type of one.
    ///
    /// Upstream declares the function as a DOUBLE and narrows it to a BIGINT when the specifier is
    /// a constant naming a part that is whole. So `typeof(date_part('minute', ts))` is BIGINT,
    /// `typeof(date_part('epoch', ts))` is DOUBLE because seconds carry a fraction, and
    /// `typeof(date_part(p, ts))` over a column of specifiers is DOUBLE whatever that column turns
    /// out to hold, since nothing at binding time can know. All three were measured.
    ///
    /// A specifier that names nothing is left alone here rather than refused, so that the message
    /// about it comes from the one place that writes it, which is the kernel.
    ///
    /// `round` and `trunc` of a decimal with a count of digits are the other two, for the same
    /// reason. The scale of the answer is the count when that is a literal below the scale the
    /// decimal has, none at all when the count is negative, and the scale the decimal has otherwise,
    /// so `round(1.2345, 2)` is a `DECIMAL(5,2)` and `round(12.345, -1)` a `DECIMAL(5,0)`.
    fn narrowed_part(&self, name: &str, args: &[ExprRef], returns: LogicalType) -> LogicalType {
        if matches!(name, "round" | "trunc" | "round_even") {
            return self.narrowed_scale(args, returns);
        }
        if matches!(name, "strptime" | "try_strptime") {
            let Some(&format) = args.get(1) else { return returns };
            let Ok(Some(format)) = fold::value_of(self.plan(), format) else { return returns };
            return match rudb_kernels::strptime::Formats::from_value(&format) {
                Ok(Some(formats)) => formats.returns(),
                _ => returns,
            };
        }
        if name != "date_part" {
            return returns;
        }
        let Some(&spec) = args.first() else { return returns };
        let Expr::Constant(value) = *self.plan().expr(spec) else { return returns };
        let Value::Varchar(spelling) = self.plan().value(value) else { return returns };
        part_type(spelling)
    }

    /// The struct `date_part` answers for a list of parts, with a field named for each part and
    /// typed the way the part would be on its own.
    ///
    /// The pin reads the list once when it binds, so a list that is not a constant is refused, and
    /// so are an empty one, a null in it and a part named twice, all in the pin's words. A part
    /// that names nothing is left for the kernel to refuse, as it is for a single part.
    fn part_list(&self, list: ExprRef) -> Result<LogicalType> {
        let Ok(Some(Value::List { values, .. })) = fold::value_of(self.plan(), list) else {
            return Err(Error::binder(
                "The \"part_list\" argument in function \"date_part\" must be a constant expression",
            ));
        };
        if values.is_empty() {
            return Err(Error::binder("\"date_part\" requires non-empty lists of part names"));
        }
        let mut fields: Vec<Field> = Vec::with_capacity(values.len());
        for value in values {
            let Value::Varchar(spelling) = value else {
                return Err(Error::binder("NULL struct entry name in \"date_part\""));
            };
            if fields.iter().any(|field| field.name == spelling) {
                return Err(Error::binder(format!(
                    "Duplicate struct entry name \"{spelling}\" in \"date_part\""
                )));
            }
            let ty = part_type(&spelling);
            fields.push(Field::new(spelling, ty));
        }
        Ok(LogicalType::Struct(fields))
    }

    /// The option string of `regexp_extract_all`, which the pin reads once when it binds, so one
    /// that is not a constant is refused and so is a null one, each in the pin's words.
    fn settled_options(&self, args: &[ExprRef]) -> Result<()> {
        let Some(&options) = args.get(3) else { return Ok(()) };
        match fold::value_of(self.plan(), options) {
            Ok(Some(Value::Null)) => {
                Err(Error::invalid_input("Regex options field must not be NULL"))
            }
            Ok(Some(_)) => Ok(()),
            _ => Err(Error::binder(
                "The \"options\" argument in function \"regexp_extract_all\" must be a constant expression",
            )),
        }
    }

    /// The structs `regexp_extract_all` answers for a list of names, with a string field for each
    /// name holding the group in the same place.
    ///
    /// The pin reads the pattern and the list once when it binds, so either one that is not a
    /// constant is refused. So are a null list, an empty one, a null name, a name given twice, a
    /// pattern that does not compile and more names than the pattern has groups, all in the pin's
    /// words.
    fn group_names(&self, args: &[ExprRef]) -> Result<LogicalType> {
        let (Some(&pattern), Some(&names)) = (args.get(1), args.get(2)) else {
            return Err(Error::internal("regexp_extract_all with a list and no pattern"));
        };
        let Ok(Some(Value::Varchar(pattern))) = fold::value_of(self.plan(), pattern) else {
            return Err(Error::binder(
                "\"regexp_extract_all\" with LIST requires a constant pattern",
            ));
        };
        let values = match fold::value_of(self.plan(), names) {
            Ok(Some(Value::List { values, .. })) => values,
            Ok(Some(_)) => {
                return Err(Error::binder("Group specification must be a non-NULL LIST"));
            }
            _ => {
                return Err(Error::binder(
                    "The \"name_list\" argument in function \"regexp_extract_all\" must be a constant expression",
                ));
            }
        };
        if values.is_empty() {
            return Err(Error::binder("Group name list must be non-empty"));
        }
        let mut fields: Vec<Field> = Vec::with_capacity(values.len());
        for value in values {
            let Value::Varchar(name) = value else {
                return Err(Error::binder("NULL group name in regexp_extract_all"));
            };
            if fields.iter().any(|field| field.name == name) {
                return Err(Error::binder(format!(
                    "Duplicate group name '{name}' in regexp_extract_all"
                )));
            }
            fields.push(Field::new(name, LogicalType::Varchar));
        }
        let spelling = match args.get(3).map(|&options| fold::value_of(self.plan(), options)) {
            Some(Ok(Some(Value::Varchar(spelling)))) => spelling,
            _ => String::new(),
        };
        let options = rudb_regex::Options::parse(&spelling)?;
        let regex = rudb_regex::Regex::with_options(&pattern, options).map_err(|error| {
            Error::binder(format!("Pattern failed to parse: {}", error.message()))
        })?;
        if regex.groups() < fields.len() {
            return Err(Error::binder(format!(
                "Not enough capturing groups ({}) for provided names ({})",
                regex.groups(),
                fields.len()
            )));
        }
        Ok(LogicalType::list(LogicalType::Struct(fields)))
    }

    /// The scale a decimal keeps once rounded to the digits the second argument asks for.
    fn narrowed_scale(&self, args: &[ExprRef], returns: LogicalType) -> LogicalType {
        let LogicalType::Decimal { width, scale } = returns else { return returns };
        let Some(&digits) = args.get(1) else { return returns };
        let Ok(Some(digits)) = fold::value_of(self.plan(), digits) else { return returns };
        let Some(digits) = digits.as_i64() else { return returns };
        let kept = if digits < 0 { 0 } else { u8::try_from(digits).unwrap_or(u8::MAX).min(scale) };
        LogicalType::Decimal { width, scale: kept }
    }

    /// Casts the string literals among the arguments of a call that promotes across all of them to
    /// the type the other arguments meet at, which is how the pin types `coalesce(1, '2')` as an
    /// `INTEGER` rather than refusing it.
    ///
    /// The pin folds the types left to right and a string literal takes the type of whatever it
    /// meets. Two string literals, or a string literal and a null, meet at `VARCHAR`, which is no
    /// longer a literal, so `coalesce(NULL, '2', 3)` is still refused. `kinds` says which arguments
    /// were literals in the query, since a constant `'2'::VARCHAR` is not one. Two types that do
    /// not meet are refused here with the pin's sentence for the call, which names an integer
    /// literal as `INTEGER_LITERAL` while it has not met anything yet.
    fn adopt_literals(
        &mut self,
        function: &str,
        kinds: &[Literal],
        bound: &mut [ExprRef],
    ) -> Result<()> {
        let mut met: Option<(LogicalType, Literal)> = None;
        for (&arg, &kind) in bound.iter().zip(kinds) {
            let ty = self.plan().expr_type(arg).clone();
            met = Some(match (met, kind) {
                (None, _) => (ty, kind),
                (Some((_, Literal::Text)), Literal::Text) => (LogicalType::Varchar, Literal::Other),
                (Some((LogicalType::Null, _)), Literal::Text) => {
                    (LogicalType::Varchar, Literal::Other)
                }
                (Some((before, _)), Literal::Text) => (before, Literal::Other),
                (Some((_, Literal::Text)), _) if ty == LogicalType::Null => {
                    (LogicalType::Varchar, Literal::Other)
                }
                (Some((_, Literal::Text)), _) => (ty, Literal::Other),
                (Some((before, held)), _) => match before.promote(&ty) {
                    Some(common) => (common, Literal::Other),
                    None => {
                        let name = |ty: &LogicalType, kind: Literal| match kind {
                            Literal::Integer => "INTEGER_LITERAL".to_string(),
                            _ => ty.to_string(),
                        };
                        let (left, right) = (name(&before, held), name(&ty, kind));
                        return Err(Error::binder(if function == "coalesce" {
                            format!(
                                "Cannot mix values of type {left} and {right} in COALESCE operator - an explicit cast is required"
                            )
                        } else {
                            format!(
                                "Cannot combine types of {left} and {right} - an explicit cast is required"
                            )
                        }));
                    }
                },
            });
        }
        let Some((target, held)) = met else { return Ok(()) };
        if held == Literal::Text || matches!(target, LogicalType::Varchar | LogicalType::Null) {
            return Ok(());
        }
        for (arg, &kind) in bound.iter_mut().zip(kinds) {
            if kind == Literal::Text {
                *arg = self.cast_to(*arg, &target);
            }
        }
        Ok(())
    }

    /// A `coalesce` whose later arguments could fail on a row it never reads, written as the `CASE`
    /// it means so that they are only read where every argument before them was null.
    ///
    /// The pin evaluates `coalesce` lazily, so `coalesce(1, 'x')` answers 1 rather than failing to
    /// read `'x'` as a number. The kernel reads every argument of every row, which is faster and
    /// gives the same answer whenever nothing can fail, so the `CASE` is only used when a later
    /// argument casts text, the one conversion that fails on the data rather than on the types.
    fn lazy_coalesce(&mut self, call: ExprRef) -> Result<ExprRef> {
        let Expr::Function { args, .. } = *self.plan().expr(call) else { return Ok(call) };
        let args = self.plan().expr_list(args).to_vec();
        if !args[1..].iter().any(|&arg| casts_text(self.plan(), arg)) {
            return Ok(call);
        }
        let ty = self.plan().expr_type(call).clone();
        let (&otherwise, tested) = args.split_last().expect("more than one argument");
        let mut arms = Vec::with_capacity(tested.len());
        for &then in tested {
            let when = self.against_null(CompareOp::DistinctFrom, then)?;
            arms.push(Arm { when, then });
        }
        let arms = self.plan_mut().add_arms(&arms);
        Ok(self.add_expr(Expr::Case { arms, otherwise: Some(otherwise) }, ty))
    }

    /// A cast to `ty`, or the expression itself when it is already that type.
    pub(crate) fn cast_to(&mut self, expr: ExprRef, ty: &LogicalType) -> ExprRef {
        if self.plan().expr_type(expr) == ty {
            return expr;
        }
        self.resolve_placeholder(expr, ty);
        self.add_expr(Expr::Cast { input: expr, try_cast: false }, ty.clone())
    }

    /// A cast checked against the session choices that can forbid a conversion.
    pub(crate) fn checked_cast_to(
        &mut self,
        expr: ExprRef,
        ty: &LogicalType,
        try_cast: bool,
    ) -> Result<ExprRef> {
        let from = self.plan().expr_type(expr);
        if self.semantics.disable_timestamptz_casts()
            && matches!(from, LogicalType::Date | LogicalType::Timestamp)
            && *ty == LogicalType::TimestampTz
        {
            return Err(Error::binder(
                "Casting from TIMESTAMP to TIMESTAMP WITH TIME ZONE without an explicit time zone has been disabled  - use \"AT TIME ZONE ...\"",
            ));
        }
        if from == ty {
            return Ok(expr);
        }
        struct_members_meet(from, ty)?;
        self.resolve_placeholder(expr, ty);
        Ok(self.add_expr(Expr::Cast { input: expr, try_cast }, ty.clone()))
    }

    /// Gives the parameter `expr` stands for the type `ty`, when `expr` is the null of a parameter
    /// of no known type and the statement is being described.
    pub(crate) fn resolve_placeholder(&self, expr: ExprRef, ty: &LogicalType) {
        let Some(placeholders) = self.parameters.placeholders() else { return };
        for (name, ty) in self.placeholders_under(expr, ty, 0) {
            placeholders.resolve(&name, &ty);
        }
    }

    /// Gives the parameter that `expr` stands for the PostgreSQL type `declared`, which a cast or
    /// the declaration of the column that `expr` is stored into wrote. `expr` has the logical type
    /// of `declared`, and the cast to that type can be on `expr` or inside the `VALUES` that it
    /// reads, so one cast is looked through, and only one, because `$1::int` stored into a
    /// `varchar` column is an `int4` parameter.
    pub(crate) fn resolve_written(&self, expr: ExprRef, declared: DeclaredType) {
        let Some(placeholders) = self.parameters.placeholders() else { return };
        let ty = self.plan().expr_type(expr).clone();
        for (name, _) in self.placeholders_under(expr, &ty, 1) {
            placeholders.resolve_written(&name, declared);
        }
    }

    /// The parameters of no known type that `expr` stands for when it is given the type `ty`,
    /// each with the type that it gets. The way from `expr` to a parameter can go through at most
    /// `casts` casts.
    ///
    /// A column is followed back to the projection or the `VALUES` that makes it, since that is
    /// how a parameter in a `SET`, in an `INSERT ... SELECT` or in a `VALUES` of several rows gets
    /// to the cast to the type of the column it is written to. A row is followed into its fields,
    /// so in `(a, c) = ($1, $2)` each parameter takes the type of its own column.
    fn placeholders_under(
        &self,
        expr: ExprRef,
        ty: &LogicalType,
        casts: usize,
    ) -> Vec<(String, LogicalType)> {
        let mut found = Vec::new();
        if self.placeholders.is_empty() {
            return found;
        }
        let mut pending = vec![(expr, ty.clone(), casts)];
        while let Some((expr, ty, casts)) = pending.pop() {
            if let Some((_, name)) = self.placeholders.iter().find(|(held, _)| *held == expr) {
                found.push((name.clone(), ty));
                continue;
            }
            let plan = self.plan();
            match plan.expr(expr) {
                Expr::Cast { input, .. } if casts > 0 => pending.push((*input, ty, casts - 1)),
                Expr::Function { name, args }
                    if plan.string(*name) == crate::structs::STRUCT_PACK
                        && matches!(ty, LogicalType::Struct(_)) =>
                {
                    if let LogicalType::Struct(fields) = &ty {
                        for (&arg, field) in plan.expr_list(*args).iter().zip(fields) {
                            pending.push((arg, field.ty.clone(), casts));
                        }
                    }
                }
                Expr::Column(binding) => {
                    let column = binding.column as usize;
                    for node in 0..plan.node_count() {
                        match plan.node(node as NodeRef) {
                            Node::Project { index, exprs, .. } if *index == binding.table => {
                                if let Some(&made) = plan.expr_list(*exprs).get(column) {
                                    pending.push((made, ty.clone(), casts));
                                }
                            }
                            Node::Values { index, rows, .. } if *index == binding.table => {
                                for &row in plan.row_list(*rows) {
                                    if let Some(&made) = plan.expr_list(row).get(column) {
                                        pending.push((made, ty.clone(), casts));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        found
    }

    /// Whether `expr` is the null of a parameter of no known type, in a statement that is being
    /// described.
    pub(crate) fn is_placeholder(&self, expr: ExprRef) -> bool {
        self.placeholders.iter().any(|(held, _)| *held == expr)
    }

    /// A comparison, with both sides brought to the type they meet at.
    pub(crate) fn compare(
        &mut self,
        op: CompareOp,
        left: ExprRef,
        right: ExprRef,
    ) -> Result<ExprRef> {
        let left_type = self.plan().expr_type(left).clone();
        let right_type = self.plan().expr_type(right).clone();
        let equality = matches!(
            op,
            CompareOp::Equal
                | CompareOp::NotEqual
                | CompareOp::DistinctFrom
                | CompareOp::NotDistinctFrom
        );
        let common = match comparison_type(&left_type, &right_type) {
            Some(common) => common,
            None if equality => forced_type(&left_type, &right_type),
            None => {
                return Err(Error::binder(format!(
                    "Cannot compare values of type {left_type} and type {right_type} - an explicit cast is required"
                )));
            }
        };
        let left = self.checked_cast_to(left, &common, false)?;
        let right = self.checked_cast_to(right, &common, false)?;
        let (left, right) = (self.by_position(left), self.by_position(right));
        Ok(self.add_expr(Expr::Compare { op, left, right }, LogicalType::Boolean))
    }

    /// An `AND` or an `OR`, flattened into any child that uses the same connective.
    pub(crate) fn conjunction(&mut self, op: ConjunctionOp, children: Vec<ExprRef>) -> ExprRef {
        let mut flat: Vec<ExprRef> = Vec::with_capacity(children.len());
        for child in children {
            match self.plan().expr(child).clone() {
                Expr::Conjunction { op: inner, children } if inner == op => {
                    flat.extend_from_slice(self.plan().expr_list(children));
                }
                _ => flat.push(child),
            }
        }
        match flat.as_slice() {
            [] => self.add_constant(Value::Boolean(op == ConjunctionOp::And)),
            [only] => *only,
            _ => {
                let children = self.plan_mut().add_expr_list(&flat);
                self.add_expr(Expr::Conjunction { op, children }, LogicalType::Boolean)
            }
        }
    }

    /// Brings a predicate to `BOOLEAN`, which is what every place that takes one requires.
    pub(crate) fn as_boolean(&mut self, expr: ExprRef, what: &str) -> Result<ExprRef> {
        let ty = self.plan().expr_type(expr).clone();
        match ty {
            LogicalType::Boolean => Ok(expr),
            LogicalType::Null | LogicalType::Varchar => {
                Ok(self.cast_to(expr, &LogicalType::Boolean))
            }
            ref numeric if numeric.is_numeric() => Ok(self.cast_to(expr, &LogicalType::Boolean)),
            other => Err(Error::binder(format!(
                "Cannot use a value of type {other} as a {what} condition"
            ))),
        }
    }

    fn against_null(&mut self, op: CompareOp, expr: ExprRef) -> Result<ExprRef> {
        let null = self.add_constant(Value::Null);
        self.compare(op, expr, null)
    }

    fn against_boolean(&mut self, op: CompareOp, expr: ExprRef, wanted: bool) -> Result<ExprRef> {
        let condition = self.as_boolean(expr, "IS")?;
        let constant = self.add_constant(Value::Boolean(wanted));
        self.compare(op, condition, constant)
    }
}

/// What an argument of `coalesce`, `greatest` or `least` was written as, for the pin's rule that a
/// literal takes the type it meets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Literal {
    /// A string literal, `'2'`.
    Text,
    /// A whole number written out, `2`, which the pin's messages call an `INTEGER_LITERAL`.
    Integer,
    /// Anything else.
    Other,
}

/// Whether an expression casts text to another type anywhere inside it, which is the conversion
/// that can fail on one row and not the next.
fn casts_text(plan: &Plan, expr: ExprRef) -> bool {
    match *plan.expr(expr) {
        Expr::Cast { input, try_cast } => {
            (!try_cast && *plan.expr_type(input) == LogicalType::Varchar) || casts_text(plan, input)
        }
        Expr::Compare { left, right, .. } => casts_text(plan, left) || casts_text(plan, right),
        Expr::Conjunction { children: args, .. } | Expr::Function { args, .. } => {
            plan.expr_list(args).iter().any(|&arg| casts_text(plan, arg))
        }
        Expr::Case { arms, otherwise } => {
            plan.arm_list(arms)
                .iter()
                .any(|arm| casts_text(plan, arm.when) || casts_text(plan, arm.then))
                || otherwise.is_some_and(|otherwise| casts_text(plan, otherwise))
        }
        _ => false,
    }
}

/// The type two branches of a `CASE` meet at.
fn meet(left: &LogicalType, right: &LogicalType) -> Result<LogicalType> {
    left.promote(right).ok_or_else(|| {
        Error::binder(format!("Cannot mix values of type {left} and type {right} in a CASE"))
    })
}

/// Whether an expression contains an aggregate call anywhere inside it.
///
/// This decides whether a select block aggregates at all, which has to be known before the target
/// list is bound because the answer changes what every column reference in it means.
pub(crate) fn has_aggregate(ast: &Ast, expr: ast::ExprRef) -> bool {
    aggregating(ast, expr, &|_| false)
}

/// Whether an expression has an aggregate in it, where `user` says which names are macros a user
/// made whose bodies aggregate.
pub(crate) fn aggregating(ast: &Ast, expr: ast::ExprRef, user: &dyn Fn(&str) -> bool) -> bool {
    if expr == NONE {
        return false;
    }
    match ast.expr(expr) {
        ast::Expr::Star { .. }
        | ast::Expr::Columns { .. }
        | ast::Expr::Column { .. }
        | ast::Expr::Positional { .. }
        | ast::Expr::Literal { .. }
        | ast::Expr::Parameter { .. }
        | ast::Expr::Default => false,
        ast::Expr::Unary { operand, .. } => aggregating(ast, operand, user),
        ast::Expr::Binary { left, right, .. } => {
            aggregating(ast, left, user) || aggregating(ast, right, user)
        }
        ast::Expr::Function { name, args, .. } => {
            let written = ast.name(name).last().unwrap_or_default();
            kind_of(written) == Some(FunctionKind::Aggregate)
                || rudb_catalog::same_name(written, "every")
                || crate::macros::aggregates(written)
                || user(written)
                || ast.expr_list(args).iter().any(|&arg| aggregating(ast, arg, user))
        }
        ast::Expr::Cast { operand, .. } => aggregating(ast, operand, user),
        ast::Expr::Case { operand, arms, otherwise } => {
            aggregating(ast, operand, user)
                || aggregating(ast, otherwise, user)
                || ast
                    .arm_list(arms)
                    .iter()
                    .any(|arm| aggregating(ast, arm.when, user) || aggregating(ast, arm.then, user))
        }
        ast::Expr::Between { operand, low, high, .. } => {
            aggregating(ast, operand, user)
                || aggregating(ast, low, user)
                || aggregating(ast, high, user)
        }
        ast::Expr::In { operand, list, .. } => {
            aggregating(ast, operand, user)
                || ast.expr_list(list).iter().any(|&item| aggregating(ast, item, user))
        }
        ast::Expr::InSubquery { operand, .. } => aggregating(ast, operand, user),
        ast::Expr::QuantifiedSubquery { operand, .. } => aggregating(ast, operand, user),
        ast::Expr::QuantifiedArray { operand, array, .. } => {
            aggregating(ast, operand, user) || aggregating(ast, array, user)
        }
        ast::Expr::Lambda { body, .. } => aggregating(ast, body, user),
        ast::Expr::Row { items } => {
            ast.expr_list(items).iter().any(|&item| aggregating(ast, item, user))
        }
        ast::Expr::List { items } | ast::Expr::Struct { values: items, .. } => {
            ast.expr_list(items).iter().any(|&item| aggregating(ast, item, user))
        }
        // A window call is not an aggregate and is evaluated after the grouping rather than by it,
        // but what it is given to read can be one: `sum(count(x)) OVER ()` aggregates the block.
        // The partition and the order keys count for the same reason.
        ast::Expr::Window { args, spec, order, .. } => {
            let held = ast.window(spec);
            ast.expr_list(args).iter().any(|&arg| aggregating(ast, arg, user))
                || ast.order_list(order).iter().any(|item| aggregating(ast, item.expr, user))
                || ast.expr_list(held.partition).iter().any(|&key| aggregating(ast, key, user))
                || ast.order_list(held.order).iter().any(|item| aggregating(ast, item.expr, user))
        }
        // A subquery has its own aggregation and does not make the outer block aggregate.
        ast::Expr::Subquery { .. } | ast::Expr::Exists { .. } => false,
    }
}

/// Refuses a cast between two structs that share no field name, anywhere down the two types.
///
/// A struct casts to another by name on the pin, so a target field with no source is null and a
/// source field with no target is dropped. When nothing matches at all the cast would be a struct
/// of nulls, and the pin refuses that in these words rather than answering it.
fn struct_members_meet(from: &LogicalType, to: &LogicalType) -> Result<()> {
    match (from, to) {
        (LogicalType::List(from), LogicalType::List(to)) => struct_members_meet(from, to),
        (LogicalType::Struct(source), LogicalType::Struct(target))
            if Field::unnamed(source) || Field::unnamed(target) =>
        {
            if source.len() != target.len() {
                return Err(Error::mismatch_type(format!(
                    "Type {from} does not match with {to}. Cannot cast STRUCTs of different size"
                )));
            }
            for (one, field) in source.iter().zip(target) {
                struct_members_meet(&one.ty, &field.ty)?;
            }
            Ok(())
        }
        (LogicalType::Struct(source), LogicalType::Struct(target)) => {
            let mut matched = false;
            for field in target {
                let found = source.iter().find(|one| one.name.eq_ignore_ascii_case(&field.name));
                if let Some(one) = found {
                    matched = true;
                    struct_members_meet(&one.ty, &field.ty)?;
                }
            }
            if matched || source.is_empty() || target.is_empty() {
                return Ok(());
            }
            Err(Error::binder(format!(
                "STRUCT to STRUCT cast must have at least one matching member, inputs are ({from}) \
                 and ({to})"
            )))
        }
        _ => Ok(()),
    }
}

/// An identifier as a generated name writes it, which is quoted when it has to be.
///
/// DuckDB writes every identifier inside a generated name through its deparser, so
/// `SELECT min(name)` comes back as `min("name")` and `SELECT trim(' a ')` as `"trim"(' a ')`,
/// while `SELECT min(alias)` comes back bare. The rule is in [`rudb_parse::quoted`], which is where
/// the keyword table it asks is, and the `sql` column of `duckdb_tables()` asks the same one.
/// Per #251.
fn quoted(text: &str) -> String {
    rudb_parse::quoted(text)
}

/// The `FILTER` part of a generated name, which is empty when the call was written without one.
///
/// It is part of the name for the same reason `DISTINCT` is. `sum(i)` and the same sum under a
/// predicate are two different answers, so a heading that called them the same thing would be
/// naming one of them wrongly. The word `WHERE` is printed whether or not it was written, because
/// that is what upstream prints.
fn named_filter(ast: &Ast, filter: ast::ExprRef, semantics: Semantics) -> String {
    if filter == NONE {
        String::new()
    } else {
        format!(" FILTER (WHERE {})", describe(ast, filter, semantics))
    }
}

/// The name a target gets when the query did not give it one.
///
/// DuckDB uses the text the user wrote. The tokens are gone by the time the binder runs, so this
/// writes the expression back out in a shape close enough to be recognisable, which is what the
/// name is for.
///
/// Close enough is not quite the standard, though. These names are what a client reads back as the
/// column headings, so a difference here is a difference a caller sees on every query that does not
/// write `AS`, which is most of ClickBench. The shapes that are known to be exactly DuckDB's are
/// under test in `crates/rudb/tests/clickbench.rs`, which compares the whole result of all forty
/// three against the answers duckdb gives for the same file.
pub(crate) fn describe(ast: &Ast, expr: ast::ExprRef, semantics: Semantics) -> String {
    match ast.expr(expr) {
        ast::Expr::Star { qualifier, .. } => {
            if qualifier.is_empty() {
                "*".to_string()
            } else {
                format!("{}.*", ast.name_text(qualifier))
            }
        }
        // A column inside an expression keeps every part it was written with, so `a.i + 1` is
        // named `(a.i + 1)`. A column on its own is named by its last part, which the caller sees
        // to before it gets here.
        ast::Expr::Column { name } => ast.name(name).map(quoted).collect::<Vec<_>>().join("."),
        ast::Expr::Columns { .. } | ast::Expr::Positional { .. } => {
            rudb_parse::deparse::expression(ast, expr)
        }
        // The deparser is the answer for a window and not an approximation of one. Every other
        // shape here is written out again because the name DuckDB gives it is not quite what its
        // own deparser would write, and a window is the one where the two agree.
        ast::Expr::Window { .. } => rudb_parse::deparse::expression(ast, expr),
        ast::Expr::Literal { kind, text } => match kind {
            LiteralKind::Null => "NULL".to_string(),
            LiteralKind::True => "true".to_string(),
            LiteralKind::False => "false".to_string(),
            LiteralKind::String => format!("'{}'", ast.string(text)),
            LiteralKind::Blob => format!("'{}'::BLOB", ast.string(text)),
            LiteralKind::Number => number_name(ast.string(text)),
        },
        // A prefix operator is a function call with the argument in brackets, so `-i` is named
        // `-(i)` rather than `-i`. A minus in front of a whole number is the exception, because
        // DuckDB's grammar folds that sign into the number as it reads it: `-1` and `-(1)` are
        // both named `-1` and `-(-1)` is named `1`. It is whole numbers only and it is the minus
        // only, so `-(1.5)` is `-(1.5)` and `+1` is `+(1)`.
        //
        // The four `IS TRUE` spellings are named after what they mean rather than what was
        // written, since each of them is a comparison against a boolean that treats null as a
        // value. `IS UNKNOWN` is `IS NULL` with a word that only makes sense for a boolean, and
        // the name is the one it shares.
        ast::Expr::Unary { op, operand } => {
            let inner = describe(ast, operand, semantics);
            match op {
                UnaryOp::Not => format!("(NOT {inner})"),
                UnaryOp::Negate => match (whole_number(ast, operand), signed(ast, expr, operand)) {
                    (Some(number), _) => flip(&number),
                    (None, Some(number)) => number,
                    (None, None) => format!("-({inner})"),
                },
                UnaryOp::Plus => format!("+({inner})"),
                UnaryOp::BitNot => format!("~({inner})"),
                UnaryOp::Factorial => format!("factorial({inner})"),
                UnaryOp::IsNull => format!("({inner} IS NULL)"),
                UnaryOp::IsNotNull => format!("({inner} IS NOT NULL)"),
                UnaryOp::IsTrue => format!("(CAST({inner} AS BOOLEAN) IS NOT DISTINCT FROM true)"),
                UnaryOp::IsNotTrue => format!("(CAST({inner} AS BOOLEAN) IS DISTINCT FROM true)"),
                UnaryOp::IsFalse => {
                    format!("(CAST({inner} AS BOOLEAN) IS NOT DISTINCT FROM false)")
                }
                UnaryOp::IsNotFalse => format!("(CAST({inner} AS BOOLEAN) IS DISTINCT FROM false)"),
                UnaryOp::IsUnknown => format!("({inner} IS NULL)"),
                UnaryOp::IsNotUnknown => format!("({inner} IS NOT NULL)"),
            }
        }
        // Most binary operators are named as they were written, in brackets. The ones that are
        // not are the ones that resolve to a function with a different name, and a name that says
        // the operator where DuckDB says the function is a name a client keys a row by and does
        // not find.
        ast::Expr::Binary { op, left, right } => {
            let (left, right) = (describe(ast, left, semantics), describe(ast, right, semantics));
            match op {
                BinaryOp::Regex => regex_name(&left, &right, false, false, semantics),
                BinaryOp::NotRegex => regex_name(&left, &right, false, true, semantics),
                BinaryOp::RegexInsensitive => regex_name(&left, &right, true, false, semantics),
                BinaryOp::NotRegexInsensitive => regex_name(&left, &right, true, true, semantics),
                BinaryOp::SimilarTo => format!("regexp_full_match({left}, {right})"),
                BinaryOp::NotSimilarTo => format!("(NOT regexp_full_match({left}, {right}))"),
                // The one operator DuckDB names with no brackets around it at all.
                BinaryOp::Collate => format!("{left} COLLATE {right}"),
                BinaryOp::Divide if semantics.integer_division() => {
                    format!("({left} // {right})")
                }
                _ => format!("({left} {} {right})", name_spelling(ast, op)),
            }
        }
        ast::Expr::Function { name, args, distinct, filter } => {
            let written = ast.name(name).last().unwrap_or_default();
            let starred = ast
                .expr_list(args)
                .iter()
                .any(|&arg| matches!(ast.expr(arg), ast::Expr::Star { .. }));
            // `count()` with nothing in it is named after the function it really is, the same way
            // `count(*)` is, and it is the one spelling of the three that does not survive as
            // written.
            let empty = ast.expr_list(args).is_empty();
            // A call that hands out its state says so after everything else it was written with.
            let exported = if ast.exports_state(expr) { " EXPORT_STATE" } else { "" };
            if (starred || empty) && rudb_catalog::same_name(written, "count") {
                return format!("count_star(){}{exported}", named_filter(ast, filter, semantics));
            }
            if let Some(name) = subscript_name(ast, expr, written, args, semantics) {
                return name;
            }
            // The name goes to lower case, which is the one place a spelling from the query is not
            // kept. DuckDB's parser folds a function name as it reads it and the name it prints
            // here is the folded one, so `SELECT SUM(x)` comes back as a column called `sum(x)`.
            // A column name is not folded, because that one comes from the catalog rather than
            // from the query, which is why `output_name` asks the scope first and only falls
            // through to here.
            // `COALESCE` is the exception, and it is one because it is not a function name upstream.
            // It is an operator there, so there was nothing for the parser to fold and the name it
            // prints is the operator's own spelling: `coalesce(NULL, 1)` and `ifnull(NULL, 1)` both
            // come back as a column called `COALESCE(NULL, 1)`.
            let name = if rudb_catalog::same_name(written, "coalesce") {
                "COALESCE".to_string()
            } else if rudb_catalog::same_name(written, "try") {
                "TRY".to_string()
            } else {
                quoted(&written.to_ascii_lowercase())
            };
            // `DISTINCT` is part of the name because it is part of what was computed.
            // `count(UserID)` and `count(DISTINCT UserID)` are two different answers and a result
            // that called them both the first one would be reporting the wrong one.
            let word = if distinct { "DISTINCT " } else { "" };
            let (list, named) = ast.written_args(expr, args);
            let mut arguments: Vec<String> =
                list.iter().map(|&arg| describe(ast, arg, semantics)).collect();
            for target in named {
                let value = describe(ast, target.expr, semantics);
                arguments.push(format!("{} := {value}", quoted(ast.string(target.alias))));
            }
            // An `ORDER BY` inside the call is part of the name whether or not the aggregate reads
            // it, so `sum(x ORDER BY y)` keeps it too.
            let sorted: Vec<String> = ast
                .aggregate_order(expr)
                .iter()
                .map(|item| {
                    let mut key = describe(ast, item.expr, semantics);
                    key += match item.order {
                        ast::Order::Unstated => "",
                        ast::Order::Ascending => " ASC",
                        ast::Order::Descending => " DESC",
                    };
                    key += match item.nulls {
                        ast::Nulls::Unstated => "",
                        ast::Nulls::First => " NULLS FIRST",
                        ast::Nulls::Last => " NULLS LAST",
                    };
                    key
                })
                .collect();
            let sorted = if sorted.is_empty() {
                String::new()
            } else {
                format!(" ORDER BY {}", sorted.join(", "))
            };
            format!(
                "{name}({word}{}{sorted}){}{exported}",
                arguments.join(", "),
                named_filter(ast, filter, semantics)
            )
        }
        ast::Expr::Cast { operand, ty, try_cast } => {
            let word = if try_cast { "TRY_CAST" } else { "CAST" };
            // The type is named the way the grammar leaves it: a keyword type such as `int` or
            // `numeric(4,1)` comes back under its one name, `INTEGER` and `DECIMAL(4, 1)`, and a
            // name the grammar has no rule for, `tinyint` or `double`, keeps its spelling.
            let ty = rudb_parse::deparse::typename(ast.string(ty));
            format!("{word}({} AS {ty})", describe(ast, operand, semantics))
        }
        // A CASE is named as the searched form it becomes, whichever form was written, with every
        // condition and every result in brackets of their own and the `ELSE` without them. A
        // missing `ELSE` is named `ELSE NULL`, since that is what it means. The two spaces after
        // the word are DuckDB's: the operand of a simple CASE would go in that gap and a searched
        // one has nothing to put there, and a simple CASE is a searched one by the time it is
        // named, so the gap is always empty.
        ast::Expr::Case { operand, arms, otherwise } => {
            let mut text = "CASE ".to_string();
            for arm in ast.arm_list(arms) {
                let when = if operand == NONE {
                    describe(ast, arm.when, semantics)
                } else {
                    format!(
                        "({} = {})",
                        describe(ast, operand, semantics),
                        describe(ast, arm.when, semantics)
                    )
                };
                text.push_str(&format!(
                    " WHEN ({when}) THEN ({})",
                    describe(ast, arm.then, semantics)
                ));
            }
            let fallback = if otherwise == NONE {
                "NULL".to_string()
            } else {
                describe(ast, otherwise, semantics)
            };
            format!("{text} ELSE {fallback} END")
        }
        // A negated BETWEEN and a negated IN are named as the negation of the one that is not,
        // because that is what each of them is once it is bound.
        ast::Expr::Between { operand, low, high, negated } => {
            let text = format!(
                "({} BETWEEN {} AND {})",
                describe(ast, operand, semantics),
                describe(ast, low, semantics),
                describe(ast, high, semantics)
            );
            if negated { format!("(NOT {text})") } else { text }
        }
        ast::Expr::In { operand, list, negated } => {
            let items: Vec<String> =
                ast.expr_list(list).iter().map(|&item| describe(ast, item, semantics)).collect();
            let text = format!("({} IN ({}))", describe(ast, operand, semantics), items.join(", "));
            if negated { format!("(NOT {text})") } else { text }
        }
        ast::Expr::InSubquery { operand, query, negated } => {
            let any = format!(
                "({} = ANY({}))",
                describe(ast, operand, semantics),
                rudb_parse::deparse::query(ast, query)
            );
            if negated { format!("(NOT {any})") } else { any }
        }
        ast::Expr::QuantifiedSubquery { operand, op, query, all } => {
            let op = if all { negate_binary_comparison(op) } else { op };
            let any = format!(
                "({} {} ANY({}))",
                describe(ast, operand, semantics),
                name_spelling(ast, op),
                rudb_parse::deparse::query(ast, query)
            );
            if all { format!("(NOT {any})") } else { any }
        }
        ast::Expr::QuantifiedArray { operand, op, array, all } => {
            format!(
                "({} {} {}({}))",
                describe(ast, operand, semantics),
                name_spelling(ast, op),
                if all { "ALL" } else { "ANY" },
                describe(ast, array, semantics)
            )
        }
        // The parameters keep the case they were written in, which is what the pin prints even though
        // the body finds them without it.
        ast::Expr::Lambda { params, body } => {
            let params: Vec<String> = ast.name(params).map(quoted).collect();
            format!("(lambda {}: {})", params.join(", "), describe(ast, body, semantics))
        }
        // `row` is a keyword and a function of that name, so DuckDB quotes it in the name to say
        // which of the two it means.
        ast::Expr::Row { items } => {
            let items: Vec<String> =
                ast.expr_list(items).iter().map(|&item| describe(ast, item, semantics)).collect();
            format!("\"row\"({})", items.join(", "))
        }
        // DuckDB names a bracketed list after the function it is sugar for, so `SELECT [1, 2]`
        // comes back as a column called `list_value(1, 2)`. It used to qualify that name with the
        // schema and print `main.list_value(1, 2)`, which is what the reference binary said until
        // this project pinned one at the commit the grammar is vendored from.
        ast::Expr::List { items } => {
            let items: Vec<String> =
                ast.expr_list(items).iter().map(|&item| describe(ast, item, semantics)).collect();
            // `ARRAY[1, 2]` keeps the spelling it was written with, in brackets, and what is inside
            // it is named the usual way, so `ARRAY[[1]]` is `(ARRAY[list_value(1)])`.
            if ast.written_as_array(expr) {
                format!("(ARRAY[{}])", items.join(", "))
            } else {
                format!("list_value({})", items.join(", "))
            }
        }
        // And a braced struct after `struct_pack`, with every field passed by name.
        ast::Expr::Struct { names, values } => {
            let fields: Vec<String> = ast
                .name(names)
                .zip(ast.expr_list(values))
                .map(|(name, &value)| {
                    format!("{} := {}", quoted(name), describe(ast, value, semantics))
                })
                .collect();
            format!("struct_pack({})", fields.join(", "))
        }
        // DuckDB names the column after the parameter, so `SELECT ?` comes back as `$1` whatever
        // the value turns out to be.
        ast::Expr::Parameter { name } => format!("${}", ast.string(name)),
        ast::Expr::Default => "DEFAULT".to_string(),
        ast::Expr::Subquery { array: false, .. } => "subquery".to_string(),
        ast::Expr::Subquery { query, array: true } => {
            format!("ARRAY({})", rudb_parse::deparse::query(ast, query))
        }
        ast::Expr::Exists { query, negated } => {
            let exists = format!("EXISTS({})", rudb_parse::deparse::query(ast, query));
            if negated { format!("(NOT {exists})") } else { exists }
        }
    }
}

/// The type a comparison of these two happens in.
///
/// Usually it is the type they promote to, which is the same rule a `UNION` or a `CASE` follows.
/// The exception is a string against something that is not one. A `UNION` of an `INTEGER` and a
/// `VARCHAR` is a `VARCHAR` because both values fit there, but a comparison is not asking where
/// both fit, it is asking what the question means, and `x = '1'` asks whether `x` is one. So the
/// string is read as the other type and the comparison happens there, which is what DuckDB does:
/// `1 = '1.0'` is true, and `1 = 'abc'` is a conversion error rather than false.
///
/// Dates are the case ClickBench needs. Seven of its queries write `EventDate >= '2013-07-01'`,
/// and DuckDB answers `DATE '2013-07-15' = '2013-7-15'` with true, which text comparison cannot
/// do. Comparing as text would also cost a date to string conversion for every row of a hundred
/// million, against one string to date conversion for the literal.
///
/// A blob against text is the other exception and it goes the other way, to the blob. Text is
/// bytes with a rule about what they mean and a blob is the bytes, so comparing in the blob is the
/// comparison that always has an answer, where reading the blob as text has none for the bytes
/// that are not a string. DuckDB agrees, and this is what the ClickBench queries need on a file
/// written by ClickHouse: a byte array column with no annotation on it is a blob to both engines,
/// and ten of the queries write `SearchPhrase <> ''` against one.
fn comparison_type(left: &LogicalType, right: &LogicalType) -> Option<LogicalType> {
    if let Some(common) = left.promote(right) {
        return Some(common);
    }
    let reads_a_string = |ty: &LogicalType| {
        ty.is_numeric()
            || ty.is_temporal()
            || matches!(ty, LogicalType::Boolean | LogicalType::Uuid)
    };
    match (left, right) {
        (LogicalType::Varchar, LogicalType::Blob) | (LogicalType::Blob, LogicalType::Varchar) => {
            Some(LogicalType::Blob)
        }
        (LogicalType::Varchar, other) | (other, LogicalType::Varchar) if reads_a_string(other) => {
            Some(other.clone())
        }
        // An enum is compared as whatever the other side is, read from its label, so `'1'::e = 1`
        // is true and `'x'::e = 1` fails to convert `x`, which is the pin.
        (LogicalType::Enum(_), other) | (other, LogicalType::Enum(_))
            if reads_a_string(other) || matches!(other, LogicalType::Blob) =>
        {
            Some(other.clone())
        }
        _ => None,
    }
}

/// The type an equality between two types with nothing in common is forced to, which is the pin's
/// `ForceMaxLogicalType`: the one that ranks higher, and the left one when the two rank the same.
///
/// An order between two such types is refused, but an equality is cast and the cast decides. So
/// `1 = DATE '2020-01-01'` fails to cast the number to a date, and `TIME '12:00' = TIMETZ
/// '12:00:00+00'` reads the zoned time as a time and is true, while the same two the other way
/// round read the time in the session zone and are false in New York. All measured.
pub(crate) fn forced_type(left: &LogicalType, right: &LogicalType) -> LogicalType {
    if rank(left) < rank(right) { right.clone() } else { left.clone() }
}

/// The pin's `GetLogicalTypeScore`, which is what [`forced_type`] picks by.
fn rank(ty: &LogicalType) -> u32 {
    use LogicalType::*;
    match ty {
        Null => 0,
        Boolean => 10,
        UTinyInt => 11,
        TinyInt => 12,
        USmallInt => 13,
        SmallInt => 14,
        UInteger => 15,
        Integer => 16,
        UBigInt => 17,
        BigInt => 18,
        UHugeInt => 19,
        HugeInt => 20,
        Decimal { .. } => 21,
        Float => 22,
        Double => 23,
        Time | TimeTz => 50,
        Date => 52,
        TimestampS => 53,
        TimestampMs => 54,
        Timestamp | TimestampTz => 55,
        TimestampNs => 56,
        Interval => 58,
        Varchar => 77,
        Enum(_) => 78,
        Bit => 100,
        Blob => 101,
        Uuid => 102,
        BigNum => 103,
        Struct(_) => 125,
        List(_) | Array(..) => 126,
        Map(..) => 127,
        Union(_) => 150,
        _ => 1000,
    }
}

/// The most elements of a known array that `x op ANY (array)` is written out for. A longer array
/// is compared element by element at run time.
const MAX_EXPANDED_ARRAY: usize = 1024;

/// The comparison an operator is, if it is one.
fn comparison_of(op: BinaryOp) -> Option<CompareOp> {
    Some(match op {
        BinaryOp::Eq => CompareOp::Equal,
        BinaryOp::NotEq => CompareOp::NotEqual,
        BinaryOp::Lt => CompareOp::Less,
        BinaryOp::LtEq => CompareOp::LessOrEqual,
        BinaryOp::Gt => CompareOp::Greater,
        BinaryOp::GtEq => CompareOp::GreaterOrEqual,
        BinaryOp::IsDistinctFrom => CompareOp::DistinctFrom,
        BinaryOp::IsNotDistinctFrom => CompareOp::NotDistinctFrom,
        _ => return None,
    })
}

fn negate_binary_comparison(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Eq => BinaryOp::NotEq,
        BinaryOp::NotEq => BinaryOp::Eq,
        BinaryOp::Lt => BinaryOp::GtEq,
        BinaryOp::LtEq => BinaryOp::Gt,
        BinaryOp::Gt => BinaryOp::LtEq,
        BinaryOp::GtEq => BinaryOp::Lt,
        _ => op,
    }
}

fn negate_comparison(op: CompareOp) -> CompareOp {
    match op {
        CompareOp::Equal => CompareOp::NotEqual,
        CompareOp::NotEqual => CompareOp::Equal,
        CompareOp::Less => CompareOp::GreaterOrEqual,
        CompareOp::LessOrEqual => CompareOp::Greater,
        CompareOp::Greater => CompareOp::LessOrEqual,
        CompareOp::GreaterOrEqual => CompareOp::Less,
        CompareOp::DistinctFrom => CompareOp::NotDistinctFrom,
        CompareOp::NotDistinctFrom => CompareOp::DistinctFrom,
    }
}

/// The function an operator resolves to, if there is one behind it.
fn function_of(op: BinaryOp) -> Option<&'static str> {
    Some(match op {
        BinaryOp::Add => "+",
        BinaryOp::Subtract => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::IntegerDivide => "//",
        BinaryOp::Modulo => "%",
        BinaryOp::Power => "**",
        BinaryOp::Caret => "^",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::ShiftLeft => "<<",
        BinaryOp::ShiftRight => ">>",
        BinaryOp::Concat => "||",
        BinaryOp::Like => "~~",
        BinaryOp::NotLike => "!~~",
        BinaryOp::ILike => "~~*",
        BinaryOp::NotILike => "!~~*",
        BinaryOp::Glob => "~~~",
        _ => return None,
    })
}

/// The whole number this is, with any minus signs already in front of it folded in.
///
/// It reads through a minus and stops at anything else, because that is where the grammar stops:
/// a sign joins the number it precedes and a `+` never does. A number with a point in it is not
/// one of these, so `-(1.5)` keeps its brackets where `-(1)` loses them.
fn whole_number(ast: &Ast, expr: ast::ExprRef) -> Option<String> {
    match ast.expr(expr) {
        ast::Expr::Literal { kind: LiteralKind::Number, text } => {
            let text = ast.string(text);
            text.bytes().all(|byte| byte.is_ascii_digit()).then(|| text.to_string())
        }
        ast::Expr::Unary { op: UnaryOp::Negate, operand } => {
            whole_number(ast, operand).map(|number| flip(&number))
        }
        _ => None,
    }
}

/// The name of `x[i]` or `x[a:b]`, which the pin keeps in the brackets it was written in, where
/// a written `array_extract(x, i)` keeps its own name.
///
/// The parser turns both into the call, so the brackets are read off the spans. A subscript begins
/// where its target does and a call begins at its name, and a bound the parser filled in, the `1`
/// of `x[:2]` or the `-1` of `x[2:]`, spans the whole subscript rather than any text of its own,
/// so it is left out of the name the way it was left out of the query.
fn subscript_name(
    ast: &Ast,
    call: ast::ExprRef,
    written: &str,
    args: ast::Slice,
    semantics: Semantics,
) -> Option<String> {
    let (&target, bounds) = ast.expr_list(args).split_first()?;
    let span = ast.expr_span(call);
    if !matches!(written, "array_extract" | "array_slice")
        || span.start != ast.expr_span(target).start
    {
        return None;
    }
    let bounds: Vec<String> = bounds
        .iter()
        .map(|&bound| {
            if ast.expr_span(bound) == span {
                String::new()
            } else {
                describe(ast, bound, semantics)
            }
        })
        .collect();
    let separator = if written == "array_slice" { ":" } else { "" };
    Some(format!("{}[{}]", describe(ast, target, semantics), bounds.join(separator)))
}

/// The name of a number, which is the value it stands for printed the way the pin prints it.
///
/// So the underscores go, `1_000.5` is `1000.5`, one with an exponent is the double it is, `1.5e3`
/// is `1500.0`, and a decimal loses the zeros in front and a point with nothing after it: `00.50`
/// is `0.50` and `1.` is `1`. One with no digits before the point has none in its name either,
/// which is how `.5` prints as a `DECIMAL(1,1)`.
fn number_name(text: &str) -> String {
    let plain: String = text.chars().filter(|&c| c != '_').collect();
    let hex = plain.starts_with("0x") || plain.starts_with("0X");
    if hex {
        return plain;
    }
    if plain.contains(['e', 'E']) {
        if let Ok(value) = plain.parse::<f64>() {
            return Value::Double(value).to_string();
        }
        return plain;
    }
    let Some((whole, fraction)) = plain.split_once('.') else {
        return plain;
    };
    let trimmed = whole.trim_start_matches('0');
    let whole = if trimmed.is_empty() && !whole.is_empty() { "0" } else { trimmed };
    if fraction.is_empty() { whole.to_string() } else { format!("{whole}.{fraction}") }
}

/// The name of a minus written straight onto a number with a point in it, which the pin reads as
/// part of the number: `-1.5` is `-1.5`, `-(1.5)` keeps its brackets, and a negative zero loses
/// its sign. A minus whose operand is another unbracketed minus is not one of these, since the pin
/// names `- -1.5` as `-(-(1.5))`.
///
/// The tree has no brackets in it, so a bracket is read off the spans, which take in the brackets
/// around what they cover: the operand of `-(1.5)` is wider than its own text, and the minus of
/// `-(-1.5)` ends after its operand does, which is why that one is named `-(-1.5)`.
fn signed(ast: &Ast, negation: ast::ExprRef, operand: ast::ExprRef) -> Option<String> {
    let ast::Expr::Literal { kind: LiteralKind::Number, text } = ast.expr(operand) else {
        return None;
    };
    let (inner, outer) = (ast.expr_span(operand), ast.expr_span(negation));
    let written = ast.string(text).len() as u32;
    let bracketed = inner.end - inner.start > written;
    let doubled = outer.end == inner.end
        && ast.exprs.iter().any(|held| {
            matches!(held, ast::Expr::Unary { op: UnaryOp::Negate, operand } if *operand == negation)
        });
    if bracketed || doubled {
        return None;
    }
    let name = number_name(ast.string(text));
    let zero = name.bytes().all(|byte| matches!(byte, b'0' | b'.'));
    Some(if zero { name } else { format!("-{name}") })
}

/// The same number with the other sign.
fn flip(number: &str) -> String {
    match number.strip_prefix('-') {
        Some(rest) => rest.to_string(),
        None => format!("-{number}"),
    }
}

/// How an operator is written in the name of a column, which is not always how it was written in
/// the query.
///
/// DuckDB names an unaliased expression after the function the operator resolved to, and for most
/// of them that function is spelled the way the operator is. The ones that are not are here. `<>`
/// and `!=` are one function and it is called `!=`, so both spellings come back as the second.
/// The pattern operators are the tilde spellings Postgres gives them, which is why `x LIKE 'a'`
/// is named `(x ~~ 'a')` and `x GLOB 'a'` is named `(x ~~~ 'a')`.
///
/// This is not [`spelling`], which is the language somebody wrote and is what an error message
/// about an operator has to quote back at them.
fn name_spelling(ast: &Ast, op: BinaryOp) -> String {
    match op {
        BinaryOp::NotEq => "!=".to_string(),
        BinaryOp::Glob => "~~~".to_string(),
        _ => function_of(op).map_or_else(|| spelling(ast, op), str::to_string),
    }
}

/// The generated name of a regex operator under the session's matching mode.
fn regex_name(
    left: &str,
    right: &str,
    insensitive: bool,
    negated: bool,
    semantics: Semantics,
) -> String {
    let function =
        if semantics.regex_match_full() { "regexp_full_match" } else { "regexp_matches" };
    let options = if insensitive { ", 'i'" } else { "" };
    let call = format!("{function}({left}, {right}{options})");
    if negated { format!("(NOT {call})") } else { call }
}

/// How an operator is written, for a name and for an error message.
fn spelling(ast: &Ast, op: BinaryOp) -> String {
    let fixed = match op {
        BinaryOp::Or => "OR",
        BinaryOp::And => "AND",
        BinaryOp::Eq => "=",
        BinaryOp::NotEq => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::Gt => ">",
        BinaryOp::LtEq => "<=",
        BinaryOp::GtEq => ">=",
        BinaryOp::IsDistinctFrom => "IS DISTINCT FROM",
        BinaryOp::IsNotDistinctFrom => "IS NOT DISTINCT FROM",
        BinaryOp::Add => "+",
        BinaryOp::Subtract => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::IntegerDivide => "//",
        BinaryOp::Modulo => "%",
        BinaryOp::Power => "**",
        BinaryOp::Caret => "^",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::ShiftLeft => "<<",
        BinaryOp::ShiftRight => ">>",
        BinaryOp::Concat => "||",
        BinaryOp::Like => "LIKE",
        BinaryOp::NotLike => "NOT LIKE",
        BinaryOp::ILike => "ILIKE",
        BinaryOp::NotILike => "NOT ILIKE",
        BinaryOp::Glob => "GLOB",
        BinaryOp::SimilarTo => "SIMILAR TO",
        BinaryOp::NotSimilarTo => "NOT SIMILAR TO",
        BinaryOp::Regex => "~",
        BinaryOp::NotRegex => "!~",
        BinaryOp::RegexInsensitive => "~*",
        BinaryOp::NotRegexInsensitive => "!~*",
        BinaryOp::Collate => "COLLATE",
        BinaryOp::AtTimeZone => "AT TIME ZONE",
        BinaryOp::Arrow => "->",
        BinaryOp::LongArrow => "->>",
        BinaryOp::Contains => "@>",
        BinaryOp::ContainedBy => "<@",
        BinaryOp::Overlaps => "&&",
        BinaryOp::StartsWith => "^@",
        BinaryOp::InetContainedByOrEq => "<<=",
        BinaryOp::InetContainsOrEq => ">>=",
        BinaryOp::Named(name) => return ast.string(name).to_string(),
    };
    fixed.to_string()
}

/// The value a number literal denotes, and with it the type it has.
///
/// The type follows the shape of what was written rather than the value, except that an integer
/// takes the narrowest of the four widths that holds it. A literal with a decimal point is a
/// `DECIMAL` of exactly the digits written, which is what makes `0.1 + 0.2` come out as `0.3` here
/// and as something else in a system that reads it as a double.
///
/// An underscore is a digit separator and is not part of the number, so `1_000` is a thousand and
/// `1_0.5_0` is `DECIMAL(4,2)`. The tokenizer has already thrown out the ones that are not between
/// two digits, so there is nothing to validate here and nothing to do but drop them.
///
/// There are three ways to write something that looks like a number and is not, and upstream gives a
/// different answer to each of them, which is #277. A second exponent marker is a parser error,
/// because scanning the literal is where it is noticed. Anything else with an exponent marker in it
/// is a failed conversion to `DOUBLE`, and `SELECT 1e` with nothing behind it is that. More than one
/// dot is a failed cast to the `DECIMAL` the shape of the text asked for, which is a cast rather than
/// a conversion because the text picked a type before anything tried to read it.
fn number(text: &str, negative: bool) -> Result<Value> {
    let sign = if negative { "-" } else { "" };
    let bare: String = text.chars().filter(|c| *c != '_').collect();
    let written = format!("{sign}{bare}");
    let unreadable =
        || Error::invalid_input(format!("Could not convert string '{text}' to DOUBLE"));
    if text.contains(['e', 'E']) {
        if text.matches(['e', 'E']).count() > 1 {
            return Err(Error::parser("Already found scientific notation"));
        }
        return Ok(Value::Double(written.parse::<f64>().map_err(|_| unreadable())?));
    }
    let Some(point) = text.rfind('.') else {
        if let Ok(value) = written.parse::<i32>() {
            return Ok(Value::Integer(value));
        }
        if let Ok(value) = written.parse::<i64>() {
            return Ok(Value::BigInt(value));
        }
        if let Ok(value) = written.parse::<i128>() {
            return Ok(Value::HugeInt(value));
        }
        // Past a HUGEINT and inside a UHUGEINT the pin reads a UHUGEINT, and past that, or below
        // the smallest HUGEINT, a BIGNUM.
        if let Ok(value) = written.parse::<u128>() {
            return Ok(Value::UHugeInt(value));
        }
        return rudb_common::bignum::from_text(&written).map(Value::BigNum).ok_or_else(unreadable);
    };
    // The last dot is the decimal point and every other one counts as a digit of the width, which
    // is upstream's arithmetic and the reason `1.2.3` asks for `DECIMAL(4,1)` off three digits.
    let dots = text.matches('.').count();
    let scale = text[point + 1..].chars().filter(char::is_ascii_digit).count();
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    let width = (digits.len() + dots - 1).max(1);
    if width <= MAX_DECIMAL_WIDTH as usize && scale <= MAX_DECIMAL_WIDTH as usize {
        if dots > 1 {
            return Err(Error::invalid_input(format!(
                "Failed to cast value: Could not convert string \"{text}\" to DECIMAL({width},{scale})"
            )));
        }
        if let Ok(unscaled) = format!("{sign}{digits}").parse::<i128>() {
            return Ok(Value::Decimal { unscaled, width: width as u8, scale: scale as u8 });
        }
    }
    Ok(Value::Double(written.parse::<f64>().map_err(|_| unreadable())?))
}

/// The parts of a name written inside a string, the way the pin reads the name `nextval` is given:
/// split at each dot outside double quotes, with a doubled quote inside quotes standing for one.
fn qualified_parts(text: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                part.push('"');
            }
            '"' => quoted = !quoted,
            '.' if !quoted => parts.push(std::mem::take(&mut part)),
            c => part.push(c),
        }
    }
    if quoted {
        return Err(Error::parser(format!(
            "Unterminated quote in qualified name! (input: {text})"
        )));
    }
    parts.push(part);
    Ok(parts)
}

/// The math functions that check their domain when `ieee_floating_point_ops` is off.
const STRICT_MATH: &[&str] = &[
    "sqrt", "ln", "log", "log10", "log2", "sin", "cos", "tan", "cot", "asin", "acos", "atanh",
    "gamma", "lgamma", "pow",
];

/// Whether a bound expression calls anything in [`fold::VOLATILE`], anywhere in it.
pub(crate) fn volatile(plan: &Plan, expr: ExprRef) -> bool {
    let within = |slice| plan.expr_list(slice).iter().any(|&child| volatile(plan, child));
    match *plan.expr(expr) {
        Expr::Function { name, args } => {
            fold::VOLATILE.contains(&plan.string(name)) || within(args)
        }
        Expr::Cast { input, .. } => volatile(plan, input),
        Expr::Compare { left, right, .. } => volatile(plan, left) || volatile(plan, right),
        Expr::Conjunction { children, .. } => within(children),
        Expr::Case { arms, otherwise } => {
            plan.arm_list(arms)
                .iter()
                .any(|arm| volatile(plan, arm.when) || volatile(plan, arm.then))
                || otherwise.is_some_and(|otherwise| volatile(plan, otherwise))
        }
        _ => false,
    }
}

/// The names that read one part of a date, and the part each one reads.
///
/// The pin has each of these as a function of its own with its own overloads, and every one of them
/// answers what `date_part` does with that part, including the type.
const PART_SHORTCUTS: &[(&str, &str)] = &[
    ("year", "year"),
    ("month", "month"),
    ("day", "day"),
    ("dayofmonth", "day"),
    ("hour", "hour"),
    ("minute", "minute"),
    ("second", "second"),
    ("millisecond", "millisecond"),
    ("microsecond", "microsecond"),
    ("week", "week"),
    ("weekofyear", "week"),
    ("weekday", "dow"),
    ("dayofweek", "dow"),
    ("isodow", "isodow"),
    ("dayofyear", "doy"),
    ("quarter", "quarter"),
    ("decade", "decade"),
    ("century", "century"),
    ("millennium", "millennium"),
    ("era", "era"),
    ("epoch", "epoch"),
    ("isoyear", "isoyear"),
    ("yearweek", "yearweek"),
    ("julian", "julian"),
    ("timezone_hour", "timezone_hour"),
    ("timezone_minute", "timezone_minute"),
    ("timezone", "timezone"),
];

/// The part shortcuts that read a time of day as well, which are the ones whose part a time has.
const TIMED_SHORTCUTS: &[&str] = &[
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "epoch",
    "timezone",
    "timezone_hour",
    "timezone_minute",
];

/// The part shortcuts whose names are keywords, which the pin quotes when it lists the overloads.
const KEYWORD_SHORTCUTS: &[&str] = &[
    "century",
    "day",
    "decade",
    "hour",
    "microsecond",
    "millennium",
    "millisecond",
    "minute",
    "month",
    "quarter",
    "second",
    "week",
    "year",
];

/// The type a call declares at an argument's position, for the calls whose one list of parameters
/// puts a number or a blob there, which a string literal written there is cast to before the call
/// is resolved.
fn declared_parameter(written: &str, at: usize, count: usize) -> Option<LogicalType> {
    let named = |name: &str| rudb_catalog::same_name(written, name);
    if ["format_bytes", "pg_size_pretty", "formatReadableSize", "formatReadableDecimalSize"]
        .into_iter()
        .any(named)
    {
        return Some(LogicalType::BigInt);
    }
    match at {
        1 if named("lpad") || named("rpad") => Some(LogicalType::Integer),
        1 if named("repeat") => Some(LogicalType::BigInt),
        0 if named("to_base") => Some(LogicalType::BigInt),
        _ if named("to_base") => Some(LogicalType::Integer),
        _ if named("bar") && (3..=4).contains(&count) => Some(LogicalType::Double),
        0 if named("decode") || named("base64") || named("to_base64") => Some(LogicalType::Blob),
        1.. if ["left_grapheme", "right_grapheme", "substring_grapheme"].into_iter().any(named) => {
            Some(LogicalType::BigInt)
        }
        _ => None,
    }
}

/// A refusal of a call no overload takes, with the arguments that were literals in the query spelled
/// the way the pin spells them there.
///
/// The signature table only sees types, so it writes `levenshtein(INTEGER, INTEGER)` for
/// `levenshtein(1, 2)`, where the pin writes `levenshtein(INTEGER_LITERAL, INTEGER_LITERAL)`
/// because the literals have not been given a type yet when it looks for an overload.
fn literals_spelled(
    ast: &Ast,
    error: Error,
    arguments: &[ast::ExprRef],
    types: &[LogicalType],
) -> Error {
    let plain = types.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
    let spelled = types
        .iter()
        .zip(arguments)
        .map(|(ty, &arg)| spelled_type(ast, arg, ty))
        .collect::<Vec<_>>()
        .join(", ");
    let (from, to) = (format!("({plain})'. You might"), format!("({spelled})'. You might"));
    if plain == spelled || !error.message().contains(&from) {
        return error;
    }
    let message = error.message().replacen(&from, &to, 1);
    let respelled = Error::new(error.code(), message);
    match error.span() {
        Some(span) => respelled.with_span(span),
        None => respelled,
    }
}

/// The error of PostgreSQL for a call that no function takes, which a PostgreSQL session sends in
/// place of the error of the pin. The message names the types of the arguments as `format_type`
/// does, with `unknown` for a string literal and a null. A name with no function at all gets the
/// detail that says so, and a name with the wrong types gets another detail and a hint. Another error stays as it is.
fn undefined_function(
    ast: &Ast,
    error: Error,
    written: &str,
    arguments: &[ast::ExprRef],
    types: &[LogicalType],
) -> Error {
    let unknown = kind_of(written).is_none();
    if !unknown && !error.message().starts_with("No function matches") {
        return error;
    }
    let spelled = types
        .iter()
        .zip(arguments)
        .map(|(ty, &arg)| match ast.expr(arg) {
            ast::Expr::Literal { kind: LiteralKind::String, .. } => "unknown".into(),
            _ if *ty == LogicalType::Null => "unknown".into(),
            _ => rudb_pgtypes::format_type(rudb_pgtypes::pg_type(ty).oid),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!("function {written}({spelled}) does not exist");
    let mut mapped = Error::new(error.code(), error.message().to_string())
        .state(SqlState::UNDEFINED_FUNCTION)
        .pg(message);
    mapped = match unknown {
        true => mapped.detail("There is no function of that name."),
        false => mapped
            .detail("No function of that name accepts the given argument types.")
            .hint("You might need to add explicit type casts."),
    };
    match error.span() {
        Some(span) => mapped.with_span(span),
        None => mapped,
    }
}

/// An argument's type the way the pin's messages spell it, which names a string written in the
/// query `STRING_LITERAL`, a whole number written in it `INTEGER_LITERAL` and a null `"NULL"`.
fn spelled_type(ast: &Ast, arg: ast::ExprRef, ty: &LogicalType) -> String {
    match ast.expr(arg) {
        ast::Expr::Literal { kind: LiteralKind::String, .. } => "STRING_LITERAL".to_string(),
        _ if integer_literal(ast, arg) => "INTEGER_LITERAL".to_string(),
        _ if *ty == LogicalType::Null => "\"NULL\"".to_string(),
        _ => ty.to_string(),
    }
}

/// Whether an argument is a whole number written in the query, which the pin keeps as a literal
/// with no type yet when it looks for an overload, and so is `-3`, which it reads as one number.
fn integer_literal(ast: &Ast, arg: ast::ExprRef) -> bool {
    match ast.expr(arg) {
        ast::Expr::Literal { kind: LiteralKind::Number, text } => {
            ast.string(text).bytes().all(|byte| byte.is_ascii_digit())
        }
        ast::Expr::Unary { op: UnaryOp::Negate, operand } => {
            matches!(ast.expr(operand), ast::Expr::Literal { kind: LiteralKind::Number, text }
                if ast.string(text).bytes().all(|byte| byte.is_ascii_digit()))
        }
        _ => false,
    }
}

/// The pin's refusal of a part shortcut given something it has no overload for.
///
/// The overloads are listed in the pin's order, and a name that reads a time lists the three time
/// types among them.
fn part_mismatch(name: &str, spelled: &[String], interval: bool, timed: bool) -> Error {
    let returns = if matches!(name, "epoch" | "julian") { "DOUBLE" } else { "BIGINT" };
    let shown =
        if KEYWORD_SHORTCUTS.contains(&name) { format!("\"{name}\"") } else { name.to_string() };
    let mut message = format!(
        "No function matches the given name and argument types '{name}({})'. You might need to add explicit type casts.\n\tCandidate functions:",
        spelled.join(", ")
    );
    let over: &[&str] = match (interval, timed) {
        (_, true) => &[
            "DATE",
            "INTERVAL",
            "TIME",
            "TIMESTAMP",
            "TIME WITH TIME ZONE",
            "TIME_NS",
            "TIMESTAMP WITH TIME ZONE",
        ],
        (true, false) => &["DATE", "INTERVAL", "TIMESTAMP", "TIMESTAMP WITH TIME ZONE"],
        (false, false) => &["DATE", "TIMESTAMP", "TIMESTAMP WITH TIME ZONE"],
    };
    for ty in over {
        message += &format!("\n\t{shown}(col0 {ty}) -> {returns}");
    }
    message.push('\n');
    Error::binder(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_integer_literal_takes_the_narrowest_width_that_holds_it() {
        assert_eq!(number("1", false).expect("a number"), Value::Integer(1));
        assert_eq!(number("2147483648", false).expect("a number"), Value::BigInt(2_147_483_648));
        assert_eq!(number("2147483648", true).expect("a number"), Value::Integer(-2_147_483_648));
        assert_eq!(
            number("170141183460469231731687303715884105728", false).expect("a number"),
            Value::UHugeInt(1 << 127)
        );
        assert!(matches!(
            number("340282366920938463463374607431768211456", false).expect("a number"),
            Value::BigNum(_)
        ));
        assert!(matches!(
            number("170141183460469231731687303715884105729", true).expect("a number"),
            Value::BigNum(_)
        ));
    }

    #[test]
    fn a_literal_with_a_point_is_a_decimal_of_the_digits_written() {
        assert_eq!(
            number("0.10", false).expect("a number"),
            Value::Decimal { unscaled: 10, width: 3, scale: 2 }
        );
        assert_eq!(
            number("1.5", true).expect("a number"),
            Value::Decimal { unscaled: -15, width: 2, scale: 1 }
        );
    }

    #[test]
    fn an_exponent_is_a_double_however_it_is_written() {
        assert!(matches!(number("1e3", false).expect("a number"), Value::Double(_)));
        assert!(matches!(number("1.5E-3", false).expect("a number"), Value::Double(_)));
    }

    #[test]
    fn an_underscore_separates_digits_and_is_not_one() {
        assert_eq!(number("1_000", false).expect("a number"), Value::Integer(1000));
        assert!(matches!(number("1e1_0", false).expect("a number"), Value::Double(d) if d == 1e10));
        assert_eq!(
            number("1_0.5_0", false).expect("a number"),
            Value::Decimal { unscaled: 1050, width: 4, scale: 2 }
        );
    }

    /// Three shapes that are not a number, and the three different things upstream says about them.
    /// The messages are the point, so they are written out rather than matched on a kind.
    #[test]
    fn what_is_not_a_number_says_which_way_it_is_not_one() {
        let message =
            |text: &str| number(text, false).expect_err("not a number").message().to_owned();
        assert_eq!(message("1e"), "Could not convert string '1e' to DOUBLE");
        assert_eq!(message("1e-"), "Could not convert string '1e-' to DOUBLE");
        assert_eq!(message("1e2e"), "Already found scientific notation");
        assert_eq!(message("1E2E3"), "Already found scientific notation");
        assert_eq!(
            message("1.2.3"),
            "Failed to cast value: Could not convert string \"1.2.3\" to DECIMAL(4,1)"
        );
        assert_eq!(
            message("1_0.2.3"),
            "Failed to cast value: Could not convert string \"1_0.2.3\" to DECIMAL(5,1)"
        );
        // Too wide for any DECIMAL, so the dots stop being a cast that failed and become a double
        // that will not read, which is the one case where two of these three meet.
        let wide = "1.2.34567890123456789012345678901234567890";
        assert_eq!(message(wide), format!("Could not convert string '{wide}' to DOUBLE"));
    }

    /// Text against a number reads the text as the number, and text against a blob goes the other
    /// way, to the blob. Both are exceptions to promotion and they point in opposite directions,
    /// which is the part worth a test rather than a comment.
    #[test]
    fn a_comparison_of_two_types_that_do_not_promote_picks_the_one_that_has_an_answer() {
        let common = |left, right| comparison_type(&left, &right);
        assert_eq!(
            common(LogicalType::Varchar, LogicalType::Blob),
            Some(LogicalType::Blob),
            "bytes are the reading that always has an answer"
        );
        assert_eq!(common(LogicalType::Blob, LogicalType::Varchar), Some(LogicalType::Blob));
        assert_eq!(common(LogicalType::Varchar, LogicalType::Integer), Some(LogicalType::Integer));
        assert_eq!(common(LogicalType::Varchar, LogicalType::Date), Some(LogicalType::Date));
        assert_eq!(common(LogicalType::Blob, LogicalType::Integer), None);
        assert_eq!(common(LogicalType::Integer, LogicalType::BigInt), Some(LogicalType::BigInt));
    }
}

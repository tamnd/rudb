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
    Error, LogicalType, MAX_DECIMAL_WIDTH, Result, Semantics, Session, Value,
    is_clustering_setting, looks_like_rule, rule_names,
};
use rudb_functions::{FunctionKind, kind_of, part_type, resolve};
use rudb_parse::ast::{self, BinaryOp, LiteralKind, UnaryOp};
use rudb_parse::{Ast, NONE};
use rudb_plan::{Arm, CompareOp, ConjunctionOp, Expr, ExprRef};

use crate::binder::{Binder, PendingSubquery, WindowCall};
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
    /// An empty scope, so a bare word is a column that does not resolve rather than a setting value
    /// spelled without quotes. `SET disabled_optimizers = expression_rewriter` is a name nobody
    /// declared and saying so is better than guessing which of the two was meant.
    pub(crate) fn bind_setting_value(&mut self, ast: &Ast, expr: ast::ExprRef) -> Result<ExprRef> {
        self.clause = "SET statement";
        self.bind_expr(ast, expr, &Scope::empty())
    }

    /// Binds one written expression against `scope`.
    pub(crate) fn bind_expr(
        &mut self,
        ast: &Ast,
        expr: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let span = ast.expr_span(expr);
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
        match written {
            ast::Expr::Star { .. } => {
                Err(Error::binder(format!("* is not allowed in the {}", self.clause)))
            }
            ast::Expr::Column { name } => self.bind_column(ast, name, scope),
            ast::Expr::Literal { kind, text } => self.bind_literal(ast, kind, text),
            ast::Expr::Unary { op, operand } => self.bind_unary(ast, op, operand, scope),
            ast::Expr::Binary { op, left, right } => self.bind_binary(ast, op, left, right, scope),
            ast::Expr::Function { name, args, distinct, filter } => {
                self.bind_call(ast, name, args, distinct, filter, scope)
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
                self.bind_window(ast, &call, scope)
            }
            ast::Expr::Cast { operand, ty, try_cast } => {
                let input = self.bind_expr(ast, operand, scope)?;
                let target = LogicalType::parse(ast.string(ty))?;
                self.checked_cast_to(input, &target, try_cast)
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
            ast::Expr::Row { .. } => {
                Err(Error::not_implemented("a row value outside of a VALUES clause".to_string()))
            }
            // A lambda that got here is not the argument of a function that takes one, since that
            // function binds it itself. See `crate::lambda`.
            ast::Expr::Lambda { .. } => Err(Error::binder("invalid lambda expression")),
            ast::Expr::List { items } => self.bind_list(ast, items, scope),
            ast::Expr::Parameter { name } => self.bind_parameter(ast, name),
            ast::Expr::Subquery { query } => self.bind_scalar_subquery(ast, query, scope),
            ast::Expr::Exists { query, negated } => {
                self.bind_exists_subquery(ast, query, negated, scope)
            }
        }
    }

    /// Binds an uncorrelated scalar query and returns its one output as a column expression.
    fn bind_scalar_subquery(
        &mut self,
        ast: &Ast,
        query: ast::QueryRef,
        outer: &Scope,
    ) -> Result<ExprRef> {
        let (node, scope, correlations) = self.bind_isolated_subquery(ast, query, outer)?;
        let [column] = scope.columns.as_slice() else {
            return Err(Error::binder(format!(
                "Subquery returns {} columns - expected 1",
                scope.len()
            )));
        };
        let expr = self.add_expr(Expr::Column(column.binding), column.ty.clone());
        self.scalar_subqueries.push(PendingSubquery {
            node,
            kind: rudb_plan::JoinKind::Single,
            conditions: Vec::new(),
            dependent: !correlations.is_empty(),
            reads: correlations,
            index: column.binding.table,
            inside_aggregate: self.in_aggregate,
        });
        Ok(expr)
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
        let node = self.add_node(rudb_plan::Node::Limit {
            input: node,
            count: rudb_plan::Bound::Rows(1),
            offset: rudb_plan::Bound::Rows(0),
        });
        let index = self.fresh_index();
        let marker = self.add_constant(Value::Boolean(true));
        let exprs = self.plan_mut().add_expr_list(&[marker]);
        let name = self.plan_mut().intern("exists");
        let names = self.plan_mut().add_name_list(&[name]);
        let node = self.add_node(rudb_plan::Node::Project { input: node, index, exprs, names });
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
    ) -> Result<(rudb_plan::NodeRef, Scope, Vec<rudb_plan::ColumnBinding>)> {
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
            } else if let Some(enclosing) = self.correlations.last_mut() {
                if !enclosing.contains(&binding) {
                    enclosing.push(binding);
                }
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
        let Some(value) = self.parameters.get(name) else {
            // DuckDB's first line, then ours, because the second half of its sentence names PREPARE
            // and PREPARE is #152. The library route works today.
            return Err(Error::invalid_input(
                "Prepared statement parameters cannot be used directly\nTo use prepared statement \
                 parameters, prepare the statement first, which is Connection::prepare",
            ));
        };
        Ok(self.add_constant(value.clone()))
    }

    fn bind_column(&mut self, ast: &Ast, name: ast::Slice, scope: &Scope) -> Result<ExprRef> {
        let parts: Vec<&str> = ast.name(name).collect();
        // A lambda parameter beats a column of the same name, so `lambda l: l + 1` over a table with
        // a column `l` reads the element. The innermost lambda that has the name is the one meant.
        if let [word] = parts.as_slice() {
            if let Some(parameter) = self.lambda_parameter(word) {
                return Ok(parameter);
            }
        }
        // A bare `current_date` is one of the ten session context keywords, and a column of that
        // name beats it. The scope is asked whether anything answers to the word before the fold
        // rather than the fold happening when resolution fails, so that two tables carrying the name
        // is still the ambiguity error. Both halves were measured against the pin. See
        // `crate::context`.
        if let [word] = parts.as_slice() {
            if !scope.names(word) && !self.outer_scopes.iter().any(|outer| outer.names(word)) {
                if let Some(folded) = self.context_keyword(word) {
                    return Ok(folded);
                }
            }
        }
        if let Some(found) = scope.resolve_optional(&parts)? {
            return Ok(self.add_expr(Expr::Column(found.binding), found.ty.clone()));
        }
        let mut found = None;
        for (at, outer) in self.outer_scopes.iter().enumerate().rev() {
            if let Some(visible) = outer.resolve_optional(&parts)? {
                found = Some((at, visible.binding, visible.ty.clone()));
                break;
            }
        }
        let Some((at, binding, ty)) = found else {
            return scope.resolve(&parts).map(|_| unreachable!());
        };
        // A LATERAL entry may not aggregate over what its left neighbour gave it. There is one row
        // of the left per evaluation of the entry, so `sum(o.k)` would be a sum of one value and
        // whoever wrote it meant something else. The pinned build refuses it in these words and a
        // correlated column read anywhere else in the entry, including under its own aggregate's
        // filter or inside a window, is fine.
        if self.in_aggregate && self.lateral_scopes.contains(&at) {
            return Err(Error::binder("LATERAL join cannot contain aggregates!"));
        }
        if let Some(correlations) = self.correlations.last_mut() {
            if !correlations.contains(&binding) {
                correlations.push(binding);
            }
        }
        Ok(self.add_expr(Expr::Column(binding), ty))
    }

    fn bind_literal(&mut self, ast: &Ast, kind: LiteralKind, text: ast::StrRef) -> Result<ExprRef> {
        let value = match kind {
            LiteralKind::Null => Value::Null,
            LiteralKind::True => Value::Boolean(true),
            LiteralKind::False => Value::Boolean(false),
            LiteralKind::String | LiteralKind::Blob => Value::Varchar(ast.string(text).to_string()),
            LiteralKind::Number => number(ast.string(text), false)?,
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

    fn bind_unary(
        &mut self,
        ast: &Ast,
        op: UnaryOp,
        operand: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        // A negated number literal is one constant rather than a call, so that -2147483648 is an
        // INTEGER the way it is written and not a negation of a BIGINT.
        if op == UnaryOp::Negate {
            if let ast::Expr::Literal { kind: LiteralKind::Number, text } = ast.expr(operand) {
                let value = number(ast.string(text), true)?;
                return Ok(self.add_constant(value));
            }
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
            UnaryOp::BitNot => Err(Error::not_implemented("the ~ operator".to_string())),
            UnaryOp::Factorial => Err(Error::not_implemented("the ! operator".to_string())),
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
        let left = self.bind_expr(ast, left, scope)?;
        let mut right = self.bind_expr(ast, right, scope)?;
        if let Some(comparison) = comparison_of(op) {
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

    /// Turns a zero divisor into null before the ordinary arithmetic kernel sees it.
    fn zero_to_null(&mut self, divisor: ExprRef) -> ExprRef {
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

    fn bind_call(
        &mut self,
        ast: &Ast,
        name: ast::Slice,
        args: ast::Slice,
        distinct: bool,
        filter: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let written = ast.name(name).last().unwrap_or_default().to_string();
        let arguments = ast.expr_list(args).to_vec();
        // count(*) is a different function from count(x), because one of them counts rows and the
        // other counts the rows where its argument is not null.
        // A replace list on the star is not this, and not anything: upstream's parser refuses
        // `count(* REPLACE (1 AS a))` outright and the vendored grammar has room for it, so leaving
        // it out of here sends it to the arm that says a star is not allowed where it was written.
        let starred = arguments.iter().any(|&arg| {
            matches!(ast.expr(arg), ast::Expr::Star { qualifier, replacements }
                if qualifier.is_empty() && replacements.is_empty())
        });
        if starred {
            if !rudb_catalog::same_name(&written, "count") || arguments.len() != 1 {
                return Err(Error::binder(format!("* is not allowed in {written}()")));
            }
            return self.bind_aggregate(ast, "count_star", &[], false, filter, scope);
        }
        // `count()` with nothing in it is upstream's other spelling of `count(*)`. It counts rows
        // the same way and it is not an arity mistake, which is what the signature table would
        // otherwise say about a `count` given no arguments.
        if rudb_catalog::same_name(&written, "count") && arguments.is_empty() {
            return self.bind_aggregate(ast, "count_star", &[], false, filter, scope);
        }
        if kind_of(&written) == Some(FunctionKind::Aggregate) {
            return self.bind_aggregate(ast, &written, &arguments, distinct, filter, scope);
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
        if (distinct || filter != NONE) && kind_of(&written) == Some(FunctionKind::Scalar) {
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
        for arg in arguments {
            bound.push(self.bind_expr(ast, arg, scope)?);
        }
        if let Some(expanded) = self.list_macro(&written, &bound)? {
            return Ok(expanded);
        }
        if let Some(aggregated) = self.list_aggregate(&written, &bound)? {
            return Ok(aggregated);
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
        // `current_setting` is the other one the binder answers, and it has to be answered here
        // rather than by a kernel for a reason `typeof` does not have: its declared return type is
        // ANY, so there is no type for a plan to carry until the name is read. Upstream folds it
        // too, which an `EXPLAIN` of a query that calls it shows. A call this cannot fold falls
        // through to the table, which refuses it in upstream's words.
        if rudb_catalog::same_name(&written, "current_setting") && bound.len() == 1 {
            if let Some(folded) = self.setting(bound[0])? {
                return Ok(folded);
            }
        }
        // The session context functions are the third group the binder answers, and they fold for
        // the reason the pin marks them `CONSISTENT_WITHIN_QUERY`: the answer is settled when the
        // statement starts and no row changes it. A call with arguments is not one of these and
        // falls through to the table, which has a row per name so that `now(1)` is the arity error
        // rather than a missing function. See `crate::context`.
        if bound.is_empty() {
            if let Some(folded) = self.context_call(&written) {
                return Ok(folded);
            }
        }
        // The one argument form measures from the session-local date at the start of the
        // statement. Insert that date here so the ordinary two-moment kernel remains free of a
        // session dependency and both spellings use exactly the same calendar arithmetic.
        if rudb_catalog::same_name(&written, "age") && bound.len() == 1 {
            bound.insert(0, self.current_date());
        }
        self.call(&written, bound)
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
    fn list_macro(&mut self, written: &str, bound: &[ExprRef]) -> Result<Option<ExprRef>> {
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
        let wrapped = self.call("list_value", vec![bound[element]])?;
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
        for item in written {
            let item = self.bind_expr(ast, item, scope)?;
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
        let node =
            self.add_node(rudb_plan::Node::Project { input: node, index: projected, exprs, names });
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
        args: Vec<ExprRef>,
    ) -> Result<ExprRef> {
        let types: Vec<LogicalType> =
            args.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        let resolved = resolve(resolved_name, &types)?;
        let mut cast = Vec::with_capacity(args.len());
        for (arg, wanted) in args.iter().zip(&resolved.arguments) {
            cast.push(self.checked_cast_to(*arg, wanted, false)?);
        }
        let returns = self.narrowed_part(resolved.name, &cast, resolved.returns);
        let args = self.plan_mut().add_expr_list(&cast);
        let name = self.plan_mut().intern(stored_name.unwrap_or(resolved.name));
        Ok(self.add_expr(Expr::Function { name, args }, returns))
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
    fn setting(&mut self, argument: ExprRef) -> Result<Option<ExprRef>> {
        let Expr::Constant(held) = *self.plan().expr(argument) else { return Ok(None) };
        let Value::Varchar(name) = self.plan().value(held) else { return Ok(None) };
        let name = name.clone();
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

    /// The value of a setting rudb has and DuckDB does not.
    ///
    /// The settings catalog cannot answer for these because it is the list of DuckDB's settings and
    /// these are not on it, deliberately, so that `duckdb_settings()` does not claim they are.
    /// `SET` already decides which side of that line a name falls on and this is the reading half
    /// of the same decision. Without it a session driving the engine through SQL can write one of
    /// these and then has no way to ask what it says.
    ///
    /// Three of the four are here and each is read off something the binder is already holding: the
    /// row order declarations are on the catalog, the relationship declarations and the rule
    /// switches are on the session. The seam settings are the fourth and they are not here, because
    /// the seam names live in a crate below this one that the binder does not depend on and adding
    /// the dependency to read a string is a bigger decision than this one.
    ///
    /// A rule reads back as a boolean and the other two as the text they were written as, which is
    /// what `Database::setting` answers for all three. A declaration nobody made reads back
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
        if looks_like_rule(name) {
            return Err(Error::catalog(format!(
                "no rule called {name}, the rules are {}",
                rule_names()
            )));
        }
        Ok(None)
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
    fn narrowed_part(&self, name: &str, args: &[ExprRef], returns: LogicalType) -> LogicalType {
        if name != "date_part" {
            return returns;
        }
        let Some(&spec) = args.first() else { return returns };
        let Expr::Constant(value) = *self.plan().expr(spec) else { return returns };
        let Value::Varchar(spelling) = self.plan().value(value) else { return returns };
        part_type(spelling)
    }

    /// A cast to `ty`, or the expression itself when it is already that type.
    pub(crate) fn cast_to(&mut self, expr: ExprRef, ty: &LogicalType) -> ExprRef {
        if self.plan().expr_type(expr) == ty {
            return expr;
        }
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
        Ok(self.add_expr(Expr::Cast { input: expr, try_cast }, ty.clone()))
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
        let common = comparison_type(&left_type, &right_type).ok_or_else(|| {
            Error::binder(format!(
                "Cannot compare values of type {left_type} and type {right_type}"
            ))
        })?;
        let left = self.checked_cast_to(left, &common, false)?;
        let right = self.checked_cast_to(right, &common, false)?;
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
    if expr == NONE {
        return false;
    }
    match ast.expr(expr) {
        ast::Expr::Star { .. }
        | ast::Expr::Column { .. }
        | ast::Expr::Literal { .. }
        | ast::Expr::Parameter { .. } => false,
        ast::Expr::Unary { operand, .. } => has_aggregate(ast, operand),
        ast::Expr::Binary { left, right, .. } => {
            has_aggregate(ast, left) || has_aggregate(ast, right)
        }
        ast::Expr::Function { name, args, .. } => {
            let written = ast.name(name).last().unwrap_or_default();
            kind_of(written) == Some(FunctionKind::Aggregate)
                || ast.expr_list(args).iter().any(|&arg| has_aggregate(ast, arg))
        }
        ast::Expr::Cast { operand, .. } => has_aggregate(ast, operand),
        ast::Expr::Case { operand, arms, otherwise } => {
            has_aggregate(ast, operand)
                || has_aggregate(ast, otherwise)
                || ast
                    .arm_list(arms)
                    .iter()
                    .any(|arm| has_aggregate(ast, arm.when) || has_aggregate(ast, arm.then))
        }
        ast::Expr::Between { operand, low, high, .. } => {
            has_aggregate(ast, operand) || has_aggregate(ast, low) || has_aggregate(ast, high)
        }
        ast::Expr::In { operand, list, .. } => {
            has_aggregate(ast, operand)
                || ast.expr_list(list).iter().any(|&item| has_aggregate(ast, item))
        }
        ast::Expr::InSubquery { operand, .. } => has_aggregate(ast, operand),
        ast::Expr::QuantifiedSubquery { operand, .. } => has_aggregate(ast, operand),
        ast::Expr::Lambda { body, .. } => has_aggregate(ast, body),
        ast::Expr::Row { items } => {
            ast.expr_list(items).iter().any(|&item| has_aggregate(ast, item))
        }
        ast::Expr::List { items } => {
            ast.expr_list(items).iter().any(|&item| has_aggregate(ast, item))
        }
        // A window call is not an aggregate and is evaluated after the grouping rather than by it,
        // but what it is given to read can be one: `sum(count(x)) OVER ()` aggregates the block.
        // The partition and the order keys count for the same reason.
        ast::Expr::Window { args, spec, order, .. } => {
            let held = ast.window(spec);
            ast.expr_list(args).iter().any(|&arg| has_aggregate(ast, arg))
                || ast.order_list(order).iter().any(|item| has_aggregate(ast, item.expr))
                || ast.expr_list(held.partition).iter().any(|&key| has_aggregate(ast, key))
                || ast.order_list(held.order).iter().any(|item| has_aggregate(ast, item.expr))
        }
        // A subquery has its own aggregation and does not make the outer block aggregate.
        ast::Expr::Subquery { .. } | ast::Expr::Exists { .. } => false,
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
        ast::Expr::Column { name } => quoted(ast.name(name).last().unwrap_or_default()),
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
            LiteralKind::Number => ast.string(text).to_string(),
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
                UnaryOp::Negate => match whole_number(ast, operand) {
                    Some(number) => flip(&number),
                    None => format!("-({inner})"),
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
            if (starred || empty) && rudb_catalog::same_name(written, "count") {
                return format!("count_star(){}", named_filter(ast, filter, semantics));
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
            } else {
                quoted(&written.to_ascii_lowercase())
            };
            // `DISTINCT` is part of the name because it is part of what was computed.
            // `count(UserID)` and `count(DISTINCT UserID)` are two different answers and a result
            // that called them both the first one would be reporting the wrong one.
            let word = if distinct { "DISTINCT " } else { "" };
            let arguments: Vec<String> =
                ast.expr_list(args).iter().map(|&arg| describe(ast, arg, semantics)).collect();
            format!(
                "{name}({word}{}){}",
                arguments.join(", "),
                named_filter(ast, filter, semantics)
            )
        }
        ast::Expr::Cast { operand, ty, try_cast } => {
            let word = if try_cast { "TRY_CAST" } else { "CAST" };
            format!("{word}({} AS {})", describe(ast, operand, semantics), ast.string(ty))
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
            format!("list_value({})", items.join(", "))
        }
        // DuckDB names the column after the parameter, so `SELECT ?` comes back as `$1` whatever
        // the value turns out to be.
        ast::Expr::Parameter { name } => format!("${}", ast.string(name)),
        ast::Expr::Subquery { .. } => "subquery".to_string(),
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
        ty.is_numeric() || ty.is_temporal() || matches!(ty, LogicalType::Boolean)
    };
    match (left, right) {
        (LogicalType::Varchar, LogicalType::Blob) | (LogicalType::Blob, LogicalType::Varchar) => {
            Some(LogicalType::Blob)
        }
        (LogicalType::Varchar, other) | (other, LogicalType::Varchar) if reads_a_string(other) => {
            Some(other.clone())
        }
        _ => None,
    }
}

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
        BinaryOp::Concat => "||",
        BinaryOp::Like => "~~",
        BinaryOp::NotLike => "!~~",
        BinaryOp::ILike => "~~*",
        BinaryOp::NotILike => "!~~*",
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
        return Ok(Value::Double(written.parse::<f64>().map_err(|_| unreadable())?));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_integer_literal_takes_the_narrowest_width_that_holds_it() {
        assert_eq!(number("1", false).expect("a number"), Value::Integer(1));
        assert_eq!(number("2147483648", false).expect("a number"), Value::BigInt(2_147_483_648));
        assert_eq!(number("2147483648", true).expect("a number"), Value::Integer(-2_147_483_648));
        assert!(matches!(
            number("170141183460469231731687303715884105728", false).expect("a number"),
            Value::Double(_)
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

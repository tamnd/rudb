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

use rudb_common::{Error, LogicalType, MAX_DECIMAL_WIDTH, Result, Value};
use rudb_functions::{FunctionKind, kind_of, resolve};
use rudb_parse::Ast;
use rudb_parse::ast::{self, BinaryOp, LiteralKind, UnaryOp};
use rudb_plan::{Arm, CompareOp, ConjunctionOp, Expr, ExprRef};

use crate::binder::Binder;
use crate::scope::Scope;

impl Binder<'_> {
    /// Binds one written expression against `scope`.
    pub(crate) fn bind_expr(
        &mut self,
        ast: &Ast,
        expr: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        match ast.expr(expr) {
            ast::Expr::Star { .. } => {
                Err(Error::binder(format!("* is not allowed in the {}", self.clause)))
            }
            ast::Expr::Column { name } => self.bind_column(ast, name, scope),
            ast::Expr::Literal { kind, text } => self.bind_literal(ast, kind, text),
            ast::Expr::Unary { op, operand } => self.bind_unary(ast, op, operand, scope),
            ast::Expr::Binary { op, left, right } => self.bind_binary(ast, op, left, right, scope),
            ast::Expr::Function { name, args, distinct } => {
                self.bind_call(ast, name, args, distinct, scope)
            }
            ast::Expr::Cast { operand, ty, try_cast } => {
                let input = self.bind_expr(ast, operand, scope)?;
                let target = LogicalType::parse(ast.string(ty))?;
                Ok(self.plan_mut().add_expr(Expr::Cast { input, try_cast }, target))
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
            ast::Expr::Row { .. } => {
                Err(Error::not_implemented("a row value outside of a VALUES clause".to_string()))
            }
            ast::Expr::List { items } => self.bind_list(ast, items, scope),
            ast::Expr::Parameter { name } => self.bind_parameter(ast, name),
            ast::Expr::Subquery { .. } => {
                Err(Error::not_implemented("a scalar subquery".to_string()))
            }
        }
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
        Ok(self.plan_mut().add_constant(value.clone()))
    }

    fn bind_column(&mut self, ast: &Ast, name: ast::Slice, scope: &Scope) -> Result<ExprRef> {
        let parts: Vec<&str> = ast.name(name).collect();
        let found = scope.resolve(&parts)?;
        let (binding, ty) = (found.binding, found.ty.clone());
        Ok(self.plan_mut().add_expr(Expr::Column(binding), ty))
    }

    fn bind_literal(&mut self, ast: &Ast, kind: LiteralKind, text: ast::StrRef) -> Result<ExprRef> {
        let value = match kind {
            LiteralKind::Null => Value::Null,
            LiteralKind::True => Value::Boolean(true),
            LiteralKind::False => Value::Boolean(false),
            LiteralKind::String => Value::Varchar(ast.string(text).to_string()),
            LiteralKind::Number => number(ast.string(text), false)?,
        };
        Ok(self.plan_mut().add_constant(value))
    }

    /// `[a, b, c]`, which is a LIST value.
    ///
    /// Constants only, because a list is folded into one `Value` here rather than evaluated. There
    /// is no LIST vector yet, so a list of column references has nothing to compute into, and the
    /// one thing a list is for today is the file argument of `read_parquet`, which is constants.
    ///
    /// The element type is what the items promote to, and an empty list is `INTEGER[]`, both of
    /// which are DuckDB's answers and were measured against the binary.
    fn bind_list(&mut self, ast: &Ast, items: ast::Slice, scope: &Scope) -> Result<ExprRef> {
        let written = ast.expr_list(items).to_vec();
        let mut values = Vec::with_capacity(written.len());
        for item in written {
            let bound = self.bind_expr(ast, item, scope)?;
            let Expr::Constant(reference) = *self.plan().expr(bound) else {
                return Err(Error::not_implemented("a list of anything but constants"));
            };
            values.push(self.plan().value(reference).clone());
        }
        let mut element = LogicalType::Integer;
        for (at, value) in values.iter().enumerate() {
            let ty = value.logical_type();
            if value.is_null() {
                continue;
            }
            element = if at == 0 { ty } else { mixed(&element, &ty)? };
        }
        for value in &values {
            if !value.is_null() && value.logical_type() != element {
                return Err(Error::not_implemented("a list whose items are not all one type"));
            }
        }
        Ok(self.plan_mut().add_constant(Value::List { element, values }))
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
                return Ok(self.plan_mut().add_constant(value));
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
        let right = self.bind_expr(ast, right, scope)?;
        if let Some(comparison) = comparison_of(op) {
            return self.compare(comparison, left, right);
        }
        match function_of(op) {
            Some(name) => self.call(name, vec![left, right]),
            None => Err(Error::not_implemented(format!("the {} operator", spelling(ast, op)))),
        }
    }

    fn bind_call(
        &mut self,
        ast: &Ast,
        name: ast::Slice,
        args: ast::Slice,
        distinct: bool,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let written = ast.name(name).last().unwrap_or_default().to_string();
        let arguments = ast.expr_list(args).to_vec();
        // count(*) is a different function from count(x), because one of them counts rows and the
        // other counts the rows where its argument is not null.
        let starred = arguments.iter().any(
            |&arg| matches!(ast.expr(arg), ast::Expr::Star { qualifier } if qualifier.is_empty()),
        );
        if starred {
            if !rudb_catalog::same_name(&written, "count") || arguments.len() != 1 {
                return Err(Error::binder(format!("* is not allowed in {written}()")));
            }
            return self.bind_aggregate(ast, "count_star", &[], false, scope);
        }
        if kind_of(&written) == Some(FunctionKind::Aggregate) {
            return self.bind_aggregate(ast, &written, &arguments, distinct, scope);
        }
        if distinct {
            return Err(Error::binder(format!(
                "DISTINCT is not applicable to the scalar function {written}"
            )));
        }
        let mut bound = Vec::with_capacity(arguments.len());
        for arg in arguments {
            bound.push(self.bind_expr(ast, arg, scope)?);
        }
        self.call(&written, bound)
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
        let subject = if operand == rudb_parse::NONE {
            None
        } else {
            Some(self.bind_expr(ast, operand, scope)?)
        };
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
        let fallback = if otherwise == rudb_parse::NONE {
            None
        } else {
            let bound = self.bind_expr(ast, otherwise, scope)?;
            result = meet(&result, self.plan().expr_type(bound))?;
            Some(bound)
        };
        // Every arm and the else have to hand back the same type, since a CASE produces one column.
        for arm in &mut bound {
            arm.then = self.cast_to(arm.then, &result);
        }
        let fallback = fallback.map(|expr| self.cast_to(expr, &result));
        let arms = self.plan_mut().add_arms(&bound);
        Ok(self.plan_mut().add_expr(Expr::Case { arms, otherwise: fallback }, result))
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

    /// Resolves a scalar call, casts the arguments to what the overload wants, and records it.
    pub(crate) fn call(&mut self, name: &str, args: Vec<ExprRef>) -> Result<ExprRef> {
        let types: Vec<LogicalType> =
            args.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        let resolved = resolve(name, &types)?;
        let mut cast = Vec::with_capacity(args.len());
        for (arg, wanted) in args.iter().zip(&resolved.arguments) {
            cast.push(self.cast_to(*arg, wanted));
        }
        let args = self.plan_mut().add_expr_list(&cast);
        let name = self.plan_mut().intern(resolved.name);
        Ok(self.plan_mut().add_expr(Expr::Function { name, args }, resolved.returns))
    }

    /// A cast to `ty`, or the expression itself when it is already that type.
    pub(crate) fn cast_to(&mut self, expr: ExprRef, ty: &LogicalType) -> ExprRef {
        if self.plan().expr_type(expr) == ty {
            return expr;
        }
        self.plan_mut().add_expr(Expr::Cast { input: expr, try_cast: false }, ty.clone())
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
        let left = self.cast_to(left, &common);
        let right = self.cast_to(right, &common);
        Ok(self.plan_mut().add_expr(Expr::Compare { op, left, right }, LogicalType::Boolean))
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
            [] => self.plan_mut().add_constant(Value::Boolean(op == ConjunctionOp::And)),
            [only] => *only,
            _ => {
                let children = self.plan_mut().add_expr_list(&flat);
                self.plan_mut().add_expr(Expr::Conjunction { op, children }, LogicalType::Boolean)
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
        let null = self.plan_mut().add_constant(Value::Null);
        self.compare(op, expr, null)
    }

    fn against_boolean(&mut self, op: CompareOp, expr: ExprRef, wanted: bool) -> Result<ExprRef> {
        let condition = self.as_boolean(expr, "IS")?;
        let constant = self.plan_mut().add_constant(Value::Boolean(wanted));
        self.compare(op, condition, constant)
    }
}

/// The type two items of a list literal meet at.
fn mixed(left: &LogicalType, right: &LogicalType) -> Result<LogicalType> {
    left.promote(right).ok_or_else(|| {
        Error::binder(format!("Cannot mix values of type {left} and type {right} in a list"))
    })
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
    if expr == rudb_parse::NONE {
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
        ast::Expr::Row { items } => {
            ast.expr_list(items).iter().any(|&item| has_aggregate(ast, item))
        }
        ast::Expr::List { items } => {
            ast.expr_list(items).iter().any(|&item| has_aggregate(ast, item))
        }
        // A subquery has its own aggregation and does not make the outer block aggregate.
        ast::Expr::Subquery { .. } => false,
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
pub(crate) fn describe(ast: &Ast, expr: ast::ExprRef) -> String {
    match ast.expr(expr) {
        ast::Expr::Star { qualifier } => {
            if qualifier.is_empty() {
                "*".to_string()
            } else {
                format!("{}.*", ast.name_text(qualifier))
            }
        }
        ast::Expr::Column { name } => ast.name(name).last().unwrap_or_default().to_string(),
        ast::Expr::Literal { kind, text } => match kind {
            LiteralKind::Null => "NULL".to_string(),
            LiteralKind::True => "true".to_string(),
            LiteralKind::False => "false".to_string(),
            LiteralKind::String => format!("'{}'", ast.string(text)),
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
            let inner = describe(ast, operand);
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
            let (left, right) = (describe(ast, left), describe(ast, right));
            match op {
                BinaryOp::Regex => format!("regexp_matches({left}, {right})"),
                BinaryOp::RegexInsensitive => format!("regexp_matches({left}, {right}, 'i')"),
                BinaryOp::NotRegexInsensitive => {
                    format!("(NOT regexp_matches({left}, {right}, 'i'))")
                }
                BinaryOp::SimilarTo => format!("regexp_full_match({left}, {right})"),
                BinaryOp::NotSimilarTo => format!("(NOT regexp_full_match({left}, {right}))"),
                // The one operator DuckDB names with no brackets around it at all.
                BinaryOp::Collate => format!("{left} COLLATE {right}"),
                _ => format!("({left} {} {right})", name_spelling(ast, op)),
            }
        }
        ast::Expr::Function { name, args, distinct } => {
            let written = ast.name(name).last().unwrap_or_default();
            let starred = ast
                .expr_list(args)
                .iter()
                .any(|&arg| matches!(ast.expr(arg), ast::Expr::Star { .. }));
            if starred && rudb_catalog::same_name(written, "count") {
                return "count_star()".to_string();
            }
            // The name goes to lower case, which is the one place a spelling from the query is not
            // kept. DuckDB's parser folds a function name as it reads it and the name it prints
            // here is the folded one, so `SELECT SUM(x)` comes back as a column called `sum(x)`.
            // A column name is not folded, because that one comes from the catalog rather than
            // from the query, which is why `output_name` asks the scope first and only falls
            // through to here.
            let name = written.to_ascii_lowercase();
            // `DISTINCT` is part of the name because it is part of what was computed.
            // `count(UserID)` and `count(DISTINCT UserID)` are two different answers and a result
            // that called them both the first one would be reporting the wrong one.
            let word = if distinct { "DISTINCT " } else { "" };
            let arguments: Vec<String> =
                ast.expr_list(args).iter().map(|&arg| describe(ast, arg)).collect();
            format!("{name}({word}{})", arguments.join(", "))
        }
        ast::Expr::Cast { operand, ty, try_cast } => {
            let word = if try_cast { "TRY_CAST" } else { "CAST" };
            format!("{word}({} AS {})", describe(ast, operand), ast.string(ty))
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
                let when = if operand == rudb_parse::NONE {
                    describe(ast, arm.when)
                } else {
                    format!("({} = {})", describe(ast, operand), describe(ast, arm.when))
                };
                text.push_str(&format!(" WHEN ({when}) THEN ({})", describe(ast, arm.then)));
            }
            let fallback = if otherwise == rudb_parse::NONE {
                "NULL".to_string()
            } else {
                describe(ast, otherwise)
            };
            format!("{text} ELSE {fallback} END")
        }
        // A negated BETWEEN and a negated IN are named as the negation of the one that is not,
        // because that is what each of them is once it is bound.
        ast::Expr::Between { operand, low, high, negated } => {
            let text = format!(
                "({} BETWEEN {} AND {})",
                describe(ast, operand),
                describe(ast, low),
                describe(ast, high)
            );
            if negated { format!("(NOT {text})") } else { text }
        }
        ast::Expr::In { operand, list, negated } => {
            let items: Vec<String> =
                ast.expr_list(list).iter().map(|&item| describe(ast, item)).collect();
            let text = format!("({} IN ({}))", describe(ast, operand), items.join(", "));
            if negated { format!("(NOT {text})") } else { text }
        }
        // `row` is a keyword and a function of that name, so DuckDB quotes it in the name to say
        // which of the two it means.
        ast::Expr::Row { items } => {
            let items: Vec<String> =
                ast.expr_list(items).iter().map(|&item| describe(ast, item)).collect();
            format!("\"row\"({})", items.join(", "))
        }
        // DuckDB names a bracketed list after the function it is sugar for, so `SELECT [1, 2]`
        // comes back as a column called `list_value(1, 2)`. It used to qualify that name with the
        // schema and print `main.list_value(1, 2)`, which is what the reference binary said until
        // this project pinned one at the commit the grammar is vendored from.
        ast::Expr::List { items } => {
            let items: Vec<String> =
                ast.expr_list(items).iter().map(|&item| describe(ast, item)).collect();
            format!("list_value({})", items.join(", "))
        }
        // DuckDB names the column after the parameter, so `SELECT ?` comes back as `$1` whatever
        // the value turns out to be.
        ast::Expr::Parameter { name } => format!("${}", ast.string(name)),
        ast::Expr::Subquery { .. } => "subquery".to_string(),
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
fn number(text: &str, negative: bool) -> Result<Value> {
    let sign = if negative { "-" } else { "" };
    let written = format!("{sign}{text}");
    let unreadable = || Error::conversion(format!("Could not convert string '{text}' to a number"));
    if text.contains(['e', 'E']) {
        return Ok(Value::Double(written.parse::<f64>().map_err(|_| unreadable())?));
    }
    let Some(point) = text.find('.') else {
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
    let scale = text.len() - point - 1;
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    let width = digits.len();
    if width <= MAX_DECIMAL_WIDTH as usize && scale <= MAX_DECIMAL_WIDTH as usize {
        if let Ok(unscaled) = format!("{sign}{digits}").parse::<i128>() {
            return Ok(Value::Decimal { unscaled, width: width.max(1) as u8, scale: scale as u8 });
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

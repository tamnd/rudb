//! Stars and `COLUMNS`, the expressions that stand for a set of the columns the FROM clause has.
//!
//! A star is the set written out in place, one select item per column, with its `EXCLUDE`,
//! `REPLACE` and `RENAME` lists applied on the way. `COLUMNS(...)` is the same set used as a
//! pattern: the expression it sits in is bound once per column, with the `COLUMNS` standing for
//! that column each time, and each copy is named after the column. What goes inside picks the set.
//! A star picks what the star stands for, a string is a regex the name has to match somewhere, and
//! anything else is bound and folded and has to come to a list of names. A `*` written inside that
//! expression is the list of the names it stands for, which is how a lambda over the names works:
//! the parser writes `COLUMNS(lambda x: ...)` as `COLUMNS(list_filter(*, lambda x: ...))`.
//!
//! This is the pin's `bind_star_expression.cpp`, and the errors are its errors.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_parse::ast::{self, Ast};
use rudb_parse::{NONE, deparse};
use rudb_plan::{ConjunctionOp, Expr, ExprRef};
use rudb_regex::Regex;

use crate::binder::Binder;
use crate::fold;
use crate::scope::{Scope, Visible};
use rudb_catalog::same_name;

/// One column a star or a `COLUMNS` stands for.
#[derive(Debug, Clone)]
pub(crate) struct Picked {
    /// The column.
    pub(crate) column: Visible,
    /// What a `REPLACE` put in the column's place, or `NONE`.
    pub(crate) replacement: ast::ExprRef,
    /// What the column is called in the output, which a `RENAME` or a `REPLACE` can change.
    pub(crate) name: String,
}

/// Every column a `COLUMNS` stands for, and the regex that picked them when one did.
pub(crate) struct Picks {
    pub(crate) entries: Vec<Picked>,
    regex: Option<Regex>,
}

impl Picks {
    /// The name the copy of an expression bound for `picked` goes by, which is the column's name
    /// unless the expression had an alias. An alias is the name of every copy, with `\0` standing
    /// for the column's name and `\1` to `\9` for what the regex's groups caught in it.
    pub(crate) fn name(&self, picked: &Picked, alias: Option<&str>) -> Result<String> {
        let Some(alias) = alias else { return Ok(picked.name.clone()) };
        let column = picked.column.name.as_str();
        let mut out = String::with_capacity(alias.len());
        let mut chars = alias.chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                out.push(ch);
                continue;
            }
            match chars.next() {
                None => {
                    return Err(Error::binder(format!(
                        "Unterminated backslash in COLUMNS(*) \"{alias}\" alias. Backslashes must \
                         either be escaped or followed by a number"
                    )));
                }
                Some('\\') => out.push('\\'),
                Some('0') => out.push_str(column),
                Some(digit @ '1'..='9') => {
                    let Some(regex) = &self.regex else {
                        return Err(Error::binder(
                            "Only the backslash escape code \\0 can be used when no regex is \
                             supplied to COLUMNS(*)",
                        ));
                    };
                    let group = digit as usize - '0' as usize;
                    out.push_str(regex.extract(column, group).unwrap_or_default());
                }
                Some(_) => {
                    return Err(Error::binder(format!(
                        "Invalid backslash code in COLUMNS(*) \"{alias}\" alias. Backslashes must \
                         either be escaped or followed by a number"
                    )));
                }
            }
        }
        Ok(out)
    }
}

/// The star or the `COLUMNS` an expression holds.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Found {
    /// The expression is a star and nothing else.
    Star,
    /// A `COLUMNS` somewhere inside, this one.
    Columns(ast::ExprRef),
    /// A `*COLUMNS` somewhere inside.
    Unpacked(ast::ExprRef),
}

/// The star or the `COLUMNS` in `expr`, not looking inside a subquery, which has its own.
///
/// Every `COLUMNS` in one expression has to be the same one, since they all stand for the same
/// column in each copy.
pub(crate) fn find_star(ast: &Ast, expr: ast::ExprRef) -> Result<Option<Found>> {
    if matches!(ast.expr(expr), ast::Expr::Star { .. }) {
        return Ok(Some(Found::Star));
    }
    let mut found = None;
    let mut unpacked = false;
    walk(ast, expr, false, &mut found, &mut unpacked)?;
    Ok(found
        .map(|columns| if unpacked { Found::Unpacked(columns) } else { Found::Columns(columns) }))
}

fn walk(
    ast: &Ast,
    expr: ast::ExprRef,
    in_columns: bool,
    found: &mut Option<ast::ExprRef>,
    unpacked: &mut bool,
) -> Result<()> {
    if expr == NONE {
        return Ok(());
    }
    match ast.expr(expr) {
        ast::Expr::Star { replacements, .. } => {
            if !in_columns {
                return Err(Error::binder(
                    "STAR expression is only allowed as the root element of an expression. Use \
                     COLUMNS(*) instead.",
                ));
            }
            if !replacements.is_empty() {
                return Err(Error::binder(
                    "STAR expression with REPLACE list is only allowed as the root element of \
                     COLUMNS",
                ));
            }
            if !ast.star_lists(expr).renames.is_empty() {
                return Err(Error::binder(
                    "STAR expression with RENAME list is only allowed as the root element of \
                     COLUMNS",
                ));
            }
            Ok(())
        }
        ast::Expr::Columns { inner, unpacked: written } => {
            if in_columns {
                return Err(Error::binder(
                    "COLUMNS expression is not allowed inside another COLUMNS expression",
                ));
            }
            *unpacked |= written;
            if let Some(seen) = *found {
                if deparse::expression(ast, seen) != deparse::expression(ast, expr) {
                    return Err(Error::binder(
                        "Multiple different STAR/COLUMNS in the same expression are not supported",
                    ));
                }
                return Ok(());
            }
            *found = Some(expr);
            // A star directly inside is the set itself and is allowed its lists.
            if matches!(ast.expr(inner), ast::Expr::Star { .. }) {
                return Ok(());
            }
            walk(ast, inner, true, found, unpacked)
        }
        // `count(*)` and its like, where the star is the call's own business. `count(DISTINCT *)`
        // is not one, and the pin refuses it as a star that is not the root.
        ast::Expr::Function { args, distinct: false, .. }
        | ast::Expr::Window { args, distinct: false, .. }
            if args.len == 1
                && matches!(ast.expr(ast.expr_list(args)[0]), ast::Expr::Star { .. }) =>
        {
            Ok(())
        }
        _ => {
            for child in children(ast, expr) {
                walk(ast, child, in_columns, found, unpacked)?;
            }
            Ok(())
        }
    }
}

/// Whether a subquery is anywhere in `expr`.
pub(crate) fn has_subquery(ast: &Ast, expr: ast::ExprRef) -> bool {
    matches!(
        ast.expr(expr),
        ast::Expr::Subquery { .. }
            | ast::Expr::Exists { .. }
            | ast::Expr::InSubquery { .. }
            | ast::Expr::QuantifiedSubquery { .. }
    ) || children(ast, expr).into_iter().any(|child| has_subquery(ast, child))
}

/// Whether a star or a `COLUMNS` is anywhere in `expr` outside a subquery.
pub(crate) fn has_star(ast: &Ast, expr: ast::ExprRef) -> bool {
    match ast.expr(expr) {
        ast::Expr::Star { .. } | ast::Expr::Columns { .. } => true,
        ast::Expr::Function { args, distinct: false, .. }
        | ast::Expr::Window { args, distinct: false, .. }
            if args.len == 1
                && matches!(ast.expr(ast.expr_list(args)[0]), ast::Expr::Star { .. }) =>
        {
            false
        }
        _ => children(ast, expr).into_iter().any(|child| has_star(ast, child)),
    }
}

/// The expressions directly under `expr`, leaving out the body of a subquery.
fn children(ast: &Ast, expr: ast::ExprRef) -> Vec<ast::ExprRef> {
    let list = |slice: ast::Slice| ast.expr_list(slice).to_vec();
    let mut out = match ast.expr(expr) {
        ast::Expr::Star { .. }
        | ast::Expr::Column { .. }
        | ast::Expr::Literal { .. }
        | ast::Expr::Parameter { .. }
        | ast::Expr::Default
        | ast::Expr::Subquery { .. }
        | ast::Expr::Exists { .. } => Vec::new(),
        ast::Expr::Columns { inner, .. } => vec![inner],
        ast::Expr::Unary { operand, .. }
        | ast::Expr::Cast { operand, .. }
        | ast::Expr::InSubquery { operand, .. }
        | ast::Expr::QuantifiedSubquery { operand, .. } => vec![operand],
        ast::Expr::Lambda { body, .. } => vec![body],
        ast::Expr::Binary { left, right, .. } => vec![left, right],
        ast::Expr::Function { args, filter, .. } => {
            let mut out = list(args);
            out.push(filter);
            out.extend(ast.named_args(expr).iter().map(|target| target.expr));
            out.extend(ast.aggregate_order(expr).iter().map(|item| item.expr));
            out
        }
        ast::Expr::Window { args, filter, order, spec, .. } => {
            let held = ast.window(spec);
            let mut out = list(args);
            out.push(filter);
            out.extend(ast.order_list(order).iter().map(|item| item.expr));
            out.extend(list(held.partition));
            out.extend(ast.order_list(held.order).iter().map(|item| item.expr));
            out
        }
        ast::Expr::Case { operand, arms, otherwise } => {
            let mut out = vec![operand, otherwise];
            for arm in ast.arm_list(arms) {
                out.extend([arm.when, arm.then]);
            }
            out
        }
        ast::Expr::Between { operand, low, high, .. } => vec![operand, low, high],
        ast::Expr::In { operand, list: items, .. } => {
            let mut out = vec![operand];
            out.extend(list(items));
            out
        }
        ast::Expr::List { items } | ast::Expr::Row { items } => list(items),
        ast::Expr::Struct { values, .. } => list(values),
    };
    out.retain(|&child| child != NONE);
    out
}

/// A name written in an `EXCLUDE` or a `RENAME` list, the way the pin prints one.
fn written_name(parts: &[&str]) -> String {
    parts.iter().map(|part| format!("\"{part}\"")).collect::<Vec<_>>().join(".")
}

/// Whether a column is the one a name in an `EXCLUDE` or a `RENAME` list names, which is by its
/// name alone or by its table and its name.
fn names_column(parts: &[&str], column: &Visible) -> bool {
    match parts {
        [name] => same_name(name, &column.name),
        [.., table, name] => same_name(table, &column.table) && same_name(name, &column.name),
        [] => false,
    }
}

impl Binder<'_> {
    /// The columns a star stands for, with its lists applied.
    pub(crate) fn star_columns(
        &mut self,
        ast: &Ast,
        star: ast::ExprRef,
        input: &Scope,
    ) -> Result<Vec<Picked>> {
        let ast::Expr::Star { qualifier, replacements } = ast.expr(star) else {
            return Err(Error::internal("star columns of something that is not a star"));
        };
        let table = ast.name(qualifier).last().map(str::to_string);
        let lists = ast.star_lists(star);
        let mut excluded = Vec::with_capacity(lists.exclude.len as usize);
        for &name in ast.name_list(lists.exclude) {
            excluded.push(ast.name(name).collect::<Vec<_>>());
        }
        let renames = ast.target_list(lists.renames);
        let replacements = ast.target_list(replacements);
        let mut excluded_used = vec![false; excluded.len()];
        let mut replaced = vec![false; replacements.len()];
        let mut picked = Vec::new();
        for column in input.star(table.as_deref())? {
            if let Some(at) = excluded.iter().position(|parts| names_column(parts, column)) {
                excluded_used[at] = true;
                continue;
            }
            let mut name = column.name.clone();
            let mut replacement = NONE;
            if let Some(at) =
                replacements.iter().position(|held| same_name(ast.string(held.alias), &column.name))
            {
                replaced[at] = true;
                replacement = replacements[at].expr;
                // The replacement is named the way the list spells it, which only shows when the
                // two differ in case.
                name = ast.string(replacements[at].alias).to_string();
            }
            // A rename that names no column is not refused, and the last one to name it wins.
            for rename in renames {
                if let ast::Expr::Column { name: from } = ast.expr(rename.expr) {
                    let parts: Vec<&str> = ast.name(from).collect();
                    if names_column(&parts, column) {
                        name = ast.string(rename.alias).to_string();
                    }
                }
            }
            picked.push(Picked { column: column.clone(), replacement, name });
        }
        if let Some(at) = excluded_used.iter().position(|used| !used) {
            let place = match &table {
                Some(table) => table.clone(),
                None => "FROM clause".to_string(),
            };
            return Err(Error::binder(format!(
                "Column {} in EXCLUDE list not found in {place}{}",
                written_name(&excluded[at]),
                input.candidates()
            )));
        }
        if let Some(at) = replaced.iter().position(|used| !used) {
            return Err(crate::binder::missing_replacement(
                ast.string(replacements[at].alias),
                input,
            ));
        }
        Ok(picked)
    }

    /// Every column a `COLUMNS` stands for, which is never none.
    pub(crate) fn columns_picks(
        &mut self,
        ast: &Ast,
        columns: ast::ExprRef,
        input: &Scope,
    ) -> Result<Picks> {
        let ast::Expr::Columns { inner, .. } = ast.expr(columns) else {
            return Err(Error::internal("the columns of something that is not COLUMNS"));
        };
        let written = deparse::expression(ast, columns);
        if matches!(ast.expr(inner), ast::Expr::Star { .. }) {
            let entries = self.star_columns(ast, inner, input)?;
            if entries.is_empty() {
                return Err(Error::binder(format!(
                    "Star expression \"{written}\" resulted in an empty set of columns"
                )));
            }
            return Ok(Picks { entries, regex: None });
        }
        let every = input.star(None)?.into_iter().cloned().collect::<Vec<_>>();
        let value = self.columns_argument(ast, inner, input)?;
        // The pin quotes the argument as it was bound, so a star in it is the names it stood for.
        let names: Vec<String> =
            every.iter().map(|column| format!("'{}'", column.name.replace('\'', "''"))).collect();
        let written = written.replacen("(*", &format!("([{}]", names.join(", ")), 1);
        let empty = || {
            Error::binder(format!(
                "Star expression \"{written}\" resulted in an empty set of columns"
            ))
        };
        picks_from(value, &every, empty)
    }

    /// The columns a star with a pattern applied to it stands for, when `root` is one, as in
    /// `* LIKE 'a%'`, and `None` when it is not.
    ///
    /// The pin rewrites one to `COLUMNS(list_filter(*, lambda __lambda_col: __lambda_col LIKE
    /// 'a%'))`, or to `COLUMNS('regex')` for a `SIMILAR TO` with nothing excluded, so the columns
    /// are the ones the pattern holds for and each is named the way a `COLUMNS` names it. Every
    /// other function or operator with a star on its left is refused in the pin's words, and so is
    /// a pattern that is not a constant.
    pub(crate) fn star_like(
        &mut self,
        ast: &Ast,
        root: ast::ExprRef,
        input: &Scope,
    ) -> Result<Option<Picks>> {
        let (inverse, call) = match ast.expr(root) {
            ast::Expr::Unary { op: ast::UnaryOp::Not, operand } => (true, operand),
            _ => (false, root),
        };
        let Some(applied) = star_call(ast, call) else { return Ok(None) };
        let ast::Expr::Star { replacements, .. } = ast.expr(applied.star) else { return Ok(None) };
        if !FILTERS.contains(&applied.name.as_str()) {
            return Err(Error::binder(format!(
                "Function \"\"{}\"\" cannot be applied to a star expression",
                applied.name
            )));
        }
        if !matches!(ast.expr(applied.pattern), ast::Expr::Literal { .. }) {
            return Err(Error::binder("Pattern applied to a star expression must be a constant"));
        }
        let lists = ast.star_lists(applied.star);
        if !lists.renames.is_empty() {
            return Err(Error::binder("Rename list cannot be combined with a filtering operation"));
        }
        if !replacements.is_empty() {
            return Err(Error::binder(
                "Replace list cannot be combined with a filtering operation",
            ));
        }
        let entries = self.star_columns(ast, applied.star, input)?;
        let every: Vec<Visible> = entries.iter().map(|picked| picked.column.clone()).collect();
        let mut pattern = deparse::expression(ast, applied.pattern);
        if applied.third != NONE {
            pattern = format!("{pattern}, {}", deparse::expression(ast, applied.third));
        }
        let inverse = inverse != applied.inverse;
        if !inverse
            && applied.name == "regexp_full_match"
            && applied.arity == 2
            && lists.exclude.is_empty()
        {
            let bound = self.bind_expr(ast, applied.pattern, &Scope::empty())?;
            let ty = self.plan().expr_type(bound).clone();
            let value = fold::value_of(self.plan(), bound)?.unwrap_or(Value::Null);
            let empty = || {
                Error::binder(format!(
                    "Star expression \"COLUMNS({pattern})\" resulted in an empty set of columns"
                ))
            };
            return picks_from((ty, value), &every, empty).map(Some);
        }
        let mut kept = Vec::with_capacity(entries.len());
        for picked in entries {
            self.star_name = Some(picked.column.name.clone());
            let bound = self.bind_expr(ast, root, &Scope::empty());
            self.star_name = None;
            let bound = bound?;
            if fold::value_of(self.plan(), bound)?.and_then(|value| value.as_bool()) == Some(true) {
                kept.push(picked);
            }
        }
        if kept.is_empty() {
            let names: Vec<String> = every
                .iter()
                .map(|column| format!("'{}'", column.name.replace('\'', "''")))
                .collect();
            let body = if applied.infix {
                format!("(__lambda_col {} {pattern})", applied.name)
            } else {
                format!("{}(__lambda_col, {pattern})", applied.name)
            };
            let body = if inverse { format!("(NOT {body})") } else { body };
            return Err(Error::binder(format!(
                "Star expression \"COLUMNS(list_filter([{}], (lambda __lambda_col: {body})))\" \
                 resulted in an empty set of columns",
                names.join(", ")
            )));
        }
        Ok(Some(Picks { entries: kept, regex: None }))
    }

    /// What the argument of a `COLUMNS` folds to, bound with no columns in reach. A `*` in it is
    /// the list of the names the star stands for.
    fn columns_argument(
        &mut self,
        ast: &Ast,
        inner: ast::ExprRef,
        input: &Scope,
    ) -> Result<(LogicalType, Value)> {
        let outer_scopes = std::mem::take(&mut self.outer_scopes);
        let outer_columns = self.columns_scope.replace(input.clone());
        let bound = self.bind_expr(ast, inner, &Scope::empty());
        self.columns_scope = outer_columns;
        self.outer_scopes = outer_scopes;
        let bound = bound?;
        let ty = self.plan().expr_type(bound).clone();
        let Some(value) = fold::value_with_lambdas(self.plan(), bound)? else {
            return Err(Error::binder("Unsupported expression in COLUMNS"));
        };
        Ok((ty, value))
    }

    /// A `*` inside the argument of a `COLUMNS`, which is the names it stands for as a list.
    pub(crate) fn star_names(&mut self, ast: &Ast, star: ast::ExprRef) -> Result<Option<ExprRef>> {
        let Some(input) = self.columns_scope.clone() else { return Ok(None) };
        let names: Vec<Value> = self
            .star_columns(ast, star, &input)?
            .into_iter()
            .map(|picked| Value::Varchar(picked.column.name))
            .collect();
        Ok(Some(self.add_constant(Value::List { element: LogicalType::Varchar, values: names })))
    }

    /// A `WHERE` that has a star or a `COLUMNS` in it, which is each side of an `AND` on its own
    /// and a `COLUMNS` bound once per column with the copies joined by `AND`.
    pub(crate) fn bind_star_predicate(
        &mut self,
        ast: &Ast,
        expr: ast::ExprRef,
        input: &Scope,
    ) -> Result<ExprRef> {
        let parts = match ast.expr(expr) {
            ast::Expr::Binary { op: ast::BinaryOp::And, left, right } => {
                vec![
                    self.bind_star_predicate(ast, left, input)?,
                    self.bind_star_predicate(ast, right, input)?,
                ]
            }
            ast::Expr::Star { .. } => {
                return Err(Error::parser(
                    "STAR expression is not allowed in the WHERE clause. Use COLUMNS(*) instead.",
                ));
            }
            _ => match find_star(ast, expr)? {
                None | Some(Found::Star) => vec![self.bind_expr(ast, expr, input)?],
                Some(Found::Unpacked(columns)) => {
                    let picks = self.columns_picks(ast, columns, input)?;
                    let copy = self.unpacked(ast, expr, &picks);
                    vec![self.bind_expr(&copy, expr, input)?]
                }
                Some(Found::Columns(columns)) => {
                    let picks = self.columns_picks(ast, columns, input)?;
                    let mut parts = Vec::with_capacity(picks.entries.len());
                    for picked in picks.entries {
                        self.star_entry = Some(picked);
                        let bound = self.bind_expr(ast, expr, input);
                        self.star_entry = None;
                        parts.push(bound?);
                    }
                    parts
                }
            },
        };
        let mut parts = parts.into_iter();
        let first = parts
            .next()
            .ok_or_else(|| Error::parser("COLUMNS expansion resulted in empty set of columns"))?;
        let mut predicate = self.as_boolean(first, "WHERE")?;
        for next in parts {
            let next = self.as_boolean(next, "WHERE")?;
            let children = self.plan_mut().add_expr_list(&[predicate, next]);
            let conjunction = Expr::Conjunction { op: ConjunctionOp::And, children };
            predicate = self.add_expr(conjunction, LogicalType::Boolean);
        }
        Ok(predicate)
    }

    /// The expressions an `ORDER BY` item or a `GROUP BY ALL` key stands for when it is a star or
    /// holds a `COLUMNS`, one per column and each bound in `input`, and `None` when it is neither.
    pub(crate) fn bind_star_each(
        &mut self,
        ast: &Ast,
        expr: ast::ExprRef,
        input: &Scope,
    ) -> Result<Option<Vec<ExprRef>>> {
        if let Some(picks) = self.star_like(ast, expr, input)? {
            let mut bound = Vec::with_capacity(picks.entries.len());
            for picked in picks.entries {
                self.star_entry = Some(picked);
                let expr = self.bind_picked(ast, input);
                self.star_entry = None;
                bound.push(expr?);
            }
            return Ok(Some(bound));
        }
        let columns = match find_star(ast, expr)? {
            Some(Found::Columns(columns)) => columns,
            Some(Found::Unpacked(columns)) => {
                let picks = self.columns_picks(ast, columns, input)?;
                let copy = self.unpacked(ast, expr, &picks);
                return Ok(Some(vec![self.bind_expr(&copy, expr, input)?]));
            }
            Some(Found::Star) => {
                let mut bound = Vec::new();
                for picked in self.star_columns(ast, expr, input)? {
                    bound.push(if picked.replacement == NONE {
                        self.add_expr(Expr::Column(picked.column.binding), picked.column.ty)
                    } else {
                        self.bind_expr(ast, picked.replacement, input)?
                    });
                }
                return Ok(Some(bound));
            }
            _ => return Ok(None),
        };
        let picks = self.columns_picks(ast, columns, input)?;
        let mut bound = Vec::with_capacity(picks.entries.len());
        for picked in picks.entries {
            self.star_entry = Some(picked);
            let expr = self.bind_expr(ast, expr, input);
            self.star_entry = None;
            bound.push(expr?);
        }
        Ok(Some(bound))
    }

    /// A copy of `ast` with every `*COLUMNS` in `expr` written out as the columns it picks, in the
    /// argument list, the list, the row or the `IN` list it was written in, which is where the pin
    /// unpacks one. The copy keeps every index the original has and only adds to them, so `expr`
    /// and everything else in the statement mean the same thing in both.
    ///
    /// Each column is written with the name the pin gives it, which is how the item is named: a
    /// column of a table in the catalog is `memory.main.t.a`, and one of an alias or a subquery is
    /// the alias and the column.
    pub(crate) fn unpacked(&self, ast: &Ast, expr: ast::ExprRef, picks: &Picks) -> Ast {
        let mut copy = ast.clone();
        let span = ast.expr_span(expr);
        let columns: Vec<ast::ExprRef> = picks
            .entries
            .iter()
            .map(|picked| {
                let column = &picked.column;
                let mut parts = Vec::with_capacity(4);
                if !column.table.is_empty() {
                    let catalog = self.catalog();
                    let name = rudb_catalog::QualifiedName::new(
                        catalog.default_catalog(),
                        catalog.default_schema(),
                        column.table.as_str(),
                    );
                    if catalog.entry(&name).is_ok() {
                        parts.push(catalog.default_catalog().to_string());
                        parts.push(catalog.default_schema().to_string());
                    }
                    parts.push(column.table.clone());
                }
                parts.push(column.name.clone());
                let start = copy.parts.len() as u32;
                for part in parts {
                    copy.parts.push(copy.strings.len() as ast::StrRef);
                    copy.strings.push(part);
                }
                let name = ast::Slice { start, len: copy.parts.len() as u32 - start };
                copy.exprs.push(ast::Expr::Column { name });
                copy.expr_spans.push(span);
                (copy.exprs.len() - 1) as ast::ExprRef
            })
            .collect();
        let mut stack = vec![expr];
        while let Some(at) = stack.pop() {
            stack.extend(children(ast, at));
            let mut expand = |slice: ast::Slice| {
                let items: Vec<(ast::ExprRef, bool)> = copy
                    .expr_list(slice)
                    .iter()
                    .map(|&item| {
                        (item, matches!(copy.expr(item), ast::Expr::Columns { unpacked: true, .. }))
                    })
                    .collect();
                if !items.iter().any(|&(_, unpacked)| unpacked) {
                    return None;
                }
                let start = copy.expr_lists.len() as u32;
                for (item, unpacked) in items {
                    if unpacked {
                        copy.expr_lists.extend(&columns);
                    } else {
                        copy.expr_lists.push(item);
                    }
                }
                Some(ast::Slice { start, len: copy.expr_lists.len() as u32 - start })
            };
            let rewritten = match ast.expr(at) {
                ast::Expr::Function { name, args, distinct, filter } => {
                    expand(args).map(|args| ast::Expr::Function { name, args, distinct, filter })
                }
                ast::Expr::List { items } => expand(items).map(|items| ast::Expr::List { items }),
                ast::Expr::Row { items } => expand(items).map(|items| ast::Expr::Row { items }),
                ast::Expr::In { operand, list, negated } => {
                    expand(list).map(|list| ast::Expr::In { operand, list, negated })
                }
                _ => None,
            };
            if let Some(rewritten) = rewritten {
                copy.exprs[at as usize] = rewritten;
            }
        }
        copy
    }

    /// The `COLUMNS` being bound for one of its columns, as that column.
    pub(crate) fn bind_picked(&mut self, ast: &Ast, scope: &Scope) -> Result<ExprRef> {
        match self.star_entry.clone() {
            Some(picked) if picked.replacement != NONE => {
                self.bind_expr(ast, picked.replacement, scope)
            }
            Some(picked) => {
                Ok(self.add_expr(Expr::Column(picked.column.binding), picked.column.ty))
            }
            None => Err(Error::binder("STAR expression is not supported here")),
        }
    }
}

/// The functions and operators a star can have a pattern applied to it with, by the name the pin
/// gives them.
const FILTERS: [&str; 12] = [
    "~~",
    "!~~",
    "~~~",
    "!~~~",
    "~~*",
    "!~~*",
    "regexp_full_match",
    "regexp_matches",
    "not_like_escape",
    "ilike_escape",
    "not_ilike_escape",
    "like_escape",
];

/// A function or an operator with a star as its first argument, which is what `star_call` finds.
struct Applied {
    /// The name the pin gives the function, which is its operator for most operators.
    name: String,
    star: ast::ExprRef,
    pattern: ast::ExprRef,
    /// How many arguments the pin's function has, so `~*` is three for the flag it adds.
    arity: usize,
    /// Whether the pin writes it as an operator rather than a call.
    infix: bool,
    /// Whether the pin writes it as a `NOT` around the function, as `NOT SIMILAR TO` is.
    inverse: bool,
    /// A third argument written on a call, the escape of `like_escape(*, 'a$%', '$')`.
    third: ast::ExprRef,
}

/// The function or operator `expr` is when its first argument is a star, the way the pin sees it.
/// A comparison is not one, because the pin does not parse it as a function.
fn star_call(ast: &Ast, expr: ast::ExprRef) -> Option<Applied> {
    use ast::BinaryOp as Op;
    let applied = |name: &str, star, pattern, arity, infix, inverse| Applied {
        name: name.to_string(),
        star,
        pattern,
        arity,
        infix,
        inverse,
        third: NONE,
    };
    let found = match ast.expr(expr) {
        ast::Expr::Binary { op, left, right } => {
            let infix = |name: &str| Some(applied(name, left, right, 2, true, false));
            match op {
                Op::Add => infix("+"),
                Op::Subtract => infix("-"),
                Op::Multiply => infix("*"),
                Op::Divide => infix("/"),
                Op::IntegerDivide => infix("//"),
                Op::Modulo => infix("%"),
                Op::Power => infix("**"),
                Op::Caret => infix("^"),
                Op::BitAnd => infix("&"),
                Op::BitOr => infix("|"),
                Op::ShiftLeft => infix("<<"),
                Op::ShiftRight => infix(">>"),
                Op::Concat => infix("||"),
                Op::Like => infix("~~"),
                Op::NotLike => infix("!~~"),
                Op::ILike => infix("~~*"),
                Op::NotILike => infix("!~~*"),
                Op::Glob => infix("~~~"),
                Op::Contains => infix("@>"),
                Op::ContainedBy => infix("<@"),
                Op::Overlaps => infix("&&"),
                Op::StartsWith => infix("^@"),
                Op::SimilarTo | Op::Regex => {
                    Some(applied("regexp_full_match", left, right, 2, false, false))
                }
                Op::NotSimilarTo | Op::NotRegex => {
                    Some(applied("regexp_full_match", left, right, 2, false, true))
                }
                Op::RegexInsensitive => {
                    Some(applied("regexp_full_match", left, right, 3, false, false))
                }
                Op::NotRegexInsensitive => {
                    Some(applied("regexp_full_match", left, right, 3, false, true))
                }
                _ => None,
            }
        }
        ast::Expr::Function { name, args, .. } if (2..=3).contains(&args.len) => {
            let arguments = ast.expr_list(args);
            let written = ast.name(name).last().unwrap_or_default();
            let mut found =
                applied(written, arguments[0], arguments[1], arguments.len(), false, false);
            found.third = arguments.get(2).copied().unwrap_or(NONE);
            Some(found)
        }
        _ => None,
    }?;
    matches!(ast.expr(found.star), ast::Expr::Star { .. }).then_some(found)
}

/// The columns out of `every` that a folded `COLUMNS` argument picks, which is a regex the name has
/// to match somewhere or a list of the names. `empty` is the error for a list with nothing in it.
fn picks_from(
    value: (LogicalType, Value),
    every: &[Visible],
    empty: impl Fn() -> Error,
) -> Result<Picks> {
    let pick = |column: &Visible| Picked {
        column: column.clone(),
        replacement: NONE,
        name: column.name.clone(),
    };
    match value {
        (LogicalType::Varchar, Value::Varchar(pattern)) => {
            let regex = Regex::new(&pattern).map_err(|error| {
                Error::binder(format!("Failed to compile regex \"{pattern}\": {}", error.message()))
            })?;
            let entries: Vec<Picked> =
                every.iter().filter(|column| regex.is_match(&column.name)).map(pick).collect();
            if entries.is_empty() {
                let names: Vec<String> =
                    every.iter().map(|column| format!("\"{}\"", column.name)).collect();
                let candidates = if names.is_empty() {
                    String::new()
                } else {
                    format!("\n\nDid you mean: {}", names.join(", "))
                };
                return Err(Error::binder(format!(
                    "No matching columns found that match regex \"{pattern}\"{candidates}"
                )));
            }
            Ok(Picks { entries, regex: Some(regex) })
        }
        (LogicalType::Varchar, Value::Null) => {
            Err(Error::binder("COLUMNS does not support NULL as regex argument"))
        }
        (LogicalType::List(element), value) if *element == LogicalType::Varchar => {
            let items = match value {
                Value::List { values, .. } => values,
                _ => Vec::new(),
            };
            if items.is_empty() {
                return Err(empty());
            }
            let mut wanted = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Varchar(name) => wanted.push((name, false)),
                    _ => {
                        return Err(Error::binder(
                            "Columns expression does not support NULL input parameters",
                        ));
                    }
                }
            }
            let mut entries = Vec::new();
            for column in every {
                let mut hit = false;
                for (name, seen) in &mut wanted {
                    if same_name(name, &column.name) {
                        *seen = true;
                        hit = true;
                    }
                }
                if hit {
                    entries.push(pick(column));
                }
            }
            if let Some((name, _)) = wanted.iter().find(|(_, seen)| !seen) {
                return Err(Error::binder(format!(
                    "Column \"{name}\" was selected but was not found in the FROM clause"
                )));
            }
            Ok(Picks { entries, regex: None })
        }
        _ => Err(Error::binder(
            "COLUMNS expects either a VARCHAR argument (regex) or a LIST of VARCHAR (list of \
             columns)",
        )),
    }
}

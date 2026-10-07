//! `PIVOT` and `UNPIVOT`, which the pin binds by writing the query each one stands for and binding
//! that, and which are bound the same way here.
//!
//! A pivot is a grouped query with one filtered aggregate for each value it spreads on, so `PIVOT
//! t ON a IN (1, 2) USING sum(c)` is `SELECT b, sum(c) FILTER (WHERE CAST(a AS VARCHAR) IS NOT
//! DISTINCT FROM '1') AS "1", ... FROM t GROUP BY b`, grouped on every column the pivot does not
//! read. An unpivot is the list of the columns it reads taken apart with `unnest`, next to the list
//! of their names. The pin switches a pivot over to a list based plan once it spreads on more than
//! twenty values, which answers the same rows, and only the filtered form is written here.
//!
//! The source is bound once, before the query is written, because the query needs its column names
//! and because a source can read a `WITH` the written query would not see. The written query reads
//! it under a name of its own, which [`Binder::pivot_source`] answers with what was bound. This is
//! the pin's `bind_pivot.cpp`, and the errors are its errors.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use rudb_catalog::same_name;
use rudb_common::{Error, LogicalType, Result, Value};
use rudb_parse::ast::{self, Ast, LiteralKind};
use rudb_parse::{NONE, deparse, parse_ast_with_case};
use rudb_plan::NodeRef;

use crate::binder::Binder;
use crate::fold;
use crate::scope::Scope;
use crate::statement::read_type;

/// One column a pivot makes: the value of each expression it spreads on, as text, and its name.
#[derive(Clone)]
struct Spread {
    values: Vec<Option<String>>,
    name: String,
}

/// One entry of an unpivot's list: the expressions it reads, the column each one reads and its
/// alias, empty when it has none.
struct Unspread {
    exprs: Vec<String>,
    columns: Vec<String>,
    alias: String,
}

impl Binder<'_> {
    /// A `PIVOT` or an `UNPIVOT` in a `FROM` clause, as the query it stands for.
    pub(crate) fn bind_pivot(&mut self, ast: &Ast, index: u32) -> Result<(NodeRef, Scope)> {
        let pivot = ast.pivot(index);
        let (node, scope) = self.bind_source(ast, pivot.source)?;
        let names: Vec<String> = scope
            .star(self.semantics.identifier_compare(), None)?
            .iter()
            .map(|column| column.name.clone())
            .collect();
        let held = format!("__pivot_source_{}", self.pivot_sources.len());
        let (inner, filter) = if pivot.unpivot {
            self.unpivot_query(ast, &pivot, &names, &held, &scope)?
        } else {
            (self.pivot_query(ast, &pivot, &names, &held)?, Vec::new())
        };
        let label = if pivot.alias == NONE {
            "__unnamed_pivot".to_string()
        } else {
            ast.string(pivot.alias).to_string()
        };
        let mut text = format!("SELECT * FROM ({inner}) AS {}", ident(&label));
        if !pivot.columns_alias.is_empty() {
            let columns: Vec<String> = ast.name(pivot.columns_alias).map(ident).collect();
            text.push_str(&format!("({})", columns.join(", ")));
        }
        if !filter.is_empty() {
            text.push_str(" WHERE ");
            text.push_str(&filter.join(" AND "));
        }
        self.keep_statement(ast);
        let parsed = parse_ast_with_case(&text, self.semantics.identifier_case())?;
        let Some(&ast::Statement::Query(query)) = parsed.statements.first() else {
            return Err(Error::internal("the query a pivot stands for"));
        };
        self.pivot_sources.push((held, node, scope));
        let bound = self.bind_query(&parsed, query);
        self.pivot_sources.pop();
        let (node, mut scope) = bound?;
        scope.relabel(&label);
        Ok((node, scope))
    }

    /// What a pivot's source was bound to, when `name` is the name the query written for the pivot
    /// reads it under.
    pub(crate) fn pivot_source(&self, name: &str) -> Option<(NodeRef, Scope)> {
        self.pivot_sources
            .iter()
            .rev()
            .find(|(held, ..)| held == name)
            .map(|(_, node, scope)| (*node, scope.clone()))
    }

    /// The grouped query with a filtered aggregate for each column a pivot makes.
    fn pivot_query(
        &mut self,
        ast: &Ast,
        pivot: &ast::Pivot,
        names: &[String],
        held: &str,
    ) -> Result<String> {
        let mut handled = Vec::new();
        let aggregates = ast.target_list(pivot.aggregates).to_vec();
        let mut calls = Vec::with_capacity(aggregates.len());
        for target in &aggregates {
            if crate::columns::has_subquery(ast, target.expr) {
                return Err(Error::binder("Pivot expression cannot contain subqueries"));
            }
            if has_window(ast, target.expr) {
                return Err(Error::binder("Pivot expression cannot contain window functions"));
            }
            let mut found = Vec::new();
            pivot_aggregates(ast, target.expr, &mut found)?;
            if found.len() != 1 {
                let counted = if found.is_empty() {
                    "but no aggregates were found".to_string()
                } else {
                    format!("but {} were found", found.len())
                };
                return Err(Error::binder(format!(
                    "Pivot expression must contain exactly one aggregate, {counted}"
                )));
            }
            column_names(ast, found[0], &mut handled)?;
            calls.push(found[0]);
        }
        let columns = ast.pivot_column_list(pivot.columns).to_vec();
        let mut spreads: Vec<Spread> = vec![Spread { values: Vec::new(), name: String::new() }];
        let mut total = 1usize;
        let mut listed = Vec::with_capacity(columns.len());
        for column in &columns {
            let exprs = ast.expr_list(column.exprs).to_vec();
            let mut entries: Vec<(Vec<Option<String>>, Option<String>)> = Vec::new();
            for target in ast.target_list(column.entries) {
                let mut values = Vec::new();
                self.pivot_in_values(ast, target.expr, &mut values)?;
                if values.len() != exprs.len() {
                    return Err(Error::binder(format!(
                        "PIVOT IN list - inconsistent amount of rows - expected {} but got {}",
                        exprs.len(),
                        values.len()
                    )));
                }
                let alias = (target.alias != NONE).then(|| ast.string(target.alias).to_string());
                entries.push((values, alias));
            }
            if column.enum_name != NONE {
                let name = ast.string(column.enum_name);
                let ty = read_type(self.catalog(), name)?;
                let Some(labels) = ty.labels() else {
                    return Err(Error::binder(format!(
                        "Pivot must reference an ENUM type: \"{name}\" is of type \"{ty}\""
                    )));
                };
                for label in labels {
                    entries.push((vec![Some(label.clone())], Some(label.clone())));
                }
            }
            total = total.saturating_mul(entries.len());
            for &expr in &exprs {
                column_names(ast, expr, &mut handled)?;
            }
            let mut seen: HashSet<&Vec<Option<String>>> = HashSet::with_capacity(entries.len());
            for (values, _) in &entries {
                if !seen.insert(values) {
                    let shown = match values.as_slice() {
                        [one] => one.clone().unwrap_or_else(|| "NULL".to_string()),
                        many => {
                            let items: Vec<String> = many
                                .iter()
                                .map(|value| value.clone().unwrap_or_else(|| "NULL".to_string()))
                                .collect();
                            format!("[{}]", items.join(", "))
                        }
                    };
                    return Err(Error::binder(format!(
                        "The value \"{shown}\" was specified multiple times in the IN clause"
                    )));
                }
            }
            listed.push(entries);
        }
        let limit = self.semantics.pivot_limit();
        if total as u64 >= limit {
            return Err(Error::binder(format!(
                "Pivot column limit of {limit} exceeded. Use SET pivot_limit=X to increase \
                 the limit."
            )));
        }
        for entries in &listed {
            let mut next = Vec::with_capacity(spreads.len() * entries.len());
            for before in &spreads {
                for (values, alias) in entries {
                    let name = alias.clone().unwrap_or_else(|| {
                        let parts: Vec<String> = values
                            .iter()
                            .map(|value| value.clone().unwrap_or_else(|| "NULL".to_string()))
                            .collect();
                        parts.join("_")
                    });
                    let mut values_now = before.values.clone();
                    values_now.extend(values.iter().cloned());
                    let name = if before.name.is_empty() {
                        name
                    } else {
                        format!("{}_{name}", before.name)
                    };
                    next.push(Spread { values: values_now, name });
                }
            }
            spreads = next;
        }
        let spread_exprs: Vec<String> = columns
            .iter()
            .flat_map(|column| ast.expr_list(column.exprs).to_vec())
            .map(|expr| deparse::expression(ast, expr))
            .collect();
        let groups: Vec<String> = if pivot.groups.is_empty() {
            names
                .iter()
                .filter(|name| !handled.iter().any(|held: &String| same_name(held, name)))
                .cloned()
                .collect()
        } else {
            ast.name(pivot.groups).map(str::to_string).collect()
        };
        // Each aggregate is written once with a stand in for its filter, which is then replaced by
        // the filter of each column in turn.
        let mut copy = ast.clone();
        let start = copy.parts.len() as u32;
        copy.parts.push(copy.strings.len() as ast::StrRef);
        copy.strings.push("__pivot_filter".to_string());
        copy.exprs.push(ast::Expr::Column { name: ast::Slice { start, len: 1 } });
        copy.expr_spans.push(ast.expr_span(calls[0]));
        let stand_in = (copy.exprs.len() - 1) as ast::ExprRef;
        let marker = deparse::expression(&copy, stand_in);
        for &call in &calls {
            if let ast::Expr::Function { name, args, distinct, .. } = copy.expr(call) {
                copy.exprs[call as usize] =
                    ast::Expr::Function { name, args, distinct, filter: stand_in };
            }
        }
        let templates: Vec<String> =
            aggregates.iter().map(|target| deparse::expression(&copy, target.expr)).collect();
        let mut output: Vec<String> = groups.clone();
        let mut items: Vec<String> = groups.iter().map(|group| ident(group)).collect();
        for spread in &spreads {
            let tests: Vec<String> = spread_exprs
                .iter()
                .zip(&spread.values)
                .map(|(expr, value)| {
                    let value = value.as_deref().map_or_else(|| "NULL".to_string(), string);
                    format!("CAST(({expr}) AS VARCHAR) IS NOT DISTINCT FROM {value}")
                })
                .collect();
            let test = format!("({})", tests.join(" AND "));
            for (template, target) in templates.iter().zip(&aggregates) {
                let mut name = spread.name.clone();
                if aggregates.len() > 1 || target.alias != NONE {
                    let named = if target.alias == NONE {
                        deparse::expression(ast, target.expr)
                    } else {
                        ast.string(target.alias).to_string()
                    };
                    name = format!("{name}_{named}");
                }
                items.push(template.replace(&marker, &test));
                output.push(name);
            }
        }
        deduplicate(&mut output);
        for (item, name) in items.iter_mut().zip(&output).skip(groups.len()) {
            item.push_str(&format!(" AS {}", ident(name)));
        }
        let mut text = format!("SELECT {} FROM {}", items.join(", "), ident(held));
        if !groups.is_empty() {
            let ordinals: Vec<String> = (1..=groups.len()).map(|at| at.to_string()).collect();
            text.push_str(&format!(" GROUP BY {}", ordinals.join(", ")));
        }
        Ok(text)
    }

    /// The values an entry of a pivot's `IN` list stands for, as text: a bare name is the string it
    /// spells, a row is each of its parts, and anything else has to fold to a constant.
    fn pivot_in_values(
        &mut self,
        ast: &Ast,
        expr: ast::ExprRef,
        out: &mut Vec<Option<String>>,
    ) -> Result<()> {
        match ast.expr(expr) {
            ast::Expr::Column { name } => {
                out.push(Some(ast.name(name).last().unwrap_or_default().to_string()));
            }
            ast::Expr::Star { .. } | ast::Expr::Columns { .. } => {
                return Err(Error::binder("STAR expression is not supported here"));
            }
            ast::Expr::Row { items } => {
                for &item in ast.expr_list(items) {
                    self.pivot_in_values(ast, item, out)?;
                }
            }
            _ => {
                if reads_column(ast, expr) {
                    return Err(Error::binder("PIVOT IN list cannot contain column names"));
                }
                let bound = self.bind_expr(ast, expr, &Scope::empty())?;
                let text = self.cast_to(bound, &LogicalType::Varchar);
                match fold::value_of(self.plan(), text)? {
                    Some(Value::Varchar(text)) => out.push(Some(text)),
                    Some(Value::Null) => out.push(None),
                    _ => {
                        return Err(Error::binder(
                            "PIVOT IN list must contain constant expressions",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// The query an unpivot stands for, and the tests its rows are kept by, which are written
    /// above it so they see the columns as the alias names them.
    fn unpivot_query(
        &mut self,
        ast: &Ast,
        pivot: &ast::Pivot,
        names: &[String],
        held: &str,
        scope: &Scope,
    ) -> Result<(String, Vec<String>)> {
        let Some(column) = ast.pivot_column_list(pivot.columns).first().copied() else {
            return Err(Error::internal("an unpivot with no list"));
        };
        let mut entries = Vec::new();
        for target in ast.target_list(column.entries) {
            self.unpivot_entries(ast, *target, scope, &mut entries)?;
        }
        if entries.is_empty() {
            return Err(Error::binder(
                "UNPIVOT clause must unpivot on at least one column - zero were provided",
            ));
        }
        let mut handled: Vec<String> = Vec::new();
        for entry in &entries {
            for name in &entry.columns {
                if !handled.iter().any(|held| held == name) {
                    handled.push(name.clone());
                }
            }
        }
        let mut kept = Vec::new();
        for name in names {
            match handled.iter().position(|held| same_name(held, name)) {
                Some(at) => {
                    handled.remove(at);
                }
                None => kept.push(name.clone()),
            }
        }
        if let Some(missing) = handled.first() {
            return Err(Error::binder(format!(
                "Column \"\"{missing}\"\" referenced in UNPIVOT but no matching entry was found in \
                 the table"
            )));
        }
        // A name is spelled the way the source spells it, not the way the list does.
        let spelled =
            |name: &String| names.iter().find(|held| same_name(held, name)).unwrap_or(name).clone();
        let labels: Vec<String> = entries
            .iter()
            .map(|entry| {
                if entry.alias.is_empty() {
                    entry.columns.iter().map(spelled).collect::<Vec<_>>().join("_")
                } else {
                    entry.alias.clone()
                }
            })
            .collect();
        let width = entries[0].exprs.len();
        for entry in &entries[1..] {
            if entry.exprs.len() != width {
                return Err(Error::binder(format!(
                    "UNPIVOT value count mismatch - entry has {} values, but expected all entries \
                     to have {width} values",
                    entry.exprs.len()
                )));
            }
        }
        let mut internal = kept.clone();
        internal.push("unpivot_names".to_string());
        for at in 0..width {
            internal.push(if at == 0 {
                "unpivot_list".to_string()
            } else {
                format!("unpivot_list_{}", at + 1)
            });
        }
        deduplicate(&mut internal);
        let values: Vec<String> = ast.name(pivot.values).map(str::to_string).collect();
        if values.len() != width {
            return Err(Error::binder(format!(
                "UNPIVOT name count mismatch - got {} names but {width} expressions",
                values.len()
            )));
        }
        let mut inner: Vec<String> = kept.iter().map(|name| ident(name)).collect();
        let listed: Vec<String> = labels.iter().map(|label| string(label)).collect();
        inner.push(format!(
            "CAST([{}] AS VARCHAR[]) AS {}",
            listed.join(", "),
            ident(&internal[kept.len()])
        ));
        for at in 0..width {
            let exprs: Vec<&str> = entries.iter().map(|entry| entry.exprs[at].as_str()).collect();
            inner.push(format!(
                "unpivot_list({}) AS {}",
                exprs.join(", "),
                ident(&internal[kept.len() + 1 + at])
            ));
        }
        let aliases: Vec<String> = ast.name(pivot.columns_alias).map(str::to_string).collect();
        let named = ast.name(column.names).next().unwrap_or("name").to_string();
        let mut outer: Vec<String> =
            internal[..kept.len()].iter().map(|name| ident(name)).collect();
        outer.push(format!("unnest({}) AS {}", ident(&internal[kept.len()]), ident(&named)));
        let mut filter = Vec::new();
        for at in 0..width {
            let name = aliases.get(at).unwrap_or(&values[at]);
            outer.push(format!(
                "unnest({}) AS {}",
                ident(&internal[kept.len() + 1 + at]),
                ident(name)
            ));
            if !pivot.include_nulls {
                filter.push(format!("{} IS NOT NULL", ident(name)));
            }
        }
        let text = format!(
            "SELECT {} FROM (SELECT {} FROM {}) AS \"unpivot\"",
            outer.join(", "),
            inner.join(", "),
            ident(held)
        );
        Ok((text, filter))
    }

    /// The entries one item of an unpivot's list stands for. A list of names and constants is one
    /// entry reading those columns, a star is an entry for each column it stands for, and anything
    /// else is one entry reading the one column it is written over.
    fn unpivot_entries(
        &mut self,
        ast: &Ast,
        target: ast::Target,
        scope: &Scope,
        out: &mut Vec<Unspread>,
    ) -> Result<()> {
        let alias =
            if target.alias == NONE { String::new() } else { ast.string(target.alias).to_string() };
        let mut listed = Vec::new();
        if unpivot_list(ast, target.expr, &mut listed) {
            if listed.iter().any(String::is_empty) {
                return Err(Error::binder("UNPIVOT - empty column name not supported"));
            }
            let exprs = listed.iter().map(|name| ident(name)).collect();
            out.push(Unspread { exprs, columns: listed, alias });
            return Ok(());
        }
        let picked = match ast.expr(target.expr) {
            ast::Expr::Star { .. } => Some(self.star_columns(ast, target.expr, scope)?),
            ast::Expr::Columns { inner, .. }
                if matches!(ast.expr(inner), ast::Expr::Star { .. }) =>
            {
                Some(self.star_columns(ast, inner, scope)?)
            }
            ast::Expr::Columns { .. } => Some(self.columns_picks(ast, target.expr, scope)?.entries),
            _ => None,
        };
        if let Some(picked) = picked {
            for each in picked {
                let name = each.column.name.clone();
                out.push(Unspread {
                    exprs: vec![ident(&name)],
                    columns: vec![name],
                    alias: String::new(),
                });
            }
            return Ok(());
        }
        // An expression over COLUMNS is an entry for each column it picks, written over it.
        if let Some(found) = find_columns(ast, target.expr) {
            let picked = match ast.expr(found) {
                ast::Expr::Columns { inner, .. }
                    if matches!(ast.expr(inner), ast::Expr::Star { .. }) =>
                {
                    self.star_columns(ast, inner, scope)?
                }
                _ => self.columns_picks(ast, found, scope)?.entries,
            };
            let mut copy = ast.clone();
            let start = copy.parts.len() as u32;
            copy.parts.push(copy.strings.len() as ast::StrRef);
            copy.strings.push(String::new());
            for each in picked {
                let name = each.column.name.clone();
                copy.strings[copy.parts[start as usize] as usize] = name.clone();
                copy.exprs[found as usize] =
                    ast::Expr::Column { name: ast::Slice { start, len: 1 } };
                let exprs = vec![deparse::expression(&copy, target.expr)];
                out.push(Unspread { exprs, columns: vec![name], alias: String::new() });
            }
            return Ok(());
        }
        let written = deparse::expression(ast, target.expr);
        let mut columns = Vec::new();
        unpivot_column_names(ast, target.expr, &mut columns)?;
        match columns.len() {
            0 => Err(Error::binder(format!(
                "UNPIVOT clause must contain exactly one column - expression \"{written}\" does \
                 not contain any"
            ))),
            1 => {
                out.push(Unspread { exprs: vec![written], columns, alias });
                Ok(())
            }
            _ => Err(Error::binder(format!(
                "UNPIVOT clause must contain exactly one column - expression \"{written}\" \
                 contains multiple ({})",
                columns.join(", ")
            ))),
        }
    }
}

/// The aggregate calls in a pivot's aggregate expression, not looking inside one, and the pin's
/// refusal of a column read outside of one.
fn pivot_aggregates(ast: &Ast, expr: ast::ExprRef, found: &mut Vec<ast::ExprRef>) -> Result<()> {
    match ast.expr(expr) {
        ast::Expr::Function { name, .. } if is_aggregate(ast.name(name).last().unwrap_or("")) => {
            found.push(expr);
            Ok(())
        }
        ast::Expr::Column { .. } => Err(Error::binder(
            "Columns can only be referenced within the aggregate of a PIVOT expression",
        )),
        _ => {
            for child in ast.children(expr) {
                pivot_aggregates(ast, child, found)?;
            }
            Ok(())
        }
    }
}

/// Whether a call to `name` is to an aggregate function.
fn is_aggregate(name: &str) -> bool {
    rudb_functions::kind_of(name) == Some(rudb_functions::FunctionKind::Aggregate)
        || same_name(name, "every")
        || crate::macros::aggregates(name)
}

/// Every column an expression reads, added to `out`, and the pin's refusal of a qualified one.
fn column_names(ast: &Ast, expr: ast::ExprRef, out: &mut Vec<String>) -> Result<()> {
    if let ast::Expr::Column { name } = ast.expr(expr) {
        if name.len > 1 {
            return Err(Error::binder("PIVOT expression cannot contain qualified columns"));
        }
        out.push(ast.name(name).last().unwrap_or_default().to_string());
        return Ok(());
    }
    for child in ast.children(expr) {
        column_names(ast, child, out)?;
    }
    Ok(())
}

/// Whether a window function is written anywhere in an expression.
fn has_window(ast: &Ast, expr: ast::ExprRef) -> bool {
    matches!(ast.expr(expr), ast::Expr::Window { .. })
        || ast.children(expr).into_iter().any(|child| has_window(ast, child))
}

/// The first COLUMNS written inside an expression.
fn find_columns(ast: &Ast, expr: ast::ExprRef) -> Option<ast::ExprRef> {
    if matches!(ast.expr(expr), ast::Expr::Columns { .. }) {
        return Some(expr);
    }
    ast.children(expr).into_iter().find_map(|child| find_columns(ast, child))
}

/// Whether a column is read anywhere in an expression.
fn reads_column(ast: &Ast, expr: ast::ExprRef) -> bool {
    matches!(ast.expr(expr), ast::Expr::Column { .. })
        || ast.children(expr).into_iter().any(|child| reads_column(ast, child))
}

/// The column names an unpivot entry lists, when it is only bare names and constants, in a row or
/// on its own.
fn unpivot_list(ast: &Ast, expr: ast::ExprRef, out: &mut Vec<String>) -> bool {
    match ast.expr(expr) {
        ast::Expr::Column { name } if name.len == 1 => {
            out.push(ast.name(name).last().unwrap_or_default().to_string());
            true
        }
        ast::Expr::Literal { kind, text } => {
            out.push(match kind {
                LiteralKind::Null => "NULL".to_string(),
                LiteralKind::True => "true".to_string(),
                LiteralKind::False => "false".to_string(),
                _ => ast.string(text).to_string(),
            });
            true
        }
        ast::Expr::Row { items } => {
            ast.expr_list(items).iter().all(|&item| unpivot_list(ast, item, out))
        }
        _ => false,
    }
}

/// The columns an unpivot entry that is an expression reads, and the pin's refusal of a subquery.
fn unpivot_column_names(ast: &Ast, expr: ast::ExprRef, out: &mut Vec<String>) -> Result<()> {
    match ast.expr(expr) {
        ast::Expr::Column { name } => {
            out.push(ast.name(name).last().unwrap_or_default().to_string());
            Ok(())
        }
        ast::Expr::Subquery { .. }
        | ast::Expr::Exists { .. }
        | ast::Expr::InSubquery { .. }
        | ast::Expr::QuantifiedSubquery { .. } => {
            Err(Error::binder("UNPIVOT list cannot contain subqueries"))
        }
        _ => {
            for child in ast.children(expr) {
                unpivot_column_names(ast, child, out)?;
            }
            Ok(())
        }
    }
}

/// Makes the names unique the way the pin does, ignoring case: a repeated name gets `_1` on the
/// end, or the first number that makes it unique.
fn deduplicate(names: &mut [String]) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for name in names.iter_mut() {
        let low = name.to_lowercase();
        if let Entry::Vacant(entry) = seen.entry(low.clone()) {
            entry.insert(1);
            continue;
        }
        let mut count = seen[&low];
        let mut renamed = format!("{name}_{count}");
        while seen.contains_key(&renamed.to_lowercase()) {
            count += 1;
            renamed = format!("{name}_{count}");
        }
        seen.insert(low, count);
        *seen.entry(renamed.to_lowercase()).or_insert(0) += 1;
        *name = renamed;
    }
}

/// A name written so that it reads back as itself.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A string constant written so that it reads back as itself.
fn string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

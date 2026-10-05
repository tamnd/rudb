//! The name of an unaliased target in a PostgreSQL session, by the rules of `FigureColname` in
//! `parse_target.c`.
//!
//! PostgreSQL names a column after the column, the function or the type that the target is, and
//! calls it `?column?` when it is none of them. Drivers and tools show this name, and some map
//! result columns to fields by it, so it has to be the same name that PostgreSQL gives.

use rudb_parse::ast::{self, BinaryOp, QueryBody};
use rudb_parse::{Ast, NONE};

use crate::binder::Binder;
use crate::scope::Scope;

/// The name of a target that has no name of its own.
const NO_NAME: &str = "?column?";

impl Binder<'_> {
    /// The name of an unaliased target: the name of PostgreSQL in a PostgreSQL session, and the
    /// name of DuckDB in any other.
    pub(crate) fn target_name(&self, ast: &Ast, target: ast::ExprRef, input: &Scope) -> String {
        if self.session.postgres().is_none() {
            return self.output_name(ast, target, input);
        }
        self.figure(ast, target, input).map_or_else(|| NO_NAME.to_owned(), |(name, _)| name)
    }

    /// The name of an expression and how strong it is, as `FigureColnameInternal` gives them. A
    /// name of strength 2 comes from a column or a function, and a name of strength 1 comes from
    /// a type or from `CASE`. `None` is no name.
    fn figure(&self, ast: &Ast, expr: ast::ExprRef, input: &Scope) -> Option<(String, u8)> {
        if expr == NONE {
            return None;
        }
        match ast.expr(expr) {
            ast::Expr::Column { .. } | ast::Expr::Positional { .. } => {
                Some((self.output_name(ast, expr, input).trim_matches('"').to_owned(), 2))
            }
            ast::Expr::Function { name, .. } | ast::Expr::Window { name, .. } => {
                let last = ast.name(name).last().unwrap_or_default().to_ascii_lowercase();
                // `TRIM(x)` is a call of `btrim` in PostgreSQL.
                let last = if last == "trim" { "btrim".to_owned() } else { last };
                Some((last, 2))
            }
            ast::Expr::Cast { operand, ty, .. } => match self.figure(ast, operand, input) {
                Some((name, strength)) if strength > 1 => Some((name, strength)),
                _ => Some((type_name(ast.string(ty)), 1)),
            },
            ast::Expr::Binary { op: BinaryOp::Collate, left, .. } => self.figure(ast, left, input),
            ast::Expr::Case { otherwise, .. } => match self.figure(ast, otherwise, input) {
                Some((name, strength)) if strength > 1 => Some((name, strength)),
                _ => Some(("case".to_owned(), 1)),
            },
            ast::Expr::Exists { .. } => Some(("exists".to_owned(), 2)),
            ast::Expr::Subquery { array: true, .. } | ast::Expr::List { .. } => {
                Some(("array".to_owned(), 2))
            }
            ast::Expr::Subquery { query, array: false } => Some((first_name(ast, query), 2)),
            ast::Expr::Row { .. } => Some(("row".to_owned(), 2)),
            _ => None,
        }
    }
}

/// The name of the first column of a subquery, which PostgreSQL gives to the subquery as a whole.
/// The subquery is not bound here, so a column takes the last part of its name as written.
fn first_name(ast: &Ast, query: ast::QueryRef) -> String {
    match ast.query(query).body {
        QueryBody::Select(select) => {
            let Some(target) = ast.target_list(ast.select(select).targets).first() else {
                return NO_NAME.to_owned();
            };
            if target.alias != NONE {
                return ast.string(target.alias).to_owned();
            }
            written_name(ast, target.expr).unwrap_or_else(|| NO_NAME.to_owned())
        }
        QueryBody::SetOp { left, .. } => first_name(ast, left),
        QueryBody::Values(_) => "column1".to_owned(),
        _ => NO_NAME.to_owned(),
    }
}

/// The name of an expression from what was written alone, for a target of a subquery.
fn written_name(ast: &Ast, expr: ast::ExprRef) -> Option<String> {
    if expr == NONE {
        return None;
    }
    match ast.expr(expr) {
        ast::Expr::Column { name } => ast.name(name).last().map(str::to_owned),
        ast::Expr::Function { name, .. } | ast::Expr::Window { name, .. } => {
            ast.name(name).last().map(str::to_ascii_lowercase)
        }
        ast::Expr::Cast { operand, ty, .. } => {
            written_name(ast, operand).or_else(|| Some(type_name(ast.string(ty))))
        }
        ast::Expr::Exists { .. } => Some("exists".to_owned()),
        ast::Expr::Row { .. } => Some("row".to_owned()),
        _ => None,
    }
}

/// The name of a type as PostgreSQL gives it to a cast: the internal name for the types that the
/// grammar spells with keywords, such as `int4` for `integer`, and the last part of the name for
/// the others.
fn type_name(written: &str) -> String {
    let mut text = written.to_ascii_lowercase();
    // The modifiers and the array brackets are not part of the name.
    while let Some(open) = text.find(['(', '[']) {
        let close = text[open..].find([')', ']']).map_or(text.len(), |at| open + at + 1);
        text.replace_range(open..close, " ");
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let text = words.join(" ");
    let text = text.strip_prefix("pg_catalog.").unwrap_or(&text);
    let float = |precision: &str| match precision.parse::<u32>() {
        Ok(bits) if bits <= 24 => "float4",
        _ => "float8",
    };
    let name = match text {
        "int" | "integer" | "int4" => "int4",
        "smallint" | "int2" => "int2",
        "bigint" | "int8" => "int8",
        "real" | "float4" => "float4",
        "double precision" | "double" | "float8" => "float8",
        "float" => {
            let precision = written.split_once('(').and_then(|(_, rest)| rest.split_once(')'));
            float(precision.map_or("53", |(bits, _)| bits.trim()))
        }
        "decimal" | "dec" | "numeric" => "numeric",
        "boolean" | "bool" => "bool",
        "varchar" | "character varying" | "char varying" | "national character varying" => {
            "varchar"
        }
        "char" | "character" | "nchar" | "national character" | "national char" | "bpchar" => {
            "bpchar"
        }
        "time" | "time without time zone" => "time",
        "time with time zone" | "timetz" => "timetz",
        "timestamp" | "timestamp without time zone" => "timestamp",
        "timestamp with time zone" | "timestamptz" => "timestamptz",
        "bit varying" | "varbit" => "varbit",
        _ if text.starts_with("interval") => "interval",
        _ => {
            let last = written.rsplit('.').next().unwrap_or(written).trim();
            let last = last.split(['(', '[']).next().unwrap_or(last).trim();
            return match last.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) {
                Some(quoted) => quoted.to_owned(),
                None => last.to_ascii_lowercase(),
            };
        }
    };
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::type_name;

    #[test]
    fn the_names_of_types_in_casts() {
        let names = [
            ("INTEGER", "int4"),
            ("int", "int4"),
            ("BIGINT", "int8"),
            ("double precision", "float8"),
            ("float(10)", "float4"),
            ("float", "float8"),
            ("numeric(10, 2)", "numeric"),
            ("character varying(20)", "varchar"),
            ("char(3)", "bpchar"),
            ("timestamp with time zone", "timestamptz"),
            ("TIMESTAMP(3) WITHOUT TIME ZONE", "timestamp"),
            ("interval year to month", "interval"),
            ("int[]", "int4"),
            ("text", "text"),
            ("pg_catalog.text", "text"),
            ("public.\"MyType\"", "MyType"),
            ("regclass", "regclass"),
        ];
        for (written, name) in names {
            assert_eq!(type_name(written), name, "{written}");
        }
    }
}

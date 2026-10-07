//! The name of an unaliased target in a PostgreSQL session, by the rules of `FigureColname` in
//! `parse_target.c`.
//!
//! PostgreSQL names a column after the column, the function or the type that the target is, and
//! calls it `?column?` when it is none of them. Drivers and tools show this name, and some map
//! result columns to fields by it, so it has to be the same name that PostgreSQL gives.

use rudb_common::ColumnNames;
use rudb_parse::ast::{self, BinaryOp, QueryBody};
use rudb_parse::{Ast, NONE};

use crate::binder::Binder;
use crate::scope::Scope;

/// The name of a target that has no name of its own.
const NO_NAME: &str = "?column?";

/// The longest name that PostgreSQL keeps, in bytes, which is `NAMEDATALEN - 1`.
const NAME_BYTES: usize = 63;

impl Binder<'_> {
    /// The name of an unaliased target: the name of PostgreSQL in a PostgreSQL session, and the
    /// name of DuckDB in any other.
    pub(crate) fn target_name(&self, ast: &Ast, target: ast::ExprRef, input: &Scope) -> String {
        if self.semantics.column_names() == ColumnNames::Pin {
            return self.output_name(ast, target, input);
        }
        self.figure(ast, target, input).map_or_else(|| NO_NAME.to_owned(), |(name, _)| name)
    }

    /// The name of an element of an index that has no name, as `ChooseIndexColumnNames` gives it
    /// before it makes the names different: the name of the column or of the function, else
    /// `expr`.
    pub(crate) fn index_column_name(
        &self,
        ast: &Ast,
        element: ast::ExprRef,
        input: &Scope,
    ) -> String {
        self.figure(ast, element, input).map_or_else(|| "expr".to_owned(), |(name, _)| name)
    }

    /// The name of an expression and how strong it is, as `FigureColnameInternal` gives them. A
    /// name of strength 2 comes from a column or a function, and a name of strength 1 comes from
    /// a type or from `CASE`. `None` is no name.
    fn figure(&self, ast: &Ast, expr: ast::ExprRef, input: &Scope) -> Option<(String, u8)> {
        if expr == NONE {
            return None;
        }
        // `(x).a[1]` is named for the last field it selects, and a subscript alone for what it
        // subscripts.
        if let Some(field) = ast.indirection(expr) {
            return match ast.expr(expr) {
                _ if field != NONE => Some((ast.string(field).to_owned(), 2)),
                ast::Expr::Function { args, .. } => {
                    self.figure(ast, ast.expr_list(args).first().copied().unwrap_or(NONE), input)
                }
                _ => None,
            };
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

/// The name that PostgreSQL gives to an index with no name, by the rules of `ChooseIndexName` in
/// `indexcmds.c`. It is the table, the columns and `idx`, joined by `_` and cut to 63 bytes. When
/// `taken` says that a relation has the name, a number goes after `idx`.
pub(crate) fn index_name(table: &str, columns: &[String], taken: impl Fn(&str) -> bool) -> String {
    // A column whose name an earlier column has takes the first number that makes it new.
    let mut names: Vec<String> = Vec::with_capacity(columns.len());
    for column in columns {
        let mut name = column.clone();
        let mut number = 1;
        while names.contains(&name) {
            let suffix = number.to_string();
            name = format!("{}{suffix}", clip(column, NAME_BYTES - suffix.len()));
            number += 1;
        }
        names.push(name);
    }
    let mut addition = String::new();
    for name in &names {
        if !addition.is_empty() {
            addition.push('_');
        }
        addition.push_str(clip(name, NAME_BYTES));
        if addition.len() > NAME_BYTES {
            break;
        }
    }
    let mut label = "idx".to_owned();
    let mut pass = 0;
    loop {
        let name = object_name(table, &addition, &label);
        if !taken(&name) {
            return name;
        }
        pass += 1;
        label = format!("idx{pass}");
    }
}

/// `name1_name2_label`, with the longer of the two names cut first until the whole fits in 63
/// bytes, as `makeObjectName` does. The label is never cut.
fn object_name(first: &str, second: &str, label: &str) -> String {
    let room = NAME_BYTES - 2 - label.len();
    let (mut first_len, mut second_len) = (first.len(), second.len());
    while first_len + second_len > room {
        if first_len > second_len {
            first_len -= 1;
        } else {
            second_len -= 1;
        }
    }
    format!("{}_{}_{label}", clip(first, first_len), clip(second, second_len))
}

/// The longest start of `text` that has at most `bytes` bytes and ends at a whole character.
fn clip(text: &str, bytes: usize) -> &str {
    if text.len() <= bytes {
        return text;
    }
    let mut end = bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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
    use super::{index_name, type_name};

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

    fn named(table: &str, columns: &[&str], taken: &[&str]) -> String {
        let columns: Vec<String> = columns.iter().map(|&column| column.to_owned()).collect();
        index_name(table, &columns, |name| taken.contains(&name))
    }

    #[test]
    fn an_index_name_is_the_name_that_postgres_gives() {
        assert_eq!(named("t", &["a"], &[]), "t_a_idx");
        assert_eq!(named("t", &["a", "a", "a"], &[]), "t_a_a1_a2_idx");
        assert_eq!(named("t", &["a"], &["t_a_idx", "t_a_idx1"]), "t_a_idx2");
        // The longer name is cut first, and a cut never splits a character.
        let long = "c".repeat(70);
        assert_eq!(named("t", &[&long], &[]), format!("t_{}_idx", "c".repeat(57)));
        let wide = "é".repeat(40);
        let name = named(&wide, &["a"], &[]);
        assert!(name.len() <= 63, "{name}");
        assert_eq!(name, format!("{}_a_idx", "é".repeat(28)));
    }
}

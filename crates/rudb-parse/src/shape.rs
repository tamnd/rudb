//! An [`Ast`] written out as text with every field, so that a test can compare two trees.
//!
//! Two transforms build an [`Ast`]: the one in [`crate::transform`], from the parse tree of the
//! vendored DuckDB grammar, and the one in `rudb-pgparse`, from the raw parse tree of PostgreSQL.
//! For a statement that both dialects read the same way, the two trees must be the same. The
//! arenas of the two can hold the nodes in a different order, so `==` on two [`Ast`] values does
//! not tell. This module writes each node with its indexes followed, and two trees are the same
//! when their text is the same.
//!
//! This is not the printer of [`crate::deparse`], which writes SQL the way DuckDB writes it, and
//! it is not the printer of the tests of [`crate::transform`], which leaves fields out on purpose.
//! This one leaves nothing out of a query or an expression. The source ranges are not written,
//! because the two grammars do not end a node at the same byte.
//!
//! The statements other than a query, `EXPLAIN` and the written statements are written with their
//! `Debug` text, with the indexes in them not followed. The transform of `rudb-pgparse` does not
//! build them yet, and each one gets a writer when it does.

use std::fmt::Write as _;

use crate::ast::{
    Ast, ConflictAction, Distinct, Expr, ExprRef, Insert, LiteralKind, OrderItem, QueryBody,
    QueryRef, Slice, Source, SourceRef, Statement, StrRef, Target, WindowBound, WindowRef,
};
use crate::matcher::NONE;

/// Every statement of the script, one on each line.
#[must_use]
pub fn script(ast: &Ast) -> String {
    ast.statements.iter().map(|&statement| self::statement(ast, statement) + "\n").collect()
}

/// One statement.
#[must_use]
pub fn statement(ast: &Ast, statement: Statement) -> String {
    let shape = Shape { ast };
    match statement {
        Statement::Query(query) => shape.query(query),
        Statement::Explain { query, analyze, statistics, codegen } => {
            format!(
                "explain{{analyze: {analyze}, statistics: {statistics}, codegen: {codegen}, {}}}",
                shape.query(query)
            )
        }
        Statement::Insert(index) => shape.insert("insert", ast.insert(index)),
        Statement::Update(index) => shape.insert("update", ast.insert(index)),
        Statement::Delete(index) => shape.insert("delete", ast.insert(index)),
        other => format!("{other:?}"),
    }
}

/// One expression.
#[must_use]
pub fn expression(ast: &Ast, expr: ExprRef) -> String {
    Shape { ast }.expr(expr)
}

/// One query.
#[must_use]
pub fn query(ast: &Ast, query: QueryRef) -> String {
    Shape { ast }.query(query)
}

struct Shape<'a> {
    ast: &'a Ast,
}

impl Shape<'_> {
    /// A string in single quotes, or `-` for `NONE`.
    fn string(&self, index: StrRef) -> String {
        if index == NONE { "-".to_string() } else { format!("'{}'", self.ast.string(index)) }
    }

    /// A run of strings.
    fn names(&self, slice: Slice) -> String {
        let names: Vec<String> =
            self.ast.parts[slice.range()].iter().map(|&part| self.string(part)).collect();
        format!("[{}]", names.join(", "))
    }

    /// A run of expressions.
    fn exprs(&self, slice: Slice) -> String {
        let items: Vec<String> =
            self.ast.expr_list(slice).iter().map(|&item| self.expr(item)).collect();
        format!("[{}]", items.join(", "))
    }

    /// A run of targets.
    fn targets(&self, slice: Slice) -> String {
        self.target_list(self.ast.target_list(slice))
    }

    fn target_list(&self, targets: &[Target]) -> String {
        let items: Vec<String> = targets
            .iter()
            .map(|target| format!("{} as {}", self.expr(target.expr), self.string(target.alias)))
            .collect();
        format!("[{}]", items.join(", "))
    }

    /// A run of sort keys.
    fn order(&self, items: &[OrderItem]) -> String {
        let items: Vec<String> = items
            .iter()
            .map(|item| format!("{} {:?} {:?}", self.expr(item.expr), item.order, item.nulls))
            .collect();
        format!("[{}]", items.join(", "))
    }

    /// The rows of a `VALUES`.
    fn rows(&self, slice: Slice) -> String {
        let rows: Vec<String> = self.ast.rows(slice).iter().map(|&row| self.exprs(row)).collect();
        format!("[{}]", rows.join(", "))
    }

    fn window(&self, spec: WindowRef) -> String {
        let held = self.ast.window(spec);
        let bound = |bound: WindowBound| match bound {
            WindowBound::Preceding(offset) => format!("preceding {}", self.expr(offset)),
            WindowBound::Following(offset) => format!("following {}", self.expr(offset)),
            other => format!("{other:?}"),
        };
        format!(
            "window{{partition: {}, order: {}, {:?} {} {} {:?}}}",
            self.exprs(held.partition),
            self.order(self.ast.order_list(held.order)),
            held.unit,
            bound(held.start),
            bound(held.end),
            held.exclude
        )
    }

    /// The side tables of a call, which most calls do not have.
    fn call_extras(&self, call: ExprRef, out: &mut String) {
        let named = self.ast.named_args(call);
        if !named.is_empty() {
            let _ = write!(out, ", named: {}", self.target_list(named));
        }
        if let Some(&(_, positional, written)) =
            self.ast.named_written.iter().find(|(held, _, _)| *held == call)
        {
            let _ = write!(out, ", written: {positional} {}", self.targets(written));
        }
        if let Some((message, said)) = self.ast.misnamed(call) {
            let _ = write!(out, ", misnamed: {message:?} {said:?}");
        }
        let order = self.ast.aggregate_order(call);
        if !order.is_empty() {
            let _ = write!(out, ", order: {}", self.order(order));
        }
        if self.ast.exports_state(call) {
            out.push_str(", export_state");
        }
    }

    fn expr(&self, expr: ExprRef) -> String {
        if expr == NONE {
            return "-".to_string();
        }
        let ast = self.ast;
        match ast.expr(expr) {
            Expr::Star { qualifier, replacements } => {
                let lists = ast.star_lists(expr);
                let renames = self.targets(lists.renames);
                let excluded: Vec<String> =
                    ast.name_list(lists.exclude).iter().map(|&name| self.names(name)).collect();
                format!(
                    "star{{{}, replace: {}, exclude: [{}], rename: {renames}}}",
                    self.names(qualifier),
                    self.targets(replacements),
                    excluded.join(", ")
                )
            }
            Expr::Columns { inner, unpacked } => {
                format!("columns{{{}, unpacked: {unpacked}}}", self.expr(inner))
            }
            Expr::Column { name } => format!("column{}", self.names(name)),
            Expr::Positional { index } => format!("#{index}"),
            Expr::Literal { kind, text } => match kind {
                LiteralKind::Null | LiteralKind::True | LiteralKind::False => {
                    format!("{kind:?}").to_lowercase()
                }
                _ => format!("{}{{{}}}", format!("{kind:?}").to_lowercase(), self.string(text)),
            },
            Expr::Unary { op, operand } => format!("{op:?}({})", self.expr(operand)),
            Expr::Binary { op, left, right } => {
                let op = match op {
                    crate::ast::BinaryOp::Named(name) => self.string(name),
                    other => format!("{other:?}"),
                };
                format!("({} {op} {})", self.expr(left), self.expr(right))
            }
            Expr::Function { name, args, distinct, filter } => {
                let mut out = format!(
                    "call{{{}, {}, distinct: {distinct}, filter: {}",
                    self.names(name),
                    self.exprs(args),
                    self.expr(filter)
                );
                self.call_extras(expr, &mut out);
                out + "}"
            }
            Expr::Window { name, args, distinct, filter, ignore_nulls, order, spec } => {
                let mut out = format!(
                    "over{{{}, {}, distinct: {distinct}, filter: {}, ignore_nulls: {ignore_nulls}, order: {}, {}",
                    self.names(name),
                    self.exprs(args),
                    self.expr(filter),
                    self.order(ast.order_list(order)),
                    self.window(spec)
                );
                self.call_extras(expr, &mut out);
                out + "}"
            }
            Expr::Cast { operand, ty, try_cast } => {
                let word = if try_cast { "try_cast" } else { "cast" };
                format!("{word}{{{}, {}}}", self.expr(operand), self.string(ty))
            }
            Expr::Case { operand, arms, otherwise } => {
                let arms: Vec<String> = ast
                    .arm_list(arms)
                    .iter()
                    .map(|arm| format!("{} => {}", self.expr(arm.when), self.expr(arm.then)))
                    .collect();
                format!(
                    "case{{{}, [{}], else {}}}",
                    self.expr(operand),
                    arms.join(", "),
                    self.expr(otherwise)
                )
            }
            Expr::Between { operand, low, high, negated } => format!(
                "between{{{}, {}, {}, negated: {negated}}}",
                self.expr(operand),
                self.expr(low),
                self.expr(high)
            ),
            Expr::In { operand, list, negated } => {
                format!("in{{{}, {}, negated: {negated}}}", self.expr(operand), self.exprs(list))
            }
            Expr::InSubquery { operand, query, negated } => format!(
                "in_query{{{}, {}, negated: {negated}}}",
                self.expr(operand),
                self.query(query)
            ),
            Expr::QuantifiedSubquery { operand, op, query, all } => format!(
                "quantified_query{{{}, {op:?}, all: {all}, {}}}",
                self.expr(operand),
                self.query(query)
            ),
            Expr::QuantifiedArray { operand, op, array, all } => format!(
                "quantified_array{{{}, {op:?}, all: {all}, {}}}",
                self.expr(operand),
                self.expr(array)
            ),
            Expr::Default => "default".to_string(),
            Expr::Parameter { name } => format!("${}", ast.string(name)),
            Expr::List { items } => {
                let array = if ast.written_as_array(expr) { "array" } else { "list" };
                format!("{array}{}", self.exprs(items))
            }
            Expr::Lambda { params, body } => {
                format!("lambda{{{}, {}}}", self.names(params), self.expr(body))
            }
            Expr::Struct { names, values } => {
                format!("struct{{{}, {}}}", self.names(names), self.exprs(values))
            }
            Expr::Row { items } => format!("row{}", self.exprs(items)),
            Expr::Subquery { query, array } => {
                format!("subquery{{{}, array: {array}}}", self.query(query))
            }
            Expr::Exists { query, negated } => {
                format!("exists{{{}, negated: {negated}}}", self.query(query))
            }
        }
    }

    fn source(&self, source: SourceRef) -> String {
        let ast = self.ast;
        match ast.source(source) {
            Source::Table { name, alias, columns } => format!(
                "table{{{}, alias: {}, columns: {}}}",
                self.names(name),
                self.string(alias),
                self.names(columns)
            ),
            Source::Cte { cte, alias, columns, recurring } => format!(
                "cte{{{}, alias: {}, columns: {}, recurring: {recurring}}}",
                self.string(ast.cte(cte).name),
                self.string(alias),
                self.names(columns)
            ),
            Source::Subquery { query, alias, columns } => format!(
                "subquery{{{}, alias: {}, columns: {}}}",
                self.query(query),
                self.string(alias),
                self.names(columns)
            ),
            Source::Function { name, args, alias, columns, pragma } => format!(
                "function{{{}, {}, alias: {}, columns: {}, pragma: {pragma}}}",
                self.names(name),
                self.targets(args),
                self.string(alias),
                self.names(columns)
            ),
            Source::Values { rows, alias, columns } => format!(
                "values{{{}, alias: {}, columns: {}}}",
                self.rows(rows),
                self.string(alias),
                self.names(columns)
            ),
            Source::Join { left, right, kind, natural, on, using } => format!(
                "join{{{kind:?}, natural: {natural}, {}, {}, on: {}, using: {}}}",
                self.source(left),
                self.source(right),
                self.expr(on),
                self.names(using)
            ),
            Source::Pivot { pivot } => format!("{:?}", ast.pivot(pivot)),
        }
    }

    fn query(&self, index: QueryRef) -> String {
        if index == NONE {
            return "-".to_string();
        }
        let ast = self.ast;
        let query = ast.query(index);
        let mut out = String::from("query{");
        for &cte in ast.cte_list(query.ctes) {
            let held = ast.cte(cte);
            let _ = write!(
                out,
                "with {}{} recursive: {}, key: {}, dml: {} as {}, ",
                self.string(held.name),
                self.names(held.columns),
                held.recursive,
                self.targets(held.key),
                held.dml.map_or_else(|| "-".to_string(), |dml| statement(ast, dml)),
                self.query(held.query)
            );
        }
        out += &match query.body {
            QueryBody::Select(select) => {
                let select = ast.select(select);
                let distinct = match select.distinct {
                    Distinct::No => "no".to_string(),
                    Distinct::Yes => "yes".to_string(),
                    Distinct::On(on) => format!("on {}", self.exprs(on)),
                };
                let from: Vec<String> =
                    ast.source_list(select.from).iter().map(|&from| self.source(from)).collect();
                format!(
                    "select{{distinct: {distinct}, {}, from: [{}], where: {}, group: {}, group_all: {}, having: {}, qualify: {}}}",
                    self.targets(select.targets),
                    from.join(", "),
                    self.expr(select.filter),
                    self.exprs(select.group_by),
                    select.group_by_all,
                    self.expr(select.having),
                    self.expr(select.qualify)
                )
            }
            QueryBody::SetOp { op, quantifier, by_name, left, right } => format!(
                "{op:?}{{{quantifier:?}, by_name: {by_name}, {}, {}}}",
                self.query(left),
                self.query(right)
            ),
            QueryBody::Values(rows) => format!("values{}", self.rows(rows)),
            QueryBody::Describe(inner) => format!("describe{{{}}}", self.query(inner)),
            QueryBody::Show { name, relation } => {
                format!("show{{{}, {}}}", self.names(name), self.query(relation))
            }
        };
        let _ = write!(
            out,
            ", order: {}, order_all: {}, limit: {}, percent: {}, offset: {}}}",
            self.order(ast.order_list(query.order_by)),
            query.order_by_all,
            self.expr(query.limit),
            query.limit_percent,
            self.expr(query.offset)
        );
        out
    }

    fn insert(&self, word: &str, insert: Insert) -> String {
        let conflict = match insert.conflict {
            None => "-".to_string(),
            Some(conflict) => {
                let action = match conflict.action {
                    ConflictAction::Update { columns, query } => {
                        format!("update {} {}", self.names(columns), self.query(query))
                    }
                    other => format!("{other:?}"),
                };
                format!("{} {action}", self.names(conflict.target))
            }
        };
        format!(
            "{word}{{{}, columns: {}, {}, returning: {}, conflict: {conflict}, copy: {}, overriding: {:?}, truncate: {:?}}}",
            self.names(insert.name),
            self.names(insert.columns),
            self.query(insert.source),
            insert.returning.map_or_else(|| "-".to_string(), |query| self.query(query)),
            insert.copy,
            insert.overriding,
            insert.truncate
        )
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::session::IdentifierCase;

    use crate::transform::parse_ast_postgres;

    fn shape(sql: &str) -> String {
        super::script(&parse_ast_postgres(sql, IdentifierCase::Lower).unwrap())
    }

    #[test]
    fn a_select_is_written_with_every_field() {
        assert_eq!(
            shape("select a as x, count(*) filter (where b > 1) from t as u (c) where a = 1"),
            "query{select{distinct: no, [column['a'] as 'x', call{['count'], [star{[], replace: [], exclude: [], rename: []}], distinct: false, filter: (column['b'] Gt number{'1'})} as -], from: [table{['t'], alias: 'u', columns: ['c']}], where: (column['a'] Eq number{'1'}), group: [], group_all: false, having: -, qualify: -}, order: [], order_all: false, limit: -, percent: false, offset: -}\n"
        );
    }

    #[test]
    fn two_trees_of_one_text_are_written_the_same() {
        for sql in [
            "select 1 union all select 2 order by 1 limit 3",
            "with w as (select 1 as a) select a from w, w as v",
            "select sum(a order by b) over (partition by c rows between 1 preceding and current row) from t",
            "select array[1, 2], row(1, 'a'), cast(a as int4) from t",
        ] {
            assert_eq!(shape(sql), shape(sql), "{sql}");
        }
    }
}

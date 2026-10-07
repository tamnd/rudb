//! The calls that add nodes to the arenas of an [`Ast`].
//!
//! Two transforms build an [`Ast`]: the one in [`crate::transform`], from the parse tree of the
//! vendored DuckDB grammar, and the one in `rudb-pgparse`, from the raw parse tree of PostgreSQL.
//! Both add their nodes through these calls, so an index and a run mean the same thing whichever
//! of them made the tree.

use std::collections::HashMap;

use rudb_common::Span;

use crate::ast::{
    Ast, BinaryOp, CaseArm, ColumnDef, Expr, ExprRef, OrderItem, Query, QueryRef, Select,
    SelectRef, Slice, Source, SourceRef, StrRef, Target, WindowRef, WindowSpec,
};
use crate::matcher::NONE;

/// The strings already in an [`Ast`], so that one text is held once.
#[derive(Debug, Default)]
pub struct Interner {
    held: HashMap<String, StrRef>,
}

impl Interner {
    /// The index of `text` in `ast.strings`, added when it is not there yet.
    ///
    /// The index is `NONE` when the arena is full, which is four billion strings.
    pub fn intern(&mut self, ast: &mut Ast, text: &str) -> StrRef {
        if let Some(&index) = self.held.get(text) {
            return index;
        }
        let index = u32::try_from(ast.strings.len()).unwrap_or(NONE);
        ast.strings.push(text.to_string());
        self.held.insert(text.to_string(), index);
        index
    }
}

/// The run of `vector` from `start` to its end.
fn tail<T>(vector: &[T], start: usize) -> Slice {
    Slice { start: start as u32, len: (vector.len() - start) as u32 }
}

impl Ast {
    /// Add an expression that covers `span`.
    pub fn push_expr(&mut self, expr: Expr, span: Span) -> ExprRef {
        let index = self.exprs.len() as u32;
        self.exprs.push(expr);
        self.expr_spans.push(span);
        index
    }

    /// Add a from item.
    pub fn push_source(&mut self, source: Source) -> SourceRef {
        let index = self.sources.len() as u32;
        self.sources.push(source);
        index
    }

    /// Add a query that covers `span`.
    pub fn push_query(&mut self, query: Query, span: Span) -> QueryRef {
        let index = self.queries.len() as u32;
        self.queries.push(query);
        self.query_spans.push(span);
        index
    }

    /// Add a select block.
    pub fn push_select(&mut self, select: Select) -> SelectRef {
        let index = self.selects.len() as u32;
        self.selects.push(select);
        index
    }

    /// Add a window.
    pub fn push_window(&mut self, spec: WindowSpec) -> WindowRef {
        let index = self.windows.len() as u32;
        self.windows.push(spec);
        index
    }

    /// Add a run of sort keys.
    pub fn order_slice(&mut self, items: impl IntoIterator<Item = OrderItem>) -> Slice {
        let start = self.order_items.len();
        self.order_items.extend(items);
        tail(&self.order_items, start)
    }

    /// Add a run of expressions.
    pub fn expr_slice(&mut self, items: impl IntoIterator<Item = ExprRef>) -> Slice {
        let start = self.expr_lists.len();
        self.expr_lists.extend(items);
        tail(&self.expr_lists, start)
    }

    /// Add a run of `CASE` arms.
    pub fn case_slice(&mut self, items: impl IntoIterator<Item = CaseArm>) -> Slice {
        let start = self.case_arms.len();
        self.case_arms.extend(items);
        tail(&self.case_arms, start)
    }

    /// Add a run of from items.
    pub fn source_slice(&mut self, items: impl IntoIterator<Item = SourceRef>) -> Slice {
        let start = self.source_lists.len();
        self.source_lists.extend(items);
        tail(&self.source_lists, start)
    }

    /// Add a run of strings, which is how a qualified name is held.
    pub fn part_slice(&mut self, items: impl IntoIterator<Item = StrRef>) -> Slice {
        let start = self.parts.len();
        self.parts.extend(items);
        tail(&self.parts, start)
    }

    /// Add a run of materialised `WITH` indexes.
    pub fn cte_slice(&mut self, items: impl IntoIterator<Item = u32>) -> Slice {
        let start = self.cte_lists.len();
        self.cte_lists.extend(items);
        tail(&self.cte_lists, start)
    }

    /// Add a run of column definitions.
    pub fn column_def_slice(&mut self, items: impl IntoIterator<Item = ColumnDef>) -> Slice {
        let start = self.column_defs.len();
        self.column_defs.extend(items);
        tail(&self.column_defs, start)
    }

    /// Add a run of targets.
    pub fn target_slice(&mut self, items: impl IntoIterator<Item = Target>) -> Slice {
        let start = self.targets.len();
        self.targets.extend(items);
        tail(&self.targets, start)
    }

    /// Add a run of qualified names.
    pub fn name_list_slice(&mut self, items: impl IntoIterator<Item = Slice>) -> Slice {
        let start = self.name_lists.len();
        self.name_lists.extend(items);
        tail(&self.name_lists, start)
    }

    /// Add the rows of a `VALUES`, each a run of expressions.
    pub fn row_slice(&mut self, items: impl IntoIterator<Item = Slice>) -> Slice {
        let start = self.rows.len();
        self.rows.extend(items);
        tail(&self.rows, start)
    }
}

/// The infix operator that a symbol names in both grammars.
///
/// The symbols that only one grammar has are not here. DuckDB reads `==`, `//` and `**`, and
/// PostgreSQL has no such operators, so its transform must not find them. A symbol that is not in
/// the list is an operator that the binder looks up by its name, which is [`BinaryOp::Named`].
pub fn symbol_op(symbol: &str) -> Option<BinaryOp> {
    Some(match symbol {
        "=" => BinaryOp::Eq,
        "<>" | "!=" => BinaryOp::NotEq,
        "<" => BinaryOp::Lt,
        ">" => BinaryOp::Gt,
        "<=" => BinaryOp::LtEq,
        ">=" => BinaryOp::GtEq,
        "+" => BinaryOp::Add,
        "-" => BinaryOp::Subtract,
        "*" => BinaryOp::Multiply,
        "/" => BinaryOp::Divide,
        "%" => BinaryOp::Modulo,
        "^" => BinaryOp::Caret,
        "&" => BinaryOp::BitAnd,
        "|" => BinaryOp::BitOr,
        "<<" => BinaryOp::ShiftLeft,
        ">>" => BinaryOp::ShiftRight,
        "||" => BinaryOp::Concat,
        "->" => BinaryOp::Arrow,
        "->>" => BinaryOp::LongArrow,
        "@>" => BinaryOp::Contains,
        "<@" => BinaryOp::ContainedBy,
        "&&" => BinaryOp::Overlaps,
        "^@" => BinaryOp::StartsWith,
        "<<=" => BinaryOp::InetContainedByOrEq,
        ">>=" => BinaryOp::InetContainsOrEq,
        "~~" => BinaryOp::Like,
        "!~~" => BinaryOp::NotLike,
        "~~*" => BinaryOp::ILike,
        "!~~*" => BinaryOp::NotILike,
        "~" => BinaryOp::Regex,
        "!~" => BinaryOp::NotRegex,
        "~*" => BinaryOp::RegexInsensitive,
        "!~*" => BinaryOp::NotRegexInsensitive,
        _ => return None,
    })
}

/// The one spelling a date part keyword is written back as, which is not always the singular.
///
/// Both spellings of each of the thirteen keywords land on one name, and the name is upper case and
/// is plural for the two smallest parts and singular for the rest. That is not a rule, it is a list,
/// and it was read off the pinned binary a keyword at a time: `EXTRACT(milliseconds FROM t)` and
/// `EXTRACT(millisecond FROM t)` are both `date_part('MILLISECONDS', t)` while `EXTRACT(seconds FROM
/// t)` is `date_part('SECOND', t)`.
///
/// A word that is not in the list keeps the case it was written in. `EXTRACT(epoch FROM t)` stays
/// lower case, measured.
pub fn date_part(written: &str) -> String {
    const PARTS: &[(&str, &str)] = &[
        ("YEAR", "YEAR"),
        ("YEARS", "YEAR"),
        ("MONTH", "MONTH"),
        ("MONTHS", "MONTH"),
        ("DAY", "DAY"),
        ("DAYS", "DAY"),
        ("HOUR", "HOUR"),
        ("HOURS", "HOUR"),
        ("MINUTE", "MINUTE"),
        ("MINUTES", "MINUTE"),
        ("SECOND", "SECOND"),
        ("SECONDS", "SECOND"),
        ("MILLISECOND", "MILLISECONDS"),
        ("MILLISECONDS", "MILLISECONDS"),
        ("MICROSECOND", "MICROSECONDS"),
        ("MICROSECONDS", "MICROSECONDS"),
        ("WEEK", "WEEK"),
        ("WEEKS", "WEEK"),
        ("QUARTER", "QUARTER"),
        ("QUARTERS", "QUARTER"),
        ("DECADE", "DECADE"),
        ("DECADES", "DECADE"),
        ("CENTURY", "CENTURY"),
        ("CENTURIES", "CENTURY"),
        ("MILLENNIUM", "MILLENNIUM"),
        ("MILLENNIA", "MILLENNIUM"),
    ];
    PARTS
        .iter()
        .find(|(spelling, _)| spelling.eq_ignore_ascii_case(written))
        .map_or_else(|| written.to_string(), |(_, name)| (*name).to_string())
}

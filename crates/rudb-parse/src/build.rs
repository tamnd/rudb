//! The calls that add nodes to the arenas of an [`Ast`].
//!
//! Two transforms build an [`Ast`]: the one in [`crate::transform`], from the parse tree of the
//! vendored DuckDB grammar, and the one in `rudb-pgparse`, from the raw parse tree of PostgreSQL.
//! Both add their nodes through these calls, so an index and a run mean the same thing whichever
//! of them made the tree.

use std::collections::HashMap;

use rudb_common::Span;

use crate::ast::{
    Ast, BinaryOp, CaseArm, ColumnDef, ConflictAction, Expr, ExprRef, Insert, JoinKind,
    LiteralKind, OrderItem, Overriding, Query, QueryBody, QueryRef, Select, SelectRef, Slice,
    Source, SourceRef, Statement, StrRef, Target, Truncate, WindowRef, WindowSpec,
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

    /// Add a from item that covers `span`.
    pub fn push_source(&mut self, source: Source, span: Span) -> SourceRef {
        let index = self.sources.len() as u32;
        self.sources.push(source);
        self.source_spans.push(span);
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

/// An `UPDATE`, a `DELETE` or one table of a `TRUNCATE`, as the two transforms read it.
#[derive(Debug)]
pub struct Change {
    /// The name of the table, as a run of parts.
    pub name: Slice,
    /// The alias of the table, or `NONE`.
    pub alias: StrRef,
    /// Each column that `SET` names, with its new value. Empty for a `DELETE`.
    pub sets: Vec<(StrRef, ExprRef)>,
    /// The condition of the `WHERE`, or `NONE`.
    pub filter: ExprRef,
    /// The from items of `UPDATE ... FROM` or `DELETE ... USING`, or `None`.
    pub using: Option<Slice>,
    /// The `RETURNING` query from [`Ast::returning`], or `None`.
    pub returning: Option<QueryRef>,
    /// Whether the statement is a `DELETE`.
    pub delete: bool,
    /// The options of the `TRUNCATE` that this `DELETE` stands for, or `None`.
    pub truncate: Option<Truncate>,
}

// The writing statements. An `UPDATE`, a `DELETE` and the `DO UPDATE` of an `INSERT` are held as
// queries over the table that they write, as [`Statement::Update`] tells. These calls make those
// queries, so that both transforms give the binder the same form.
impl Ast {
    /// Add an `INSERT`, an `UPDATE` or a `DELETE`, and give its index in `Ast::inserts`.
    pub fn push_insert(&mut self, insert: Insert) -> u32 {
        let index = self.inserts.len() as u32;
        self.inserts.push(insert);
        index
    }

    /// The `FROM` of the one table that a writing statement names.
    pub fn written_table(&mut self, name: Slice, alias: StrRef, span: Span) -> Slice {
        let source =
            self.push_source(Source::Table { name, alias, columns: Slice::default() }, span);
        self.source_slice([source])
    }

    /// The query of a `RETURNING` list, which is `SELECT list FROM table [AS alias]`.
    pub fn returning(
        &mut self,
        name: Slice,
        alias: StrRef,
        targets: Slice,
        span: Span,
    ) -> QueryRef {
        let from = self.written_table(name, alias, span);
        let select = self.push_select(Select { targets, from, ..Select::empty() });
        self.push_query(Query::bare(QueryBody::Select(select)), span)
    }

    /// The action of `ON CONFLICT DO UPDATE SET ... WHERE condition`, held as `SELECT values...,
    /// condition FROM table AS alias POSITIONAL JOIN table AS excluded`. The condition is `NONE`
    /// when the statement has no `WHERE`, and is then true.
    pub fn conflict_update(
        &mut self,
        interned: &mut Interner,
        name: Slice,
        alias: StrRef,
        sets: Vec<(StrRef, ExprRef)>,
        condition: ExprRef,
        span: Span,
    ) -> ConflictAction {
        let condition = if condition == NONE { self.true_literal(span) } else { condition };
        let mut targets = Vec::with_capacity(sets.len() + 1);
        let mut columns = Vec::with_capacity(sets.len());
        for (column, value) in sets {
            columns.push(column);
            targets.push(Target { expr: value, alias: NONE });
        }
        targets.push(Target { expr: condition, alias: NONE });
        let targets = self.target_slice(targets);
        let left = self.push_source(Source::Table { name, alias, columns: Slice::default() }, span);
        let excluded = interned.intern(self, "excluded");
        let right = self
            .push_source(Source::Table { name, alias: excluded, columns: Slice::default() }, span);
        let joined = self.push_source(
            Source::Join {
                left,
                right,
                kind: JoinKind::Positional,
                natural: false,
                on: NONE,
                using: Slice::default(),
            },
            span,
        );
        let from = self.source_slice([joined]);
        let select = self.push_select(Select { targets, from, ..Select::empty() });
        let query = self.push_query(Query::bare(QueryBody::Select(select)), span);
        let columns = self.part_slice(columns);
        ConflictAction::Update { columns, query }
    }

    /// An `UPDATE` or a `DELETE`, held with the source `SELECT *, condition, values... FROM table`.
    /// With no `WHERE` the condition is true, because every row is the one meant.
    ///
    /// `UPDATE ... FROM` and `DELETE ... USING` read the condition and the values from a lateral
    /// join instead, `SELECT t.*, m.hit, m.values... FROM table AS t LEFT JOIN (SELECT true AS hit,
    /// values... FROM sources WHERE condition LIMIT 1) AS m ON true`. The `LIMIT 1` makes a table
    /// row that several source rows match change once, to the values of one of them, which is what
    /// DuckDB and PostgreSQL both do. A row that nothing matches has a null for the flag and is left
    /// alone.
    pub fn changed_rows(
        &mut self,
        interned: &mut Interner,
        change: Change,
        span: Span,
    ) -> Statement {
        let Change { name, alias, sets, filter, using, returning, delete, truncate } = change;
        let columns: Vec<StrRef> = sets.iter().map(|&(column, _)| column).collect();
        let source = match using {
            None => {
                let hit = if filter == NONE { self.true_literal(span) } else { filter };
                let star = self.push_expr(
                    Expr::Star { qualifier: Slice::default(), replacements: Slice::default() },
                    span,
                );
                let mut targets =
                    vec![Target { expr: star, alias: NONE }, Target { expr: hit, alias: NONE }];
                targets.extend(sets.iter().map(|&(_, expr)| Target { expr, alias: NONE }));
                let targets = self.target_slice(targets);
                let from = self.written_table(name, alias, span);
                let select = self.push_select(Select { targets, from, ..Select::empty() });
                self.push_query(Query::bare(QueryBody::Select(select)), span)
            }
            Some(from) => self.changed_rows_using(interned, name, alias, &sets, filter, from, span),
        };
        let columns = self.part_slice(columns);
        let index = self.push_insert(Insert {
            name,
            columns,
            source,
            returning,
            conflict: None,
            copy: false,
            overriding: Overriding::None,
            truncate,
        });
        if delete { Statement::Delete(index) } else { Statement::Update(index) }
    }

    /// The lateral source of [`Ast::changed_rows`], for a statement with a `FROM` or a `USING`.
    #[allow(clippy::too_many_arguments)]
    fn changed_rows_using(
        &mut self,
        interned: &mut Interner,
        name: Slice,
        alias: StrRef,
        sets: &[(StrRef, ExprRef)],
        filter: ExprRef,
        from: Slice,
        span: Span,
    ) -> QueryRef {
        let hit = interned.intern(self, "__rudb_hit");
        let matched = interned.intern(self, "__rudb_matched");
        let alias =
            if alias == NONE { self.parts[(name.start + name.len - 1) as usize] } else { alias };
        let yes = self.true_literal(span);
        let mut inner = vec![Target { expr: yes, alias: hit }];
        let mut outer_names = vec![hit];
        for (at, &(_, value)) in sets.iter().enumerate() {
            let named = interned.intern(self, &format!("__rudb_value_{at}"));
            inner.push(Target { expr: value, alias: named });
            outer_names.push(named);
        }
        let inner = self.target_slice(inner);
        let select = self.push_select(Select { targets: inner, from, filter, ..Select::empty() });
        let one = interned.intern(self, "1");
        let limit = self.push_expr(Expr::Literal { kind: LiteralKind::Number, text: one }, span);
        let query =
            self.push_query(Query { limit, ..Query::bare(QueryBody::Select(select)) }, span);
        let right = self.push_source(
            Source::Subquery { query, alias: matched, columns: Slice::default() },
            span,
        );
        let left = self.push_source(Source::Table { name, alias, columns: Slice::default() }, span);
        let on = self.true_literal(span);
        let join = self.push_source(
            Source::Join {
                left,
                right,
                kind: JoinKind::Left,
                natural: false,
                on,
                using: Slice::default(),
            },
            span,
        );
        let from = self.source_slice([join]);
        let qualifier = self.part_slice([alias]);
        let star = self.push_expr(Expr::Star { qualifier, replacements: Slice::default() }, span);
        let mut targets = vec![Target { expr: star, alias: NONE }];
        for named in outer_names {
            let name = self.part_slice([matched, named]);
            let column = self.push_expr(Expr::Column { name }, span);
            targets.push(Target { expr: column, alias: NONE });
        }
        let targets = self.target_slice(targets);
        let select = self.push_select(Select { targets, from, ..Select::empty() });
        self.push_query(Query::bare(QueryBody::Select(select)), span)
    }

    /// Puts the held `WITH` definitions `once` of a writing statement ahead of the ones that each
    /// of its queries carries already: the source, the `RETURNING` and the values of an `ON
    /// CONFLICT DO UPDATE`. Each of these queries is bound on its own, so each carries them.
    pub fn carry_definitions(&mut self, statement: Statement, once: &[u32]) {
        let (Statement::Insert(index) | Statement::Update(index) | Statement::Delete(index)) =
            statement
        else {
            return;
        };
        if once.is_empty() {
            return;
        }
        let insert = self.inserts[index as usize];
        let update = match insert.conflict.map(|conflict| conflict.action) {
            Some(ConflictAction::Update { query, .. }) => Some(query),
            _ => None,
        };
        for query in std::iter::once(insert.source).chain(insert.returning).chain(update) {
            if query == NONE {
                continue;
            }
            // Outermost first, so the statement's own come ahead of any the query wrote.
            let own = self.queries[query as usize].ctes;
            let mut all = once.to_vec();
            all.extend_from_slice(self.cte_list(own));
            let slice = self.cte_slice(all);
            self.queries[query as usize].ctes = slice;
        }
    }

    fn true_literal(&mut self, span: Span) -> ExprRef {
        self.push_expr(Expr::Literal { kind: LiteralKind::True, text: NONE }, span)
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

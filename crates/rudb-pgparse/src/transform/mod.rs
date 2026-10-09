//! The transform from the raw parse tree of PostgreSQL to the [`Ast`] of `rudb-parse`.
//!
//! The binder reads one [`Ast`] for both dialects. The DuckDB transform in `rudb-parse` makes it
//! from the parse tree of the DuckDB grammar, and this transform makes it from the tree that
//! [`crate::parse`] gives for the PostgreSQL grammar. Both add their nodes through
//! `rudb_parse::build`, so a rule that the two dialects share is written once, there. A statement
//! that the two dialects read the same way gives the same tree from both transforms, and the test
//! `tests/transform.rs` compares the two with `rudb_parse::shape`.
//!
//! The errors that PostgreSQL gives while it reads the raw tree, before it looks at the catalog,
//! are given here with the same message, SQLSTATE and position. Examples are a `WITH` name that is
//! written twice and a window that copies a window with a frame. A node that this transform does
//! not build yet is [`Refused::NotYet`], with the name of the node, and the caller uses the DuckDB
//! transform for that statement until it does.

mod expr;
mod index;
mod query;
mod types;
mod write;

use std::cell::OnceCell;
use std::ops::Range;

use rudb_common::notice::{Level, Notice};
use rudb_common::session::IdentifierCase;
use rudb_common::{Error, Span};
use rudb_parse::Ast;
use rudb_parse::ast::{
    Expr, ExprRef, OptionArg, QueryRef, Slice, SourceRef, Statement, StrRef, UtilityOption, Vacuum,
    VacuumTarget, WindowRef,
};
use rudb_parse::build::Interner;

use crate::nodes::{CTEMaterialize, ExplainStmt, List, Node, RawStmt, VacuumStmt};
use query::{Place, SelfRead};

/// Why [`transform`] did not give a tree.
#[derive(Debug)]
pub enum Refused {
    /// The text is not valid for the PostgreSQL grammar, with the error that PostgreSQL gives.
    Syntax(crate::Error),
    /// The text uses a node that the transform does not build yet, named as PostgreSQL names it.
    NotYet(String),
    /// The tree is valid for the grammar but PostgreSQL refuses it when it reads the tree, with the
    /// error that PostgreSQL gives.
    Error(Error),
}

impl From<Error> for Refused {
    fn from(error: Error) -> Self {
        Refused::Error(error)
    }
}

/// The [`Ast`] of a script of PostgreSQL statements.
///
/// # Errors
///
/// [`Refused`] when the text does not parse, when PostgreSQL refuses the tree, or when the tree has
/// a node that the transform does not build yet.
pub fn transform(text: &str) -> Result<Ast, Refused> {
    let (list, _) = crate::parse(text).map_err(Refused::Syntax)?;
    transform_list(text, &list)
}

/// The [`Ast`] of the statements `list` that the grammar read from `text`.
fn transform_list(text: &str, list: &List) -> Result<Ast, Refused> {
    let mut transform = Transform::new(text);
    for node in list.iter().flatten() {
        let Node::RawStmt(raw) = node else {
            return Err(not_yet(node));
        };
        transform.statement(raw)?;
    }
    Ok(transform.ast)
}

/// Where each statement of a script is in `text`, from the raw parse of the whole script, the way
/// `exec_simple_query` reads a query. `pg_parse_query` parses all of the text before one statement
/// runs, so a syntax error in any statement fails the query, also in a failed transaction block.
/// A statement is the text from its `stmt_location` for its `stmt_len`, where a length of 0 is the
/// rest of the text. An empty statement between two semicolons has no `RawStmt`.
///
/// # Errors
///
/// The error of the grammar, with the SQLSTATE and the position that PostgreSQL gives.
pub fn statements(text: &str) -> Result<Vec<Range<usize>>, Error> {
    let (list, _) = crate::parse(text).map_err(Error::from)?;
    let mut found = Vec::new();
    for node in list.iter().flatten() {
        let Node::RawStmt(raw) = node else { continue };
        let start = usize::try_from(raw.stmt_location).unwrap_or(0).min(text.len());
        let end = match usize::try_from(raw.stmt_len) {
            Ok(0) | Err(_) => text.len(),
            Ok(len) => (start + len).min(text.len()),
        };
        found.push(start..start + text[start..end].trim_end().len());
    }
    Ok(found)
}

/// The [`Ast`] of a script that a PostgreSQL session sent. This is the parse entry of such a session.
///
/// The PostgreSQL grammar reads the text, so a syntax error is the error that PostgreSQL gives. A
/// script with a statement that [`transform`] does not build yet goes through the DuckDB grammar
/// of `rudb-parse`, with its unquoted names folded to lower case as PostgreSQL folds them, until
/// the transform builds that statement.
///
/// # Errors
///
/// The error of the grammar or of the transform, with the SQLSTATE and the position that
/// PostgreSQL gives.
pub fn parse_ast(text: &str) -> Result<(Ast, Vec<Notice>), Error> {
    let (list, notices) = crate::parse(text).map_err(Error::from)?;
    let notices = notices.into_iter().map(noted).collect();
    let ast = match transform_list(text, &list) {
        Ok(ast) => ast,
        Err(Refused::NotYet(_)) => rudb_parse::parse_ast_postgres(text, IdentifierCase::Lower)?,
        Err(Refused::Syntax(error)) => return Err(error.into()),
        Err(Refused::Error(error)) => return Err(error),
    };
    Ok((ast, notices))
}

/// A notice of the lexer or the grammar, as the engine raises it.
fn noted(notice: crate::Notice) -> Notice {
    let level = match notice.severity {
        crate::Severity::Notice => Level::Notice,
        crate::Severity::Warning => Level::Warning,
    };
    let made = Notice::new(level, notice.code, notice.message);
    match notice.location.and_then(|location| u32::try_from(location).ok()) {
        Some(location) => made.at(location),
        None => made,
    }
}

/// The result of one step of the transform.
type Made<T> = Result<T, Refused>;

/// [`Refused::NotYet`] for a node, named by its variant.
fn not_yet(node: &Node) -> Refused {
    let text = format!("{node:?}");
    let name = text.split(['(', ' ']).next().unwrap_or_default();
    Refused::NotYet(name.to_string())
}

/// [`Refused::NotYet`] for a clause that is not a node of its own, for example `FOR UPDATE`.
fn clause<T>(name: &str) -> Made<T> {
    Err(Refused::NotYet(name.to_string()))
}

/// A `WITH` definition that the query being transformed can read.
struct Definition {
    /// The name, as PostgreSQL compares it, which is byte for byte.
    name: String,
    /// The column names written after the name, as a run of strings.
    declared: Slice,
    /// What `MATERIALIZED` or `NOT MATERIALIZED` asked for.
    materialized: CTEMaterialize,
    /// The query of the definition, or `NONE` while a recursive definition reads its own query.
    query: QueryRef,
    /// The slot in `Ast::ctes` of a recursive definition, which has one before its query is read,
    /// or `NONE`.
    slot: u32,
    /// The definition reads itself, which only a definition under `WITH RECURSIVE` can do.
    recursive: bool,
    /// Each read of the definition: the source that was put in its place, and the alias and the
    /// column names written on the read.
    reads: Vec<(SourceRef, StrRef, Slice)>,
}

struct Transform<'a> {
    text: &'a str,
    ast: Ast,
    interned: Interner,
    /// Each token of the text as the lexer gives it, with its start, its end and its kind. A node
    /// has the start of one token as its location, and this is how its span is found. The text is
    /// lexed the first time a span is needed, so a statement that the transform refuses at once
    /// does not pay for it.
    tokens: OnceCell<Vec<(u32, u32, u16)>>,
    /// The span of the statement being transformed, which a node with no location gets.
    span: Span,
    /// The windows of the `WINDOW` clause of the select being transformed, each with its name,
    /// its spec and whether it has a frame clause.
    windows: Vec<(String, WindowRef, bool)>,
    /// The `WITH` definitions in scope, innermost last.
    scope: Vec<Definition>,
    /// How many queries deep the query being transformed is, counting itself.
    depth: usize,
    /// The slots of the recursive definitions whose own query is being transformed, each with the
    /// depth of that query and the number of places around the definition, which are not places
    /// inside its query.
    recursing: Vec<(u32, usize, usize)>,
    /// Each read of a recursive definition from inside its own query.
    self_reads: Vec<SelfRead>,
    /// The places around the node being transformed that PostgreSQL does not let a recursive
    /// definition read itself from, outermost first.
    places: Vec<Place>,
}

impl<'a> Transform<'a> {
    fn new(text: &'a str) -> Self {
        Transform {
            text,
            ast: Ast { source: text.into(), ..Ast::default() },
            interned: Interner::default(),
            tokens: OnceCell::new(),
            span: Span::new(0, 0),
            windows: Vec::new(),
            scope: Vec::new(),
            depth: 0,
            recursing: Vec::new(),
            self_reads: Vec::new(),
            places: Vec::new(),
        }
    }

    /// The tokens of the text. The grammar has read the text already, so the lexer gives no error
    /// here.
    fn tokens(&self) -> &[(u32, u32, u16)] {
        self.tokens.get_or_init(|| {
            let mut lexer = crate::Lexer::new(self.text);
            let mut tokens = Vec::new();
            while let Ok(token) = lexer.next_token()
                && token.kind != 0
            {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a statement is less than 4 GiB"
                )]
                tokens.push((token.start as u32, token.end as u32, token.kind));
            }
            tokens
        })
    }

    /// One statement, added to the statements of the tree. A `TRUNCATE` of several tables adds one
    /// statement for each table.
    fn statement(&mut self, raw: &RawStmt) -> Made<()> {
        let start = u32::try_from(raw.stmt_location).unwrap_or(0);
        let end = match u32::try_from(raw.stmt_len) {
            Ok(0) | Err(_) => self.text.len() as u32,
            Ok(len) => start + len,
        };
        self.span = Span::new(start, end);
        let statement = match &raw.stmt {
            Some(Node::SelectStmt(select)) => Statement::Query(self.query(select)?),
            Some(Node::InsertStmt(insert)) => self.insert(insert)?,
            Some(Node::UpdateStmt(update)) => self.update(update)?,
            Some(Node::DeleteStmt(delete)) => self.delete(delete)?,
            Some(Node::IndexStmt(index)) => self.create_index(index)?,
            Some(Node::ExplainStmt(explain)) => self.explain(explain)?,
            Some(Node::VacuumStmt(vacuum)) => self.vacuum(vacuum)?,
            Some(Node::TruncateStmt(truncate)) => {
                let statements = self.truncate(truncate)?;
                self.ast.statements.extend(statements);
                return Ok(());
            }
            Some(node) => return Err(not_yet(node)),
            None => return clause("RawStmt"),
        };
        self.ast.statements.push(statement);
        Ok(())
    }

    /// `EXPLAIN` of a query, with its option list as it was written.
    ///
    /// The older spellings `EXPLAIN ANALYZE VERBOSE` and `EXPLAIN VERBOSE` come from the grammar as
    /// the options `analyze` and `verbose`, so every spelling arrives as one list. PostgreSQL reads
    /// the options after it binds the query, in `ExplainQuery`, so they are only kept here. Only a
    /// query is explained so far, and the other statements stay with the DuckDB transform.
    fn explain(&mut self, explain: &ExplainStmt) -> Made<Statement> {
        let query = match &explain.query {
            Some(Node::SelectStmt(select)) => self.query(select)?,
            Some(node) => return Err(not_yet(node)),
            None => return clause("ExplainStmt"),
        };
        let options = self.utility_options(&explain.options)?;
        Ok(Statement::Explain { query, analyze: false, statistics: false, codegen: false, options })
    }

    /// The option list of a utility statement, as a run of [`Ast::utility_options`]. The grammar
    /// writes each option as a `DefElem`, the older spellings such as `VACUUM FULL` too, so every
    /// spelling arrives as one list.
    fn utility_options(&mut self, list: &List) -> Made<Slice> {
        let mut options = Vec::with_capacity(list.len());
        for node in list.iter().flatten() {
            let Node::DefElem(option) = node else {
                return Err(not_yet(node));
            };
            let arg = match &option.arg {
                None => OptionArg::None,
                Some(Node::String(text)) => OptionArg::Word(self.intern(text)),
                Some(Node::Boolean(value)) => {
                    OptionArg::Word(self.intern(if *value { "true" } else { "false" }))
                }
                Some(Node::Integer(value)) => OptionArg::Integer(i64::from(*value)),
                Some(Node::Float(text)) => OptionArg::Number(self.intern(text)),
                Some(node) => return Err(not_yet(node)),
            };
            let name = self.intern(option.defname.as_deref().unwrap_or_default());
            options.push(UtilityOption { name, arg, span: self.at(option.location) });
        }
        let start = self.ast.utility_options.len() as u32;
        self.ast.utility_options.extend(options);
        Ok(Slice { start, len: self.ast.utility_options.len() as u32 - start })
    }

    /// `VACUUM` or `ANALYZE`, which are one statement in the grammar, with the options as written
    /// and each table with the columns written after it.
    fn vacuum(&mut self, vacuum: &VacuumStmt) -> Made<Statement> {
        let options = self.utility_options(&vacuum.options)?;
        let mut targets = Vec::with_capacity(vacuum.rels.len());
        for node in vacuum.rels.iter().flatten() {
            let Node::VacuumRelation(relation) = node else {
                return Err(not_yet(node));
            };
            let table = relation.relation.as_deref();
            let span = table.map_or(self.span, |table| self.at(table.location));
            let (name, _) = self.written_table(table)?;
            let mut columns = Vec::with_capacity(relation.va_cols.len());
            for column in relation.va_cols.iter().flatten() {
                let Node::String(column) = column else {
                    return Err(not_yet(column));
                };
                columns.push(self.intern(column));
            }
            let columns = self.ast.part_slice(columns);
            targets.push(VacuumTarget { name, columns, span });
        }
        let index = self.ast.vacuums.len() as u32;
        self.ast.vacuums.push(Vacuum { vacuum: vacuum.is_vacuumcmd, options, targets });
        Ok(Statement::Vacuum(index))
    }

    /// The span of the token at a location, or the span of the statement when the location is not
    /// known, which PostgreSQL writes as -1.
    fn at(&self, location: i32) -> Span {
        let Ok(location) = u32::try_from(location) else {
            return self.span;
        };
        let tokens = self.tokens();
        match tokens.binary_search_by_key(&location, |&(start, _, _)| start) {
            Ok(index) => Span::new(location, tokens[index].1),
            Err(_) => Span::new(location, location),
        }
    }

    fn intern(&mut self, text: &str) -> StrRef {
        self.interned.intern(&mut self.ast, text)
    }

    fn push(&mut self, expr: Expr, location: i32) -> ExprRef {
        let span = self.at(location);
        self.ast.push_expr(expr, span)
    }

    /// A run of names from a list of `String` nodes, for example the column names of an alias.
    fn names(&mut self, list: &[Option<Node>]) -> Made<Slice> {
        let mut parts = Vec::with_capacity(list.len());
        for node in list.iter().flatten() {
            let Node::String(text) = node else {
                return Err(not_yet(node));
            };
            parts.push(self.intern(text));
        }
        Ok(self.ast.part_slice(parts))
    }

    /// The text of each `String` node of a list.
    fn strings(list: &[Option<Node>]) -> Made<Vec<&str>> {
        let mut texts = Vec::with_capacity(list.len());
        for node in list.iter().flatten() {
            let Node::String(text) = node else {
                return Err(not_yet(node));
            };
            texts.push(&**text);
        }
        Ok(texts)
    }
}

#[cfg(test)]
mod tests {
    use super::{Refused, transform};

    fn error(sql: &str) -> String {
        match transform(sql) {
            Err(Refused::Error(error)) => error.to_string(),
            Err(other) => format!("{other:?}"),
            Ok(_) => "ok".to_string(),
        }
    }

    #[test]
    fn not_yet_names_the_node() {
        let Err(Refused::NotYet(name)) = transform("create table t (a int)") else {
            panic!("a create table is not built yet")
        };
        assert_eq!(name, "CreateStmt");
    }

    #[test]
    fn errors_of_the_raw_tree() {
        assert!(
            error("with q as (select 1), q as (select 2) select 1")
                .contains("specified more than once")
        );
        assert!(
            error("select rank() over (w) from t window w as (order by a rows 2 preceding)")
                .contains("because it has a frame clause")
        );
        assert!(
            error("select rank() over (w partition by b) from t window w as (order by a)")
                .contains("cannot override PARTITION BY")
        );
        assert!(error("select rank() over v from t").contains("window \"v\" does not exist"));
        assert!(error("select 1 from t window w as (), w as ()").contains("is already defined"));
        assert!(
            error("with recursive r as (select 1 from r union select 2) select 1 from r")
                .contains("non-recursive term")
        );
        assert!(
            error("with recursive r as (select 1 from r) select 1 from r")
                .contains("does not have the form")
        );
        assert!(
            error(
                "with recursive r as (select 1 union select 2 from r order by 1) select 1 from r"
            )
            .contains("ORDER BY in a recursive query")
        );
        assert_eq!(
            error("with recursive r as (select 1 union select 2 order by 1) select 1 from r"),
            "ok"
        );
    }

    /// The cases of `with.sql` and more, with the message and the column of the caret that
    /// PostgreSQL gives.
    #[test]
    fn a_definition_that_reads_itself_is_checked_the_way_postgres_checks_it() {
        let refused = |sql: &str| {
            let Err(Refused::Error(error)) = transform(sql) else {
                panic!("{sql} should be refused")
            };
            let column = error.span().map(|span| sql[..span.start as usize].chars().count() + 1);
            (error.message().to_string(), column)
        };
        let cases: &[(&str, &str, Option<usize>)] = &[
            (
                "WITH RECURSIVE x(n) AS (SELECT 1 INTERSECT SELECT n+1 FROM x) SELECT * FROM x",
                "recursive query \"x\" does not have the form non-recursive-term UNION [ALL] recursive-term",
                Some(16),
            ),
            (
                "WITH RECURSIVE x(n) AS (SELECT n FROM x UNION ALL SELECT 1) SELECT * FROM x",
                "recursive reference to query \"x\" must not appear within its non-recursive term",
                Some(39),
            ),
            (
                "WITH RECURSIVE x(n) AS (WITH x1 AS (SELECT 1 FROM x) SELECT 0 UNION SELECT * FROM x1) SELECT * FROM x",
                "recursive reference to query \"x\" must not appear within a subquery",
                Some(51),
            ),
            (
                "WITH RECURSIVE x(n) AS (SELECT 0 UNION SELECT 1 ORDER BY (SELECT n FROM x)) SELECT * FROM x",
                "ORDER BY in a recursive query is not implemented",
                Some(58),
            ),
            (
                "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT x.n+1 FROM y LEFT JOIN x ON x.n = y.a) SELECT * FROM x",
                "recursive reference to query \"x\" must not appear within an outer join",
                Some(74),
            ),
            (
                "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT x.n+1 FROM x RIGHT JOIN y ON x.n = y.a) SELECT * FROM x",
                "recursive reference to query \"x\" must not appear within an outer join",
                Some(62),
            ),
            (
                "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT x.n+1 FROM x FULL JOIN y ON x.n = y.a) SELECT * FROM x",
                "recursive reference to query \"x\" must not appear within an outer join",
                Some(62),
            ),
            (
                "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n IN (SELECT * FROM x)) SELECT * FROM x",
                "recursive reference to query \"x\" must not appear within a subquery",
                Some(88),
            ),
            (
                "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x LIMIT 10 OFFSET 1) SELECT * FROM x",
                "OFFSET in a recursive query is not implemented",
                Some(78),
            ),
            (
                "WITH RECURSIVE x(id) AS (VALUES (1) UNION ALL SELECT (SELECT * FROM x) FROM x WHERE id < 5) SELECT * FROM x",
                "recursive reference to query \"x\" must not appear within a subquery",
                Some(69),
            ),
            (
                "WITH RECURSIVE foo(i) AS (VALUES (1) UNION ALL (SELECT i+1 FROM foo WHERE i < 10 UNION ALL SELECT i+1 FROM foo WHERE i < 5)) SELECT * FROM foo",
                "recursive reference to query \"foo\" must not appear more than once",
                Some(108),
            ),
            (
                "WITH RECURSIVE foo(i) AS (VALUES (1) UNION ALL (SELECT i+1 FROM foo WHERE i < 10 EXCEPT SELECT i+1 FROM foo WHERE i < 5)) SELECT * FROM foo",
                "recursive reference to query \"foo\" must not appear within EXCEPT",
                Some(105),
            ),
            (
                "WITH RECURSIVE foo(i) AS (VALUES (1) UNION ALL (SELECT i+1 FROM foo WHERE i < 10 INTERSECT SELECT i+1 FROM foo WHERE i < 5)) SELECT * FROM foo",
                "recursive reference to query \"foo\" must not appear more than once",
                Some(108),
            ),
            (
                "WITH RECURSIVE foo(i) AS (VALUES (1) UNION ALL (SELECT i+1 FROM foo EXCEPT ALL SELECT 1)) SELECT * FROM foo",
                "recursive reference to query \"foo\" must not appear within EXCEPT",
                Some(65),
            ),
            (
                "WITH RECURSIVE foo(i) AS (VALUES (1) UNION ALL (SELECT 1 INTERSECT ALL SELECT i+1 FROM foo)) SELECT * FROM foo",
                "recursive reference to query \"foo\" must not appear within INTERSECT",
                Some(88),
            ),
        ];
        for &(sql, message, column) in cases {
            assert_eq!(refused(sql), (message.to_string(), column), "{sql}");
        }
        // A read of a definition further out, from inside a definition that reads itself, is a
        // read of that one.
        assert_eq!(
            refused(
                "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n < (WITH RECURSIVE z(m) AS (SELECT 1 UNION ALL SELECT m+1 FROM z, x) SELECT 1)) SELECT * FROM x"
            ),
            (
                "recursive reference to query \"x\" must not appear within a subquery".to_string(),
                Some(135)
            )
        );
    }

    /// PostgreSQL holds a definition that is read more than once wherever it is written, so that
    /// a volatile function in it gives each read the same rows.
    #[test]
    fn a_definition_read_twice_is_held_at_any_depth() {
        let held = |sql: &str| transform(sql).map(|ast| ast.ctes.len()).ok();
        let nested = "select count(*) from (with q as (select random()) select * from q union select * from q) s";
        assert_eq!(held(nested), Some(1));
        assert_eq!(held("select (with q as (select 1) select count(*) from q, q q2)"), Some(1));
        assert_eq!(held("select (with q as (select 1) select count(*) from q)"), Some(0));
        assert_eq!(
            held("select (with q as not materialized (select 1) select count(*) from q, q q2)"),
            Some(0)
        );
    }

    #[test]
    fn errors_of_the_writing_statements() {
        assert_eq!(error("update t set (a, b) = (1, 2), c = 3"), "ok");
        assert!(
            error("update t set (a, b) = (1, 2, 3)")
                .contains("number of columns does not match number of values")
        );
        assert!(
            error("update t set (a) = (1)").contains("must be a sub-SELECT or ROW() expression")
        );
        assert!(
            error("insert into t values (1) on conflict do update set b = 1")
                .contains("requires inference specification or constraint name")
        );
        assert_eq!(error("truncate a, b"), "ok");
    }
}

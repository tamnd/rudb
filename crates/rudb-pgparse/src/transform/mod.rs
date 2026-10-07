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
mod query;
mod types;

use rudb_common::{Error, Span};
use rudb_parse::Ast;
use rudb_parse::ast::{Expr, ExprRef, QueryRef, Slice, SourceRef, Statement, StrRef, WindowRef};
use rudb_parse::build::Interner;

use crate::nodes::{CTEMaterialize, Node, RawStmt};

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
    let mut transform = Transform::new(text)?;
    for node in list.iter().flatten() {
        let Node::RawStmt(raw) = node else {
            return Err(not_yet(node));
        };
        let statement = transform.statement(raw)?;
        transform.ast.statements.push(statement);
    }
    Ok(transform.ast)
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
    /// The place in [`Transform::defined`] after the definition and its query. The names that are
    /// defined after this place are the ones that can hide this definition.
    logged: usize,
    /// Each read of the definition: the source that was put in its place, and the alias and the
    /// column names written on the read.
    reads: Vec<(SourceRef, StrRef, Slice)>,
}

struct Transform<'a> {
    text: &'a str,
    ast: Ast,
    interned: Interner,
    /// Each token of the text as the lexer gives it, with its start, its end and its kind. A node
    /// has the start of one token as its location, and this is how its span is found.
    tokens: Vec<(u32, u32, u16)>,
    /// The span of the statement being transformed, which a node with no location gets.
    span: Span,
    /// The windows of the `WINDOW` clause of the select being transformed, each with its name,
    /// its spec and whether it has a frame clause.
    windows: Vec<(String, WindowRef, bool)>,
    /// The `WITH` definitions in scope, innermost last.
    scope: Vec<Definition>,
    /// Every `WITH` name in the order the definitions were found, which tells whether a name is
    /// defined again after a definition.
    defined: Vec<String>,
    /// How many queries deep the query being transformed is, counting itself.
    depth: usize,
    /// The slots of the recursive definitions whose own query is being transformed.
    recursing: Vec<u32>,
    /// Each read of a recursive definition from inside its own query, with the slot, the span and
    /// how many queries were made before the read. The count tells the non-recursive term from the
    /// recursive term, because the left query of the `UNION` is made before its right query.
    self_reads: Vec<(u32, Span, u32)>,
}

impl<'a> Transform<'a> {
    fn new(text: &'a str) -> Made<Self> {
        let mut lexer = crate::Lexer::new(text);
        let mut tokens = Vec::new();
        loop {
            let token = lexer.next_token().map_err(Refused::Syntax)?;
            if token.kind == 0 {
                break;
            }
            #[expect(clippy::cast_possible_truncation, reason = "a statement is less than 4 GiB")]
            tokens.push((token.start as u32, token.end as u32, token.kind));
        }
        Ok(Transform {
            text,
            ast: Ast { source: text.into(), ..Ast::default() },
            interned: Interner::default(),
            tokens,
            span: Span::new(0, 0),
            windows: Vec::new(),
            scope: Vec::new(),
            defined: Vec::new(),
            depth: 0,
            recursing: Vec::new(),
            self_reads: Vec::new(),
        })
    }

    /// One statement. Only a query is built yet.
    fn statement(&mut self, raw: &RawStmt) -> Made<Statement> {
        let start = u32::try_from(raw.stmt_location).unwrap_or(0);
        let end = match u32::try_from(raw.stmt_len) {
            Ok(0) | Err(_) => self.text.len() as u32,
            Ok(len) => start + len,
        };
        self.span = Span::new(start, end);
        match &raw.stmt {
            Some(Node::SelectStmt(select)) => Ok(Statement::Query(self.query(select)?)),
            Some(node) => Err(not_yet(node)),
            None => clause("RawStmt"),
        }
    }

    /// The span of the token at a location, or the span of the statement when the location is not
    /// known, which PostgreSQL writes as -1.
    fn at(&self, location: i32) -> Span {
        let Ok(location) = u32::try_from(location) else {
            return self.span;
        };
        match self.tokens.binary_search_by_key(&location, |&(start, _, _)| start) {
            Ok(index) => Span::new(location, self.tokens[index].1),
            Err(_) => Span::new(location, location),
        }
    }

    /// The kind of the token at a location, and the kind of the token after it.
    fn token_at(&self, location: i32) -> Option<(u16, u16)> {
        let location = u32::try_from(location).ok()?;
        let index = self.tokens.binary_search_by_key(&location, |&(start, _, _)| start).ok()?;
        let next = self.tokens.get(index + 1).map_or(0, |&(_, _, kind)| kind);
        Some((self.tokens[index].2, next))
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
}

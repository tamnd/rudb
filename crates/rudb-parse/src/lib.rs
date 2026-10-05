//! The SQL lexer, parser and AST, with a textual form that round trips.
//!
//! Rank 1 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! The grammar under `grammar/` is DuckDB's own, vendored verbatim and checked by
//! `cargo xtask grammar`. Everything derived from it lands in `generated` and is written by
//! `cargo xtask gen-grammar`. `spec/20-the-grammar.md` is the argument for doing it that way and
//! the list of what it does and does not buy.

#![forbid(unsafe_code)]

pub mod ast;
#[cfg(test)]
mod corpus;
pub mod deparse;
pub mod dialect;
pub mod generate;
pub mod generated;
pub mod matcher;
pub mod parameters;
pub mod rules;
pub mod token;
pub mod tokenize;
pub mod transform;

pub use ast::Ast;

/// The name a row trigger's body reads the rows that fired it under. It is quoted where the parser
/// writes it into the body, and the space in it keeps it apart from the names people give tables.
pub const TRIGGER_ROWS: &str = "rudb trigger rows";

/// The name a column of [`TRIGGER_ROWS`] goes by. A body reads it only through `NEW.` or `OLD.`,
/// which the parser rewrites to this, so a name the body leaves unqualified still finds the table
/// the body writes rather than the rows that fired it.
#[must_use]
pub fn trigger_column(name: &str) -> String {
    format!("rudb row {name}")
}
pub use generate::{Catalog, Generator, Table};
pub use generated::keywords::{
    COLUMN_NAME, FUNC_NAME, KEYWORDS, LONGEST, RESERVED, TYPE_NAME, UNRESERVED,
};
pub use matcher::{NONE, ParseNode, Tree, parse, parse_from, parse_tokens};
pub use rules::{Node, Op, Rule, Suggestion, alternatives, rule, token_key};
pub use token::{Flags, Kind, NOT_A_KEYWORD, Token};
pub use tokenize::{classes, hints, identifier_parts, lookup, quoted, tokenize};
pub use transform::{parse_ast, parse_ast_with_case, transform, transform_with_case};

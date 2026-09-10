//! The SQL lexer, parser and AST, with a textual form that round trips.
//!
//! Rank 1 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! The grammar under `grammar/` is DuckDB's own, vendored verbatim and checked by
//! `cargo xtask grammar`. Everything derived from it lands in `generated` and is written by
//! `cargo xtask gen-grammar`. `spec/20-the-grammar.md` is the argument for doing it that way and
//! the list of what it does and does not buy.

#![forbid(unsafe_code)]

pub mod generated;
pub mod matcher;
pub mod rules;
pub mod token;
pub mod tokenize;

pub use generated::keywords::{
    COLUMN_NAME, FUNC_NAME, KEYWORDS, LONGEST, RESERVED, TYPE_NAME, UNRESERVED,
};
pub use matcher::{NONE, ParseNode, Tree, parse, parse_from, parse_tokens};
pub use rules::{Node, Op, Rule, Suggestion, can_start, rule, token_key};
pub use token::{Flags, Kind, NOT_A_KEYWORD, Token};
pub use tokenize::{classes, lookup, tokenize};

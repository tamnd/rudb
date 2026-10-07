//! The PostgreSQL grammar: the parse tables made from `gram.y` and the parser that runs them.
//!
//! Rank 2 in the layer rule. See `xtask/layers.toml` and `notes/Spec/2140/compat/postgres/16-crate-layout.md`.
//!
//! The grammar is not written by hand. `cargo xtask pg-vendor` copies `gram.y`, `scan.l`,
//! `parser.c` and `kwlist.h` from PostgreSQL at the pin into `vendor/`, and makes the LALR(1)
//! tables in `src/generated/` from them, with the state numbers that `bison -v` gives for the same
//! grammar. So a statement that PostgreSQL parses is a statement that this crate parses, and a
//! syntax error stops at the same token. Document 08 of the PostgreSQL compatibility notes is the
//! specification, sections 8.6 to 8.8.
//!
//! # What is here
//!
//! [`keyword`], which says whether a word is a keyword and of which [`Category`], and
//! [`token`], the token numbers.
//!
//! [`Lexer`], a port of `scan.l`, and [`Tokens`], the filter of `parser.c` after it, which looks one
//! token further for the few places where the grammar needs two tokens.
//!
//! [`check`], which lexes and parses a text and gives the same first error as PostgreSQL, with the
//! same message and position. [`recognize`] runs the tables over a list of tokens.
//!
//! [`nodes`], the node types of the raw parse tree of PostgreSQL, generated from the vendored
//! headers with the names and the fields of C. Each node writes the text of `nodeToString`, which
//! PostgreSQL logs for a statement when `debug_print_raw_parse` is on, so a test compares the two
//! trees. The actions of `gram.y` that build this tree come next, and after them the transform
//! from this tree to the AST of `rudb-parse`.

mod error;
mod filter;
mod generated;
mod lexer;
pub mod nodes;
mod parser;

pub use error::{Error, Notice};
pub use filter::Tokens;
pub use generated::keywords::{Category, Keyword};
pub use generated::tables::{RULES, STATES, TOKENS, token};
pub use lexer::{Lexer, Token, Value};
pub use parser::{SyntaxError, character, check, keyword, recognize, rule_name, symbol_name};

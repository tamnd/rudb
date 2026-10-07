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
//! [`recognize`], which runs the tables over a list of tokens and says where the first token is
//! that the grammar does not accept. The lexer, the lookahead filter of `parser.c` and the actions
//! that build the AST of `rudb-parse` come next, in that order.

mod generated;
mod parser;

pub use generated::keywords::{Category, Keyword};
pub use generated::tables::{RULES, STATES, TOKENS, token};
pub use parser::{SyntaxError, character, keyword, recognize, rule_name, symbol_name};

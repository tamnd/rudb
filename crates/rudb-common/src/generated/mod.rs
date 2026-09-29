//! Tables derived from the vendored grammar that crates below the parser need too.
//!
//! Nothing in here is written by hand. `cargo xtask gen-grammar` produces it from
//! `crates/rudb-parse/grammar` in the same run as the parser's own tables, and `cargo xtask
//! gen-grammar --check` fails the gate if the two have drifted apart. The keyword table lives here
//! rather than in the parser because printing a type quotes a field name that is a keyword, and
//! the type printer is in this crate.

pub mod keywords;

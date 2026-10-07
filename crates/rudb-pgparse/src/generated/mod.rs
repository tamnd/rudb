//! Tables made from vendored PostgreSQL files.
//!
//! Nothing in here is written by hand except this file. `cargo xtask pg-vendor` writes the
//! tables from the files in `crates/rudb-pgparse/vendor`, and `cargo xtask pg-check` fails the
//! gate if the two disagree. The same command writes `gram.rules`, the grammar without its C,
//! and `productions.txt`, the name of each alternative.

pub(crate) mod keywords;
pub(crate) mod tables;

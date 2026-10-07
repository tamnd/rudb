//! Tables made from vendored PostgreSQL files.
//!
//! Nothing in here is written by hand except this file. `cargo xtask pg-vendor` writes the
//! tables from the files in `crates/rudb-pgtypes/vendor`, and `cargo xtask pg-check` fails the
//! gate if the two disagree.

pub(crate) mod casts;
pub mod oids;

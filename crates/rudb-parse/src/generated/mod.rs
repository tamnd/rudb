//! Tables derived from the vendored grammar.
//!
//! Nothing in here is written by hand. `cargo xtask gen-grammar` produces it from
//! `crates/rudb-parse/grammar`, and `cargo xtask gen-grammar --check` runs in the gate and fails
//! if the checked in files and the grammar have drifted apart. Checked in rather than built in a
//! `build.rs` so that the diff of a grammar bump shows what actually changed, which is the whole
//! point of pinning it. `spec/20-the-grammar.md` section 5.

pub mod keywords;

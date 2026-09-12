//! Operators, morsels, the scheduler, hash tables, sorting and spilling.
//!
//! Rank 12 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! This is tier 0 of `spec/08-codegen.md` section 8.1: a pull based tree of operators, one variant
//! per logical operator the binder can produce, every one of them written the simplest way that is
//! correct. Tier 0 is never removed and never optional, because it is the reference every faster
//! tier is differentially tested against, and a reference that is clever is a reference nobody can
//! read the answer out of when the clever tier disagrees with it.
//!
//! # What pull based means here and what it does not mean
//!
//! [`Operator::next`] returns the next [`Chunk`](rudb_vector::Chunk) or `None` when there are no
//! more. A pipeline of a scan, a filter and a projection is three of those calls deep and nothing
//! is materialized between them. An operator that cannot answer without seeing all of its input,
//! which is the aggregate, the sort, the join build side, the distinct and the set operations, does
//! all of its work on the first call to `next` and then hands out what it built one chunk at a
//! time. That is what `spec/07-execution.md` calls a pipeline breaker and it is the boundary the
//! morsel driven scheduler will later cut pipelines at.
//!
//! What this is not is the scheduler. There is one thread, there are no morsels, there is no
//! spilling and the hash join is a nested loop. Every one of those is M1 or later work and every
//! one of them replaces an operator here without changing the tree that builds it, because the
//! thing that builds the tree is [`build`] and the thing it builds against is a trait with two
//! methods.
//!
//! # Why a schema per operator
//!
//! A bound plan refers to columns by [`ColumnBinding`](rudb_plan::ColumnBinding), which is a table
//! index and a position, and a chunk is a row of vectors with no names on it. Something has to turn
//! one into the other, and that something is [`Schema`]: it is what an operator says it produces,
//! it carries the binding alongside the name and the type, and [`Schema::position_of`] is the whole
//! of expression column resolution. Building it is where the operators agree with the binder about
//! what a table index means, and it is checked rather than assumed, because a schema that is one
//! column out produces a wrong answer instead of an error.

#![forbid(unsafe_code)]

mod build;
mod cancel;
mod expr;
mod group;
mod join;
mod key;
mod operator;
mod prepared;
mod rows;
mod schema;
mod setop;
mod sort;
mod source;
mod spill;
mod stream;
mod topn;
mod written;

#[cfg(test)]
mod tests;

pub use build::{build, build_with};
pub use expr::{evaluate, evaluate_all};
pub use operator::Operator;
pub use prepared::{Prepared, Scratch};
pub use schema::Schema;
pub use written::written;

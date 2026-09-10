//! The compute kernels: casting, comparison, arithmetic, three-valued logic and the aggregates.
//!
//! Rank 3 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! Everything here takes vectors and values and knows nothing about plans, operators or catalogs.
//! That is what makes it callable from the interpreter, from the fused kernels of tier 1 and from
//! a test that wants to check one conversion, and it is why the comparison enum in [`Comparison`] is
//! this crate's own rather than the plan's.
//!
//! # What tier this is
//!
//! `spec/08-codegen.md` section 8.1 puts four tiers on the table and says tier 0 is never optional,
//! because it is the reference every faster tier is differentially tested against. This crate is
//! the compute half of tier 0.
//!
//! Every kernel here takes a vector and produces a vector, and the body of every one of them is a
//! scalar loop over [`rudb_vector::Vector::value_at`]. That is slow, it is slow on purpose, and it
//! is slow in a way that is visible: the interface is already the batch interface, so a generated
//! specialization that reads a `&[i32]` out of a flat vector replaces a body without touching a
//! caller. Section 7.3's kernel generator is what fills those in, and it is M1 work rather than M0
//! work because there is no benchmark to aim it at until there is an executor to run one.
//!
//! The one optimization that is here is the constant fast path: a cast or a comparison where both
//! sides are constant vectors costs one operation rather than 1024. That one is worth having now
//! because the binder turns every literal in a predicate into a constant vector, so it is on the
//! path of the first query anybody runs.
//!
//! # What is not here
//!
//! Encoded and dictionary specialization, which is M3. SIMD, which is M1 and which the generator
//! produces rather than a person writing it. Regular expressions, dates arithmetic, the string
//! functions past the four here, and the statistical aggregates. Each of those is a signature in
//! `rudb-functions` before it is a kernel here, so the missing ones fail at binding with a message
//! naming the function rather than here with a message naming a match arm.

#![forbid(unsafe_code)]

pub mod aggregate;
pub mod cast;
pub mod compare;
pub mod logic;
mod number;
pub mod scalar;

pub use aggregate::Accumulator;
pub use cast::{cast, cast_value};
pub use compare::{Comparison, compare, compare_values, order, order_with_nulls};
pub use logic::{Connective, combine, is_true};
pub use scalar::{call, call_values};

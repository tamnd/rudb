//! The compute kernels: casting, comparison, arithmetic, three-valued logic, the aggregates and
//! turning a vector of flags into the rows it keeps.
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
//! Every kernel here takes a vector and produces a vector. Each of them started as a scalar loop
//! over [`rudb_vector::Vector::value_at`], which was slow on purpose and slow in a way that was
//! visible, because the interface was already the batch interface and a specialization that reads a
//! `&[i32]` out of a flat vector replaces a body without touching a caller. Sub-milestone 2b is
//! where that replacement happens, one file at a time, each with a microbenchmark next to it.
//!
//! All five files are done. [`compare::compare`], [`scalar::call`], [`logic::combine`] and
//! [`cast::cast`] dispatch once on the form pair and once on the physical layout, hoist everything
//! that does not change from row to row out of the loop, and keep the old row at a time loop as the
//! oracle their property tests check against rather than as dead code. The fifth,
//! [`aggregate::Accumulator`], is the odd one out because it has state rather than an output vector,
//! so its batch interface folds a vector into the running state instead of returning one. The
//! shapes none of them has a loop for still run the old loop, and they are still correct.
//!
//! [`select::selection`] arrived after those five and is the other end of the same measurement.
//! A comparison that produces a boolean vector in under a nanosecond a row is no use if the
//! operator above it then reads that vector back a value at a time, which is what a filter was
//! doing, so the sixth file turns the flags into the positions that survived without a branch in
//! the loop.
//!
//! The other optimization that has been here from the start is the constant fast path: a cast or a
//! comparison where both sides are constant vectors costs one operation rather than 1024. That one
//! is worth having because the binder turns every literal in a predicate into a constant vector, so
//! it is on the path of the first query anybody runs.
//!
//! # Knowing what to specialize next
//!
//! There are four physical forms and so sixteen form pairs, and a hand written loop for all sixteen
//! of them in every kernel is both a lot of code and a lot of places for a wrong answer to hide.
//! The rule this crate follows instead is to specialize the pairs a scan actually produces and to
//! count the rest. [`fallback`] is the counter. A kernel that falls through to the row at a time
//! path increments a cell, a harness prints the cells that are not zero at the end of a run, and a
//! pair worth another loop then arrives as a number rather than as an opinion.
//!
//! # What is not here
//!
//! Encoded vector specialization, which arrives with the fifth form at layer three. SIMD, which the
//! generator of section 7.3 produces rather than a person writing it. Regular expressions, date
//! arithmetic, the string functions past the four here, and the statistical aggregates. Each of
//! those is a signature in `rudb-functions` before it is a kernel here, so the missing ones fail at
//! binding with a message naming the function rather than here with a message naming a match arm.

#![forbid(unsafe_code)]

pub mod aggregate;
pub mod cast;
pub mod compare;
mod datetime;
pub mod fallback;
pub mod logic;
mod number;
mod regexp;
pub mod scalar;
pub mod select;
mod shape;
mod subscript;
mod text;

pub use aggregate::Accumulator;
pub use cast::{cast, cast_value};
pub use compare::{Comparison, compare, compare_values, order, order_with_nulls, refine};
pub use fallback::Kernel;
pub use logic::{Connective, combine, is_true};
pub use scalar::{call, call_values};
pub use select::{refine as refine_flags, selection};

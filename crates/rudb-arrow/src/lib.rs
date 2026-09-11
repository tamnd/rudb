//! Arrow interchange, zero copy where the layouts permit.
//!
//! Rank 5 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! Arrow is how a result leaves the process without being formatted first. Pandas, Polars, R, Java
//! and every other client DuckDB has a binding for already read it, so a result that arrives as
//! Arrow arrives as columns the caller can use rather than as text the caller has to parse. That is
//! the whole reason this crate exists, and it is why `spec/13-client-api.md` names Arrow before it
//! names any of the per language bindings.
//!
//! Three pieces:
//!
//! - [`DataType`], one of our types as Arrow names it, and the format string the C data interface
//!   spells that name with.
//! - [`Array`], one column in Arrow's buffer layout.
//! - [`RecordBatch`], a chunk of them with a [`Schema`] over the top.
//!
//! # Zero copy, and where it stops
//!
//! The crate's description promises zero copy where the layouts permit, and this first version
//! copies everywhere, so the promise is worth being exact about. Our validity bitmap is already
//! Arrow's, bit for bit, and a run of `i32` values is a run of `i32` values, so for those two
//! buffers the copy here is a memcpy that a later change removes by handing over ownership of the
//! pages instead. That change needs a buffer whose lifetime an FFI consumer can hold, which needs
//! the buffer manager, which is M2 work. Writing the copying version first means the mapping itself
//! is settled and tested before the lifetime question is opened.
//!
//! One buffer will never be zero copy, and that is `VARCHAR`. We store a 16 byte view with an
//! inline prefix plus an arena, and Arrow's `u` is a run of offsets over one contiguous block of
//! bytes in row order. Those are different data structures rather than different spellings of one,
//! so the export builds the block. Arrow does have a view layout of its own now, and adopting it
//! later would make this one free, which is a thing to measure rather than a thing to assume.
//!
//! # What is deliberately not here
//!
//! The FFI boundary. `ArrowArray` and `ArrowSchema` are C structs with release callbacks and raw
//! pointers, and the release callback is the part that is easy to get wrong and expensive to debug.
//! It belongs in `rudb-c-api` with the rest of the `unsafe`, built on top of these owned types,
//! which this crate can then keep testing without a single raw pointer. [`DataType::format`] is
//! here rather than there because the format string is defined by Arrow's document, so it is the
//! part a test can check against that document rather than against our own opinion.
//!
//! The nested types. A list is offsets and a child array, a struct is a list of child arrays, and
//! there is no child array to build one out of until `rudb-vector` has a nested vector.

#![forbid(unsafe_code)]

mod array;
mod batch;
mod types;

pub use array::Array;
pub use batch::RecordBatch;
pub use types::{DataType, Field, Schema, TimeUnit};

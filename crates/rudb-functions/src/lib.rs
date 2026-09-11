//! The scalar, aggregate and window function library.
//!
//! Rank 7 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! Most of what is here is the signature half: given a name and the types of the arguments, which
//! function is that and what does it return. The implementations are separate work and they go
//! behind the same names, so the binder does not change when they arrive.
//!
//! [`mod@file`] is the exception and it reads a file. A table function that scans a Parquet file cannot
//! be resolved from a table of names, because its columns are in the file, so resolving one means
//! opening it. That is the only I/O in this crate and it is the reason `rudb-io` and `rudb-parquet`
//! are dependencies of it.

#![forbid(unsafe_code)]

pub mod file;
pub mod signature;
pub mod table;

pub use file::{open_parquet, parquet_fields};
pub use signature::{FunctionKind, Resolved, kind_of, resolve};
pub use table::{Columns, ResolvedTable, TableFunction, resolve_table, series, series_length};

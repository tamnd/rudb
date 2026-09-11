//! Schemas, tables, views, constraints, dependency tracking, dictionaries and symbol tables.
//!
//! Rank 8 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! What is here today is the part the binder cannot be written without: a name, the rule for
//! comparing two of them, a table with columns and rows, and the registry that turns
//! `catalog.schema.table` into one object. Transactional catalog versions, views, constraints and
//! the dependency graph are M4 work and they hang off [`Catalog`] rather than replacing it.
//!
//! The rows live in [`rudb_storage::MemoryTable`] for now. That is the one piece here that is
//! openly temporary, and it is behind [`Table::rows`] precisely so that swapping it for the real
//! storage format at M2 is a change to one field.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod name;
pub mod table;

pub use catalog::{Catalog, DEFAULT_CATALOG, DEFAULT_SCHEMA, Database, Schema};
pub use name::{QualifiedName, same_name};
pub use table::{Table, duplicate_check};

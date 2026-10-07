//! Schemas, tables, views, constraints, dependency tracking, dictionaries and symbol tables.
//!
//! Rank 8 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! What is here today is the part the binder cannot be written without: a name, the rule for
//! comparing two of them, a table with columns and rows, a view with the query it stands for, and
//! the registry that turns `catalog.schema.table` into one object. Transactional catalog versions,
//! constraints and the dependency graph are M4 work and they hang off [`Catalog`] rather than
//! replacing it.
//!
//! The rows live in [`rudb_storage::MemoryTable`] for now. That is the one piece here that is
//! openly temporary, and it is behind [`Table::rows`] precisely so that swapping it for the real
//! storage format at M2 is a change to one field.

#![forbid(unsafe_code)]

pub mod alter;
pub mod catalog;
pub mod gone;
pub mod held;
pub mod index;
pub mod keys;
pub mod macros;
pub mod mirror;
pub mod name;
pub mod parent;
pub mod points;
pub mod search;
pub mod system;
pub mod table;
pub mod trigger;
pub mod view;

pub use alter::Alteration;
pub use catalog::{
    Catalog, DEFAULT_CATALOG, DEFAULT_SCHEMA, DETACHED, Database, Entry, Schema, Sequence, UserType,
};
pub use gone::Gone;
pub use held::Held;
pub use index::Index;
pub use keys::{Constraint, ForeignKey, Identity, Key, KeyLog};
pub use macros::{Macro, Overload, Parameter};
pub use mirror::{FileStamp, MIRROR_CATALOG};
pub use name::{QualifiedName, same_name};
pub use parent::{Parent, Placement};
pub use points::{Point, Reach, Spot, looks_up};
pub use rudb_native::StoredPart;
pub use search::SearchEntry;
pub use system::{INFORMATION_SCHEMA, PG_CATALOG, SYSTEM_CATALOG, TEMP_CATALOG};
pub use table::{
    CodedRows, Rows, Table, duplicate_check, next_revision, null_in, revision_now, row_of,
};
pub use trigger::{Event, Trigger};
pub use view::View;

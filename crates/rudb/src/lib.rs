//! The embedding API: connections, prepared statements, configuration and results.
//!
//! Rank 13 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! This is the crate somebody who wants a database depends on. Everything below it is an
//! implementation detail that happens to be published, and everything above it is a different way
//! of reaching this same API: `rudb-c-api` is this over the C ABI and `rudb-cli` is this behind a
//! prompt.
//!
//! ```
//! use rudb::Database;
//! use rudb_common::{Field, LogicalType, Value};
//!
//! let mut db = Database::new();
//! db.create_table("t", vec![Field::new("x", LogicalType::Integer)])?;
//! db.append("t", &[vec![Value::Integer(1)], vec![Value::Integer(7)]])?;
//!
//! let result = db.query("SELECT x FROM t WHERE x > 5")?;
//! assert_eq!(result.len(), 1);
//! assert_eq!(result.value_at(0, 0), Value::Integer(7));
//! # Ok::<(), rudb_common::Error>(())
//! ```
//!
//! # What a query is today
//!
//! Parse, bind, optimize, execute. The optimizer is one pass, which is column pruning, so a scan
//! reads the columns something above it asks for and the plan is otherwise the shape the binder
//! built it. The rest of `spec/09-optimizer.md`'s sequence is M1 work. There are no transactions, and `rudb-txn` is in the dependency list for the same
//! reason: the seam is where it will be and nothing goes through it yet.
//!
//! There is also no `CREATE TABLE` and no `INSERT`, because the M0 parser handles `SELECT` and
//! nothing else. [`Database::create_table`] and [`Database::append`] are how rows get in, and they
//! are a real part of the API rather than a test helper, since an embedded analytical database gets
//! most of its data from a program rather than from a string of SQL. When the DDL statements arrive
//! they bind to the same catalog calls these make.
//!
//! # Threading
//!
//! [`Database::query`] takes `&self`. Binding reads the catalog and running a plan reads the tables,
//! so two queries can run at once against one database and the compiler enforces that nothing is
//! being written while they do. Writing takes `&mut self`, which is the whole of the concurrency
//! story until `rudb-txn` has one.

#![forbid(unsafe_code)]

mod database;
mod result;
mod statements;

#[cfg(test)]
mod tests;

pub use database::Database;
pub use result::QueryResult;
pub use statements::{Statement, is_complete, statements};

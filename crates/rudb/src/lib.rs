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
//! use rudb::{Database, Field, LogicalType, Value};
//!
//! let db = Database::new();
//! db.create_table("t", vec![Field::new("x", LogicalType::Integer)])?;
//! db.append("t", &[vec![Value::Integer(1)], vec![Value::Integer(7)]])?;
//!
//! let result = db.query("SELECT x FROM t WHERE x > 5")?;
//! assert_eq!(result.len(), 1);
//! assert_eq!(result.value_at(0, 0), Value::Integer(7));
//! # Ok::<(), rudb::Error>(())
//! ```
//!
//! # What a query is today
//!
//! Parse, bind, optimize, execute. The optimizer is one pass, which is column pruning, so a scan
//! reads the columns something above it asks for and the plan is otherwise the shape the binder
//! built it. The rest of `spec/09-optimizer.md`'s sequence is M1 work. There are no transactions, and `rudb-txn` is in the dependency list for the same
//! reason: the seam is where it will be and nothing goes through it yet.
//!
//! [`Database::create_table`] and [`Database::append`] are how rows get in without SQL, and they
//! are a real part of the API rather than a test helper, since an embedded analytical database gets
//! most of its data from a program rather than from a string of SQL. The DDL statements bind to the
//! same catalog calls these make.
//!
//! # Prepared statements
//!
//! [`Database::prepare`] and [`Connection::prepare`] parse a statement once and hand back a
//! [`Prepared`] that runs with values for its parameters, written `?`, `?1`, `$1` or `$name`. The
//! statement is bound again for each set of values rather than planned once and filled in, because
//! an analytical plan depends on what the values are: a scan that keeps one row in a million and a
//! scan that keeps half the table want different plans, and the binder is cheap next to either.
//!
//! # Threading
//!
//! A [`Database`] is a handle. Cloning one, or calling [`Database::connect`], gives another handle
//! on the same database, and every method takes `&self`, so the program embedding this is the one
//! that decides how many threads there are. The catalog is behind a reader writer lock: any number
//! of queries read at once and a statement that writes has the database to itself while it runs.
//!
//! That lock is the whole of the concurrency story until `rudb-txn` has one. A statement that writes
//! is serialized against every reader rather than isolated from them, which is correct and is
//! coarse, and the thing that makes it finer is a transaction rather than a different lock.

#![forbid(unsafe_code)]

mod config;
mod connection;
mod database;
mod prepared;
mod result;
mod statements;

#[cfg(test)]
mod tests;

pub use config::{Config, parse_size};
pub use connection::Connection;
pub use database::Database;
pub use prepared::Prepared;
pub use result::QueryResult;
pub use statements::{Statement, is_complete, statements};

// The types the API deals in, so a program that embeds rudb depends on this crate and nothing else.
// `rudb-compat` and `rudb-bench` driving the library through one crate is the point of #110, and a
// caller who had to reach for `rudb-common` to name the type of a value would not be doing that.
pub use rudb_common::{Error, Field, LogicalType, Result, Value};
pub use rudb_vector::Chunk;

/// Arrow interchange, which is what [`QueryResult::to_arrow`] hands back.
///
/// A module rather than a flat re-export because Arrow has a `Field` and a `Schema` of its own and
/// so do we, and two types called `Field` in one namespace is a worse trade than four extra
/// characters at the call site.
pub mod arrow {
    pub use rudb_arrow::{Array, DataType, Field, RecordBatch, Schema, TimeUnit};
}

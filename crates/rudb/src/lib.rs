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
//!
//! # Stopping a query
//!
//! Two ways, and they answer two different questions. [`Config::with_query_timeout`] is a limit the
//! statement enforces on itself, which is what a harness running somebody else's SQL wants, because
//! the thing it is guarding against is a query that never ends rather than a person who changed
//! their mind. [`Connection::interrupt`] is the other end of a token somebody else holds, which is
//! what a signal handler wants, and it is DuckDB's model as well: `duckdb_interrupt` takes a
//! connection.
//!
//! ```
//! use std::time::Duration;
//! use rudb::{Config, Database};
//!
//! let db = Database::with_config(Config::new().with_query_timeout(Duration::from_millis(50)));
//! let error = db.query("SELECT count(*) FROM range(100000000000)").expect_err("too slow");
//! assert_eq!(error.code().duckdb_name(), "Interrupt Error");
//! ```
//!
//! A query stops at its next chunk boundary rather than immediately, which is a thousand rows of
//! work later, and the reason is in [`Cancel`]. Nothing is rolled back, because there are no
//! transactions yet: a stopped `INSERT` has written nothing, since the source runs to completion
//! before anything is appended, and a stopped `CREATE TABLE AS SELECT` leaves no table behind for
//! the same reason. That stops being true the day the writes stream, and the thing that makes it
//! true again is a transaction.
//!
//! # Running out of memory
//!
//! The third way a query stops. [`Config::with_memory_limit`] is a budget for the whole database,
//! and the operators that buffer without bound charge what they hold against it. A query that asks
//! for more than is left stops with an `Out of Memory Error` rather than being killed from outside,
//! which is the difference between a harness that reports a result for a file and a harness that
//! reports nothing because the process died.
//!
//! There is a budget without anybody setting one. It is eighty percent of what the machine has, the
//! way DuckDB's is, and [`Config::memory_limit`] says what it is on this machine and what to do to
//! turn it off. The default is the whole point of the error: a limit that has to be typed is a limit
//! that is not there on the machine where the query went wrong.
//!
//! ```
//! use rudb::{Config, Database};
//!
//! let db = Database::with_config(Config::new().with_memory_limit(1 << 20));
//! let error = db.query("SELECT * FROM range(10000000) ORDER BY range").expect_err("too large");
//! assert_eq!(error.code().duckdb_name(), "Out of Memory Error");
//! // And the budget is given back, so the connection is still usable.
//! assert_eq!(db.memory().used(), 0);
//! assert_eq!(db.value("SELECT 1").expect("a small query still runs"), rudb::Value::Integer(1));
//! ```
//!
//! What is counted is what the operators said they were holding, which is not the resident size of
//! the process. [`rudb_common::Memory`] says exactly what that covers and which direction it errs
//! in.

#![forbid(unsafe_code)]

mod config;
mod connection;
mod database;
mod prepared;
mod result;
mod settings;
mod statements;
mod syntax;

#[cfg(test)]
mod tests;

pub use config::{Config, parse_size};
pub use connection::Connection;
pub use database::Database;
pub use prepared::Prepared;
pub use result::QueryResult;
pub use statements::{Statement, is_complete, statements};
pub use syntax::{RowOrder, accepts, line_and_column, parses, row_order, split, where_it_happened};

// The types the API deals in, so a program that embeds rudb depends on this crate and nothing else.
// `rudb-compat` and `rudb-bench` driving the library through one crate is the point of #110, and a
// caller who had to reach for `rudb-common` to name the type of a value would not be doing that.
pub use rudb_common::{Cancel, Error, ErrorCode, Field, LogicalType, Result, Span, Value};
pub use rudb_vector::Chunk;

/// Every optimizer pass, by the name `SET disabled_optimizers` knows it by, in the order they run.
///
/// What a caller does with it is turn the optimizer off: `SET disabled_optimizers` takes DuckDB's
/// comma separated spelling, and the whole list joined by commas is every rewrite off and the bound
/// plan running as the binder produced it. `spec/09-optimizer.md` section 9.1 makes that a gate
/// rather than a curiosity, because the unoptimized answer is the right answer by construction and
/// any query that answers differently with the passes on is a pass that changed an answer. The
/// corpus in `tamnd/rudb-compat` runs both ways and compares, and it needs the names to do it.
///
/// DuckDB spells the same question `SELECT name FROM duckdb_optimizers()`, which is a table
/// function rudb does not have yet. When it arrives it reads this.
///
/// ```
/// use rudb::Database;
///
/// let db = Database::new();
/// db.execute(&format!("SET disabled_optimizers = '{}'", rudb::optimizers().join(",")))?;
/// // The bound plan, with nothing folded, so the addition is still a call.
/// assert!(db.plan("SELECT 1 + 2")?.contains("\"+\""));
/// # Ok::<(), rudb::Error>(())
/// ```
#[must_use]
pub fn optimizers() -> Vec<&'static str> {
    rudb_opt::PASSES.iter().map(|pass| pass.name()).collect()
}

/// The seams, which are the parts of the engine there is more than one published way to build.
///
/// A module rather than a flat re-export, because `Settings` here is which implementation runs at
/// each seam and `Settings` in `rudb-cli` is how a result is printed, and a name that has to be
/// read in context is a name worth qualifying. [`Database::seams`] is what hands one back.
pub mod seam {
    pub use rudb_seam::{
        ChoiceReason, Determinism, Policy, PolicyMode, Provenance, SEAM_PREFIX, SeamId, Settings,
    };
}

/// What one execution reported about itself, which is what [`QueryResult::metrics`] hands back.
///
/// A module rather than a flat re-export, for the same reason the seams are one: `Operator` here is
/// a row of measurements and `Operator` in the executor is a thing that runs, and the two want
/// telling apart at a call site. The shell writes [`metrics::Document::render`] out under
/// `--metrics`, which is the file `rudb-bench` reads.
pub mod metrics {
    pub use rudb_metrics::{Document, Operator, Pipeline};
}

/// Arrow interchange, which is what [`QueryResult::to_arrow`] hands back.
///
/// A module rather than a flat re-export because Arrow has a `Field` and a `Schema` of its own and
/// so do we, and two types called `Field` in one namespace is a worse trade than four extra
/// characters at the call site.
pub mod arrow {
    pub use rudb_arrow::{Array, DataType, Field, RecordBatch, Schema, TimeUnit};
}

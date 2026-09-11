//! One caller's handle on a database.

use rudb_common::{Error, Result, Value};

use crate::database::Shared;
use crate::prepared::Prepared;
use crate::result::QueryResult;

/// A connection to a database.
///
/// Many of these share one [`crate::Database`], which is the model DuckDB has and the reason this
/// type exists before there is anything of its own to put in it. A connection will carry a
/// transaction, a set of temporary tables and a prepared statement cache, and every one of those
/// three is a thing that arrives later and belongs here rather than on the database. Adding the type
/// afterwards would mean moving every method a caller already wrote.
///
/// Every method takes `&self`. The lock is inside, so two connections in two threads are two
/// callers of one database rather than two borrows the compiler has to arbitrate, which is what an
/// embedded database is for.
#[derive(Debug, Clone)]
pub struct Connection {
    shared: Shared,
}

impl Connection {
    /// A connection on this database. Called by [`crate::Database::connect`].
    pub(crate) fn new(shared: Shared) -> Self {
        Self { shared }
    }

    /// Runs one query and returns every row it produced.
    ///
    /// # Errors
    ///
    /// A parse error, a binder error, or anything the operators raise while running, which is
    /// mostly cast failures and arithmetic that leaves the range of its type.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        self.shared.query(sql)
    }

    /// Runs one statement, which may change the database.
    ///
    /// This is [`Connection::query`] plus the statements that write. A `SELECT` returns its rows,
    /// and a `CREATE TABLE`, a `DROP TABLE` or an `INSERT` returns an empty result, which is what
    /// DuckDB's own C API does for them.
    ///
    /// # Errors
    ///
    /// A parse error, a binder error, a catalog error, or anything the operators raise.
    pub fn execute(&self, sql: &str) -> Result<QueryResult> {
        self.shared.execute(sql)
    }

    /// The plan for a query, in the textual form `spec/07-execution.md` describes, without running
    /// it.
    ///
    /// # Errors
    ///
    /// A parse error or a binder error.
    pub fn plan(&self, sql: &str) -> Result<String> {
        self.shared.plan(sql)
    }

    /// Parses a statement so it can be run more than once, with values for its parameters.
    ///
    /// # Errors
    ///
    /// A parse error. A name that does not resolve or a type that does not work out is an error at
    /// execution rather than here, because a parameter has no type until it has a value.
    pub fn prepare(&self, sql: &str) -> Result<Prepared> {
        Prepared::new(self.shared.clone(), sql)
    }

    /// Runs a query and returns the single value it produced.
    ///
    /// # Errors
    ///
    /// Everything [`Connection::query`] can raise, plus an error if the result is not one row of
    /// one column.
    pub fn value(&self, sql: &str) -> Result<Value> {
        let result = self.query(sql)?;
        single(&result)
    }
}

/// The one cell of a one by one result, and an error for anything else.
pub(crate) fn single(result: &QueryResult) -> Result<Value> {
    if result.len() != 1 || result.width() != 1 {
        return Err(Error::invalid_input(format!(
            "expected one row of one column, got {} rows of {} columns",
            result.len(),
            result.width()
        )));
    }
    Ok(result.value_at(0, 0))
}

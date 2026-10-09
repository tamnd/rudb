//! One caller's handle on a database.

use std::sync::Arc;

use rudb_catalog::QualifiedName;
use rudb_common::session::Postgres;
use rudb_common::{Cancel, Error, Field, Result, Value};

use crate::database::Shared;
use crate::prepared::Prepared;
use crate::result::{Notice, QueryResult};

/// Where a connection is in a transaction block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transaction {
    /// No `BEGIN` is open. Each statement is its own transaction.
    Idle,
    /// A `BEGIN` is open.
    Open,
    /// A statement failed in the open transaction, and only `COMMIT` or `ROLLBACK` runs until it
    /// ends.
    Aborted,
}

/// The table that a load of rows writes into, from [`Connection::load_target`]: what
/// `COPY FROM STDIN` of a PostgreSQL server needs to read its data before it has any rows.
#[derive(Debug, Clone)]
pub struct LoadTarget {
    pub(crate) name: QualifiedName,
    fields: Vec<Field>,
    defaults: Vec<Option<String>>,
}

impl LoadTarget {
    /// The columns of the table, in their order.
    #[must_use]
    pub fn columns(&self) -> &[Field] {
        &self.fields
    }

    /// The name of the table, without its schema.
    #[must_use]
    pub fn table(&self) -> &str {
        &self.name.table
    }

    /// The schema of the table.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.name.schema
    }
}

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
    cancel: Cancel,
}

impl Connection {
    /// A connection on this database. Called by [`crate::Database::connect`].
    pub(crate) fn new(shared: Shared) -> Self {
        Self { shared, cancel: Cancel::new() }
    }

    /// Stops the statement this connection is running.
    ///
    /// Returns straight away. The statement stops at its next chunk boundary and the thread running
    /// it gets an `INTERRUPT Error` back, so a caller that wants to know it has stopped waits on
    /// that thread rather than on this call. A connection with nothing running is unaffected,
    /// because the flag is cleared at the top of each statement.
    ///
    /// This is the call a signal handler makes, and a [`Connection`] is cheap to clone, so the
    /// handler holds a clone and the query holds the original. It is also the call a watchdog thread
    /// makes for a limit that is not a plain time limit: for a plain one, set
    /// [`crate::Config::with_query_timeout`] and the statement enforces it itself.
    pub fn interrupt(&self) {
        self.cancel.cancel();
    }

    /// The flag that [`Connection::interrupt`] sets, for a wait outside of the engine that has to
    /// stop with the statement, such as the wait for an advisory lock of a PostgreSQL session.
    #[must_use]
    pub fn cancel_flag(&self) -> Cancel {
        self.cancel.clone()
    }

    /// The token for one statement: this connection's flag, and the configured time limit.
    fn token(&self) -> Cancel {
        self.shared.restart(&self.cancel)
    }

    /// Where the connection is in a transaction block. A PostgreSQL server sends this in each
    /// `ReadyForQuery`.
    pub fn transaction(&self) -> Transaction {
        self.shared.block()
    }

    /// Marks the open transaction block as aborted, for a statement that failed outside of the
    /// engine, such as a `SET ROLE` that a server runs itself. Does nothing with no block open.
    pub fn abort_transaction(&self) {
        self.shared.abort_block();
    }

    /// Records the PostgreSQL session that speaks through this connection. From the next statement
    /// on, `current_setting()` reads its parameters and `version()` and the user functions answer
    /// for it. A server calls this again each time the parameters change.
    pub fn set_postgres(&self, postgres: Arc<Postgres>) {
        self.shared.set_postgres(postgres);
    }

    /// The statements of a script in the dialect of the session. A PostgreSQL session reads the
    /// script with the PostgreSQL grammar, so a syntax error in any statement is the error of the
    /// script. Any other session splits it as [`crate::statements`] does.
    ///
    /// # Errors
    ///
    /// The syntax error of a PostgreSQL session, or the tokenizer error of any other session.
    pub fn statements<'a>(&self, script: &'a str) -> Result<Vec<crate::Statement<'a>>> {
        if self.shared.is_postgres() {
            crate::statements::postgres_statements(script)
        } else {
            crate::statements(script)
        }
    }

    /// Records when the client sent the statement that runs next, in microseconds since the epoch.
    /// `statement_timestamp()` gives it, and a `BEGIN` takes it as the start of the transaction
    /// that `now()` gives. A server calls this each time it reads a statement.
    pub fn set_statement_start(&self, micros: i64) {
        self.shared.set_statement_start(micros);
    }

    /// The notices that the last statement of this thread raised, once, whether it failed or not.
    /// These are the notices of the parse, such as the one for an identifier that is too long, and
    /// they come before the notices of [`QueryResult::notices`]. Each [`Connection::query`],
    /// [`Connection::execute`], [`Connection::prepare`] and run of a [`Prepared`] starts with no
    /// notices.
    #[must_use]
    pub fn notices(&self) -> Vec<Notice> {
        rudb_common::notice::take()
    }

    /// The rows that the last statement of this thread made before it failed, once.
    ///
    /// Only a statement of a PostgreSQL session keeps them, see [`Connection::set_postgres`],
    /// because PostgreSQL sends the columns and the rows that a query made before an error and
    /// then the error. A server takes them after each error. The result has no rows when the query
    /// failed at its first row, and there is `None` when the statement failed before it ran.
    #[must_use]
    pub fn rows_before_error(&self) -> Option<QueryResult> {
        crate::database::rows_before_error()
    }

    /// Runs one query and returns every row it produced.
    ///
    /// # Errors
    ///
    /// A parse error, a binder error, or anything the operators raise while running, which is
    /// mostly cast failures and arithmetic that leaves the range of its type.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        drop(rudb_common::notice::take());
        self.shared.query(sql, &self.token()).map_err(|error| self.shared.process_error(error))
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
        drop(rudb_common::notice::take());
        self.shared.execute(sql, &self.token()).map_err(|error| self.shared.process_error(error))
    }

    /// The plan for a query, in the textual form `spec/07-execution.md` describes, without running
    /// it.
    ///
    /// # Errors
    ///
    /// A parse error or a binder error.
    pub fn plan(&self, sql: &str) -> Result<String> {
        self.shared.plan(sql).map_err(|error| self.shared.process_error(error))
    }

    /// Parses a statement so it can be run more than once, with values for its parameters.
    ///
    /// # Errors
    ///
    /// A parse error. A name that does not resolve or a type that does not work out is an error at
    /// execution rather than here, because a parameter has no type until it has a value.
    pub fn prepare(&self, sql: &str) -> Result<Prepared> {
        drop(rudb_common::notice::take());
        Prepared::new(self.shared.clone(), sql, self.cancel.clone())
            .map_err(|error| self.shared.process_error(error))
    }

    /// The table `parts` names, for [`Connection::load`].
    ///
    /// # Errors
    ///
    /// If the name does not resolve to a table.
    pub fn load_target(&self, parts: &[&str]) -> Result<LoadTarget> {
        let (name, fields, defaults) =
            self.shared.load_target(parts).map_err(|error| self.shared.process_error(error))?;
        Ok(LoadTarget { name, fields, defaults })
    }

    /// Writes rows into the table of `target`, inside the open transaction when there is one.
    ///
    /// `columns` holds one list of values for each column in `given`, all `rows` long. A column
    /// that is not in `given` gets its default, worked out for each row as an `INSERT` works it
    /// out, or a null. A value that is not of its column's type is cast to it. The rows go in as
    /// the rows of an `INSERT` go in: `CHECK`, `NOT NULL`, keys and foreign keys, then the log.
    ///
    /// # Errors
    ///
    /// A value that does not cast to its column's type, a constraint the rows break, a default
    /// that fails, or a transaction that is read only.
    pub fn load(
        &self,
        target: &LoadTarget,
        given: &[usize],
        columns: Vec<Vec<Value>>,
        rows: usize,
    ) -> Result<()> {
        let mut full: Vec<Option<Vec<Value>>> = vec![None; target.fields.len()];
        for (&at, values) in given.iter().zip(columns) {
            let ty = &target.fields[at].ty;
            let values = if values.iter().all(|v| v.is_null() || &v.logical_type() == ty) {
                values
            } else {
                values
                    .iter()
                    .map(|value| rudb_kernels::cast::cast_value(value, ty, false))
                    .collect::<Result<Vec<_>>>()?
            };
            full[at] = Some(values);
        }
        let mut filled = Vec::with_capacity(full.len());
        for (at, values) in full.into_iter().enumerate() {
            let field = &target.fields[at];
            let values = match (values, &target.defaults[at]) {
                (Some(values), _) => values,
                (None, None) => vec![Value::Null; rows],
                (None, Some(default)) => {
                    let sql =
                        format!("SELECT CAST(({default}) AS {}) FROM range({rows})", field.ty);
                    let result = self.query(&sql)?;
                    (0..result.len()).map(|row| result.value_at(row, 0)).collect()
                }
            };
            filled.push(values);
        }
        self.shared.load(&target.name, &target.fields, &filled, rows).map_err(|error| {
            let error = self.shared.process_error(error);
            if self.shared.aborts(&error) {
                self.shared.abort_block();
            }
            error
        })
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

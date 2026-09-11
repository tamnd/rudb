//! A statement parsed once and run many times, with values for its parameters.

use rudb_bind::Parameters;
use rudb_common::{Error, Result, Value};
use rudb_parse::ast::Ast;
use rudb_parse::parse_ast;

use crate::connection::single;
use crate::database::Shared;
use crate::result::QueryResult;

/// A prepared statement.
///
/// The statement is parsed here and bound at each execution, with the values in hand. That is the
/// opposite of the usual arrangement, where a prepared statement is planned once and the values are
/// pushed into the plan, and it is on purpose for now: an analytical query is planned against the
/// data it reads, so a plan built without knowing that `$1` is `1` or `1000000` is a plan built
/// blind. Parsing is the part that is pure overhead, and that happens once.
///
/// What a parameter can be written as is DuckDB's list: `?` numbered by where it is, `?1` and `$1`
/// numbered by hand, and `$name`. A parameter used twice is one parameter, because it is one value
/// to provide.
///
/// ```
/// use rudb::Database;
/// use rudb_common::Value;
///
/// let db = Database::new();
/// db.execute("CREATE TABLE t (a INTEGER)")?;
/// db.execute("INSERT INTO t VALUES (1), (2), (3)")?;
///
/// let counted = db.prepare("SELECT count(*) FROM t WHERE a > ?")?;
/// assert_eq!(counted.value(&[Value::Integer(1)])?, Value::BigInt(2));
/// assert_eq!(counted.value(&[Value::Integer(2)])?, Value::BigInt(1));
/// # Ok::<(), rudb_common::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct Prepared {
    shared: Shared,
    sql: String,
    ast: Ast,
    names: Vec<String>,
}

impl Prepared {
    /// Parses `sql` and reads the parameters out of it.
    pub(crate) fn new(shared: Shared, sql: &str) -> Result<Self> {
        let ast = parse_ast(sql)?;
        let names = ast.parameters().into_iter().map(str::to_string).collect();
        Ok(Self { shared, sql: sql.to_string(), ast, names })
    }

    /// The statement as it was written.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The parameters the statement uses, once each, in the order they were written.
    ///
    /// The identifier of a positional parameter is its number as a string, so a statement written
    /// with `?` twice has parameters `1` and `2`.
    #[must_use]
    pub fn parameters(&self) -> &[String] {
        &self.names
    }

    /// Runs the statement with values by position, numbered from one.
    ///
    /// # Errors
    ///
    /// If a parameter was given no value, if a value was given for a parameter the statement does
    /// not use, or anything binding and running the statement reports.
    pub fn execute(&self, values: &[Value]) -> Result<QueryResult> {
        self.run(Parameters::positional(values.to_vec()))
    }

    /// Runs the statement with values by name, which is what `$name` wants.
    ///
    /// A name is matched without regard to case, which is what DuckDB does for this and for nothing
    /// else. Positional parameters can be given this way too, under the identifiers `1`, `2` and so
    /// on, since a position is only a name that happens to be a number.
    ///
    /// # Errors
    ///
    /// The same as [`Prepared::execute`].
    pub fn execute_named(&self, values: &[(&str, Value)]) -> Result<QueryResult> {
        let mut parameters = Parameters::new();
        for (name, value) in values {
            parameters.set(*name, value.clone());
        }
        self.run(parameters)
    }

    /// Runs the statement with values by position and returns the single value it produced.
    ///
    /// # Errors
    ///
    /// Everything [`Prepared::execute`] can raise, plus an error if the result is not one row of one
    /// column.
    pub fn value(&self, values: &[Value]) -> Result<Value> {
        single(&self.execute(values)?)
    }

    /// Checks the values against the statement and runs it.
    fn run(&self, parameters: Parameters) -> Result<QueryResult> {
        self.check(&parameters)?;
        self.shared.execute_ast(&self.ast, &parameters)
    }

    /// Both halves of the mismatch, in DuckDB's words.
    ///
    /// Missing first, because that is the order duckdb v1.4.1 reports them in when both are true:
    /// running a statement written with `$a` and `$b` by position says the values for `a` and `b`
    /// were not provided rather than that the values for `1` and `2` were not wanted.
    fn check(&self, parameters: &Parameters) -> Result<()> {
        let missing: Vec<&str> = self
            .names
            .iter()
            .filter(|name| parameters.get(name).is_none())
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(Error::invalid_input(format!(
                "Values were not provided for the following prepared statement parameters: {}",
                missing.join(", ")
            )));
        }
        let excess: Vec<&str> = parameters
            .names()
            .filter(|name| !self.names.iter().any(|held| held.eq_ignore_ascii_case(name)))
            .collect();
        if !excess.is_empty() {
            return Err(Error::invalid_input(format!(
                "Parameter argument/count mismatch, identifiers of the excess parameters: {}",
                excess.join(", ")
            )));
        }
        Ok(())
    }
}

//! A statement parsed once and run many times, with values for its parameters.

use std::sync::{Mutex, PoisonError};

use rudb_bind::Parameters;
use rudb_catalog::QualifiedName;
use rudb_common::{Error, Result, Value};
use rudb_parse::ast::{self, Ast};
use rudb_parse::parse_ast_with_case;

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
    direct: Option<Direct>,
}

/// An `INSERT INTO t [(columns)] VALUES (row)` whose items are parameters or `NULL`, with nothing
/// after the row: no `RETURNING`, no `ON CONFLICT`.
///
/// This is the trickle insert, one row per statement, and binding it builds a plan of a projection
/// over a one row `VALUES` only for the executor to walk it back down to the row. So the shape is
/// read once here, and an execution that finds a plain table under the name puts the row straight
/// in. Anything the shape does not settle by itself, a constraint, a default or a value that needs
/// more than a widening to fit its column, goes the long way, so the errors and the answers are
/// the ones the plan gives.
#[derive(Debug, Clone)]
pub(crate) struct Direct {
    /// The table's name, as it was written.
    pub(crate) name: Vec<String>,
    /// The column list, empty when the statement did not write one.
    pub(crate) columns: Vec<String>,
    /// The row, one item for each column it names.
    pub(crate) items: Vec<Item>,
    /// What the last execution worked out about the table, kept while the catalog stays as it was.
    pub(crate) found: Found,
}

/// The table a [`Direct`] insert found and where its items land, with the catalog generation it
/// was found at.
///
/// Resolving the name and matching the column list cost more than putting the row in, and neither
/// changes until the catalog does. The insert itself moves the generation on, so it is the
/// generation after the insert that is kept.
#[derive(Debug, Default)]
pub(crate) struct Found(Mutex<Option<(u64, QualifiedName, Vec<usize>)>>);

impl Found {
    /// The name and targets found at `generation`, taken out so the caller can hand them back.
    pub(crate) fn take(&self, generation: u64) -> Option<(QualifiedName, Vec<usize>)> {
        let mut found = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match found.take() {
            Some((at, name, targets)) if at == generation => Some((name, targets)),
            _ => None,
        }
    }

    /// Keeps `name` and `targets` as what the catalog holds at `generation`.
    pub(crate) fn keep(&self, generation: u64, name: QualifiedName, targets: Vec<usize>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some((generation, name, targets));
    }
}

impl Clone for Found {
    /// A copy starts over, since nothing it would keep is worth sharing.
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// One item of a [`Direct`] row.
#[derive(Debug, Clone)]
pub(crate) enum Item {
    /// A parameter, by its identifier.
    Parameter(String),
    /// A `NULL` written into the statement.
    Null,
}

impl Direct {
    /// The shape of `ast`, if it is one statement of it.
    fn of(ast: &Ast) -> Option<Self> {
        let [ast::Statement::Insert(at)] = ast.statements.as_slice() else { return None };
        let insert = ast.insert(*at);
        if insert.returning.is_some()
            || insert.conflict.is_some()
            || insert.copy
            || insert.source == rudb_parse::NONE
        {
            return None;
        }
        let query = ast.query(insert.source);
        let ast::QueryBody::Values(rows) = query.body else { return None };
        if query != ast::Query::bare(query.body) {
            return None;
        }
        let [row] = ast.rows(rows) else { return None };
        let items = ast
            .expr_list(*row)
            .iter()
            .map(|&expr| match ast.expr(expr) {
                ast::Expr::Parameter { name } => Some(Item::Parameter(ast.string(name).to_owned())),
                ast::Expr::Literal { kind: ast::LiteralKind::Null, .. } => Some(Item::Null),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            name: ast.name(insert.name).map(str::to_owned).collect(),
            columns: ast.name(insert.columns).map(str::to_owned).collect(),
            items,
            found: Found::default(),
        })
    }
}

impl Prepared {
    /// Parses `sql` and reads the parameters out of it.
    pub(crate) fn new(shared: Shared, sql: &str) -> Result<Self> {
        let session = shared.session();
        let ast = parse_ast_with_case(sql, session.semantics().identifier_case())?;
        let names = ast.parameters().into_iter().map(str::to_string).collect();
        let direct = Direct::of(&ast);
        Ok(Self { shared, sql: sql.to_string(), ast, names, direct })
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
        let result = self.check(&parameters).and_then(|()| {
            if let Some(direct) = &self.direct
                && let Some(done) = self.shared.insert_direct(direct, &parameters, &self.sql)
            {
                return done;
            }
            // Zero for the parse, because this statement was parsed once at `PREPARE` and the
            // whole point of it is that this execution did not parse anything.
            self.shared.execute_ast(&self.ast, &self.sql, &parameters, &self.shared.token(), 0)
        });
        result.map_err(|error| self.shared.process_error(error))
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

#[cfg(test)]
mod tests {
    use rudb_bind::Parameters;
    use rudb_common::Value;

    use super::Direct;
    use crate::Database;

    /// The tests in `tests/direct_insert.rs` check that the short way lands what the plan lands,
    /// which they would also do if the short way were never taken. This checks that it is.
    #[test]
    fn a_plain_table_takes_the_short_way() {
        let db = Database::new();
        db.execute("CREATE TABLE t (id BIGINT, name VARCHAR, price DOUBLE, qty INTEGER)")
            .expect("creates");
        let prepared = db.prepare("INSERT INTO t VALUES (?, ?, ?, NULL)").expect("prepares");
        let direct = prepared.direct.as_ref().expect("the shape is recognised");
        let values = vec![Value::BigInt(1), Value::Varchar("a".into()), Value::Integer(2)];
        let taken =
            prepared.shared.insert_direct(direct, &Parameters::positional(values), prepared.sql());
        assert_eq!(taken.expect("taken").expect("runs").value_at(0, 0), Value::BigInt(1));
        assert_eq!(db.table_len("t").expect("counts"), 1);

        for sql in [
            "INSERT INTO t VALUES (?, ?, ?, ?) RETURNING id",
            "INSERT INTO t SELECT ?, ?, ?, ?",
            "INSERT INTO t VALUES (?, ?, ?, 1 + ?)",
            "INSERT INTO t VALUES (?, ?, ?, ?), (?, ?, ?, ?)",
        ] {
            assert!(db.prepare(sql).expect("prepares").direct.is_none(), "{sql}");
        }
        assert!(Direct::of(&rudb_parse::parse_ast("SELECT ?").expect("parses")).is_none());
    }
}

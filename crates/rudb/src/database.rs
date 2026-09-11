//! The handle everything else hangs off.

use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rudb_bind::{Bound, Parameters};
use rudb_catalog::Catalog;
use rudb_common::{Error, Field, Result, Value};

use rudb_parse::ast::Ast;

use crate::config::Config;
use crate::connection::{Connection, single};
use crate::prepared::Prepared;
use crate::result::QueryResult;

/// The name that means no file, which is DuckDB's spelling and SQLite's before it.
const MEMORY: &str = ":memory:";

/// An in process database.
///
/// One catalog, held in memory, with no file behind it. `ATTACH` and the storage format are E2, and
/// the shape of this type does not change when they arrive: a database with a file behind it is a
/// catalog whose tables read from a block manager rather than from a `Vec` of chunks, which is a
/// change under [`rudb_catalog::Table`] and not a change here.
///
/// A handle rather than the thing itself. Cloning one is cheap and gives another handle on the same
/// database, and [`Database::connect`] gives a [`Connection`], which is the same sharing with a
/// name that says what it is for. The catalog is behind a lock, so every method here takes `&self`
/// and a write from one thread is serialized against a read from another rather than refused by the
/// compiler. That is what an embedded database has to do, because the program embedding it is the
/// one that decided how many threads it has.
#[derive(Debug, Clone)]
pub struct Database {
    shared: Shared,
}

/// The state one database is, however many handles are on it.
#[derive(Debug, Clone)]
pub(crate) struct Shared {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    catalog: RwLock<Catalog>,
    config: Config,
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

impl Database {
    /// An empty database with the default catalog and schema, held in memory.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    /// An empty database held in memory, opened with these settings.
    #[must_use]
    pub fn with_config(config: Config) -> Self {
        let inner = Inner { catalog: RwLock::new(Catalog::new()), config };
        Self { shared: Shared { inner: Arc::new(inner) } }
    }

    /// What this database was opened with.
    ///
    /// Read only, because the settings are set once at open time. See [`Config`] for why that is
    /// narrower than DuckDB on purpose and what it would take to widen it.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.shared.inner.config
    }

    /// Opens a database by name.
    ///
    /// `:memory:` and the empty string are an in memory database, which are DuckDB's two spellings
    /// of it. Anything else names a file, and a file needs a storage format, which is #103. It is an
    /// error here rather than a silent in memory database, because a program that opened a file and
    /// wrote to it would be told nothing until it looked for its data again.
    ///
    /// # Errors
    ///
    /// When the name is a file.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_with(path, Config::default())
    }

    /// Opens a database by name, with these settings.
    ///
    /// # Errors
    ///
    /// When the name is a file.
    pub fn open_with(path: &str, config: Config) -> Result<Self> {
        if path.is_empty() || path == MEMORY {
            return Ok(Self::with_config(config));
        }
        Err(Error::not_implemented(format!(
            "cannot open \"{path}\", because there is no storage format yet, see \
             https://github.com/tamnd/rudb/issues/103"
        )))
    }

    /// A connection to this database.
    #[must_use]
    pub fn connect(&self) -> Connection {
        Connection::new(self.shared.clone())
    }

    /// Parses a statement so it can be run more than once, with values for its parameters.
    ///
    /// The same call as [`Connection::prepare`].
    ///
    /// # Errors
    ///
    /// A parse error. A name that does not resolve or a type that does not work out is an error at
    /// execution rather than here, because a parameter has no type until it has a value.
    pub fn prepare(&self, sql: &str) -> Result<Prepared> {
        Prepared::new(self.shared.clone(), sql)
    }

    /// Reads the catalog.
    ///
    /// A closure rather than a returned reference, because the catalog is behind a lock and a
    /// reference out of it would outlive the guard. The lock is held for the call and no longer.
    pub fn with_catalog<T>(&self, read: impl FnOnce(&Catalog) -> T) -> T {
        read(&self.shared.read())
    }

    /// Writes the catalog.
    ///
    /// Public because a program that builds its own catalog rather than parsing SQL to build one is
    /// a real thing an embedded database gets used for.
    pub fn with_catalog_mut<T>(&self, write: impl FnOnce(&mut Catalog) -> T) -> T {
        write(&mut self.shared.write())
    }

    /// Defines a table.
    ///
    /// The name is `table`, `schema.table` or `catalog.schema.table`, and anything unqualified goes
    /// to the default catalog and schema, which is what an unqualified name in a query resolves
    /// against too.
    ///
    /// # Errors
    ///
    /// If the name has more than three parts, if the catalog or the schema does not exist, if the
    /// table already exists, or if two of the columns have the same name.
    pub fn create_table(&self, name: &str, columns: Vec<Field>) -> Result<()> {
        let parts: Vec<&str> = name.split('.').collect();
        let mut catalog = self.shared.write();
        let resolved = catalog.resolve_for_create(&parts)?;
        catalog.create_table(resolved, columns)
    }

    /// Drops a table.
    ///
    /// # Errors
    ///
    /// If the name does not resolve or the table does not exist.
    pub fn drop_table(&self, name: &str) -> Result<()> {
        let parts: Vec<&str> = name.split('.').collect();
        let mut catalog = self.shared.write();
        let resolved = catalog.resolve(&parts)?;
        catalog.drop_table(&resolved)
    }

    /// Appends rows to a table, each row left to right in the table's column order.
    ///
    /// The row shaped write path, because the caller with rows in hand is the common case and the
    /// caller with columns in hand can reach [`Database::with_catalog_mut`] and append a
    /// [`rudb_vector::Chunk`] directly. Values are converted to the column's type on the way in, so
    /// an `Integer` lands in a `BIGINT` column.
    ///
    /// # Errors
    ///
    /// If the name does not resolve, if a row is not as wide as the table, or if a value cannot be
    /// converted to its column's type.
    pub fn append(&self, name: &str, rows: &[Vec<Value>]) -> Result<()> {
        let parts: Vec<&str> = name.split('.').collect();
        let mut catalog = self.shared.write();
        let resolved = catalog.resolve(&parts)?;
        catalog.table_mut(&resolved)?.append_rows(rows)
    }

    /// How many rows a table holds.
    ///
    /// # Errors
    ///
    /// If the name does not resolve or the table does not exist.
    pub fn table_len(&self, name: &str) -> Result<usize> {
        let parts: Vec<&str> = name.split('.').collect();
        let catalog = self.shared.read();
        let resolved = catalog.resolve(&parts)?;
        Ok(catalog.table(&resolved)?.rows().len())
    }

    /// Every table in the database, unqualified, in creation order.
    ///
    /// Unqualified because that is what a person typing `.tables` wants to read and what they would
    /// then type into a query. Two tables of the same name in different schemas both appear, which
    /// is the same thing DuckDB's `.tables` does.
    #[must_use]
    pub fn table_names(&self) -> Vec<String> {
        self.shared.read().tables().map(|table| table.name().table.clone()).collect()
    }

    /// The `CREATE TABLE` that would define a table as it stands.
    ///
    /// Built from the catalog rather than remembered from the statement that made it, so a table
    /// defined by [`Database::create_table`] describes itself as well as one defined by SQL. It
    /// carries the column names, the types and `NOT NULL`, and nothing else, because nothing else
    /// is in the catalog yet. Defaults, primary keys and check constraints appear here the day the
    /// catalog holds them.
    ///
    /// # Errors
    ///
    /// If the name does not resolve or the table does not exist.
    pub fn table_sql(&self, name: &str) -> Result<String> {
        let parts: Vec<&str> = name.split('.').collect();
        let catalog = self.shared.read();
        let resolved = catalog.resolve(&parts)?;
        let table = catalog.table(&resolved)?;
        let columns: Vec<String> = table
            .columns()
            .iter()
            .map(|field| {
                let null = if field.not_null { " NOT NULL" } else { "" };
                format!("{} {}{null}", field.name, field.ty)
            })
            .collect();
        Ok(format!("CREATE TABLE {}({});", resolved.table, columns.join(", ")))
    }

    /// Runs one query and returns every row it produced.
    ///
    /// The same call as [`Connection::query`], for a program that has one database and no reason to
    /// name a connection.
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
    /// # Errors
    ///
    /// A parse error, a binder error, a catalog error, or anything the operators raise.
    pub fn execute(&self, sql: &str) -> Result<QueryResult> {
        self.shared.execute(sql)
    }

    /// The plan for a query, in the textual form `spec/07-execution.md` describes, without running
    /// it.
    ///
    /// This is `EXPLAIN` before there is an `EXPLAIN`, and it is what the plan tests and the
    /// optimizer work read. The text round trips: `rudb_plan::Plan::parse` of this string gives
    /// back the plan it was printed from.
    ///
    /// # Errors
    ///
    /// A parse error or a binder error.
    pub fn plan(&self, sql: &str) -> Result<String> {
        self.shared.plan(sql)
    }

    /// Runs a query and returns the single value it produced.
    ///
    /// A convenience for `SELECT count(*) FROM t` and the rest of the one cell queries, which are
    /// most of what a program embedded in something else asks.
    ///
    /// # Errors
    ///
    /// Everything [`Database::query`] can raise, plus an error if the result is not one row of one
    /// column.
    pub fn value(&self, sql: &str) -> Result<Value> {
        single(&self.query(sql)?)
    }
}

impl Shared {
    /// The catalog, for reading.
    ///
    /// A poisoned lock is taken rather than reported. Poisoning says some thread panicked while it
    /// held the lock, and the catalog is a `Vec` of chunks rather than an invariant somebody was
    /// halfway through breaking, so refusing every later query would turn one panicked query into a
    /// dead database.
    fn read(&self) -> RwLockReadGuard<'_, Catalog> {
        self.inner.catalog.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The catalog, for writing.
    fn write(&self) -> RwLockWriteGuard<'_, Catalog> {
        self.inner.catalog.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs one query and returns every row it produced.
    pub(crate) fn query(&self, sql: &str) -> Result<QueryResult> {
        let catalog = self.read();
        let plan = planned(sql, &catalog)?;
        run(&plan, &catalog)
    }

    /// The plan a query runs.
    pub(crate) fn plan(&self, sql: &str) -> Result<String> {
        Ok(planned(sql, &self.read())?.to_string())
    }

    /// Runs one statement, which may change the database.
    ///
    /// The write lock is taken for the whole statement rather than for the part that writes,
    /// because the part that writes is decided by what the part that reads produced. `INSERT INTO t
    /// SELECT * FROM t` would otherwise read the table under a read lock, let go, and append to
    /// whatever the table had become in between.
    pub(crate) fn execute(&self, sql: &str) -> Result<QueryResult> {
        let ast = rudb_parse::parse_ast(sql)?;
        self.execute_ast(&ast, &Parameters::new())
    }

    /// Runs one parsed statement, with values for its parameters.
    ///
    /// The prepared statement path, and the path an ordinary statement takes once it is parsed, so
    /// that there is one description of what running a statement does.
    pub(crate) fn execute_ast(&self, ast: &Ast, parameters: &Parameters) -> Result<QueryResult> {
        let mut catalog = self.write();
        match rudb_bind::bind_statement_with(ast, &catalog, parameters)? {
            Bound::Query(mut plan) => {
                rudb_opt::optimize(&mut plan)?;
                run(&plan, &catalog)
            }
            Bound::CreateTable(create) => {
                create_table(create, &mut catalog)?;
                Ok(QueryResult::empty())
            }
            Bound::DropTable(drop) => {
                for name in &drop.names {
                    catalog.drop_table(name)?;
                }
                Ok(QueryResult::empty())
            }
            Bound::Insert(mut insert) => {
                // The source runs to completion before anything is appended, which is not an
                // implementation detail. `INSERT INTO t SELECT * FROM t` reads the table it writes,
                // and a version of this that appended chunk by chunk would either read its own
                // output forever or depend on how the scan holds its chunks.
                rudb_opt::optimize(&mut insert.source)?;
                let result = run(&insert.source, &catalog)?;
                let table = catalog.table_mut(&insert.name)?;
                for chunk in result.into_chunks() {
                    table.append(chunk)?;
                }
                Ok(QueryResult::empty())
            }
        }
    }
}

/// A query bound and then optimized, which is the plan that runs.
///
/// Both of the ways in are through here, so that what a plan dump shows is what the query does. A
/// dump of the bound plan and a run of the optimized one would make the dump a description of
/// something nobody executes, which is the one thing a plan dump must not be.
fn planned(sql: &str, catalog: &Catalog) -> Result<rudb_plan::Plan> {
    let mut plan = rudb_bind::bind_sql(sql, catalog)?;
    rudb_opt::optimize(&mut plan)?;
    Ok(plan)
}

/// Builds and drains one plan.
fn run(plan: &rudb_plan::Plan, catalog: &Catalog) -> Result<QueryResult> {
    let mut root = rudb_exec::build(plan, catalog)?;
    let names = root.schema().names();
    let types = root.schema().types();
    let mut chunks = Vec::new();
    while let Some(chunk) = root.next()? {
        if chunk.is_empty() {
            continue;
        }
        chunks.push(chunk.flatten()?);
    }
    Ok(QueryResult::new(names, types, chunks))
}

/// The `CREATE TABLE` half of a statement.
fn create_table(mut create: rudb_bind::CreateTable, catalog: &mut Catalog) -> Result<()> {
    if create.if_not_exists && catalog.table(&create.name).is_ok() {
        return Ok(());
    }
    // The query runs before the old table is dropped, so `CREATE OR REPLACE TABLE t AS SELECT * FROM
    // t` reads the table it is about to replace rather than the empty new one.
    let rows = match &mut create.source {
        Some(plan) => {
            rudb_opt::optimize(plan)?;
            Some(run(plan, catalog)?)
        }
        None => None,
    };
    if create.or_replace && catalog.table(&create.name).is_ok() {
        catalog.drop_table(&create.name)?;
    }
    catalog.create_table(create.name.clone(), create.columns)?;
    if let Some(rows) = rows {
        let table = catalog.table_mut(&create.name)?;
        for chunk in rows.into_chunks() {
            table.append(chunk)?;
        }
    }
    Ok(())
}

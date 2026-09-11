//! The handle everything else hangs off.

use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rudb_bind::{Bound, Parameters};
use rudb_catalog::{Catalog, Entry, View};
use rudb_common::{Cancel, Error, Field, Memory, Result, Value};

use rudb_parse::ast::Ast;

use crate::config::Config;
use crate::connection::{Connection, single};
use crate::prepared::Prepared;
use crate::result::QueryResult;
use crate::settings::Settings;

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
    settings: Settings,
    memory: Memory,
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
        let memory = Memory::new(config.memory_limit());
        let settings = Settings::new(config);
        let inner = Inner { catalog: RwLock::new(Catalog::new()), settings, memory };
        Self { shared: Shared { inner: Arc::new(inner) } }
    }

    /// What this database is running with now.
    ///
    /// By value rather than by reference, because `SET` changes it while the database is open and a
    /// reference into the settings would be a lock held for as long as the caller kept it. A
    /// `Config` is three numbers, so a copy costs nothing worth avoiding.
    #[must_use]
    pub fn config(&self) -> Config {
        self.shared.inner.settings.config()
    }

    /// What this database was opened with, which is what `RESET` puts a setting back to.
    #[must_use]
    pub fn opened_with(&self) -> Config {
        self.shared.inner.settings.defaults()
    }

    /// One setting, by the name `SET` uses for it, in the spelling DuckDB prints.
    ///
    /// The Rust side of reading a setting back. `current_setting()` is the SQL side and it is not
    /// written yet, because a scalar function over engine state is a shape no function in rudb has.
    ///
    /// # Errors
    ///
    /// For a name that is not a setting, with the names there are.
    pub fn setting(&self, name: &str) -> Result<String> {
        self.shared.inner.settings.value(name)
    }

    /// The memory budget every query against this database is held to.
    ///
    /// One budget for the database rather than one per query, which is what
    /// [`Config::memory_limit`] means: two queries running at once share the limit rather than
    /// getting one each. Public because [`rudb_common::Memory::used`] is the only way to see what
    /// is being held, and a program that sets a limit wants to know how close it is.
    #[must_use]
    pub fn memory(&self) -> &Memory {
        &self.shared.inner.memory
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
    /// The query timeout in [`Database::config`] applies, and nothing can interrupt it, because an
    /// interrupt needs somebody holding the other end of a token and a bare database hands out no
    /// token. [`Connection::interrupt`] is that other end.
    ///
    /// # Errors
    ///
    /// A parse error, a binder error, or anything the operators raise while running, which is
    /// mostly cast failures and arithmetic that leaves the range of its type.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        self.shared.query(sql, &self.shared.token())
    }

    /// Runs one statement, which may change the database.
    ///
    /// # Errors
    ///
    /// A parse error, a binder error, a catalog error, or anything the operators raise.
    pub fn execute(&self, sql: &str) -> Result<QueryResult> {
        self.shared.execute(sql, &self.shared.token())
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
    pub(crate) fn query(&self, sql: &str, cancel: &Cancel) -> Result<QueryResult> {
        let catalog = self.read();
        let plan = planned(sql, &catalog, &self.optimizer()?)?;
        run(&plan, &catalog, cancel, &self.inner.memory)
    }

    /// The passes this database's queries run, as `SET disabled_optimizers` has left them.
    ///
    /// Rebuilt for each statement rather than held, because the statement before this one may have
    /// been the `SET`. It cannot fail: the names were checked when they were set, and the `?` is
    /// here because nothing stops a later version from having a pass that goes away.
    fn optimizer(&self) -> Result<rudb_opt::pass::Context> {
        rudb_opt::pass::Context::without(&self.inner.settings.disabled_optimizers())
    }

    /// The query timeout this database was opened with.
    pub(crate) fn timeout(&self) -> Option<std::time::Duration> {
        self.inner.settings.config().query_timeout()
    }

    /// The token a statement of this database's runs under, when nobody holds one of their own.
    ///
    /// It carries the configured query timeout and nothing can interrupt it, because there is
    /// nobody holding the other half. [`Connection`] is where the other half lives.
    pub(crate) fn token(&self) -> Cancel {
        match self.inner.settings.config().query_timeout() {
            Some(timeout) => Cancel::after(timeout),
            None => Cancel::new(),
        }
    }

    /// The plan a query runs.
    pub(crate) fn plan(&self, sql: &str) -> Result<String> {
        Ok(planned(sql, &self.read(), &self.optimizer()?)?.to_string())
    }

    /// Runs one statement, which may change the database.
    ///
    /// The write lock is taken for the whole statement rather than for the part that writes,
    /// because the part that writes is decided by what the part that reads produced. `INSERT INTO t
    /// SELECT * FROM t` would otherwise read the table under a read lock, let go, and append to
    /// whatever the table had become in between.
    pub(crate) fn execute(&self, sql: &str, cancel: &Cancel) -> Result<QueryResult> {
        let ast = rudb_parse::parse_ast(sql)?;
        self.execute_ast(&ast, &Parameters::new(), cancel)
    }

    /// Runs one parsed statement, with values for its parameters.
    ///
    /// The prepared statement path, and the path an ordinary statement takes once it is parsed, so
    /// that there is one description of what running a statement does.
    pub(crate) fn execute_ast(
        &self,
        ast: &Ast,
        parameters: &Parameters,
        cancel: &Cancel,
    ) -> Result<QueryResult> {
        let mut catalog = self.write();
        let context = self.optimizer()?;
        match rudb_bind::bind_statement_with(ast, &catalog, parameters)? {
            Bound::Query(mut plan) => {
                rudb_opt::optimize_with(&mut plan, &context)?;
                run(&plan, &catalog, cancel, &self.inner.memory)
            }
            Bound::Setting(setting) => {
                let value = setting.value.as_ref();
                self.inner.settings.apply(
                    &self.inner.memory,
                    &setting.name,
                    setting.scope,
                    value,
                )?;
                Ok(QueryResult::empty())
            }
            Bound::CreateTable(create) => {
                create_table(create, &mut catalog, cancel, &self.inner.memory, &context)?;
                Ok(QueryResult::empty())
            }
            Bound::CreateView(create) => {
                create_view(create, &mut catalog)?;
                Ok(QueryResult::empty())
            }
            Bound::DropTable(drop) => {
                for name in &drop.names {
                    match drop.kind {
                        Entry::Table => catalog.drop_table(name)?,
                        Entry::View => catalog.drop_view(name)?,
                    }
                }
                Ok(QueryResult::empty())
            }
            Bound::Insert(mut insert) => {
                // The source runs to completion before anything is appended, which is not an
                // implementation detail. `INSERT INTO t SELECT * FROM t` reads the table it writes,
                // and a version of this that appended chunk by chunk would either read its own
                // output forever or depend on how the scan holds its chunks.
                rudb_opt::optimize_with(&mut insert.source, &context)?;
                let result = run(&insert.source, &catalog, cancel, &self.inner.memory)?;
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
fn planned(
    sql: &str,
    catalog: &Catalog,
    context: &rudb_opt::pass::Context,
) -> Result<rudb_plan::Plan> {
    let mut plan = rudb_bind::bind_sql(sql, catalog)?;
    rudb_opt::optimize_with(&mut plan, context)?;
    Ok(plan)
}

/// Builds and drains one plan, stopping if the token says to or if it runs out of memory.
///
/// The result is materialized, so it is charged, and the charge is handed to the result and
/// released when the result is dropped. That is what makes a program holding ten results at once
/// count as holding ten results: the limit is on the database and a result outlives the query.
fn run(
    plan: &rudb_plan::Plan,
    catalog: &Catalog,
    cancel: &Cancel,
    memory: &Memory,
) -> Result<QueryResult> {
    let mut root = rudb_exec::build_with(plan, catalog, cancel, memory)?;
    let names = root.schema().names();
    let types = root.schema().types();
    let mut held = memory.reservation();
    let mut chunks = Vec::new();
    while let Some(chunk) = root.next()? {
        if chunk.is_empty() {
            continue;
        }
        let chunk = chunk.flatten()?;
        held.grow(u64::try_from(chunk.footprint()).unwrap_or(u64::MAX))?;
        chunks.push(chunk);
    }
    Ok(QueryResult::new(names, types, chunks, held))
}

/// The `CREATE VIEW` half of a statement.
///
/// There is nothing to run. The body was bound by the binder to check that it can be, and what is
/// kept is the text, so this is the two modifiers and a catalog call.
fn create_view(create: rudb_bind::CreateView, catalog: &mut Catalog) -> Result<()> {
    if create.if_not_exists && catalog.entry(&create.name).is_ok() {
        return Ok(());
    }
    if create.or_replace && catalog.view(&create.name).is_ok() {
        catalog.drop_view(&create.name)?;
    }
    catalog.create_view(View::new(create.name, create.sql, create.aliases))
}

/// The `CREATE TABLE` half of a statement.
fn create_table(
    mut create: rudb_bind::CreateTable,
    catalog: &mut Catalog,
    cancel: &Cancel,
    memory: &Memory,
    context: &rudb_opt::pass::Context,
) -> Result<()> {
    if create.if_not_exists && catalog.table(&create.name).is_ok() {
        return Ok(());
    }
    // The query runs before the old table is dropped, so `CREATE OR REPLACE TABLE t AS SELECT * FROM
    // t` reads the table it is about to replace rather than the empty new one.
    let rows = match &mut create.source {
        Some(plan) => {
            rudb_opt::optimize_with(plan, context)?;
            Some(run(plan, catalog, cancel, memory)?)
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

//! The handle everything else hangs off.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rudb_bind::{Bound, Parameters};
use rudb_catalog::{Catalog, Entry, View};
use rudb_common::{Cancel, Error, Field, LogicalType, Memory, Result, Session, Value};
use rudb_metrics::{Document, Report, Span};

use rudb_parse::ast::Ast;
use rudb_pipeline::{Lease, Morsel, Pool, Progress, Sink, keep_pages};
use rudb_vector::{Chunk, Form, Vector};

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
    path: Option<PathBuf>,
    settings: Settings,
    memory: Memory,
    pool: Pool,
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

/// The two things a database sets up before it will run anything, neither of which is per query.
///
/// The threads are the obvious one. The other is the system allocator, which by default hands every
/// large block back to the kernel the moment a query is done with it and then faults the same pages
/// in again on the next one, and which is asked here to stop. Both of them are process wide or
/// database wide rather than query wide, both of them are cheap to set and expensive to find out
/// about later, and opening a database is the one place that knows a query is coming.
fn runtime(config: &Config) -> Pool {
    keep_pages();
    Pool::new(config.threads())
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
        let pool = runtime(&config);
        let settings = Settings::new(config);
        let inner =
            Inner { catalog: RwLock::new(Catalog::new()), path: None, settings, memory, pool };
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
    /// The Rust side of reading a setting back. `SELECT current_setting('threads')` is the SQL side
    /// and it answers with the same text, typed as whatever the setting holds.
    ///
    /// # Errors
    ///
    /// For a name that is not a setting, with the names there are.
    pub fn setting(&self, name: &str) -> Result<String> {
        self.shared.inner.settings.value(name)
    }

    /// Which implementation runs at each seam, as this session has left it.
    ///
    /// The session half of the three surfaces. The other two reach the same place: a process flag
    /// is a `SET` the shell runs before anything else, and a per query hint is this with the
    /// query's own pins laid on top, which is [`Database::seams_for`].
    #[must_use]
    pub fn seams(&self) -> rudb_seam::Settings {
        self.shared.inner.settings.seams()
    }

    /// The seam settings one query runs under, which is [`Database::seams`] plus its hints.
    ///
    /// `SELECT /*+ hash.table(unchained) */ ...` pins a seam for one statement and leaves the
    /// session alone, which is what a researcher comparing two implementations of one thing over a
    /// suite needs, because the alternative is a `SET` before every query and a `RESET` after it
    /// that somebody eventually forgets.
    ///
    /// # Errors
    ///
    /// A parse error, and everything a hint naming a seam nobody has raises.
    pub fn seams_for(&self, sql: &str) -> Result<rudb_seam::Settings> {
        self.shared.seams(sql)
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
        let path = PathBuf::from(path);
        let mut catalog = Catalog::new();
        if path.exists() {
            catalog.create_native_table(rudb_native::Reader::open(&path)?)?;
        }
        let memory = Memory::new(config.memory_limit());
        let pool = runtime(&config);
        let settings = Settings::new(config);
        let inner =
            Inner { catalog: RwLock::new(catalog), path: Some(path), settings, memory, pool };
        Ok(Self { shared: Shared { inner: Arc::new(inner) } })
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
        Prepared::new(self.shared.clone(), sql).map_err(|error| self.shared.process_error(error))
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
        self.shared
            .query(sql, &self.shared.token())
            .map_err(|error| self.shared.process_error(error))
    }

    /// Runs one statement, which may change the database.
    ///
    /// # Errors
    ///
    /// A parse error, a binder error, a catalog error, or anything the operators raise.
    pub fn execute(&self, sql: &str) -> Result<QueryResult> {
        self.shared
            .execute(sql, &self.shared.token())
            .map_err(|error| self.shared.process_error(error))
    }

    /// The plan for a query, in the textual form `spec/07-execution.md` describes, without running
    /// it.
    ///
    /// The same plan `EXPLAIN` prints, without the estimates and as a `String` rather than a result
    /// set, which is what the plan tests and the optimizer work read. The text round trips:
    /// `rudb_plan::Plan::parse` of this string gives back the plan it was printed from, and that is
    /// why the estimates are not on it.
    ///
    /// # Errors
    ///
    /// A parse error or a binder error.
    pub fn plan(&self, sql: &str) -> Result<String> {
        self.shared.plan(sql).map_err(|error| self.shared.process_error(error))
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

/// Writes the one-table catalog as a complete native snapshot and publishes it by rename.
fn persist(path: &Path, catalog: &Catalog) -> Result<()> {
    let mut tables = catalog.tables();
    let table =
        tables.next().ok_or_else(|| Error::not_implemented("a native database with no table"))?;
    if tables.next().is_some() {
        return Err(Error::not_implemented("more than one table in a native database file"));
    }
    if table.rows().is_native() {
        return Ok(());
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if temporary.exists() {
        std::fs::remove_file(&temporary).map_err(|error| Error::io(error.to_string()))?;
    }
    let mut writer = rudb_native::Writer::create(
        &temporary,
        table.name().table.clone(),
        table.columns().to_vec(),
    )?;
    for at in 0..table.rows().chunk_count() {
        let chunk = table.rows().chunk(at).ok_or_else(|| {
            Error::not_implemented("checkpointing a table already backed by a native file")
        })?;
        writer.append(chunk)?;
    }
    writer.finish()?;
    std::fs::rename(&temporary, path).map_err(|error| Error::io(error.to_string()))?;
    Ok(())
}

/// One pipeline instance's place in the source, and the run of chunks it is holding.
///
/// The run is what makes the sink safe to instance. A stripe has to be a contiguous run of the
/// source in order, and the writer cannot work out which of several interleaved callers a chunk
/// belongs to, so each instance groups its own and hands over whole stripes.
#[derive(Debug, Default)]
struct NativePlace {
    morsel: u64,
    chunk: u64,
    held: Vec<((u64, u64), Chunk)>,
}

/// The root of a file-backed initial insert.
#[derive(Debug)]
struct NativeSink {
    writer: Mutex<Option<rudb_native::Writer>>,
    temporary: PathBuf,
    target: PathBuf,
    table: String,
    fields: Vec<Field>,
}

impl NativeSink {
    fn create(target: &Path, name: String, fields: Vec<Field>) -> Result<Self> {
        let temporary = target.with_extension(format!("{}.tmp", std::process::id()));
        if temporary.exists() {
            std::fs::remove_file(&temporary).map_err(|error| Error::io(error.to_string()))?;
        }
        let writer = rudb_native::Writer::create(&temporary, name.clone(), fields.clone())?;
        Ok(Self {
            writer: Mutex::new(Some(writer)),
            temporary,
            target: target.to_path_buf(),
            table: name,
            fields,
        })
    }

    /// Hands whatever this instance is holding to the writer as one stripe.
    fn hand_over(&self, place: &mut NativePlace) -> Result<()> {
        if place.held.is_empty() {
            return Ok(());
        }
        let parts = std::mem::take(&mut place.held);
        let mut writer =
            self.writer.lock().map_err(|_| Error::internal("native writer panicked"))?;
        writer
            .as_mut()
            .ok_or_else(|| Error::internal("native writer was already committed"))?
            .append_stripe(parts)
    }
}

impl Sink for NativeSink {
    type Local = NativePlace;

    fn parallel(&self) -> bool {
        // The writer is one file behind one lock, so the encode and the write of a stripe still
        // happen one at a time. What more than one instance buys is the read: the source is a
        // Parquet scan and decoding a row group is the single largest thing this pipeline does on
        // one thread. Saying yes here lets the instances that are not holding the lock decode the
        // next row groups while the one that is encodes the last stripe.
        true
    }

    fn local(&self) -> Self::Local {
        NativePlace::default()
    }

    fn at(&self, morsel: &Morsel, place: &mut Self::Local) -> Result<()> {
        // A stripe never spans two morsels, so that its parts are a run of the source with nothing
        // from another instance in the middle of them. The cost is a short stripe at the end of
        // each morsel, and a morsel on ClickBench is a whole row group of about a million rows.
        self.hand_over(place)?;
        place.morsel = morsel.index();
        place.chunk = 0;
        Ok(())
    }

    fn sink(&self, chunk: &Chunk, place: &mut Self::Local) -> Result<Progress> {
        for (at, field) in self.fields.iter().enumerate().filter(|(_, field)| field.not_null) {
            let vector = chunk.column(at)?;
            let null = match vector.form() {
                Form::Dictionary | Form::Rle => (0..vector.len()).any(|row| vector.is_null_at(row)),
                _ => vector.validity().has_nulls(vector.len()),
            };
            if null {
                return Err(Error::constraint(format!(
                    "NOT NULL constraint failed: {}.{}",
                    self.table, field.name
                )));
            }
        }
        place.held.push(((place.morsel, place.chunk), chunk.clone()));
        place.chunk = place.chunk.saturating_add(1);
        if place.held.len() == rudb_native::STRIPE_PARTS {
            self.hand_over(place)?;
        }
        Ok(Progress::More)
    }

    fn combine(&self, mut local: Self::Local) -> Result<()> {
        self.hand_over(&mut local)
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        let writer = self
            .writer
            .lock()
            .map_err(|_| Error::internal("native writer panicked"))?
            .take()
            .ok_or_else(|| Error::internal("native writer was already committed"))?;
        writer.finish()?;
        std::fs::rename(&self.temporary, &self.target).map_err(|error| Error::io(error.to_string()))
    }
}

impl Shared {
    /// Applies the session's public error rendering mode at the API boundary.
    pub(crate) fn process_error(&self, error: Error) -> Error {
        if self.session().semantics().errors_as_json() { error.into_json() } else { error }
    }

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

    /// The memory and the threads this database will lend a query.
    fn budget(&self) -> Budget<'_> {
        Budget { memory: &self.inner.memory, pool: &self.inner.pool }
    }

    /// What the settings are now, read once and handed to both the binder and the executor.
    ///
    /// It used to be read only for a plan that turned out to mention `duckdb_settings()`, on the
    /// argument that no other query looks at a setting. `current_setting()` is the other one that
    /// does and the binder folds it, so the values have to be in hand before there is a plan to
    /// look at, and the check that would say whether a statement needs them is a walk of the parse
    /// tree that costs about what reading them costs. So it is read once per statement and the
    /// special case is gone. [`crate::settings::Settings::session`] takes two locks for it.
    pub(crate) fn session(&self) -> Session {
        self.inner.settings.session()
    }

    /// Runs one query and returns every row it produced.
    ///
    /// `EXPLAIN` comes through here as well as through [`Shared::execute`], because it answers with
    /// rows and this is the path that reads rows back. It takes the read lock like any other query,
    /// since printing a plan changes nothing. A statement that writes is refused here rather than
    /// run under a read lock.
    pub(crate) fn query(&self, sql: &str, cancel: &Cancel) -> Result<QueryResult> {
        let catalog = self.read();
        let seams = self.seams(sql)?;
        let context = self.optimizer(&catalog)?;
        let session = self.session();
        let (ast, parse_ns) =
            timed(|| rudb_parse::parse_ast_with_case(sql, session.semantics().identifier_case()))?;
        let (bound, bind_ns) =
            timed(|| rudb_bind::bind_statement_with(&ast, &catalog, &Parameters::new(), &session))?;
        match bound {
            Bound::Query(mut plan) => {
                let ((), optimize_ns) = timed(|| rudb_opt::optimize_with(&mut plan, &context))?;
                let budget = self.budget();
                let under =
                    Under::new(budget, context.statistics(), &seams, &session, Rows::ForACaller)
                        .after(Planning { parse_ns, bind_ns, optimize_ns });
                run(sql, &plan, &catalog, cancel, under)
            }
            Bound::Explain { mut plan, analyze } => {
                let ((), optimize_ns) = timed(|| rudb_opt::optimize_with(&mut plan, &context))?;
                let seams = rudb_opt::explain::Seams::new(&seams, rudb_exec::registries());
                explaining(
                    &plan,
                    &catalog,
                    cancel,
                    self.budget(),
                    &context,
                    seams,
                    &session,
                    analyze,
                    sql,
                    Planning { parse_ns, bind_ns, optimize_ns },
                )
            }
            _ => Err(Error::not_implemented("a statement that is not a query, on the query path")),
        }
    }

    /// The seam settings a statement runs under, which is the session's with its hints on top.
    ///
    /// What the settings choose goes nowhere yet, because no seam has a second implementation to
    /// choose between until F1 and every one of the twenty seven is unregistered. What they do
    /// today is fail a statement whose hint names a seam nobody has, and feed the seam section of
    /// `EXPLAIN`, which is the half of the behaviour worth having before the other half arrives: a
    /// hint that is quietly ignored is a measurement of the wrong thing.
    pub(crate) fn seams(&self, sql: &str) -> Result<rudb_seam::Settings> {
        let mut seams = self.inner.settings.seams();
        for hint in rudb_parse::hints(sql)? {
            seams.hint(hint)?;
        }
        Ok(seams)
    }

    /// The passes this database's queries run, as `SET disabled_optimizers` has left them, with
    /// the row counts the catalog holds.
    ///
    /// Rebuilt for each statement rather than held, because the statement before this one may have
    /// been the `SET` and the statement before that may have been an `INSERT`. It cannot fail: the
    /// names were checked when they were set, and the `?` is here because nothing stops a later
    /// version from having a pass that goes away.
    ///
    /// The catalog comes in as an argument rather than being read from the lock here, because
    /// every caller is already holding that lock and one of them is holding it for writing. This
    /// is also the seam that stops the optimizer from reaching the catalog on its own: what it
    /// gets is a copy of the counts, which is the whole of what estimation reads today.
    fn optimizer(&self, catalog: &Catalog) -> Result<rudb_opt::pass::Context> {
        let mut context =
            rudb_opt::pass::Context::without(&self.inner.settings.disabled_optimizers())?;
        let mut statistics = rudb_opt::estimate::Statistics::new();
        for table in catalog.tables() {
            let name = table.name();
            let rows = u64::try_from(table.rows().len()).unwrap_or(u64::MAX);
            statistics.record(&name.catalog, &name.schema, &name.table, rows);
            for (at, column) in table.columns().iter().enumerate() {
                // A table that cannot answer leaves the column out, which is every in memory table
                // and every column of a native one that has no dictionary. The estimate falls back
                // to the shape it used before there were any of these.
                let Ok(Some(distinct)) = table.rows().distinct_values(at) else {
                    continue;
                };
                statistics.record_distinct(
                    &name.catalog,
                    &name.schema,
                    &name.table,
                    &column.name,
                    distinct,
                );
            }
        }
        context.measure(statistics);
        Ok(context)
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
        let catalog = self.read();
        let context = self.optimizer(&catalog)?;
        Ok(planned(sql, &catalog, &context, &self.session())?.to_string())
    }

    /// Runs one statement, which may change the database.
    ///
    /// The write lock is taken for the whole statement rather than for the part that writes,
    /// because the part that writes is decided by what the part that reads produced. `INSERT INTO t
    /// SELECT * FROM t` would otherwise read the table under a read lock, let go, and append to
    /// whatever the table had become in between.
    pub(crate) fn execute(&self, sql: &str, cancel: &Cancel) -> Result<QueryResult> {
        let session = self.session();
        let (ast, parse_ns) =
            timed(|| rudb_parse::parse_ast_with_case(sql, session.semantics().identifier_case()))?;
        self.execute_ast(&ast, sql, &Parameters::new(), cancel, parse_ns)
    }

    /// Runs one parsed statement, with values for its parameters.
    ///
    /// The prepared statement path, and the path an ordinary statement takes once it is parsed, so
    /// that there is one description of what running a statement does.
    ///
    /// `parse_ns` is how long the caller spent getting the AST, because this function is below the
    /// parse and the metrics document is below this. A prepared statement passes zero, which is not
    /// a missing measurement: the parse happened once at `PREPARE` and charging it again to every
    /// execution would make a statement prepared once and run a thousand times report the same
    /// parse a thousand times.
    pub(crate) fn execute_ast(
        &self,
        ast: &Ast,
        sql: &str,
        parameters: &Parameters,
        cancel: &Cancel,
        parse_ns: u64,
    ) -> Result<QueryResult> {
        let seams = self.seams(sql)?;
        let mut catalog = self.write();
        let context = self.optimizer(&catalog)?;
        let session = self.session();
        let (bound, bind_ns) =
            timed(|| rudb_bind::bind_statement_with(ast, &catalog, parameters, &session))?;
        match bound {
            Bound::Query(mut plan) => {
                let ((), optimize_ns) = timed(|| rudb_opt::optimize_with(&mut plan, &context))?;
                let budget = self.budget();
                let under =
                    Under::new(budget, context.statistics(), &seams, &session, Rows::ForACaller)
                        .after(Planning { parse_ns, bind_ns, optimize_ns });
                run(sql, &plan, &catalog, cancel, under)
            }
            Bound::Explain { mut plan, analyze } => {
                let ((), optimize_ns) = timed(|| rudb_opt::optimize_with(&mut plan, &context))?;
                let seams = rudb_opt::explain::Seams::new(&seams, rudb_exec::registries());
                explaining(
                    &plan,
                    &catalog,
                    cancel,
                    self.budget(),
                    &context,
                    seams,
                    &session,
                    analyze,
                    sql,
                    Planning { parse_ns, bind_ns, optimize_ns },
                )
            }
            Bound::Setting(setting) if setting.pragma => {
                self.inner.settings.toggle(&setting.name)?;
                Ok(QueryResult::empty())
            }
            Bound::Setting(setting) => {
                let value = setting.value.as_ref();
                self.inner.settings.apply(
                    &self.inner.memory,
                    &self.inner.pool,
                    &setting.name,
                    setting.scope,
                    value,
                )?;
                Ok(QueryResult::empty())
            }
            Bound::Checkpoint => {
                if let Some(path) = &self.inner.path {
                    persist(path, &catalog)?;
                }
                Ok(QueryResult::empty())
            }
            Bound::CreateTable(create) => {
                create_table(
                    sql,
                    create,
                    &mut catalog,
                    cancel,
                    self.budget(),
                    &context,
                    &seams,
                    &session,
                )?;
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
                let ((), optimize_ns) =
                    timed(|| rudb_opt::optimize_with(&mut insert.source, &context))?;
                if let Some(path) = &self.inner.path {
                    let target = catalog.table(&insert.name)?;
                    if target.rows().is_empty() {
                        let sink = Arc::new(NativeSink::create(
                            path,
                            target.name().table.clone(),
                            target.columns().to_vec(),
                        )?);
                        let query = rudb_exec::build_measured_into(
                            &insert.source,
                            &catalog,
                            cancel,
                            &self.inner.memory,
                            &seams,
                            &session,
                            sink,
                        )?;
                        query.run(cancel, &self.inner.pool)?;
                        drop(query);
                        let reader = rudb_native::Reader::open(path)?;
                        catalog.table_mut(&insert.name)?.commit_native(reader)?;
                        return Ok(QueryResult::empty());
                    }
                }
                let statistics = context.statistics();
                let under =
                    Under::new(self.budget(), statistics, &seams, &session, Rows::ForATable)
                        .after(Planning { parse_ns, bind_ns, optimize_ns });
                let result = run(sql, &insert.source, &catalog, cancel, under)?;
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
/// What [`Database::plan`] dumps, and it optimizes rather than stopping at the bound plan because a
/// dump of the bound plan next to a run of the optimized one would make the dump a description of
/// something nobody executes, which is the one thing a plan dump must not be. `EXPLAIN` and the
/// query path do the same two steps in that order for the same reason.
fn planned(
    sql: &str,
    catalog: &Catalog,
    context: &rudb_opt::pass::Context,
    session: &Session,
) -> Result<rudb_plan::Plan> {
    let mut plan = rudb_bind::bind_sql_with(sql, catalog, session)?;
    rudb_opt::optimize_with(&mut plan, context)?;
    Ok(plan)
}

/// Builds and drains one plan, stopping if the token says to or if it runs out of memory.
///
/// The result is materialized, so it is charged, and the charge is handed to the result and
/// released when the result is dropped. That is what makes a program holding ten results at once
/// count as holding ten results: the limit is on the database and a result outlives the query.
///
/// This is also the one place a metrics document is made. Everything in it below the top level
/// comes out of the report the builder filled, and the two spans here are the two things only this
/// function knows: how long the tree took to build and how long it took to drain. Parsing, binding
/// and optimizing happened before this was called, so they are measured up there and arrive in
/// [`Under::planning`], and a caller that does not know says zero rather than guessing.
///
/// A query that fails part way through has a document too, and it is thrown away here, because an
/// error is a [`rudb_common::Error`] and that type is two ranks below the one the document lives
/// in. Carrying it out of a failure is worth doing and it is a change to how an error is reported
/// rather than a change to this function.
/// The two budgets a query draws on, which belong to the database rather than to the query.
///
/// They travel together because they are the same kind of thing. Memory is how much a query may
/// hold and the pool is how many threads it may run on, both are shared with whatever else the
/// database is doing at the same time, and neither is a property of the plan. Passing them as one
/// also keeps the argument lists of the functions below from growing a slot every time a new
/// resource turns up.
#[derive(Clone, Copy)]
struct Budget<'a> {
    memory: &'a Memory,
    pool: &'a Pool,
}

/// Who the rows a query produced are for, which is what decides whether they are flattened.
///
/// A caller outside the engine reads a value at a time and has never heard of a dictionary vector,
/// so a result going to one has every column copied into flat form first. A table has heard of it,
/// because storage holds the same vector forms execution does, so a result going into one keeps
/// whatever form the scan handed up.
///
/// The difference is not small. `CREATE TABLE t AS SELECT * FROM 'hits.parquet'` over a hundred and
/// five columns does its reading on every thread in the pool and then flattens the whole answer on
/// the one thread draining it. The string columns of that file are dictionary encoded, so flattening
/// them is a copy per row per column, single threaded, at the end of a query that was parallel up to
/// that point. Measured against duckdb on a nine row group file, that tail is the difference between
/// getting 1.8 times out of thirty two threads and getting 5.4.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rows {
    /// Going out of the engine, so every column is flattened on the way.
    ForACaller,
    /// Going into a table, so nothing is copied.
    ForATable,
}

/// What a statement spent before there was a plan to build, in nanoseconds.
///
/// Parsing, binding and optimizing all happen on the statement path, above the function that makes
/// the metrics document, so until this existed those three fields were zero in every document ever
/// written and `total_ns` was the physical build plus the run. Planning was the one cost of a query
/// that nothing could see.
///
/// That is the case milestone E1 asks for a column and an assertion about, and it states it as
/// plainly as it can be stated: a query that plans for four hundred milliseconds and runs for two
/// hundred is a query the optimizer made slower, and without a number nobody finds out, because
/// nobody profiles the planner. An optimizer only ever gets added to, every pass costs something to
/// run, and the pass that pays for itself on a scan of ten million rows does not pay for itself on
/// a point lookup.
///
/// Wall clock and not CPU. All three phases are single threaded, so the two are the same number up
/// to scheduling noise, and the wall clock is the one a caller waited.
#[derive(Clone, Copy, Default, Debug)]
struct Planning {
    parse_ns: u64,
    bind_ns: u64,
    optimize_ns: u64,
}

impl Planning {
    /// Everything before the physical build, which is what a budget is asserted against.
    fn total_ns(self) -> u64 {
        self.parse_ns.saturating_add(self.bind_ns).saturating_add(self.optimize_ns)
    }
}

/// Run something and say how long it took, in wall nanoseconds.
///
/// Here so that the three phases are timed the same way rather than three ways, and so that adding
/// a span around a call that already existed does not also re-indent it. A failure is not timed,
/// because there is no document to put the number in and a partial phase is not a phase.
fn timed<T>(what: impl FnOnce() -> Result<T>) -> Result<(T, u64)> {
    let span = Span::start();
    let out = what()?;
    Ok((out, span.stop().0))
}

/// Everything a query runs under that is not the plan, the catalog or the cancel flag.
///
/// The same reasoning as [`Budget`], one level out. These travel together because every caller of
/// `run` has to say all of them and none is a property of the plan, and passing them as one keeps
/// the argument list from growing a slot every time something new turns out to be true of a running
/// query rather than of the query itself.
#[derive(Clone, Copy)]
struct Under<'a> {
    budget: Budget<'a>,
    statistics: &'a rudb_opt::estimate::Statistics,
    seams: &'a rudb_seam::Settings,
    session: &'a Session,
    going: Rows,
    planning: Planning,
}

impl<'a> Under<'a> {
    fn new(
        budget: Budget<'a>,
        statistics: &'a rudb_opt::estimate::Statistics,
        seams: &'a rudb_seam::Settings,
        session: &'a Session,
        going: Rows,
    ) -> Self {
        Self { budget, statistics, seams, session, going, planning: Planning::default() }
    }

    /// What the statement path spent getting to this plan.
    ///
    /// Separate from [`Under::new`] and defaulting to zero, because not every path that runs a plan
    /// knows. A prepared statement parsed at `PREPARE` time and an `EXPLAIN ANALYZE` that is handed
    /// a plan somebody else optimized both run a query whose planning happened somewhere this
    /// cannot see, and a zero there says so. The alternative is a number one of those paths made up
    /// out of the part it did measure, which is the kind of thing a budget is later asserted
    /// against and nobody remembers is partly invented.
    fn after(mut self, planning: Planning) -> Self {
        self.planning = planning;
        self
    }
}

fn run(
    sql: &str,
    plan: &rudb_plan::Plan,
    catalog: &Catalog,
    cancel: &Cancel,
    under: Under<'_>,
) -> Result<QueryResult> {
    let Under { budget: Budget { memory, pool }, statistics, seams, session, going, planning } =
        under;
    // The budget is shared by the database and its high-water mark survives a query. Reset it to
    // what is live now before measuring this execution, otherwise a metrics document either says
    // zero forever (when nobody copies the mark) or inherits the largest earlier query. A caller
    // with concurrent statements cannot attribute the shared budget to one query; this field is a
    // database-level peak in that case. The CLI benchmark path has one statement in flight.
    memory.forget_peak();
    let report = Report::new();
    let building = Span::start();
    let query = rudb_exec::build_measured(plan, catalog, cancel, memory, seams, session, &report)?;
    let (built_wall, built_cpu) = building.stop();
    let names = query.schema().names();
    let types = query.schema().types();
    let mut held = memory.reservation();
    let mut chunks = Vec::new();
    // Every pipeline the query runs is timed against its own driver inside `run`, so what is left
    // for this span to say is how long the whole of the execution took, which is what `execute_ns`
    // is. The loop after it is the one that turns the queued chunks into a result set, and it is
    // inside the span because a caller waiting for rows is waiting for that too.
    let driving = Span::start();
    query.run(cancel, pool)?;
    while let Some(chunk) = query.next_chunk()? {
        if chunk.is_empty() {
            continue;
        }
        let chunk = match going {
            // flatten: this is the top of the query and the chunk is about to become a result set
            // that somebody outside the engine reads. A caller holding a `Result` gets a value at a
            // time, so a dictionary or a constant here would be a form every one of them has to
            // understand to read a row. The decode stops at this line and nothing below it sees a
            // flat column. The other arm is a chunk going into a table, where there is nobody
            // outside the engine to protect: storage holds the same forms execution does, and this
            // loop is the one part of a parallel query that runs on a single thread, so a copy made
            // here is a copy the rest of the pool sits idle through.
            Rows::ForACaller => chunk.flatten()?,
            Rows::ForATable => chunk,
        };
        held.grow(u64::try_from(chunk.footprint()).unwrap_or(u64::MAX))?;
        chunks.push(chunk);
    }
    let (ran_wall, ran_cpu) = driving.stop();
    let mut metrics = Document::new(sql);
    metrics.settings.memory_limit = memory.limit();
    metrics.settings.threads = u32::try_from(pool.threads()).unwrap_or(u32::MAX);
    metrics.timing.parse_ns = planning.parse_ns;
    metrics.timing.bind_ns = planning.bind_ns;
    metrics.timing.optimize_ns = planning.optimize_ns;
    metrics.timing.physical_ns = built_wall;
    metrics.timing.execute_ns = ran_wall;
    // Every phase and not the two this function timed itself. A total that left the planner out was
    // the reason planning time could grow without anything going up, and the harness reads this
    // field as the cost of the statement.
    metrics.timing.total_ns =
        planning.total_ns().saturating_add(built_wall).saturating_add(ran_wall);
    // The span above reads this thread's CPU clock, which is the only clock that says which thread
    // did the work and therefore the one clock that cannot see the workers. The query counted what
    // they burned as they finished, so it goes on here rather than going missing.
    let ran_cpu = ran_cpu.saturating_add(query.worker_cpu_ns());
    metrics.resource.cpu_ns = built_cpu.saturating_add(ran_cpu);
    metrics.resource.build_cpu_ns = built_cpu;
    metrics.resource.peak_bytes = memory.peak();
    report.fill(&mut metrics);
    rudb_opt::explain::record_estimates(plan, statistics, &mut metrics);
    Ok(QueryResult::new(names, types, chunks, held).in_session(session.clone()).measured(metrics))
}

/// The plan `EXPLAIN` prints, run first if `ANALYZE` was asked for.
///
/// `ANALYZE` runs the query and throws the rows away. That is the whole difference between the two,
/// and it is deliberately the only difference: the plan that is printed is the plan that was built
/// and drained, so a number on a line came from the operator on that line rather than from an
/// operator something else would have built.
///
/// The rows are dropped rather than returned because the result set of `EXPLAIN ANALYZE` is the
/// plan. DuckDB does the same and calls the row `analyzed_plan`, and a client that gets a query's
/// rows back from an `EXPLAIN` has no way to tell which it asked for.
#[allow(clippy::too_many_arguments)]
fn explaining(
    plan: &rudb_plan::Plan,
    catalog: &Catalog,
    cancel: &Cancel,
    budget: Budget<'_>,
    context: &rudb_opt::pass::Context,
    seams: rudb_opt::explain::Seams<'_>,
    session: &Session,
    analyze: bool,
    sql: &str,
    planning: Planning,
) -> Result<QueryResult> {
    let statistics = context.statistics();
    if !analyze {
        return explained(
            "logical_plan",
            &rudb_opt::explain::explain_with(plan, statistics, seams),
        );
    }
    // `EXPLAIN ANALYZE` is the one place a person reads these numbers with their own eyes rather
    // than through the harness, so the planning that produced the plan being printed has to reach
    // the document. It is the planning of the inner query and not of the `EXPLAIN`: the bind above
    // is what turned the statement into this plan and the optimize above is what ran on it.
    let under =
        Under::new(budget, statistics, seams.settings(), session, Rows::ForACaller).after(planning);
    let result = run(sql, plan, catalog, cancel, under)?;
    let measured = result.metrics().expect("a query that ran reports what it did");
    let text = rudb_opt::explain::analyzed(plan, statistics, seams, measured);
    explained("analyzed_plan", &text)
}

/// One row of two strings, which is the result set `EXPLAIN` hands back.
///
/// The column names and the shape are DuckDB's, `explain_key` and `explain_value`, because a
/// client reading a result set has to cope with whatever comes out and there is no reason to make
/// it cope with something new. The text in the second column is ours, since
/// `spec/12-duckdb-compat.md` section 12.5 excludes explain output from the guarantee.
///
/// One row rather than one per operator. DuckDB puts its whole tree in a single value and every
/// shell prints it as a block, and splitting it into rows would mean a shell's column width
/// deciding where a plan wraps.
fn explained(key: &str, text: &str) -> Result<QueryResult> {
    let key = Vector::from_values(LogicalType::Varchar, &[Value::Varchar(key.to_owned())])?;
    let value = Vector::from_values(LogicalType::Varchar, &[Value::Varchar(text.to_owned())])?;
    Ok(QueryResult::new(
        vec!["explain_key".to_owned(), "explain_value".to_owned()],
        vec![LogicalType::Varchar, LogicalType::Varchar],
        vec![Chunk::new(vec![key, value])?],
        Memory::unlimited().reservation(),
    ))
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
    catalog.create_view(View::new(
        create.name,
        create.sql,
        create.statement,
        create.aliases,
        create.columns,
    ))
}

/// The `CREATE TABLE` half of a statement.
#[allow(clippy::too_many_arguments)]
fn create_table(
    sql: &str,
    mut create: rudb_bind::CreateTable,
    catalog: &mut Catalog,
    cancel: &Cancel,
    budget: Budget<'_>,
    context: &rudb_opt::pass::Context,
    seams: &rudb_seam::Settings,
    session: &Session,
) -> Result<()> {
    if create.if_not_exists && catalog.table(&create.name).is_ok() {
        return Ok(());
    }
    // The query runs before the old table is dropped, so `CREATE OR REPLACE TABLE t AS SELECT * FROM
    // t` reads the table it is about to replace rather than the empty new one.
    let rows = match &mut create.source {
        Some(plan) => {
            rudb_opt::optimize_with(plan, context)?;
            let under = Under::new(budget, context.statistics(), seams, session, Rows::ForATable);
            Some(run(sql, plan, catalog, cancel, under)?)
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

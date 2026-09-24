//! The catalog: attached databases, their schemas, and the tables in them.

use std::fmt;
use std::sync::Arc;

use rudb_common::sequence::Counter;
use rudb_common::{Error, Field, LogicalType, Result};

use crate::name::{QualifiedName, same_name};
use crate::system::{
    INFORMATION_SCHEMA, INTERNAL_VIEWS, PG_CATALOG, SYSTEM_CATALOG, TEMP_CATALOG, statement,
};
use crate::table::Table;
use crate::view::View;
use rudb_native::Reader as NativeReader;

use crate::mirror::{FileStamp, MIRROR_CATALOG, Mirror};

/// The default attached database, which is the one an in-memory session gets.
pub const DEFAULT_CATALOG: &str = "memory";
/// The default schema inside it.
pub const DEFAULT_SCHEMA: &str = "main";

/// The oid of an entry that is not in a catalog.
///
/// Every database, schema, table and view reachable from a [`Catalog`] has a real one, because the
/// four calls that put an entry in are the four that stamp it. A [`Table`] or a [`View`] built with
/// its own constructor and never handed over has this until it is, which is a thing the tests do and
/// nothing else does.
pub const DETACHED: i64 = 0;

/// One attached database.
#[derive(Debug, Clone)]
pub struct Database {
    name: String,
    schemas: Vec<Schema>,
    oid: i64,
    internal: bool,
}

impl Database {
    /// The name it is attached as.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the engine made this one rather than a person.
    ///
    /// True for `system` and `temp` and false for everything a person attaches, which is the pin's
    /// answer in the `internal` column of `duckdb_databases()`.
    ///
    /// Not the same question as whether an entry inside it is internal. It is the same answer for
    /// `memory`, where nobody but a person puts anything, and for `system`, where nobody but the
    /// engine does. `temp` is the one that comes apart: the engine owns the database and every
    /// table in it was written by somebody, so `duckdb_tables()` says false there while
    /// `duckdb_databases()` says true. `rudb_exec`'s `entrynames` has that rule.
    #[must_use]
    pub fn internal(&self) -> bool {
        self.internal
    }

    /// The number the catalog tables join on.
    #[must_use]
    pub fn oid(&self) -> i64 {
        self.oid
    }

    /// The schemas in it.
    #[must_use]
    pub fn schemas(&self) -> &[Schema] {
        &self.schemas
    }
}

/// What a name in a schema turned out to be.
///
/// Tables and views share one namespace, so a lookup that only asked about tables would answer that
/// `v` does not exist when what is true is that `v` is a view. Every message that tells those two
/// apart is spelled with this, and the spelling is the binary's: `Table` and `View`, capitalised,
/// in sentences such as `Existing object "v" is of type View, trying to drop type Table`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// A table, which holds rows.
    Table,
    /// A view, which holds a query.
    View,
}

impl fmt::Display for Entry {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::Table => "Table",
            Self::View => "View",
        })
    }
}

/// One sequence.
///
/// Sequences have a namespace of their own, the way they do on the pin, so a table and a sequence
/// can share a name. The counter is behind an [`Arc`] so that the copy of the catalog a transaction
/// keeps to roll back to shares it, which is what makes a value `nextval` handed out stay handed
/// out after a `ROLLBACK`.
#[derive(Debug, Clone)]
pub struct Sequence {
    name: QualifiedName,
    oid: i64,
    counter: Arc<Counter>,
    /// The table or view `ALTER SEQUENCE ... OWNED BY` gave it to, which takes it along when it
    /// is dropped.
    owner: Option<QualifiedName>,
}

impl Sequence {
    /// Its full name.
    #[must_use]
    pub fn name(&self) -> &QualifiedName {
        &self.name
    }

    /// The number the catalog tables join on.
    #[must_use]
    pub fn oid(&self) -> i64 {
        self.oid
    }

    /// Its counter.
    #[must_use]
    pub fn counter(&self) -> &Arc<Counter> {
        &self.counter
    }

    /// The table or view that owns it, if one does.
    #[must_use]
    pub fn owner(&self) -> Option<&QualifiedName> {
        self.owner.as_ref()
    }
}

/// A type `CREATE TYPE` made, which is another name for the type it was made from.
///
/// The name is read once, when a column or a cast is bound, and what is kept there is the type it
/// stood for. So a table does not depend on the type its column was declared with, and the pin
/// agrees: dropping the type leaves the table and its `DESCRIBE` as they were. A type made from
/// another one does depend on it, which is what [`UserType::uses`] is for.
#[derive(Debug, Clone)]
pub struct UserType {
    name: QualifiedName,
    oid: i64,
    ty: LogicalType,
    uses: Vec<QualifiedName>,
}

impl UserType {
    /// Its full name.
    #[must_use]
    pub fn name(&self) -> &QualifiedName {
        &self.name
    }

    /// The number the catalog tables join on.
    #[must_use]
    pub fn oid(&self) -> i64 {
        self.oid
    }

    /// The type it stands for.
    #[must_use]
    pub fn ty(&self) -> &LogicalType {
        &self.ty
    }

    /// The other made types its definition named, which it cannot outlive.
    #[must_use]
    pub fn uses(&self) -> &[QualifiedName] {
        &self.uses
    }
}

/// One schema.
#[derive(Debug, Clone)]
pub struct Schema {
    name: String,
    tables: Vec<Table>,
    views: Vec<View>,
    sequences: Vec<Sequence>,
    types: Vec<UserType>,
    oid: i64,
}

impl Schema {
    /// A schema of that name with nothing in it.
    fn empty(name: &str, oid: i64) -> Self {
        Self {
            name: name.to_string(),
            tables: Vec::new(),
            views: Vec::new(),
            sequences: Vec::new(),
            types: Vec::new(),
            oid,
        }
    }

    /// The sequences in it.
    #[must_use]
    pub fn sequences(&self) -> &[Sequence] {
        &self.sequences
    }

    /// The schema name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The number the catalog tables join on.
    #[must_use]
    pub fn oid(&self) -> i64 {
        self.oid
    }

    /// The tables in it.
    #[must_use]
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    /// The views in it.
    #[must_use]
    pub fn views(&self) -> &[View] {
        &self.views
    }

    /// What a name in this schema is, if it is anything.
    fn kind(&self, name: &str) -> Option<Entry> {
        if self.tables.iter().any(|held| same_name(&held.name().table, name)) {
            return Some(Entry::Table);
        }
        if self.views.iter().any(|held| same_name(&held.name().table, name)) {
            return Some(Entry::View);
        }
        None
    }
}

/// Every attached database, and the rule for turning a written name into one object.
///
/// # Errors it produces
///
/// The messages are DuckDB's, per `spec/12-duckdb-compat.md` section 12.5, because a great many
/// tests in the wild assert on the text. What is missing is the `Did you mean "hits"?` line that
/// upstream appends to a missing entry, which needs a similarity search over the catalog and a
/// tie break rule that matches theirs. That is compatibility work rather than catalog work and it
/// is not done here.
#[derive(Debug, Clone)]
pub struct Catalog {
    databases: Vec<Database>,
    default_catalog: String,
    default_schema: String,
    /// The search path as `SET schema`, `SET search_path` or `USE` left it, empty for none.
    search: Vec<crate::SearchEntry>,
    /// Which version of the contents this is, counted from one.
    ///
    /// Anything that can change what a query would read moves it on, which is every method here
    /// that takes the catalog by mutable reference, including [`Catalog::table_mut`], because a
    /// caller that asked for a table that way is about to append to it or replace it. Handing out
    /// one number for two different states is the failure this has to avoid, so a method that might
    /// not change anything moves it anyway. Counting a change that did not happen costs a rebuild
    /// nobody needed. Missing one serves a plan facts about a table that is no longer there.
    ///
    /// Counted from one so that zero can mean no catalog was ever read, which is what a set of
    /// facts assembled by hand in a test carries.
    generation: u64,
    /// The next oid to hand out.
    ///
    /// A counter rather than a position, because a position changes when the thing before it is
    /// dropped and a client that cached the oid of one table would then be joining against another.
    /// Upstream's are a counter too, and its values are not reproduced here for the same reason
    /// `duckdb_types()` does not reproduce `database_oid`: an allocation counter says what order a
    /// process happened to create things in, so matching it would mean matching an accident.
    next: i64,
    /// The Parquet files read through a native mirror, apart from every schema. See [`crate::mirror`].
    mirrors: Vec<Mirror>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

impl Catalog {
    /// What an in-memory session starts from: `memory.main` to create in, the `system` database
    /// with the views the engine ships with, and an empty `temp`.
    ///
    /// Three databases and five schemas, which is what the pin reports from a session that has
    /// attached nothing. See `crate::system` for what goes in `system` and why the bodies are
    /// upstream's own text.
    #[must_use]
    pub fn new() -> Self {
        // The three databases and the five schemas take 1 to 8 between them and the views the
        // engine ships with take the numbers after that, so the first table a person makes carries
        // whatever is left.
        let (system, next) = system(9);
        Self {
            databases: vec![
                Database {
                    name: DEFAULT_CATALOG.to_string(),
                    schemas: vec![Schema::empty(DEFAULT_SCHEMA, 2)],
                    oid: 1,
                    internal: false,
                },
                system,
                Database {
                    name: TEMP_CATALOG.to_string(),
                    schemas: vec![Schema::empty(DEFAULT_SCHEMA, 8)],
                    oid: 7,
                    internal: true,
                },
            ],
            default_catalog: DEFAULT_CATALOG.to_string(),
            default_schema: DEFAULT_SCHEMA.to_string(),
            search: Vec::new(),
            next,
            generation: 1,
            mirrors: Vec::new(),
        }
    }

    /// Which version of the contents this is.
    ///
    /// Two reads that answer the same number are looking at the same catalog, so anything derived
    /// from it can be kept between them instead of being built again. The row counts and distinct
    /// counts the optimizer plans from are the caller this is for.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Says the contents have changed.
    ///
    /// Called by every method that takes the catalog by mutable reference rather than by the ones
    /// that really wrote something, which is deliberate and the field's own comment says why.
    fn changed(&mut self) {
        self.generation += 1;
    }

    /// Puts back a catalog kept from before, which is what a `ROLLBACK` does.
    ///
    /// The generation still moves on rather than going back to the kept one's, because a plan or a
    /// set of facts built since was built against contents that are gone and has to be built again.
    /// The oid counter keeps the higher of the two, so an oid handed out inside the transaction is
    /// never handed out again to something else.
    pub fn restore(&mut self, before: Self) {
        let generation = self.generation;
        let next = self.next.max(before.next);
        *self = before;
        self.generation = generation;
        self.next = next;
        self.changed();
    }

    /// The next oid, and moves the counter on.
    ///
    /// Never handed out twice in the life of one catalog, including across a drop and a create of
    /// the same name, because that is the whole point of an oid.
    fn stamp(&mut self) -> i64 {
        let oid = self.next;
        self.next += 1;
        oid
    }

    /// The catalog an unqualified name resolves in, which is the first entry of the search path's
    /// when one is set.
    #[must_use]
    pub fn default_catalog(&self) -> &str {
        match self.search.first() {
            Some(entry) if !entry.catalog.is_empty() => &entry.catalog,
            _ => &self.default_catalog,
        }
    }

    /// The schema an unqualified name resolves in, which is the first entry of the search path's
    /// when one is set.
    #[must_use]
    pub fn default_schema(&self) -> &str {
        self.search.first().map_or(&self.default_schema, |entry| &entry.schema)
    }

    /// The search path the way `current_setting('search_path')` writes it, empty when none is set.
    #[must_use]
    pub fn search_path(&self) -> String {
        self.search.iter().map(crate::SearchEntry::text).collect::<Vec<_>>().join(",")
    }

    /// The schemas on the search path, which `current_schemas` lists. Only the ones that were set
    /// unless `implicit` is, and then `temp.main` before them and the default database's `main`,
    /// `system.main` and `system.pg_catalog` after them.
    #[must_use]
    pub fn search_schemas(&self, implicit: bool) -> Vec<String> {
        let set = self.search.iter().map(|entry| entry.schema.clone());
        if !implicit {
            return set.collect();
        }
        let mut schemas = vec!["main".to_string()];
        schemas.extend(set);
        schemas.extend(["main", "main", "pg_catalog"].map(String::from));
        schemas
    }

    /// Takes a `SET schema`, when `one` is set, or a `SET search_path`, with the pin's checks: each
    /// entry has to name a schema that is there, or for a lone name a database, which stands for
    /// its `main`. A lone schema is taken to be in the database the path already starts in.
    ///
    /// # Errors
    ///
    /// For text the path cannot be read out of, for an entry that names nothing, and for a
    /// `SET schema` into `temp` or `system`.
    pub fn set_search_path(&mut self, text: &str, one: bool) -> Result<()> {
        let set = if one { "SET schema" } else { "SET search_path" };
        let mut entries = if one {
            vec![crate::search::parse_one(text)?]
        } else {
            crate::search::parse_list(text)?
        };
        let first = self.search.first().map(|entry| entry.catalog.clone()).unwrap_or_default();
        for entry in &mut entries {
            let catalog =
                if entry.catalog.is_empty() { self.default_catalog() } else { &entry.catalog };
            if let Ok(schema) = self.schema(catalog, &entry.schema) {
                entry.schema = schema.name.clone();
                if entry.catalog.is_empty() {
                    entry.catalog.clone_from(&first);
                }
                continue;
            }
            if entry.catalog.is_empty() {
                if let Ok(database) = self.database(&entry.schema) {
                    if let Some(main) = database.schemas.first() {
                        entry.catalog = database.name.clone();
                        entry.schema = main.name.clone();
                        continue;
                    }
                }
            }
            return Err(Error::catalog(format!(
                "{set}: No catalog + schema named \"{}\" found.",
                entry.text()
            )));
        }
        if one {
            if let Some(entry) = entries.first() {
                if same_name(&entry.catalog, TEMP_CATALOG)
                    || same_name(&entry.catalog, SYSTEM_CATALOG)
                {
                    return Err(Error::catalog(format!(
                        "{set} cannot be set to internal schema \"{}\"",
                        entry.catalog
                    )));
                }
            }
        }
        self.search = entries;
        Ok(())
    }

    /// Clears the search path, which is what `RESET schema` and `RESET search_path` both do.
    pub fn reset_search_path(&mut self) {
        self.search.clear();
    }

    /// Where a search path entry points, with the default database filled in.
    fn searched(&self, entry: &crate::SearchEntry) -> (String, String) {
        let catalog = if entry.catalog.is_empty() {
            self.default_catalog.clone()
        } else {
            entry.catalog.clone()
        };
        (catalog, entry.schema.clone())
    }

    /// The attached databases.
    #[must_use]
    pub fn databases(&self) -> &[Database] {
        &self.databases
    }

    /// Attaches an empty database with a `main` schema in it.
    ///
    /// # Errors
    ///
    /// If a database of that name is already attached.
    pub fn attach(&mut self, name: &str) -> Result<()> {
        self.changed();
        if self.databases.iter().any(|held| same_name(&held.name, name)) {
            return Err(Error::catalog(format!("Database with name \"{name}\" already exists!")));
        }
        let oid = self.stamp();
        let schema = self.stamp();
        self.databases.push(Database {
            name: name.to_string(),
            schemas: vec![Schema::empty(DEFAULT_SCHEMA, schema)],
            oid,
            internal: false,
        });
        Ok(())
    }

    /// The database and the schema a written schema name means, for `CREATE SCHEMA` and `DROP
    /// SCHEMA`.
    ///
    /// One part is a schema in the default database and two are a database and a schema in it. The
    /// pin blames the first part that is not an attached database when that reading fails, which
    /// for three parts is the middle one once the first is a database.
    ///
    /// # Errors
    ///
    /// If a part that has to be a database is not one.
    pub fn schema_name(&self, parts: &[&str]) -> Result<(String, String)> {
        let not_one = |part: &str| Error::catalog(format!("\"{part}\" is not a catalog or schema"));
        match parts {
            [schema] => Ok((self.default_catalog().to_string(), (*schema).to_string())),
            [catalog, schema] => match self.database(catalog) {
                Ok(database) => Ok((database.name.clone(), (*schema).to_string())),
                Err(_) => Err(not_one(catalog)),
            },
            [catalog, middle, ..] if self.database(catalog).is_ok() => Err(not_one(middle)),
            [first, ..] => Err(not_one(first)),
            [] => Err(Error::internal("a schema name with no parts")),
        }
    }

    /// Whether a schema of that name is in that database.
    #[must_use]
    pub fn has_schema(&self, catalog: &str, name: &str) -> bool {
        self.schema(catalog, name).is_ok()
    }

    /// Creates a schema in an attached database.
    ///
    /// The two schemas the engine keeps its own views in are refused by name wherever they are
    /// written, which is the pin's answer even for a database that does not hold them.
    ///
    /// # Errors
    ///
    /// If the database is not attached, or is one the engine owns, or a schema of that name is
    /// already in it.
    pub fn create_schema(&mut self, catalog: &str, name: &str) -> Result<()> {
        self.changed();
        let oid = self.stamp();
        let reserved = [INFORMATION_SCHEMA, PG_CATALOG].iter().any(|held| same_name(held, name));
        let database = self.database_mut(catalog)?;
        if same_name(&database.name, TEMP_CATALOG) && !reserved {
            return Err(Error::invalid_input(format!(
                "Cannot create non-temporary entry \"{name}\" in temporary catalog"
            )));
        }
        if database.internal || reserved {
            return Err(Error::binder("Cannot create schema in system catalog"));
        }
        if database.schemas.iter().any(|held| same_name(&held.name, name)) {
            return Err(Error::catalog(format!("Schema with name \"{name}\" already exists!")));
        }
        database.schemas.push(Schema::empty(name, oid));
        Ok(())
    }

    /// Removes a schema, and with `cascade` everything in it.
    ///
    /// Without `cascade` a schema that still holds anything stays, and the pin's sentence lists
    /// what it holds, views first and then tables, the newest of each first. The pin's own order
    /// follows its hash sets, which is not one worth copying, and every file in the corpus that
    /// spells the list out holds one entry. With `cascade` a table another schema holds a foreign
    /// key into still stays, for the same reason [`Catalog::drop_table`] gives.
    ///
    /// # Errors
    ///
    /// If there is no such schema, if it is `main` or one the engine owns, or if it holds something
    /// and `cascade` was not asked for.
    pub fn drop_schema(&mut self, catalog: &str, name: &str, cascade: bool) -> Result<()> {
        let database = self.database(catalog)?;
        let schema = self.schema(catalog, name)?;
        if database.internal || same_name(&schema.name, DEFAULT_SCHEMA) {
            return Err(Error::catalog(format!(
                "Cannot drop entry \"{}\" because it is an internal system entry",
                schema.name
            )));
        }
        if !cascade
            && (!schema.tables.is_empty()
                || !schema.views.is_empty()
                || !schema.sequences.is_empty()
                || !schema.types.is_empty())
        {
            let mut message = format!(
                "Cannot drop entry \"{}\" because there are entries that depend on it.\n",
                schema.name
            );
            for view in schema.views.iter().rev() {
                message += &format!(
                    "view \"{}\" depends on schema \"{}\".\n",
                    view.name().table,
                    schema.name
                );
            }
            for table in schema.tables.iter().rev() {
                message += &format!(
                    "table \"{}\" depends on schema \"{}\".\n",
                    table.name().table,
                    schema.name
                );
            }
            for sequence in schema.sequences.iter().rev() {
                message += &format!(
                    "sequence \"{}\" depends on schema \"{}\".\n",
                    sequence.name.table, schema.name
                );
            }
            for made in schema.types.iter().rev() {
                message += &format!(
                    "type \"{}\" depends on schema \"{}\".\n",
                    made.name.table, schema.name
                );
            }
            message += "Use DROP...CASCADE to drop all dependents.";
            return Err(Error::dependency(message));
        }
        let inside = |held: &QualifiedName| {
            same_name(&held.catalog, catalog) && same_name(&held.schema, name)
        };
        let holder = self.tables().find(|table| {
            !inside(table.name()) && table.foreign().iter().any(|foreign| inside(&foreign.table))
        });
        if let Some(holder) = holder {
            return Err(Error::catalog(format!(
                "Could not drop the table because this table is main key table of the table \"{}\"",
                holder.name().table
            )));
        }
        self.changed();
        self.database_mut(catalog)?.schemas.retain(|held| !same_name(&held.name, name));
        // A path entry for a schema that is gone would send every bare name to nowhere, and the
        // pin goes back to `main` when the schema it was in is dropped.
        let search = std::mem::take(&mut self.search);
        self.search = search
            .into_iter()
            .filter(|entry| {
                let (held, schema) = self.searched(entry);
                !(same_name(&held, catalog) && same_name(&schema, name))
            })
            .collect();
        Ok(())
    }

    /// Creates an empty table.
    ///
    /// # Errors
    ///
    /// If the database or the schema is missing, if a table or a view of that name is already
    /// there, or if two columns have the same name.
    pub fn create_table(&mut self, name: QualifiedName, columns: Vec<Field>) -> Result<()> {
        self.changed();
        let mut table = Table::new(name.clone(), columns)?;
        // Stamped before the name is checked, so a refused create burns an oid rather than handing
        // the next table the number the refused one would have had. A gap in the sequence costs
        // nothing and a number handed out twice costs a wrong join.
        table.stamp(self.stamp());
        let schema = self.schema_mut(&name.catalog, &name.schema)?;
        if let Some(found) = schema.kind(&name.table) {
            return Err(taken(found, &name.table));
        }
        schema.tables.push(table);
        Ok(())
    }

    /// Registers the table committed in one native file in the default catalog and schema.
    ///
    /// # Errors
    ///
    /// If its name is already used or its stored schema is invalid.
    pub fn create_native_table(&mut self, reader: NativeReader) -> Result<()> {
        self.changed();
        let name = QualifiedName::new(
            self.default_catalog.clone(),
            self.default_schema.clone(),
            reader.table().name(),
        );
        let mut table = Table::native(name.clone(), reader)?;
        table.stamp(self.stamp());
        let schema = self.schema_mut(&name.catalog, &name.schema)?;
        if let Some(found) = schema.kind(&name.table) {
            return Err(taken(found, &name.table));
        }
        schema.tables.push(table);
        Ok(())
    }

    /// Registers a view read back out of a native file in the default catalog and schema.
    ///
    /// The columns go in as the cache they were written as, which is the whole reason they are in
    /// the file. Nothing has bound the body in this process, so a catalog that started the columns
    /// empty would answer `is_bound` false until somebody selected from the view, and the pin
    /// answers true straight after an open. Binding every view here instead would make opening a
    /// database cost a bind per view and would fail on a view whose table is in a database that has
    /// not been attached yet.
    ///
    /// # Errors
    ///
    /// If its name is already used by a table or another view.
    pub fn create_native_view(&mut self, view: &rudb_native::ViewEntry) -> Result<()> {
        let name = QualifiedName::new(
            self.default_catalog.clone(),
            self.default_schema.clone(),
            view.name.clone(),
        );
        self.create_view(View::new(
            name,
            view.sql.clone(),
            view.statement.clone(),
            view.aliases.clone(),
            view.columns.clone(),
        ))
    }

    /// Creates a view.
    ///
    /// The body is not checked here. Whether it binds is the binder's question and it is asked
    /// before this is called, because a view that cannot bind is refused at creation.
    ///
    /// # Errors
    ///
    /// If the database or the schema is missing, or if a table or a view of that name is already
    /// there.
    pub fn create_view(&mut self, mut view: View) -> Result<()> {
        self.changed();
        let name = view.name().clone();
        view.stamp(self.stamp());
        let schema = self.schema_mut(&name.catalog, &name.schema)?;
        if let Some(found) = schema.kind(&name.table) {
            return Err(taken(found, &name.table));
        }
        schema.views.push(view);
        Ok(())
    }

    /// Removes a table and everything in it.
    ///
    /// # Errors
    ///
    /// If there is no such table, or if the name is a view, which is a different sentence because
    /// it is a different mistake.
    pub fn drop_table(&mut self, name: &QualifiedName) -> Result<()> {
        // A table another one holds a foreign key into stays until that one is gone, which is the
        // pin's rule and its sentence.
        let holder = self.tables().find(|table| {
            table.name() != name && table.foreign().iter().any(|foreign| &foreign.table == name)
        });
        if let Some(holder) = holder {
            return Err(Error::catalog(format!(
                "Could not drop the table because this table is main key table of the table \"{}\"",
                holder.name().table
            )));
        }
        self.changed();
        self.drop_entry(name, Entry::Table)
    }

    /// Removes a view.
    ///
    /// # Errors
    ///
    /// If there is no such view, or if the name is a table.
    pub fn drop_view(&mut self, name: &QualifiedName) -> Result<()> {
        self.changed();
        self.drop_entry(name, Entry::View)
    }

    /// Removes whichever of the two the caller said it was dropping, refusing the other one.
    fn drop_entry(&mut self, name: &QualifiedName, wanted: Entry) -> Result<()> {
        // `temp` is an internal database holding entries that are not internal, which is the one
        // place those two answers come apart. A person wrote the temporary table and a person gets
        // to drop it.
        if !name.temporary() && self.database(&name.catalog)?.internal {
            return Err(Error::catalog(format!(
                "Cannot drop internal catalog entry \"{}\"!",
                name.table
            )));
        }
        let schema = self.schema_mut(&name.catalog, &name.schema)?;
        match schema.kind(&name.table) {
            // The type in this one is the type being dropped, so `DROP VIEW gone` is a missing view
            // and `DROP TABLE gone` is a missing table over the same absent name.
            None => return Err(missing(wanted, &name.table)),
            Some(found) if found != wanted => {
                return Err(Error::catalog(format!(
                    "Existing object \"{}\" is of type {found}, trying to drop type {wanted}",
                    name.table
                )));
            }
            Some(Entry::Table) => {
                schema.tables.retain(|held| !same_name(&held.name().table, &name.table));
            }
            Some(Entry::View) => {
                schema.views.retain(|held| !same_name(&held.name().table, &name.table));
            }
        }
        // A sequence the entry owned goes with it, wherever the sequence lives.
        for database in &mut self.databases {
            for schema in &mut database.schemas {
                schema.sequences.retain(|held| held.owner.as_ref() != Some(name));
            }
        }
        Ok(())
    }

    /// Creates a sequence around a counter already made for it.
    ///
    /// With `replace` a sequence of that name is swapped for the new one, unless a table's default
    /// uses it, and with `if_not_exists` it is kept and the new one thrown away.
    ///
    /// # Errors
    ///
    /// If the schema is missing, if a sequence of that name is there and neither was asked for, or
    /// if one being replaced has a table depending on it.
    pub fn create_sequence(
        &mut self,
        name: QualifiedName,
        counter: Arc<Counter>,
        replace: bool,
        if_not_exists: bool,
    ) -> Result<()> {
        self.changed();
        let oid = self.stamp();
        let held = self
            .schema(&name.catalog, &name.schema)?
            .sequences
            .iter()
            .find(|held| same_name(&held.name.table, &name.table))
            .map(|held| held.name.clone());
        if let Some(held) = held {
            if if_not_exists {
                return Ok(());
            }
            if !replace {
                return Err(Error::catalog(format!(
                    "Sequence with name \"{}\" already exists!",
                    name.table
                )));
            }
            self.sequence_dependents(&held)?;
            self.schema_mut(&name.catalog, &name.schema)?
                .sequences
                .retain(|seq| !same_name(&seq.name.table, &name.table));
        }
        self.schema_mut(&name.catalog, &name.schema)?.sequences.push(Sequence {
            name,
            oid,
            counter,
            owner: None,
        });
        Ok(())
    }

    /// Turns the parts of a written name into the full name of a sequence that exists, reading it
    /// the way a table name is read and the temporary schema first.
    ///
    /// # Errors
    ///
    /// If the name has more than three parts or no sequence has it.
    pub fn resolve_sequence(&self, parts: &[&str]) -> Result<QualifiedName> {
        if parts.len() > 3 {
            return Err(Error::catalog(format!(
                "Sequence with name \"{}\" does not exist because schema \"{}\" does not exist.",
                parts.join("."),
                parts[..parts.len() - 1].join(".")
            )));
        }
        for candidate in self.candidates(parts)? {
            if let Ok(schema) = self.schema(&candidate.catalog, &candidate.schema) {
                if let Some(held) = schema
                    .sequences
                    .iter()
                    .find(|held| same_name(&held.name.table, &candidate.table))
                {
                    return Ok(held.name.clone());
                }
            }
        }
        Err(Error::catalog(format!(
            "Sequence with name {} does not exist!",
            parts.last().copied().unwrap_or_default()
        )))
    }

    /// A sequence by its full name.
    ///
    /// # Errors
    ///
    /// If the database, the schema or the sequence is missing.
    pub fn sequence(&self, name: &QualifiedName) -> Result<&Sequence> {
        self.schema(&name.catalog, &name.schema)?
            .sequences
            .iter()
            .find(|held| same_name(&held.name.table, &name.table))
            .ok_or_else(|| {
                Error::catalog(format!("Sequence with name {} does not exist!", name.table))
            })
    }

    /// Makes a type that stands for `ty` under `name`, with the pin's refusals: a name a built in
    /// type has or one that is taken, unless `if_not_exists` or `replace` was asked for. `uses` are
    /// the other made types `ty` was read through.
    ///
    /// # Errors
    ///
    /// If the schema is missing, if the name is taken and neither flag says what to do, or if a
    /// type being replaced has another type made from it.
    pub fn create_type(
        &mut self,
        name: QualifiedName,
        ty: LogicalType,
        uses: Vec<QualifiedName>,
        replace: bool,
        if_not_exists: bool,
    ) -> Result<()> {
        self.changed();
        let oid = self.stamp();
        let taken = || Error::catalog(format!("Type with name \"{}\" already exists!", name.table));
        if LogicalType::parse(&name.table).is_ok() {
            if if_not_exists {
                return Ok(());
            }
            return Err(taken());
        }
        let held = self
            .schema(&name.catalog, &name.schema)?
            .types
            .iter()
            .any(|held| same_name(&held.name.table, &name.table));
        if held {
            if if_not_exists {
                return Ok(());
            }
            if !replace {
                return Err(taken());
            }
            self.type_dependents(&name)?;
            self.schema_mut(&name.catalog, &name.schema)?
                .types
                .retain(|held| !same_name(&held.name.table, &name.table));
        }
        self.schema_mut(&name.catalog, &name.schema)?.types.push(UserType { name, oid, ty, uses });
        Ok(())
    }

    /// The made type a written name stands for, read the way a table name is read and the
    /// temporary schema first, and `None` when there is none.
    #[must_use]
    pub fn resolve_type(&self, parts: &[&str]) -> Option<&UserType> {
        if parts.is_empty() || parts.len() > 3 {
            return None;
        }
        for candidate in self.candidates(parts).ok()? {
            if let Ok(schema) = self.schema(&candidate.catalog, &candidate.schema) {
                let found =
                    schema.types.iter().find(|held| same_name(&held.name.table, &candidate.table));
                if found.is_some() {
                    return found;
                }
            }
        }
        None
    }

    /// Drops a made type, and with `cascade` every type made from it, all the way down.
    ///
    /// # Errors
    ///
    /// If there is no such type, or if another type was made from it and `cascade` was not asked
    /// for.
    pub fn drop_type(&mut self, name: &QualifiedName, cascade: bool) -> Result<()> {
        if !cascade {
            self.type_dependents(name)?;
        }
        self.changed();
        let mut gone = vec![name.clone()];
        while let Some(next) = gone.pop() {
            let dependents: Vec<QualifiedName> = self
                .types()
                .filter(|held| held.uses.contains(&next))
                .map(|held| held.name.clone())
                .collect();
            gone.extend(dependents);
            self.schema_mut(&next.catalog, &next.schema)?
                .types
                .retain(|held| !same_name(&held.name.table, &next.table));
        }
        Ok(())
    }

    /// Refuses when another made type was read through this one, in the pin's sentence.
    fn type_dependents(&self, name: &QualifiedName) -> Result<()> {
        let dependents: Vec<&UserType> =
            self.types().filter(|held| held.uses.contains(name)).collect();
        if dependents.is_empty() {
            return Ok(());
        }
        let mut message = format!(
            "Cannot drop entry \"{}\" because there are entries that depend on it.\n",
            name.table
        );
        for held in dependents {
            message +=
                &format!("type \"{}\" depends on type \"{}\".\n", held.name.table, name.table);
        }
        message += "Use DROP...CASCADE to drop all dependents.";
        Err(Error::dependency(message))
    }

    /// Every made type in every database, in the order they were made within each schema.
    pub fn types(&self) -> impl Iterator<Item = &UserType> {
        self.databases
            .iter()
            .flat_map(|database| database.schemas.iter())
            .flat_map(|schema| schema.types.iter())
    }

    /// Every sequence in every database, in the order they were made within each schema.
    pub fn sequences(&self) -> impl Iterator<Item = &Sequence> {
        self.databases
            .iter()
            .flat_map(|database| database.schemas.iter())
            .flat_map(|schema| schema.sequences.iter())
    }

    /// Refuses when a table's default uses the sequence, in the pin's sentence, newest first.
    fn sequence_dependents(&self, name: &QualifiedName) -> Result<()> {
        let dependents: Vec<&Table> =
            self.tables().filter(|table| table.sequences().contains(name)).collect();
        if dependents.is_empty() {
            return Ok(());
        }
        let mut message = format!(
            "Cannot drop entry \"{}\" because there are entries that depend on it.\n",
            name.table
        );
        for table in dependents.iter().rev() {
            message += &format!(
                "table \"{}\" depends on sequence \"{}\".\n",
                table.name().table,
                name.table
            );
        }
        message += "Use DROP...CASCADE to drop all dependents.";
        Err(Error::dependency(message))
    }

    /// The table or view an `OWNED BY` names, which the pin looks for in the default database and
    /// in `main` unless a schema is written, whatever the search path says.
    ///
    /// # Errors
    ///
    /// If there is no table or view by that name there.
    pub fn resolve_owner(&self, parts: &[&str]) -> Result<QualifiedName> {
        let (schema, table) = match parts {
            [.., schema, table] => (*schema, *table),
            [table] => (DEFAULT_SCHEMA, *table),
            [] => return Err(Error::internal("an OWNED BY without a name")),
        };
        let missing =
            || Error::catalog(format!("CatalogElement \"{schema}.{table}\" does not exist!"));
        let held = self.schema(&self.default_catalog, schema).map_err(|_| missing())?;
        let found = held
            .tables
            .iter()
            .map(Table::name)
            .chain(held.views.iter().map(View::name))
            .find(|name| same_name(&name.table, table))
            .ok_or_else(missing)?;
        Ok(found.clone())
    }

    /// Gives a sequence to a table or view, so that dropping the owner drops the sequence and the
    /// sequence cannot be dropped on its own while the owner is there.
    ///
    /// # Errors
    ///
    /// If the sequence is missing, or something else owns it already.
    pub fn own_sequence(&mut self, name: &QualifiedName, owner: QualifiedName) -> Result<()> {
        if let Some(held) = self.sequence(name)?.owner() {
            if *held == owner {
                return Ok(());
            }
            return Err(Error::dependency(format!(
                "\"{}\" is already owned by \"{}\"",
                name.table, held.table
            )));
        }
        self.changed();
        if let Ok(table) = self.table_mut(&owner) {
            if !table.sequences().contains(name) {
                let mut sequences = table.sequences().to_vec();
                sequences.push(name.clone());
                table.set_sequences(sequences);
            }
        }
        let schema = self.schema_mut(&name.catalog, &name.schema)?;
        if let Some(sequence) =
            schema.sequences.iter_mut().find(|held| same_name(&held.name.table, &name.table))
        {
            sequence.owner = Some(owner);
        }
        Ok(())
    }

    /// Makes one `ALTER TABLE` change, or renames a view. `rows` is every row of the table as it
    /// reads after the change, for the changes that move data.
    ///
    /// # Errors
    ///
    /// The refusals of the table's own alter, a new name that is taken, and any change but adding a
    /// column or changing a default to a table another table's foreign key points at, which the
    /// pin refuses after the table's own checks.
    pub fn alter(
        &mut self,
        name: &QualifiedName,
        alteration: crate::Alteration,
        rows: Option<Vec<rudb_vector::Chunk>>,
        workers: usize,
    ) -> Result<()> {
        let renamed = match &alteration {
            crate::Alteration::Rename(to) => Some(to.clone()),
            _ => None,
        };
        if self.entry(name)? == Entry::View {
            let Some(to) = renamed else {
                return Err(Error::catalog("Can only modify view with ALTER VIEW statement"));
            };
            self.rename_check(name, &to)?;
            self.changed();
            let schema = self.schema_mut(&name.catalog, &name.schema)?;
            if let Some(view) =
                schema.views.iter_mut().find(|held| same_name(&held.name().table, &name.table))
            {
                view.rename(&to);
            }
            return Ok(());
        }
        let keeps = alteration.keeps_dependents();
        // The column a type change or a drop is about, with which of the two it is, for the
        // index refusals that name the column's index rather than the table's dependents.
        let changed = match &alteration {
            crate::Alteration::Type { column, .. } => Some((*column, true)),
            crate::Alteration::DropColumn { column, .. } => Some((*column, false)),
            _ => None,
        };
        let original = self.table(name)?;
        let indexed =
            |column: usize| original.indexes().iter().any(|index| index.columns.contains(&column));
        match changed {
            Some((column, true)) if indexed(column) => {
                return Err(Error::catalog(
                    "Cannot change the type of this column: an index depends on it!",
                ));
            }
            // Every column after a dropped one moves down a place, which the pin's indexes cannot
            // follow, so one over any later column refuses the drop as well.
            Some((column, false))
                if original
                    .indexes()
                    .iter()
                    .any(|index| index.columns.iter().any(|&at| at > column)) =>
            {
                return Err(Error::catalog(
                    "Cannot drop this column: an index depends on a column after it!",
                ));
            }
            Some((column, false)) if indexed(column) => {
                return Err(Error::catalog("Cannot drop this column: an index depends on it!"));
            }
            _ => {}
        }
        let depended = !original.indexes().is_empty()
            || self.tables().any(|held| {
                held.name() != name && held.foreign().iter().any(|foreign| &foreign.table == name)
            });
        if depended && !keeps {
            return Err(Error::dependency(format!(
                "Cannot alter entry \"{}\" because there are entries that depend on it.",
                name.table
            )));
        }
        let mut table = original.clone();
        table.alter(alteration, rows, workers)?;
        if let Some(to) = &renamed {
            self.rename_check(name, to)?;
        }
        self.changed();
        let schema = self.schema_mut(&name.catalog, &name.schema)?;
        let Some(held) =
            schema.tables.iter_mut().find(|held| same_name(&held.name().table, &name.table))
        else {
            return Err(missing_table(&name.table));
        };
        *held = table;
        let Some(to) = renamed else { return Ok(()) };
        let moved = QualifiedName::new(name.catalog.clone(), name.schema.clone(), to);
        // What points at the table by name points at the new one: its own foreign keys into
        // itself, and the sequences it owns.
        let held = self.table_mut(&moved)?;
        let mut foreign = held.foreign().to_vec();
        for key in &mut foreign {
            if &key.table == name {
                key.table = moved.clone();
            }
        }
        held.set_foreign(foreign);
        for database in &mut self.databases {
            for schema in &mut database.schemas {
                for sequence in &mut schema.sequences {
                    if sequence.owner.as_ref() == Some(name) {
                        sequence.owner = Some(moved.clone());
                    }
                }
            }
        }
        Ok(())
    }

    /// Adds an index over a table, stamping its oid.
    ///
    /// `OR REPLACE` is no help with a name that is taken, which is the pin's rule as well: it
    /// refuses the second index the same way it would without the clause.
    ///
    /// # Errors
    ///
    /// If another index in the table's schema has the name, unless `quiet` says to leave that one
    /// be, and if a unique index finds a key the rows already repeat.
    pub fn create_index(
        &mut self,
        table: &QualifiedName,
        mut index: crate::Index,
        quiet: bool,
    ) -> Result<()> {
        let schema = QualifiedName::new(table.catalog.clone(), table.schema.clone(), &index.name);
        if self.index_in(&schema).is_some() {
            if quiet {
                return Ok(());
            }
            return Err(Error::catalog(format!(
                "Index with name \"{}\" already exists!",
                index.name
            )));
        }
        index.oid = self.stamp();
        self.changed();
        self.table_mut(table)?.add_index(index)
    }

    /// Removes an index by its written name.
    ///
    /// # Errors
    ///
    /// If no index has the name, unless `quiet` says that is fine.
    pub fn drop_index(&mut self, parts: &[&str], quiet: bool) -> Result<()> {
        let found = self.candidates(parts)?.iter().find_map(|candidate| self.index_in(candidate));
        let Some((holder, at)) = found else {
            if quiet {
                return Ok(());
            }
            let name = parts.last().copied().unwrap_or_default();
            return Err(Error::catalog(format!("Index with name {name} does not exist!")));
        };
        self.changed();
        self.table_mut(&holder)?.drop_index(at);
        Ok(())
    }

    /// The table holding the index this name means, read as schema and index name, and where the
    /// index is in its list.
    fn index_in(&self, name: &QualifiedName) -> Option<(QualifiedName, usize)> {
        let schema = self.schema(&name.catalog, &name.schema).ok()?;
        schema.tables.iter().find_map(|table| {
            let at =
                table.indexes().iter().position(|index| same_name(&index.name, &name.table))?;
            Some((table.name().clone(), at))
        })
    }

    /// Refuses a rename onto a name something else in the schema already has.
    fn rename_check(&self, name: &QualifiedName, to: &str) -> Result<()> {
        let schema = self.schema(&name.catalog, &name.schema)?;
        if !same_name(&name.table, to) && schema.kind(to).is_some() {
            return Err(Error::catalog(format!(
                "Could not rename \"{}\" to \"{to}\": another entry with this name already exists!",
                name.table
            )));
        }
        Ok(())
    }

    /// Removes a sequence, and with `cascade` every table whose default uses it.
    ///
    /// # Errors
    ///
    /// If it is missing, or a table depends on it and `cascade` was not asked for.
    pub fn drop_sequence(&mut self, name: &QualifiedName, cascade: bool) -> Result<()> {
        self.sequence(name)?;
        if !cascade {
            self.sequence_dependents(name)?;
        }
        self.changed();
        let dependents: Vec<QualifiedName> = self
            .tables()
            .filter(|table| table.sequences().contains(name))
            .map(|table| table.name().clone())
            .collect();
        for table in &dependents {
            self.drop_entry(table, Entry::Table)?;
        }
        self.schema_mut(&name.catalog, &name.schema)?
            .sequences
            .retain(|held| !same_name(&held.name.table, &name.table));
        Ok(())
    }

    /// What a full name is, if it is anything.
    ///
    /// # Errors
    ///
    /// If the database, the schema, or the name itself is missing.
    pub fn entry(&self, name: &QualifiedName) -> Result<Entry> {
        self.schema(&name.catalog, &name.schema)?
            .kind(&name.table)
            .ok_or_else(|| missing_table(&name.table))
    }

    /// A view by its full name.
    ///
    /// # Errors
    ///
    /// If the database, the schema or the view is missing.
    pub fn view(&self, name: &QualifiedName) -> Result<&View> {
        let schema = self.schema(&name.catalog, &name.schema)?;
        schema
            .views
            .iter()
            .find(|held| same_name(&held.name().table, &name.table))
            .ok_or_else(|| missing_table(&name.table))
    }

    /// A table by its full name.
    ///
    /// # Errors
    ///
    /// If the database, the schema or the table is missing.
    pub fn table(&self, name: &QualifiedName) -> Result<&Table> {
        if name.catalog == MIRROR_CATALOG {
            return self
                .mirrors
                .iter()
                .map(|mirror| &mirror.table)
                .find(|held| held.name() == name)
                .ok_or_else(|| missing_table(&name.table));
        }
        let schema = self.schema(&name.catalog, &name.schema)?;
        schema
            .tables
            .iter()
            .find(|held| same_name(&held.name().table, &name.table))
            .ok_or_else(|| missing_table(&name.table))
    }

    /// A table by its full name, to change.
    ///
    /// # Errors
    ///
    /// If the database, the schema or the table is missing.
    pub fn table_mut(&mut self, name: &QualifiedName) -> Result<&mut Table> {
        self.changed();
        let table = name.table.clone();
        let schema = self.schema_mut(&name.catalog, &name.schema)?;
        schema
            .tables
            .iter_mut()
            .find(|held| same_name(&held.name().table, &table))
            .ok_or_else(|| missing_table(&table))
    }

    /// Turns the parts of a written name into the full name of a table or a view that exists.
    ///
    /// One part is a table in the default schema. Three parts are a catalog, a schema and a table.
    /// Two parts are the interesting case: they are a schema and a table if the first part names a
    /// schema in the default catalog, and a catalog and a table otherwise, which is the order
    /// DuckDB tries them in and matters for `information_schema.tables` and for `memory.hits`
    /// meaning what they each look like they mean.
    ///
    /// The name that comes back is the one the object was created with rather than the one that was
    /// written, so a plan built from it prints the spelling a person would recognise.
    ///
    /// # Errors
    ///
    /// If the name has no parts or more than three, or if it does not resolve to either.
    pub fn resolve(&self, parts: &[&str]) -> Result<QualifiedName> {
        self.resolve_as(parts, Entry::Table)
    }

    /// The same as [`Catalog::resolve`], except that a name which is not there is reported as a
    /// missing `wanted` rather than as a missing table.
    ///
    /// A statement that says which of the two it meant gets to say it in the complaint, so `DROP
    /// VIEW gone` is a missing view and `DROP TABLE gone` is a missing table over the same absent
    /// name. A statement that does not say, such as a read, is resolving a table as far as the
    /// message is concerned, which is why plain `resolve` passes [`Entry::Table`].
    ///
    /// # Errors
    ///
    /// If the name has no parts or more than three, or if it does not resolve to either.
    pub fn resolve_as(&self, parts: &[&str], wanted: Entry) -> Result<QualifiedName> {
        let candidates = self.candidates(parts)?;
        let mut first_error = None;
        for candidate in &candidates {
            let held = match self.schema(&candidate.catalog, &candidate.schema) {
                Ok(schema) => schema.kind(&candidate.table),
                // The first reading is the preferred one, so its complaint is the one that names
                // the piece the writer most likely meant and got wrong. A read of a table in a
                // schema that is not there says both, the way the pin does.
                Err(error) => {
                    let error = if wanted == Entry::Table
                        && self.database(&candidate.catalog).is_ok()
                    {
                        Error::catalog(format!(
                            "Table with name \"{}.{}\" does not exist because schema \"{}\" does \
                             not exist.",
                            candidate.schema, candidate.table, candidate.schema
                        ))
                    } else {
                        error
                    };
                    first_error = first_error.or(Some(error));
                    continue;
                }
            };
            match held {
                Some(Entry::Table) => return Ok(self.table(candidate)?.name().clone()),
                Some(Entry::View) => return Ok(self.view(candidate)?.name().clone()),
                None => {
                    first_error = first_error.or_else(|| Some(missing(wanted, &candidate.table)));
                }
            }
        }
        Err(first_error.unwrap_or_else(|| missing(wanted, &parts.join("."))))
    }

    /// The full name a `CREATE` of this written name would make, without requiring it to exist.
    ///
    /// # Errors
    ///
    /// If the name has no parts or more than three, or if the schema it names is missing.
    pub fn resolve_for_create(&self, parts: &[&str]) -> Result<QualifiedName> {
        let candidates = self.written(parts)?;
        let mut first_error = None;
        for candidate in &candidates {
            match self.schema(&candidate.catalog, &candidate.schema) {
                Ok(_) if same_name(&candidate.catalog, TEMP_CATALOG) => {
                    // A create that names `temp` out loud is a create of a temporary table written
                    // the long way, and upstream refuses it from the parser rather than making one.
                    return Err(Error::parser(format!(
                        "Only TEMPORARY table names can use the \"{TEMP_CATALOG}\" catalog"
                    )));
                }
                Ok(_) if self.database(&candidate.catalog)?.internal => {
                    return Err(in_the_system_catalog());
                }
                Ok(_) => return Ok(candidate.clone()),
                Err(error) => first_error = first_error.or(Some(error)),
            }
        }
        Err(first_error.unwrap_or_else(|| {
            Error::catalog(format!("Schema with name {} does not exist!", parts.join(".")))
        }))
    }

    /// The full name a `CREATE TEMPORARY` of this written name would make.
    ///
    /// Everything temporary goes in the `temp` database and nowhere else, so this is not a search
    /// over a path the way the other two are. A bare name is `temp.main`, a two part name names a
    /// schema inside `temp` unless it names `temp` itself, and a three part name has to say `temp`.
    /// Anything else names a database that is not `temp` and is refused with the pin's own
    /// sentence, asterisks included.
    ///
    /// A schema in `temp` other than `main` is a thing the pin cannot have either, because it
    /// refuses `CREATE SCHEMA` there. So the two part form exists to let somebody write `main.t`
    /// and get the temporary one, and to say which schema is missing when they write anything else.
    ///
    /// # Errors
    ///
    /// If the name has no parts or more than three, if it names a database other than `temp`, or if
    /// the schema inside `temp` is missing.
    pub fn resolve_for_create_temporary(&self, parts: &[&str]) -> Result<QualifiedName> {
        let outside = || {
            Error::parser(format!(
                "TEMPORARY table names can *only* use the \"{TEMP_CATALOG}\" catalog"
            ))
        };
        let name = match parts {
            [table] => QualifiedName::new(TEMP_CATALOG, DEFAULT_SCHEMA, *table),
            [first, table] if same_name(first, TEMP_CATALOG) => {
                QualifiedName::new(TEMP_CATALOG, DEFAULT_SCHEMA, *table)
            }
            // A database that is attached is a database the writer meant, so naming it is the
            // refusal rather than a schema of that name being missing.
            [first, _] if self.database(first).is_ok() => return Err(outside()),
            [first, table] => QualifiedName::new(TEMP_CATALOG, *first, *table),
            [catalog, schema, table] if same_name(catalog, TEMP_CATALOG) => {
                QualifiedName::new(TEMP_CATALOG, *schema, *table)
            }
            [catalog, _, _] if self.database(catalog).is_ok() => return Err(outside()),
            // A three part name whose first part is no database at all is read the way the pin
            // reads it, which is as a schema that is not there.
            [catalog, _, _] => {
                return Err(Error::catalog(format!("Schema with name {catalog} does not exist!")));
            }
            _ => {
                return Err(Error::catalog(format!(
                    "a name of {} parts, and a table name has one, two or three",
                    parts.len()
                )));
            }
        };
        self.schema(&name.catalog, &name.schema)?;
        Ok(name)
    }

    /// Every table a database file would hold, which is every table that is not temporary.
    ///
    /// A checkpoint writes what this returns and nothing else. A temporary table is gone when the
    /// database closes, so writing it would leave rows in the file that the next open reports as a
    /// table nobody asked for, and the questions the checkpoint asks about whether the file is
    /// already up to date are asked over the same set or they would never agree.
    pub fn stored_tables(&self) -> impl Iterator<Item = &Table> {
        self.tables().filter(|table| !table.name().temporary())
    }

    /// Every table, in creation order within a schema.
    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.databases
            .iter()
            .flat_map(|database| database.schemas.iter())
            .flat_map(|schema| schema.tables.iter())
    }

    /// The native mirror of the Parquet file at `path`, where there is one made from the file as
    /// it is now.
    ///
    /// `path` is canonical and `stamp` is what the file system says about it now. A mirror of the
    /// file as it was before somebody wrote it has a different stamp and is not an answer.
    #[must_use]
    pub fn mirror(
        &self,
        path: &str,
        binary_as_string: bool,
        stamp: FileStamp,
    ) -> Option<&QualifiedName> {
        self.mirrors
            .iter()
            .find(|mirror| {
                mirror.path == path
                    && mirror.binary_as_string == binary_as_string
                    && mirror.stamp == stamp
            })
            .map(|mirror| mirror.table.name())
    }

    /// Reads the Parquet file at `path` through `reader` from here on, while its stamp holds.
    ///
    /// A mirror of the same file under the same options that is already here is replaced, since it
    /// was made from the file before it last changed.
    ///
    /// # Errors
    ///
    /// When the mirror's columns cannot be a table's, which a mirror written by the load path does
    /// not produce.
    pub fn add_mirror(
        &mut self,
        path: &str,
        binary_as_string: bool,
        stamp: FileStamp,
        reader: NativeReader,
    ) -> Result<()> {
        self.changed();
        let at = self
            .mirrors
            .iter()
            .position(|mirror| mirror.path == path && mirror.binary_as_string == binary_as_string)
            .unwrap_or(self.mirrors.len());
        let table = Table::native(Mirror::name(at), reader)?;
        let mirror = Mirror { path: path.to_string(), binary_as_string, stamp, table };
        if at == self.mirrors.len() {
            self.mirrors.push(mirror);
        } else {
            self.mirrors[at] = mirror;
        }
        Ok(())
    }

    /// The tables of every mirror, which the optimizer counts rows and distinct values in the way
    /// it does for the schemas' own.
    pub fn mirrored_tables(&self) -> impl Iterator<Item = &Table> {
        self.mirrors.iter().map(|mirror| &mirror.table)
    }

    /// Every row order declaration the catalog holds, in the spelling `SET cluster_by` takes.
    ///
    /// The read half of that setting, and the reason nothing about it is kept in the session. A
    /// file carries the declarations of the tables in it, so a database opened on one has
    /// declarations that no `SET` in this session made, and a session copy would answer that there
    /// are none. Built out of the catalog, the answer is the same whichever way the declaration got
    /// there, and it is empty for a database where nothing is declared.
    ///
    /// Here rather than beside the setting because this is where the declarations are, and both the
    /// Rust reader and the binder folding `current_setting('cluster_by')` need it.
    #[must_use]
    pub fn clustering(&self) -> String {
        self.tables()
            .filter_map(|table| {
                let clustering = table.clustering()?;
                let names =
                    table.columns().iter().map(|field| field.name.clone()).collect::<Vec<_>>();
                Some(format!("{}({})", table.name().table, clustering.describe(&names)))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Every view a database file would hold, which is every view a person made in a real database.
    ///
    /// The filter is on the database rather than on the name, unlike [`Catalog::stored_tables`],
    /// because views have two kinds to leave out and tables only have one. `temp` holds the ones
    /// somebody made that go when the session does, and `system` holds the ones the engine ships
    /// with, which are in every catalog already and would come back doubled if a file named them.
    /// Both of those are the internal databases, so one question answers both.
    pub fn stored_views(&self) -> impl Iterator<Item = &View> {
        self.databases
            .iter()
            .filter(|database| !database.internal)
            .flat_map(|database| database.schemas.iter())
            .flat_map(|schema| schema.views.iter())
    }

    /// Every view, in creation order within a schema, the engine's own included.
    pub fn views(&self) -> impl Iterator<Item = &View> {
        self.databases
            .iter()
            .flat_map(|database| database.schemas.iter())
            .flat_map(|schema| schema.views.iter())
    }

    /// The readings of a written name, best first.
    ///
    /// The tail of both lists is the search path, which is the reason `information_schema.tables`
    /// and a bare `duckdb_views` find anything at all: neither is in the database a session creates
    /// in, and a name that is not found where it was written is looked for in `system` before it is
    /// reported missing. Upstream's path is `temp.main`, the current database's `main`, `system.main`
    /// and `system.pg_catalog`, which `current_schemas(true)` prints.
    ///
    /// `temp.main` comes first, which is what makes a temporary table shadow a stored one of the
    /// same name. Both exist at once and a bare name finds the temporary one, so `CREATE TABLE t`
    /// after `CREATE TEMPORARY TABLE t` makes a second table rather than complaining, and the
    /// `SELECT` that follows reads the temporary one. The catalog a bare `CREATE` writes into is
    /// still the default one, so the two directions genuinely differ and the pin differs the same
    /// way.
    fn candidates(&self, parts: &[&str]) -> Result<Vec<QualifiedName>> {
        let mut readings = self.written(parts)?;
        // Only a name that did not say which database it meant can land in `temp`, so a one part
        // name gets the temporary schema in front and a two part name gets it as another schema to
        // try. A three part name said the database out loud and is left alone.
        match parts {
            [table] => readings.insert(0, QualifiedName::new(TEMP_CATALOG, DEFAULT_SCHEMA, *table)),
            [first, table] => readings.insert(0, QualifiedName::new(TEMP_CATALOG, *first, *table)),
            _ => {}
        }
        Ok(readings)
    }

    /// The readings of a written name with the temporary schema left out, which is what a `CREATE`
    /// wants.
    ///
    /// A bare `CREATE TABLE t` writes into the default database even when a temporary `t` is in
    /// scope, so the list a create resolves against is the one that existed before `temp` held
    /// anything. `CREATE TEMPORARY` does not come through here at all; it has
    /// [`Catalog::resolve_for_create_temporary`].
    fn written(&self, parts: &[&str]) -> Result<Vec<QualifiedName>> {
        match parts {
            [table] => {
                // The pin's order: the path, then the default database's `main`, then the two
                // schemas of `system`.
                let mut readings: Vec<QualifiedName> = self
                    .search
                    .iter()
                    .map(|entry| {
                        let (catalog, schema) = self.searched(entry);
                        QualifiedName::new(catalog, schema, *table)
                    })
                    .collect();
                readings.push(QualifiedName::new(
                    &self.default_catalog,
                    &self.default_schema,
                    *table,
                ));
                readings.push(QualifiedName::new(SYSTEM_CATALOG, DEFAULT_SCHEMA, *table));
                readings.push(QualifiedName::new(SYSTEM_CATALOG, PG_CATALOG, *table));
                Ok(readings)
            }
            [first, table] => {
                // A database named on its own is read in the schema the path has for it, and in
                // its `main` when the path has none.
                let schema = self
                    .search
                    .iter()
                    .find(|entry| same_name(&self.searched(entry).0, first))
                    .map_or(self.default_schema.as_str(), |entry| entry.schema.as_str());
                let mut readings = vec![QualifiedName::new(self.default_catalog(), *first, *table)];
                if !same_name(self.default_catalog(), &self.default_catalog) {
                    readings.push(QualifiedName::new(&self.default_catalog, *first, *table));
                }
                readings.push(QualifiedName::new(*first, schema, *table));
                readings.push(QualifiedName::new(SYSTEM_CATALOG, *first, *table));
                Ok(readings)
            }
            [catalog, schema, table] => Ok(vec![QualifiedName::new(*catalog, *schema, *table)]),
            _ => Err(Error::catalog(format!(
                "a name of {} parts, and a table name has one, two or three",
                parts.len()
            ))),
        }
    }

    fn database(&self, catalog: &str) -> Result<&Database> {
        self.databases
            .iter()
            .find(|held| same_name(&held.name, catalog))
            .ok_or_else(|| Error::catalog(format!("Catalog with name {catalog} does not exist!")))
    }

    fn database_mut(&mut self, catalog: &str) -> Result<&mut Database> {
        self.databases
            .iter_mut()
            .find(|held| same_name(&held.name, catalog))
            .ok_or_else(|| Error::catalog(format!("Catalog with name {catalog} does not exist!")))
    }

    fn schema(&self, catalog: &str, schema: &str) -> Result<&Schema> {
        self.database(catalog)?
            .schemas
            .iter()
            .find(|held| same_name(&held.name, schema))
            .ok_or_else(|| Error::catalog(format!("Schema with name {schema} does not exist!")))
    }

    fn schema_mut(&mut self, catalog: &str, schema: &str) -> Result<&mut Schema> {
        self.database_mut(catalog)?
            .schemas
            .iter_mut()
            .find(|held| same_name(&held.name, schema))
            .ok_or_else(|| Error::catalog(format!("Schema with name {schema} does not exist!")))
    }
}

/// The `system` database with the views the engine ships with in it, and the next free oid.
///
/// Built whole rather than through [`Catalog::create_view`], because a create can fail and this one
/// cannot: the schemas it puts things in are the three made in its first three lines.
fn system(mut oid: i64) -> (Database, i64) {
    let mut main = Schema::empty(DEFAULT_SCHEMA, 4);
    let mut standard = Schema::empty(INFORMATION_SCHEMA, 5);
    let mut postgres = Schema::empty(PG_CATALOG, 6);
    for view in INTERNAL_VIEWS {
        let name = QualifiedName::new(SYSTEM_CATALOG, view.schema, view.name);
        // Nothing is bound here, so the column list is empty and stays that way until somebody reads
        // the view. That is the pin's answer too, where a fresh session reports `is_bound` false for
        // every one of these and reading one fills it in.
        let mut made =
            View::new(name, view.sql.to_string(), statement(view), Vec::new(), Vec::new());
        made.stamp(oid);
        oid += 1;
        match view.schema {
            INFORMATION_SCHEMA => standard.views.push(made),
            PG_CATALOG => postgres.views.push(made),
            _ => main.views.push(made),
        }
    }
    let database = Database {
        name: SYSTEM_CATALOG.to_string(),
        schemas: vec![main, standard, postgres],
        oid: 3,
        internal: true,
    };
    (database, oid)
}

/// The error for creating something in a database the engine owns.
///
/// Upstream reports this from the binder rather than from the catalog, and it names the catalog
/// rather than the schema, so `CREATE TABLE pg_catalog.x` and `CREATE TABLE system.main.x` are the
/// same sentence.
fn in_the_system_catalog() -> Error {
    Error::binder("Cannot create entry in system catalog")
}

fn missing_table(name: &str) -> Error {
    Error::catalog(format!("Table with name {name} does not exist!"))
}

/// The error for a name that is not there, named after what was being looked for.
///
/// A read says table whatever the name turns out to be, because a query that reads from `v` is
/// asking for a table and does not know or care that `v` could have been a view. A drop says which
/// of the two it was dropping, because `DROP VIEW` said so.
fn missing(wanted: Entry, name: &str) -> Error {
    Error::catalog(format!("{wanted} with name {name} does not exist!"))
}

/// The error for creating something over a name that is already taken.
///
/// The type in the sentence is the one that is already there, not the one being created. `CREATE
/// TABLE v` over an existing view `v` is `View with name "v" already exists!` and `CREATE VIEW t`
/// over an existing table `t` is `Table with name "t" already exists!`, both measured against
/// v2.0.0-dev84237 at cc7e7bac7f, which is the commit the grammar is vendored from.
///
/// It went the other way round in v1.5.1, where the sentence named the type being created. That
/// reads backwards and upstream changed it, which is the argument for pinning the reference to the
/// vendored commit rather than to whatever is released.
fn taken(found: Entry, name: &str) -> Error {
    Error::catalog(format!("{found} with name \"{name}\" already exists!"))
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::*;
    use crate::view::View;

    fn with_hits() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(
                QualifiedName::new("memory", "main", "hits"),
                vec![
                    Field::new("UserID", LogicalType::BigInt),
                    Field::new("SearchPhrase", LogicalType::Varchar),
                ],
            )
            .expect("a table in the default schema");
        catalog
    }

    #[test]
    fn a_fresh_catalog_has_the_in_memory_database_in_it() {
        let catalog = Catalog::new();
        assert_eq!(catalog.default_catalog(), "memory");
        assert_eq!(catalog.default_schema(), "main");
        // Three databases and five schemas, which is the pin's count from a session that has
        // attached nothing, and no tables, because everything the engine ships with is a view.
        assert_eq!(catalog.databases().len(), 3);
        assert_eq!(catalog.databases().iter().flat_map(Database::schemas).count(), 5);
        assert_eq!(catalog.tables().count(), 0);
        assert!(!catalog.databases()[0].internal(), "memory is the one a person creates in");
        assert!(catalog.databases()[1].internal(), "system is the engine's");
    }

    /// The views a session has without making any, and the two rules about where they live.
    #[test]
    fn the_system_catalog_holds_the_views_the_engine_ships_with() {
        let catalog = Catalog::new();
        let views: Vec<&View> = catalog
            .databases()
            .iter()
            .flat_map(Database::schemas)
            .flat_map(Schema::views)
            .collect();
        assert_eq!(views.len(), INTERNAL_VIEWS.len());
        assert!(
            views.iter().all(|view| same_name(&view.name().catalog, SYSTEM_CATALOG)),
            "every one of them is in the system catalog"
        );
        // Unbound, which is what `is_bound` reports and what the pin reports from a fresh session.
        assert!(views.iter().all(|view| view.columns().is_empty()));
    }

    /// The search path, which is the reason a name that is nowhere a person put anything resolves.
    #[test]
    fn a_name_the_engine_owns_is_found_without_being_written_out() {
        let catalog = Catalog::new();
        let found = catalog.resolve(&["duckdb_views"]).expect("a wrapper in system.main");
        assert_eq!(found.catalog, "system");
        assert_eq!(found.schema, "main");
        let found = catalog.resolve(&["information_schema", "tables"]).expect("a standard view");
        assert_eq!(found.catalog, "system");
        assert_eq!(found.schema, "information_schema");
    }

    /// Nothing goes into a database the engine owns, whichever way the name is written.
    #[test]
    fn the_system_catalog_refuses_what_a_statement_would_create_in_it() {
        let mut catalog = Catalog::new();
        for parts in
            [vec!["information_schema", "x"], vec!["pg_catalog", "x"], vec!["system", "main", "x"]]
        {
            let error = catalog.resolve_for_create(&parts).expect_err("the system catalog");
            assert_eq!(error.message(), "Cannot create entry in system catalog", "{parts:?}");
        }
        let error = catalog.resolve_for_create(&["temp", "main", "x"]).expect_err("the temp one");
        assert!(error.message().contains("Only TEMPORARY table names"), "{error}");
        let error = catalog.create_schema("system", "s").expect_err("a schema in system");
        assert_eq!(error.message(), "Cannot create schema in system catalog");
    }

    /// And nothing comes out of one either, which is a different sentence from a missing name.
    #[test]
    fn a_view_the_engine_owns_cannot_be_dropped() {
        let mut catalog = Catalog::new();
        let name = catalog.resolve(&["duckdb_views"]).expect("a wrapper in system.main");
        let error = catalog.drop_view(&name).expect_err("an internal entry");
        assert_eq!(error.message(), "Cannot drop internal catalog entry \"duckdb_views\"!");
    }

    /// The property `duckdb_schemas()` and `duckdb_tables()` are built on top of, checked here
    /// because this is the only place that hands a number out.
    #[test]
    fn no_two_entries_carry_the_same_oid() {
        let mut catalog = with_hits();
        catalog
            .create_table(
                QualifiedName::new("memory", "main", "visits"),
                vec![Field::new("id", LogicalType::BigInt)],
            )
            .expect("a second table");
        catalog.create_schema("memory", "s").expect("a fresh schema");
        let database = &catalog.databases()[0];
        let mut oids = vec![database.oid()];
        oids.extend(database.schemas().iter().map(Schema::oid));
        oids.extend(catalog.tables().map(Table::oid));
        let mut sorted = oids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), oids.len(), "{oids:?}");
        assert!(oids.iter().all(|oid| *oid != DETACHED), "{oids:?}");
    }

    #[test]
    fn an_unqualified_name_resolves_in_the_default_schema() {
        let catalog = with_hits();
        let name = catalog.resolve(&["HITS"]).expect("the table exists whatever the case");
        assert_eq!(name.to_string(), "memory.main.hits");
    }

    #[test]
    fn a_three_part_name_resolves_to_itself() {
        let catalog = with_hits();
        let name = catalog.resolve(&["memory", "main", "hits"]).expect("the full name");
        assert_eq!(name.to_string(), "memory.main.hits");
    }

    /// Two parts are a schema and a table before they are a catalog and a table, which is what
    /// makes `information_schema.tables` work, and they fall back to a catalog and a table, which
    /// is what makes `memory.hits` work.
    #[test]
    fn two_parts_are_a_schema_first_and_a_catalog_second() {
        let mut catalog = with_hits();
        catalog.create_schema("memory", "reporting").expect("a second schema");
        catalog
            .create_table(
                QualifiedName::new("memory", "reporting", "hits"),
                vec![Field::new("n", LogicalType::Integer)],
            )
            .expect("a table in it");

        let by_schema = catalog.resolve(&["reporting", "hits"]).expect("the reporting one");
        assert_eq!(by_schema.to_string(), "memory.reporting.hits");

        let by_catalog = catalog.resolve(&["memory", "hits"]).expect("the default schema one");
        assert_eq!(by_catalog.to_string(), "memory.main.hits");
    }

    #[test]
    fn a_table_that_is_not_there_says_so_the_way_duckdb_does() {
        let catalog = with_hits();
        let error = catalog.resolve(&["nope"]).expect_err("there is no table called nope");
        assert_eq!(error.to_string(), "Catalog Error: Table with name nope does not exist!");
    }

    #[test]
    fn a_schema_that_is_not_there_says_which_schema() {
        let catalog = with_hits();
        let error =
            catalog.resolve(&["memory", "nope", "hits"]).expect_err("there is no schema nope");
        assert_eq!(
            error.to_string(),
            "Catalog Error: Table with name \"nope.hits\" does not exist because schema \"nope\" does not exist."
        );
    }

    #[test]
    fn creating_the_same_table_twice_is_an_error() {
        let mut catalog = with_hits();
        let error = catalog
            .create_table(
                QualifiedName::new("memory", "main", "HITS"),
                vec![Field::new("n", LogicalType::Integer)],
            )
            .expect_err("hits is already there");
        assert_eq!(error.to_string(), "Catalog Error: Table with name \"HITS\" already exists!");
    }

    #[test]
    fn a_dropped_table_is_gone() {
        let mut catalog = with_hits();
        let name = catalog.resolve(&["hits"]).expect("it is there");
        catalog.drop_table(&name).expect("dropping it works");
        assert!(catalog.resolve(&["hits"]).is_err(), "it is not there any more");
        assert!(catalog.drop_table(&name).is_err(), "dropping it twice does not");
    }

    #[test]
    fn a_name_for_a_create_does_not_have_to_exist_yet() {
        let catalog = Catalog::new();
        let name = catalog.resolve_for_create(&["new_table"]).expect("the default schema is there");
        assert_eq!(name.to_string(), "memory.main.new_table");
        assert!(
            catalog.resolve_for_create(&["nope", "new_table"]).is_err(),
            "a schema that is not there is still an error"
        );
    }

    #[test]
    fn a_name_of_four_parts_is_not_a_table_name() {
        let catalog = Catalog::new();
        let error = catalog.resolve(&["a", "b", "c", "d"]).expect_err("four parts");
        assert!(error.message().contains("4 parts"), "{error}");
    }

    fn with_view() -> Catalog {
        let mut catalog = with_hits();
        catalog
            .create_view(View::new(
                QualifiedName::new("memory", "main", "recent"),
                "SELECT * FROM hits".to_string(),
                "CREATE VIEW recent AS SELECT * FROM hits;".to_string(),
                Vec::new(),
                Vec::new(),
            ))
            .expect("a view in the default schema");
        catalog
    }

    #[test]
    fn a_view_resolves_the_way_a_table_does() {
        let catalog = with_view();
        let name = catalog.resolve(&["RECENT"]).expect("the view, whatever the case");
        assert_eq!(name.to_string(), "memory.main.recent");
        assert_eq!(catalog.entry(&name).expect("it is there"), Entry::View);
        assert_eq!(catalog.view(&name).expect("the body").sql(), "SELECT * FROM hits");
    }

    #[test]
    fn the_two_share_one_namespace_and_the_message_names_what_was_being_made() {
        let mut catalog = with_view();
        let error = catalog
            .create_table(
                QualifiedName::new("memory", "main", "recent"),
                vec![Field::new("n", LogicalType::Integer)],
            )
            .expect_err("recent is a view");
        assert_eq!(error.to_string(), "Catalog Error: View with name \"recent\" already exists!");

        let error = catalog
            .create_view(View::new(
                QualifiedName::new("memory", "main", "HITS"),
                "SELECT 1".to_string(),
                "CREATE VIEW HITS AS SELECT 1;".to_string(),
                Vec::new(),
                Vec::new(),
            ))
            .expect_err("hits is a table");
        assert_eq!(error.to_string(), "Catalog Error: Table with name \"HITS\" already exists!");
    }

    #[test]
    fn dropping_one_as_the_other_names_both_types() {
        let mut catalog = with_view();
        let view = catalog.resolve(&["recent"]).expect("the view");
        let error = catalog.drop_table(&view).expect_err("it is a view");
        assert_eq!(
            error.to_string(),
            "Catalog Error: Existing object \"recent\" is of type View, trying to drop type Table"
        );
        let table = catalog.resolve(&["hits"]).expect("the table");
        let error = catalog.drop_view(&table).expect_err("it is a table");
        assert_eq!(
            error.to_string(),
            "Catalog Error: Existing object \"hits\" is of type Table, trying to drop type View"
        );
        catalog.drop_view(&view).expect("dropping it as what it is");
        assert!(catalog.resolve(&["recent"]).is_err(), "it is gone");
    }

    #[test]
    fn attaching_gives_a_second_database_with_its_own_main() {
        let mut catalog = with_hits();
        catalog.attach("other").expect("a second database");
        catalog
            .create_table(
                QualifiedName::new("other", "main", "hits"),
                vec![Field::new("n", LogicalType::Integer)],
            )
            .expect("a table of the same name in it");
        assert_eq!(catalog.tables().count(), 2);
        let name = catalog.resolve(&["other", "main", "hits"]).expect("the other one");
        assert_eq!(name.catalog, "other");
        assert!(catalog.attach("OTHER").is_err(), "attaching it twice does not work");
    }

    /// Every way of writing a temporary name that the pin accepts lands in `temp.main`, and the
    /// ones it refuses are refused with the sentence it uses.
    #[test]
    fn a_temporary_create_resolves_into_the_temp_database_or_is_refused() {
        let mut catalog = Catalog::new();
        catalog.attach("other").expect("a second database");
        for parts in [
            vec!["t"],
            vec!["temp", "t"],
            vec!["TEMP", "t"],
            vec!["main", "t"],
            vec!["temp", "main", "t"],
        ] {
            let name = catalog.resolve_for_create_temporary(&parts).expect("a temporary name");
            assert!(
                name.same_as(&QualifiedName::new("temp", "main", "t")),
                "{parts:?} gave {name}"
            );
            assert!(name.temporary());
        }

        // Naming a database that is there is the writer saying where they meant, so it is the
        // refusal rather than a schema of that name being looked for.
        for parts in [vec!["other", "t"], vec!["memory", "main", "t"], vec!["other", "main", "t"]] {
            let error =
                catalog.resolve_for_create_temporary(&parts).expect_err("not the temp database");
            assert!(error.to_string().contains("can *only* use"), "{parts:?} gave {error}");
        }

        // A first part that is no database at all reads as a schema, and `temp` holds only `main`.
        let error = catalog.resolve_for_create_temporary(&["nope", "t"]).expect_err("no schema");
        assert!(error.to_string().contains("nope"), "{error}");
    }

    /// The temporary reading of a name comes first when reading and is left out when writing, which
    /// is how `CREATE TABLE t` makes a second `t` while `SELECT ... FROM t` still finds the first.
    #[test]
    fn a_read_tries_the_temp_database_first_and_a_create_does_not() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(
                QualifiedName::new("memory", "main", "t"),
                vec![Field::new("n", LogicalType::Integer)],
            )
            .expect("a stored table");
        assert_eq!(catalog.resolve(&["t"]).expect("the stored one").catalog, "memory");

        catalog
            .create_table(
                catalog.resolve_for_create_temporary(&["t"]).expect("a temporary name"),
                vec![Field::new("n", LogicalType::Integer)],
            )
            .expect("a temporary table");
        assert_eq!(catalog.resolve(&["t"]).expect("the temporary one").catalog, "temp");
        assert_eq!(
            catalog.resolve(&["main", "t"]).expect("still the temporary one").catalog,
            "temp"
        );
        assert_eq!(
            catalog.resolve(&["memory", "main", "t"]).expect("the stored one").catalog,
            "memory"
        );

        // The create still writes into `memory`, which is the direction that does not follow the
        // read, and both tables exist at once.
        let name = catalog.resolve_for_create(&["t"]).expect("a create name");
        assert_eq!(name.catalog, "memory");
        assert_eq!(catalog.tables().count(), 2);
        assert_eq!(catalog.stored_tables().count(), 1);
    }

    /// A temporary table can be dropped even though the database holding it is the engine's, which
    /// is the one place the guard on internal databases has to stand aside.
    #[test]
    fn a_temporary_table_can_be_dropped_and_a_system_one_cannot() {
        let mut catalog = Catalog::new();
        let name = catalog.resolve_for_create_temporary(&["t"]).expect("a temporary name");
        catalog
            .create_table(name.clone(), vec![Field::new("n", LogicalType::Integer)])
            .expect("a temporary table");
        catalog.drop_table(&name).expect("a person wrote it and a person drops it");
        assert_eq!(catalog.tables().count(), 0);

        let system = catalog.resolve(&["duckdb_tables"]).expect("a system table").clone();
        assert!(catalog.drop_table(&system).is_err(), "the engine's own stays");
    }

    #[test]
    fn the_generation_starts_at_one_and_moves_on_for_everything_that_can_change_a_read() {
        // Zero is reserved for a set of facts nobody read a catalog for, so a fresh one is one.
        let mut catalog = Catalog::new();
        assert_eq!(catalog.generation(), 1);

        let mut seen = vec![catalog.generation()];
        catalog.attach("other").expect("a second database");
        seen.push(catalog.generation());
        catalog.create_schema("other", "extra").expect("a schema in it");
        seen.push(catalog.generation());
        catalog
            .create_table(
                QualifiedName::new("memory", "main", "hits"),
                vec![Field::new("n", LogicalType::Integer)],
            )
            .expect("a table");
        seen.push(catalog.generation());
        let name = catalog.resolve(&["hits"]).expect("the table");
        catalog.table_mut(&name).expect("it is there").rows_mut();
        seen.push(catalog.generation());
        catalog
            .create_view(View::new(
                QualifiedName::new("memory", "main", "recent"),
                "SELECT * FROM hits".to_string(),
                "CREATE VIEW recent AS SELECT * FROM hits;".to_string(),
                Vec::new(),
                Vec::new(),
            ))
            .expect("a view");
        seen.push(catalog.generation());
        catalog.drop_view(&catalog.resolve(&["recent"]).expect("the view").clone()).expect("gone");
        seen.push(catalog.generation());
        catalog.drop_table(&name).expect("gone too");
        seen.push(catalog.generation());

        // Every step is a number nobody else has. Handing the same number out for two different
        // catalogs is the failure this has to avoid, and counting a step that changed nothing only
        // costs a rebuild.
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "{seen:?}");

        // Reading does not move it, which is the whole point: two statements that read the same
        // catalog plan from the same counts.
        let before = catalog.generation();
        assert_eq!(catalog.tables().count(), 0);
        assert_eq!(catalog.default_schema(), "main");
        assert_eq!(catalog.generation(), before);
    }
}

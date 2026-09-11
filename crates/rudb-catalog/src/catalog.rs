//! The catalog: attached databases, their schemas, and the tables in them.

use std::fmt;

use rudb_common::{Error, Field, Result};

use crate::name::{QualifiedName, same_name};
use crate::table::Table;
use crate::view::View;

/// The default attached database, which is the one an in-memory session gets.
pub const DEFAULT_CATALOG: &str = "memory";
/// The default schema inside it.
pub const DEFAULT_SCHEMA: &str = "main";

/// One attached database.
#[derive(Debug, Clone)]
pub struct Database {
    name: String,
    schemas: Vec<Schema>,
}

impl Database {
    /// The name it is attached as.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
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

/// One schema.
#[derive(Debug, Clone)]
pub struct Schema {
    name: String,
    tables: Vec<Table>,
    views: Vec<View>,
}

impl Schema {
    /// A schema of that name with nothing in it.
    fn empty(name: &str) -> Self {
        Self { name: name.to_string(), tables: Vec::new(), views: Vec::new() }
    }

    /// The schema name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
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
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

impl Catalog {
    /// A catalog with `memory.main` in it and nothing else, which is what an in-memory session
    /// starts from.
    #[must_use]
    pub fn new() -> Self {
        Self {
            databases: vec![Database {
                name: DEFAULT_CATALOG.to_string(),
                schemas: vec![Schema::empty(DEFAULT_SCHEMA)],
            }],
            default_catalog: DEFAULT_CATALOG.to_string(),
            default_schema: DEFAULT_SCHEMA.to_string(),
        }
    }

    /// The catalog an unqualified name resolves in.
    #[must_use]
    pub fn default_catalog(&self) -> &str {
        &self.default_catalog
    }

    /// The schema an unqualified name resolves in.
    #[must_use]
    pub fn default_schema(&self) -> &str {
        &self.default_schema
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
        if self.databases.iter().any(|held| same_name(&held.name, name)) {
            return Err(Error::catalog(format!("Database with name \"{name}\" already exists!")));
        }
        self.databases.push(Database {
            name: name.to_string(),
            schemas: vec![Schema::empty(DEFAULT_SCHEMA)],
        });
        Ok(())
    }

    /// Creates a schema in an attached database.
    ///
    /// # Errors
    ///
    /// If the database is not attached, or a schema of that name is already in it.
    pub fn create_schema(&mut self, catalog: &str, name: &str) -> Result<()> {
        let database = self.database_mut(catalog)?;
        if database.schemas.iter().any(|held| same_name(&held.name, name)) {
            return Err(Error::catalog(format!("Schema with name \"{name}\" already exists!")));
        }
        database.schemas.push(Schema::empty(name));
        Ok(())
    }

    /// Creates an empty table.
    ///
    /// # Errors
    ///
    /// If the database or the schema is missing, if a table or a view of that name is already
    /// there, or if two columns have the same name.
    pub fn create_table(&mut self, name: QualifiedName, columns: Vec<Field>) -> Result<()> {
        let table = Table::new(name.clone(), columns)?;
        let schema = self.schema_mut(&name.catalog, &name.schema)?;
        if let Some(found) = schema.kind(&name.table) {
            return Err(taken(found, &name.table));
        }
        schema.tables.push(table);
        Ok(())
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
    pub fn create_view(&mut self, view: View) -> Result<()> {
        let name = view.name().clone();
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
        self.drop_entry(name, Entry::Table)
    }

    /// Removes a view.
    ///
    /// # Errors
    ///
    /// If there is no such view, or if the name is a table.
    pub fn drop_view(&mut self, name: &QualifiedName) -> Result<()> {
        self.drop_entry(name, Entry::View)
    }

    /// Removes whichever of the two the caller said it was dropping, refusing the other one.
    fn drop_entry(&mut self, name: &QualifiedName, wanted: Entry) -> Result<()> {
        let schema = self.schema_mut(&name.catalog, &name.schema)?;
        match schema.kind(&name.table) {
            // The type in this one is the type being dropped, so `DROP VIEW gone` is a missing view
            // and `DROP TABLE gone` is a missing table over the same absent name.
            None => Err(missing(wanted, &name.table)),
            Some(found) if found != wanted => Err(Error::catalog(format!(
                "Existing object \"{}\" is of type {found}, trying to drop type {wanted}",
                name.table
            ))),
            Some(Entry::Table) => {
                schema.tables.retain(|held| !same_name(&held.name().table, &name.table));
                Ok(())
            }
            Some(Entry::View) => {
                schema.views.retain(|held| !same_name(&held.name().table, &name.table));
                Ok(())
            }
        }
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
                // the piece the writer most likely meant and got wrong.
                Err(error) => {
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
        let candidates = self.candidates(parts)?;
        let mut first_error = None;
        for candidate in &candidates {
            match self.schema(&candidate.catalog, &candidate.schema) {
                Ok(_) => return Ok(candidate.clone()),
                Err(error) => first_error = first_error.or(Some(error)),
            }
        }
        Err(first_error.unwrap_or_else(|| {
            Error::catalog(format!("Schema with name {} does not exist!", parts.join(".")))
        }))
    }

    /// Every table, in creation order within a schema.
    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.databases
            .iter()
            .flat_map(|database| database.schemas.iter())
            .flat_map(|schema| schema.tables.iter())
    }

    /// The readings of a written name, best first.
    fn candidates(&self, parts: &[&str]) -> Result<Vec<QualifiedName>> {
        match parts {
            [table] => {
                Ok(vec![QualifiedName::new(&self.default_catalog, &self.default_schema, *table)])
            }
            [first, table] => Ok(vec![
                QualifiedName::new(&self.default_catalog, *first, *table),
                QualifiedName::new(*first, &self.default_schema, *table),
            ]),
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
        assert_eq!(catalog.databases().len(), 1);
        assert_eq!(catalog.tables().count(), 0);
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
        assert_eq!(error.to_string(), "Catalog Error: Schema with name nope does not exist!");
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
}

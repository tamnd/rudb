//! The catalog: attached databases, their schemas, and the tables in them.

use rudb_common::{Error, Field, Result};

use crate::name::{QualifiedName, same_name};
use crate::table::Table;

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

/// One schema.
#[derive(Debug, Clone)]
pub struct Schema {
    name: String,
    tables: Vec<Table>,
}

impl Schema {
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
                schemas: vec![Schema { name: DEFAULT_SCHEMA.to_string(), tables: Vec::new() }],
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
            schemas: vec![Schema { name: DEFAULT_SCHEMA.to_string(), tables: Vec::new() }],
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
        database.schemas.push(Schema { name: name.to_string(), tables: Vec::new() });
        Ok(())
    }

    /// Creates an empty table.
    ///
    /// # Errors
    ///
    /// If the database or the schema is missing, if a table of that name is already there, or if
    /// two columns have the same name.
    pub fn create_table(&mut self, name: QualifiedName, columns: Vec<Field>) -> Result<()> {
        let table = Table::new(name.clone(), columns)?;
        let schema = self.schema_mut(&name.catalog, &name.schema)?;
        if schema.tables.iter().any(|held| same_name(&held.name().table, &name.table)) {
            return Err(Error::catalog(format!(
                "Table with name \"{}\" already exists!",
                name.table
            )));
        }
        schema.tables.push(table);
        Ok(())
    }

    /// Removes a table and everything in it.
    ///
    /// # Errors
    ///
    /// If there is no such table.
    pub fn drop_table(&mut self, name: &QualifiedName) -> Result<()> {
        let schema = self.schema_mut(&name.catalog, &name.schema)?;
        match schema.tables.iter().position(|held| same_name(&held.name().table, &name.table)) {
            Some(at) => {
                schema.tables.remove(at);
                Ok(())
            }
            None => Err(missing_table(&name.table)),
        }
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

    /// Turns the parts of a written name into the full name of a table that exists.
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
    /// If the name has no parts or more than three, or if it does not resolve to a table.
    pub fn resolve(&self, parts: &[&str]) -> Result<QualifiedName> {
        let candidates = self.candidates(parts)?;
        let mut first_error = None;
        for candidate in &candidates {
            match self.table(candidate) {
                Ok(table) => return Ok(table.name().clone()),
                // The first reading is the preferred one, so its complaint is the one that names
                // the piece the writer most likely meant and got wrong.
                Err(error) => first_error = first_error.or(Some(error)),
            }
        }
        Err(first_error.unwrap_or_else(|| missing_table(&parts.join("."))))
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

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::*;

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

//! The handle everything else hangs off.

use rudb_catalog::Catalog;
use rudb_common::{Error, Field, Result, Value};

use crate::result::QueryResult;

/// An in process database.
///
/// One catalog, held in memory, with no file behind it. `ATTACH` and the storage format are M2, and
/// the shape of this type does not change when they arrive: a database with a file behind it is a
/// catalog whose tables read from a block manager rather than from a `Vec` of chunks, which is a
/// change under [`rudb_catalog::Table`] and not a change here.
///
/// There is no separate connection type yet. DuckDB has one because a connection carries a
/// transaction, a set of temporary tables and a prepared statement cache, and none of those three
/// exist in M0. Adding `Connection` before there is anything to put in it would be an empty struct
/// that the API can never remove.
#[derive(Debug)]
pub struct Database {
    catalog: Catalog,
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

impl Database {
    /// An empty database with the default catalog and schema.
    #[must_use]
    pub fn new() -> Self {
        Self { catalog: Catalog::new() }
    }

    /// The catalog, for a caller that wants to look at what is defined.
    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The catalog, mutably.
    ///
    /// Public because the DDL statements do not exist yet and something has to be able to define a
    /// schema or attach a second database. It stays public afterwards, since a program that builds
    /// its own catalog rather than parsing SQL to build one is a real thing an embedded database
    /// gets used for.
    pub fn catalog_mut(&mut self) -> &mut Catalog {
        &mut self.catalog
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
    pub fn create_table(&mut self, name: &str, columns: Vec<Field>) -> Result<()> {
        let parts: Vec<&str> = name.split('.').collect();
        let resolved = self.catalog.resolve_for_create(&parts)?;
        self.catalog.create_table(resolved, columns)
    }

    /// Drops a table.
    ///
    /// # Errors
    ///
    /// If the name does not resolve or the table does not exist.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        let parts: Vec<&str> = name.split('.').collect();
        let resolved = self.catalog.resolve(&parts)?;
        self.catalog.drop_table(&resolved)
    }

    /// Appends rows to a table, each row left to right in the table's column order.
    ///
    /// This is the M0 write path, and it is deliberately the row shaped one, because the caller
    /// with rows in hand is the common case and the caller with columns in hand can reach
    /// [`Database::catalog_mut`] and append a [`rudb_vector::Chunk`] directly. Values are converted
    /// to the column's type on the way in, so an `Integer` lands in a `BIGINT` column.
    ///
    /// # Errors
    ///
    /// If the name does not resolve, if a row is not as wide as the table, or if a value cannot be
    /// converted to its column's type.
    pub fn append(&mut self, name: &str, rows: &[Vec<Value>]) -> Result<()> {
        let parts: Vec<&str> = name.split('.').collect();
        let resolved = self.catalog.resolve(&parts)?;
        self.catalog.table_mut(&resolved)?.rows_mut().append_rows(rows)
    }

    /// How many rows a table holds.
    ///
    /// # Errors
    ///
    /// If the name does not resolve or the table does not exist.
    pub fn table_len(&self, name: &str) -> Result<usize> {
        let parts: Vec<&str> = name.split('.').collect();
        let resolved = self.catalog.resolve(&parts)?;
        Ok(self.catalog.table(&resolved)?.rows().len())
    }

    /// Runs one query and returns every row it produced.
    ///
    /// Takes `&self`, so a query cannot change the database and two of them can run at once. See
    /// the crate documentation for why that is the whole of the concurrency story today.
    ///
    /// # Errors
    ///
    /// A parse error, a binder error, or anything the operators raise while running, which is
    /// mostly cast failures and arithmetic that leaves the range of its type.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        let plan = rudb_bind::bind_sql(sql, &self.catalog)?;
        let mut root = rudb_exec::build(&plan, &self.catalog)?;
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

    /// The plan for a query, in the textual form `spec/07-execution.md` describes, without running
    /// it.
    ///
    /// This is `EXPLAIN` before there is an `EXPLAIN`, and it is what the plan tests and the
    /// optimizer work in M1 read. The text round trips: `rudb_plan::Plan::parse` of this string
    /// gives back the plan it was printed from.
    ///
    /// # Errors
    ///
    /// A parse error or a binder error.
    pub fn plan(&self, sql: &str) -> Result<String> {
        let plan = rudb_bind::bind_sql(sql, &self.catalog)?;
        Ok(plan.to_string())
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
        let result = self.query(sql)?;
        if result.len() != 1 || result.width() != 1 {
            return Err(Error::invalid_input(format!(
                "expected one row of one column, got {} rows of {} columns",
                result.len(),
                result.width()
            )));
        }
        Ok(result.value_at(0, 0))
    }
}

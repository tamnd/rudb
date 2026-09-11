//! The handle everything else hangs off.

use rudb_bind::Bound;
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
        self.catalog.table_mut(&resolved)?.append_rows(rows)
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

    /// Every table in the database, unqualified, in creation order.
    ///
    /// Unqualified because that is what a person typing `.tables` wants to read and what they would
    /// then type into a query. Two tables of the same name in different schemas both appear, which
    /// is the same thing DuckDB's `.tables` does.
    #[must_use]
    pub fn table_names(&self) -> Vec<String> {
        self.catalog.tables().map(|table| table.name().table.clone()).collect()
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
        let resolved = self.catalog.resolve(&parts)?;
        let table = self.catalog.table(&resolved)?;
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
    /// Takes `&self`, so a query cannot change the database and two of them can run at once. See
    /// the crate documentation for why that is the whole of the concurrency story today.
    ///
    /// # Errors
    ///
    /// A parse error, a binder error, or anything the operators raise while running, which is
    /// mostly cast failures and arithmetic that leaves the range of its type.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        let plan = self.planned(sql)?;
        self.run(&plan)
    }

    /// A query bound and then optimized, which is the plan that runs.
    ///
    /// Both of the ways in are through here, so that what a plan dump shows is what the query does.
    /// A dump of the bound plan and a run of the optimized one would make the dump a description of
    /// something nobody executes, which is the one thing a plan dump must not be.
    fn planned(&self, sql: &str) -> Result<rudb_plan::Plan> {
        let mut plan = rudb_bind::bind_sql(sql, &self.catalog)?;
        rudb_opt::optimize(&mut plan)?;
        Ok(plan)
    }

    /// Runs one statement, which may change the database.
    ///
    /// This is [`Database::query`] plus the statements that write. A `SELECT` returns its rows, and
    /// a `CREATE TABLE`, a `DROP TABLE` or an `INSERT` returns an empty result, which is what
    /// DuckDB's own C API does for them. `INSERT` does not report a row count yet, because a row
    /// count wants a `Count` column in the result and that is the same shape as `RETURNING`, which
    /// is not bound yet either.
    ///
    /// # Errors
    ///
    /// A parse error, a binder error, a catalog error, or anything the operators raise.
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult> {
        match rudb_bind::bind_statement_sql(sql, &self.catalog)? {
            Bound::Query(mut plan) => {
                rudb_opt::optimize(&mut plan)?;
                self.run(&plan)
            }
            Bound::CreateTable(create) => {
                self.run_create_table(create)?;
                Ok(QueryResult::empty())
            }
            Bound::DropTable(drop) => {
                for name in &drop.names {
                    self.catalog.drop_table(name)?;
                }
                Ok(QueryResult::empty())
            }
            Bound::Insert(mut insert) => {
                // The source runs to completion before anything is appended, which is not an
                // implementation detail. `INSERT INTO t SELECT * FROM t` reads the table it writes,
                // and a version of this that appended chunk by chunk would either read its own
                // output forever or depend on how the scan holds its chunks.
                rudb_opt::optimize(&mut insert.source)?;
                let result = self.run(&insert.source)?;
                let table = self.catalog.table_mut(&insert.name)?;
                for chunk in result.chunks() {
                    table.append(chunk.clone())?;
                }
                Ok(QueryResult::empty())
            }
        }
    }

    /// The `CREATE TABLE` half of [`Database::execute`].
    fn run_create_table(&mut self, mut create: rudb_bind::CreateTable) -> Result<()> {
        if create.if_not_exists && self.catalog.table(&create.name).is_ok() {
            return Ok(());
        }
        // The query runs before the old table is dropped, so `CREATE OR REPLACE TABLE t AS SELECT
        // * FROM t` reads the table it is about to replace rather than the empty new one.
        let rows = match &mut create.source {
            Some(plan) => {
                rudb_opt::optimize(plan)?;
                Some(self.run(plan)?)
            }
            None => None,
        };
        if create.or_replace && self.catalog.table(&create.name).is_ok() {
            self.catalog.drop_table(&create.name)?;
        }
        self.catalog.create_table(create.name.clone(), create.columns)?;
        if let Some(rows) = rows {
            let table = self.catalog.table_mut(&create.name)?;
            for chunk in rows.chunks() {
                table.append(chunk.clone())?;
            }
        }
        Ok(())
    }

    /// Builds and drains one plan.
    fn run(&self, plan: &rudb_plan::Plan) -> Result<QueryResult> {
        let mut root = rudb_exec::build(plan, &self.catalog)?;
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
        Ok(self.planned(sql)?.to_string())
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

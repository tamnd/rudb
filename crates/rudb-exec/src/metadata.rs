//! A table whose rows are a fact about the engine rather than data somebody stored.
//!
//! `rudb_strategies()` was the first of these and `duckdb_keywords()`, `duckdb_types()`,
//! `duckdb_functions()` and `duckdb_settings()` followed it, and D2 has about eight more in it: the
//! schemas, the tables, the columns, the views, the databases, the extensions and the optimizer
//! passes. Every one of them is the same operator with a different list of rows behind it, so the
//! operator is here once and each table is the function that builds its rows.
//!
//! Four of the five build their rows out of something that cannot change while the process runs.
//! `duckdb_settings()` is the exception and it reads the session the query is being built with,
//! which is why [`crate::build_measured`] takes one.
//!
//! What the operator does is the part that is easy to get subtly wrong twelve times. The plan's
//! column list is resolved against the table's own by name, because the binder projects every
//! column in order today and the moment a pass trims the list the operator has to hand back the
//! trimmed one rather than its own first few columns. The rows are then cut into chunks of
//! [`VECTOR_SIZE`] and handed out one chunk per morsel, which is what lets a metadata table be
//! scanned by the same parallel driver as a real one without being a special case in it.
//!
//! The rows are built when the operator is. The largest of these tables is in the low thousands, so
//! there is nothing to stream, and a row that arrived halfway through a scan would be a table that
//! answers two different questions in one query. That is the reason the settings are read once when
//! the query is built rather than looked up per row, as well: a `SET` that landed between two chunks
//! would otherwise show up as two values in one result set.

use rudb_common::{Error, Field, Result, Value};
use rudb_pipeline::{Morsel, Progress, Source};
use rudb_plan::{Plan, Slice};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::schema::Schema;
use crate::source::{Handout, position};

/// The rows of one metadata table, in the columns the plan asked for.
#[derive(Debug)]
pub(crate) struct Metadata {
    schema: Schema,
    chunks: Vec<Chunk>,
    handout: Handout,
}

impl Metadata {
    /// Build the table from its full column list and its rows.
    ///
    /// `name` is only used to say which table a bad column list belongs to, since the message is
    /// for whoever is reading a panic in the binder rather than for whoever wrote the query.
    ///
    /// # Errors
    ///
    /// If the plan asks for a column this table does not have, which is a bug in the binder rather
    /// than anything a query can write, or if a row is a different width from the column list.
    pub(crate) fn new(
        name: &str,
        all: &[Field],
        rows: &[Vec<Value>],
        plan: &Plan,
        index: u32,
        columns: Slice,
    ) -> Result<Self> {
        let wanted = plan.field_list(columns).to_vec();
        let mut positions = Vec::with_capacity(wanted.len());
        for field in &wanted {
            let position =
                all.iter().position(|held| held.name == field.name).ok_or_else(|| {
                    Error::internal(format!("{name}() has no column named {}", field.name))
                })?;
            positions.push(position);
        }
        // Checked once here rather than trusted per row below, because a row of the wrong width
        // otherwise comes out as a panic on an index deep inside the chunk loop, with nothing in it
        // saying which table built the row.
        if let Some(row) = rows.iter().find(|row| row.len() != all.len()) {
            return Err(Error::internal(format!(
                "{name}() has {} columns and built a row of {}",
                all.len(),
                row.len()
            )));
        }
        let schema = Schema::numbered(wanted, index);

        let types = schema.types();
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < rows.len() {
            let end = (start + VECTOR_SIZE).min(rows.len());
            let mut built = Vec::with_capacity(types.len());
            for (wanted, ty) in positions.iter().zip(&types) {
                let column: Vec<Value> =
                    rows[start..end].iter().map(|row| row[*wanted].clone()).collect();
                built.push(Vector::from_values(ty.clone(), &column)?);
            }
            chunks.push(Chunk::with_rows(built, end - start)?);
            start = end;
        }
        let handout = Handout::new(chunks.len());
        Ok(Self { schema, chunks, handout })
    }

    /// The columns of the table, in the order the plan asked for them.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }
}

impl Source for Metadata {
    fn morsel(&self) -> Option<Morsel> {
        self.handout.take()
    }

    fn morsels(&self) -> Option<usize> {
        Some(self.handout.total())
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        *out = match self.chunks.get(position(morsel)) {
            Some(chunk) => chunk.clone(),
            None => Chunk::empty(&self.schema.types()),
        };
        morsel.advance(1);
        Ok(Progress::Done)
    }
}

/// A `VARCHAR` value, which is what most of a metadata table is made of.
pub(crate) fn text(value: &str) -> Value {
    Value::Varchar(value.to_string())
}

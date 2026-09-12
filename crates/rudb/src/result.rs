//! What a query hands back.

use std::sync::Arc;

use rudb_arrow::{DataType, Field, RecordBatch, Schema};
use rudb_common::{LogicalType, Memory, Reservation, Result, Value};
use rudb_metrics::Document;
use rudb_vector::Chunk;

/// The rows a query produced, with the names and types of its columns.
///
/// Materialized rather than streamed. A streaming result would have to hold the operator tree,
/// which holds a borrow of the plan and of the catalog, and that would make a result set a value
/// nobody can put in a struct. Section 7.5's streaming result is the M1 answer and it arrives as a
/// second type beside this one rather than as a change to it, because the overwhelming majority of
/// queries somebody embeds a database to run produce a result that fits in memory and the API for
/// those should not be the harder one.
///
/// The chunks are kept as chunks rather than flattened into rows. Anything that wants the columnar
/// form gets it without a transpose, which is what an Arrow export and a dataframe binding both
/// want, and anything that wants a row gets it through [`QueryResult::row`].
#[derive(Debug, Clone)]
pub struct QueryResult {
    names: Vec<String>,
    types: Vec<LogicalType>,
    chunks: Vec<Chunk>,
    /// Where each chunk starts, so a row number finds its chunk by a search rather than by walking.
    /// The last entry is the row count, which is what makes the search a plain partition point.
    starts: Vec<usize>,
    rows: usize,
    /// What these chunks are charged against the database's memory limit, given back when the last
    /// handle on this result is dropped.
    ///
    /// Behind an `Arc` because a result is cloneable and a reservation is not. A clone copies the
    /// chunks and shares the charge, so two handles on one result are charged once. That under
    /// counts, and it is the direction to under count in: a program that clones a result to hand it
    /// to another thread has not doubled its data in any sense it would recognize, and refusing its
    /// next query because it did would be a worse answer than the one it gets.
    held: Arc<Reservation>,
    /// What the execution that produced these rows measured about itself, when it was an execution
    /// at all.
    metrics: Option<Document>,
}

impl QueryResult {
    /// A result of the given columns and chunks.
    #[must_use]
    pub(crate) fn new(
        names: Vec<String>,
        types: Vec<LogicalType>,
        chunks: Vec<Chunk>,
        held: Reservation,
    ) -> Self {
        let mut starts = Vec::with_capacity(chunks.len() + 1);
        let mut rows = 0;
        for chunk in &chunks {
            starts.push(rows);
            rows += chunk.len();
        }
        starts.push(rows);
        Self { names, types, chunks, starts, rows, held: Arc::new(held), metrics: None }
    }

    /// The same result, carrying the document the execution that produced it filled in.
    #[must_use]
    pub(crate) fn measured(mut self, metrics: Document) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// A result of no columns and no rows, which is what a statement that writes hands back.
    ///
    /// Distinct from a query that produced no rows only by its width. DuckDB answers a `CREATE
    /// TABLE` with a `Count` column holding zero, and copying that would mean every caller checking
    /// whether a column is the real answer or the acknowledgement. `RETURNING` is the shape that
    /// makes a writing statement produce rows, and when it lands it produces them here.
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self::new(Vec::new(), Vec::new(), Vec::new(), Memory::unlimited().reservation())
    }

    /// The column names, in order.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The column types, in order.
    #[must_use]
    pub fn types(&self) -> &[LogicalType] {
        &self.types
    }

    /// How many bytes this result is charged against the database's memory limit.
    ///
    /// The chunks, not the names and the types, and it is what
    /// [`rudb_common::Memory::used`] stops counting when this is dropped. Worth reading for a
    /// program deciding whether to keep a result or re-run the query for it.
    #[must_use]
    pub fn footprint(&self) -> u64 {
        self.held.bytes()
    }

    /// What the execution measured about itself, for a result that came from one.
    ///
    /// There is nothing here for a statement that ran no plan, which is a `SET`, a `CREATE` with no
    /// query in it, or an `EXPLAIN`, since none of those execute anything to measure. Everything
    /// else carries a document with a row per operator and a row per pipeline, which is what
    /// `EXPLAIN ANALYZE` prints and what `--metrics` writes out as JSON.
    ///
    /// It hangs off the result rather than off the connection because a result outlives the query
    /// and two of them can be held at once. Numbers kept on the connection would be the numbers of
    /// whichever query ran most recently, which is not a question anybody is asking when they are
    /// holding the result of a particular one.
    #[must_use]
    pub fn metrics(&self) -> Option<&Document> {
        self.metrics.as_ref()
    }

    /// How many columns.
    #[must_use]
    pub fn width(&self) -> usize {
        self.names.len()
    }

    /// How many rows, across every chunk.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows
    }

    /// Whether the query produced no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// The name of one column, or the empty string if there is no such column.
    #[must_use]
    pub fn column_name(&self, column: usize) -> &str {
        self.names.get(column).map_or("", String::as_str)
    }

    /// The type of one column, or `NULL` if there is no such column.
    #[must_use]
    pub fn column_type(&self, column: usize) -> LogicalType {
        self.types.get(column).cloned().unwrap_or(LogicalType::Null)
    }

    /// The batches, for a caller that wants the columnar form.
    #[must_use]
    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// How many batches the result is in.
    ///
    /// A caller that walks the columnar form walks this rather than the row count, which is the
    /// whole point of having it: a result of ten million rows is a few thousand chunks and reading
    /// it that way never builds a row.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// One batch, or `None` if it is past the end.
    #[must_use]
    pub fn chunk(&self, at: usize) -> Option<&Chunk> {
        self.chunks.get(at)
    }

    /// The batches, one at a time.
    ///
    /// The same walk as `chunks().iter()` and the name a caller looks for. What it is not is a
    /// promise about where the rows came from: they are all here already, and a result that does
    /// not materialize is section 7.5's streaming result, which is a second type beside this one.
    pub fn chunk_iter(&self) -> impl ExactSizeIterator<Item = &Chunk> {
        self.chunks.iter()
    }

    /// The batches, taken rather than borrowed.
    ///
    /// For a caller that is turning the result into something else, an Arrow record batch or a
    /// table to append to, and would otherwise clone every chunk to do it.
    #[must_use]
    pub fn into_chunks(self) -> Vec<Chunk> {
        self.chunks
    }

    /// Which chunk a row is in, and where in it, or `None` if the row is past the end.
    ///
    /// A search rather than a walk. Reading a large result by row number is the ordinary way a
    /// program uses one, and walking the chunk list for each row makes that quadratic in a result
    /// of a few thousand chunks.
    fn locate(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.rows {
            return None;
        }
        let at = self.starts.partition_point(|&start| start <= row) - 1;
        Some((at, row - self.starts[at]))
    }

    /// One value, or null if the row or the column is past the end.
    ///
    /// Null for a row that does not exist rather than an option, because every caller that asks for
    /// a value in range would then have to unwrap one, and a query result is read in a loop over
    /// [`QueryResult::len`].
    #[must_use]
    pub fn value_at(&self, row: usize, column: usize) -> Value {
        match self.locate(row) {
            Some((at, offset)) => self.chunks[at].value_at(offset, column),
            None => Value::Null,
        }
    }

    /// One row, left to right, or `None` if it is past the end.
    #[must_use]
    pub fn row(&self, row: usize) -> Option<Vec<Value>> {
        let (at, offset) = self.locate(row)?;
        Some(self.chunks[at].row(offset).collect())
    }

    /// Every row in order, which is the shape a test and a script both want.
    pub fn rows(&self) -> impl Iterator<Item = Vec<Value>> + '_ {
        self.chunks.iter().flat_map(|chunk| (0..chunk.len()).map(|row| chunk.row(row).collect()))
    }

    /// One column, top to bottom, across every chunk.
    ///
    /// Empty for a column that is not there. This is the read that matches how the rows are held, so
    /// a program summing a column or handing one to a plotting library never transposes anything.
    pub fn column(&self, column: usize) -> impl Iterator<Item = Value> + '_ {
        let width = self.width();
        self.chunks
            .iter()
            .filter(move |_| column < width)
            .flat_map(move |chunk| (0..chunk.len()).map(move |row| chunk.value_at(row, column)))
    }

    /// The column names and Arrow types, without converting any values.
    ///
    /// Separate from [`QueryResult::to_arrow`] because a result of no rows has no batches and still
    /// has columns, and a consumer that reads the schema off the first batch would have nothing to
    /// read it off. Cheap enough to call on its own: it looks at the types and never at the rows.
    ///
    /// # Errors
    ///
    /// For a column of a type Arrow has no counterpart for here yet.
    pub fn arrow_schema(&self) -> Result<Schema> {
        let mut fields = Vec::with_capacity(self.width());
        for (name, ty) in self.names.iter().zip(&self.types) {
            fields.push(Field::new(name.clone(), DataType::of(ty)?));
        }
        Ok(Schema::new(fields))
    }

    /// The result as Arrow record batches, one per chunk.
    ///
    /// One per chunk rather than one for the whole result, because the chunks are what the executor
    /// produced and concatenating them would mean copying every value a second time to build a
    /// single large batch that most consumers immediately walk in pieces anyway. A consumer that
    /// does want one batch has every column in hand to build it.
    ///
    /// A result of no rows converts to no batches. The schema is [`QueryResult::arrow_schema`], and
    /// [`RecordBatch::empty`] turns it into the empty batch for a consumer that needs one.
    ///
    /// # Errors
    ///
    /// For a column of a type Arrow has no counterpart for here yet, and for a chunk whose values
    /// are not the layout its type says they are.
    pub fn to_arrow(&self) -> Result<Vec<RecordBatch>> {
        self.chunks.iter().map(|chunk| RecordBatch::of(chunk, &self.names)).collect()
    }
}

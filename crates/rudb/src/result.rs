//! What a query hands back.

use rudb_common::{LogicalType, Value};
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
    rows: usize,
}

impl QueryResult {
    /// A result of the given columns and chunks.
    #[must_use]
    pub(crate) fn new(names: Vec<String>, types: Vec<LogicalType>, chunks: Vec<Chunk>) -> Self {
        let rows = chunks.iter().map(Chunk::len).sum();
        Self { names, types, chunks, rows }
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

    /// The batches, for a caller that wants the columnar form.
    #[must_use]
    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// One value, or null if the row or the column is past the end.
    ///
    /// Null for a row that does not exist rather than an option, because every caller that asks for
    /// a value in range would then have to unwrap one, and a query result is read in a loop over
    /// [`QueryResult::len`].
    #[must_use]
    pub fn value_at(&self, row: usize, column: usize) -> Value {
        let mut remaining = row;
        for chunk in &self.chunks {
            if remaining < chunk.len() {
                return chunk.value_at(remaining, column);
            }
            remaining -= chunk.len();
        }
        Value::Null
    }

    /// One row, left to right, or `None` if it is past the end.
    #[must_use]
    pub fn row(&self, row: usize) -> Option<Vec<Value>> {
        let mut remaining = row;
        for chunk in &self.chunks {
            if remaining < chunk.len() {
                return Some(chunk.row(remaining).collect());
            }
            remaining -= chunk.len();
        }
        None
    }

    /// Every row in order, which is the shape a test and a script both want.
    pub fn rows(&self) -> impl Iterator<Item = Vec<Value>> + '_ {
        self.chunks.iter().flat_map(|chunk| (0..chunk.len()).map(|row| chunk.row(row).collect()))
    }
}

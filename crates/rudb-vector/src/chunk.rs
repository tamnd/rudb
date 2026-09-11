//! A batch of columns, which is the unit every operator passes to the next one.
//!
//! A chunk is some vectors of the same length plus that length. It is not a table and it is not a
//! result set: it is at most [`VECTOR_SIZE`] rows, because the whole point of the number in
//! `spec/04-architecture.md` section 4.3 is that a batch of this width stays in L1 while an
//! operator works on it, and a type that can hold ten times that many rows is a type that lets an
//! operator quietly stop being vectorized.
//!
//! The row count is stored rather than derived, which matters for the one case that looks like a
//! mistake and is not. `SELECT count(*) FROM t` scans no columns, so the chunk the scan produces
//! has no vectors in it and still has to say how many rows went past, and a chunk that derived its
//! length from its first column would say zero.

use rudb_common::{Error, LogicalType, Result, Value};

use crate::selection::Selection;
use crate::vector::{VECTOR_SIZE, Vector};

/// A batch of columns of equal length.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    columns: Vec<Vector>,
    rows: usize,
}

impl Chunk {
    /// A chunk of `columns`, taking the row count from the first of them.
    ///
    /// # Errors
    ///
    /// If the columns are not all the same length, or if there are more rows than [`VECTOR_SIZE`].
    pub fn new(columns: Vec<Vector>) -> Result<Self> {
        let rows = columns.first().map_or(0, Vector::len);
        Self::with_rows(columns, rows)
    }

    /// A chunk of `columns` that is `rows` long, for the case where there are no columns to take
    /// the count from.
    ///
    /// # Errors
    ///
    /// If any column is not `rows` long, or if `rows` is more than [`VECTOR_SIZE`].
    pub fn with_rows(columns: Vec<Vector>, rows: usize) -> Result<Self> {
        if rows > VECTOR_SIZE {
            return Err(Error::internal(format!(
                "a chunk of {rows} rows is longer than the {VECTOR_SIZE} row vector"
            )));
        }
        for (index, column) in columns.iter().enumerate() {
            if column.len() != rows {
                return Err(Error::internal(format!(
                    "column {index} of a chunk is {} rows and the chunk is {rows}",
                    column.len()
                )));
            }
        }
        Ok(Self { columns, rows })
    }

    /// A chunk of the given types with no rows in it.
    ///
    /// What a scan of an empty table returns and what an operator returns when it is done. The
    /// types are kept, because a consumer asks a chunk what its columns are before it asks whether
    /// there are any.
    #[must_use]
    pub fn empty(types: &[LogicalType]) -> Self {
        let columns =
            types.iter().map(|ty| Vector::constant(ty.clone(), Value::Null, 0)).collect::<Vec<_>>();
        Self { columns, rows: 0 }
    }

    /// The columns.
    #[must_use]
    pub fn columns(&self) -> &[Vector] {
        &self.columns
    }

    /// One column.
    ///
    /// # Errors
    ///
    /// If there is no column at `index`.
    pub fn column(&self, index: usize) -> Result<&Vector> {
        self.columns.get(index).ok_or_else(|| {
            Error::internal(format!(
                "column {index} of a chunk that has {} columns",
                self.columns.len()
            ))
        })
    }

    /// The columns, given up.
    #[must_use]
    pub fn into_columns(self) -> Vec<Vector> {
        self.columns
    }

    /// How many columns.
    #[must_use]
    pub fn width(&self) -> usize {
        self.columns.len()
    }

    /// How many rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows
    }

    /// Whether there are no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// The type of each column.
    #[must_use]
    pub fn types(&self) -> Vec<LogicalType> {
        self.columns.iter().map(|column| column.logical_type().clone()).collect()
    }

    /// The value at a row and a column, or null if either is past the end.
    ///
    /// The slow path, same as [`Vector::value_at`]. It is what a result set is read out with and
    /// what a test asserts on.
    #[must_use]
    pub fn value_at(&self, row: usize, column: usize) -> Value {
        match self.columns.get(column) {
            Some(held) => held.value_at(row),
            None => Value::Null,
        }
    }

    /// One row, left to right.
    pub fn row(&self, row: usize) -> impl Iterator<Item = Value> + '_ {
        self.columns.iter().map(move |column| column.value_at(row))
    }

    /// The rows a selection kept, without moving any of the values.
    ///
    /// Every column becomes a dictionary vector whose codes are the selection, which is the form
    /// `spec/07-execution.md` section 7.1 asks a filter to produce rather than compacting. It takes
    /// the chunk by value because that is what makes it free: the payload is moved into the new
    /// vector rather than copied, so a filter that keeps one row in a thousand still costs the
    /// selection and nothing else.
    ///
    /// # Errors
    ///
    /// If the selection points past the end of the chunk.
    pub fn select(self, selection: &Selection) -> Result<Self> {
        if let Some(bad) = selection.iter().find(|&index| index >= self.rows) {
            return Err(Error::internal(format!(
                "a selection keeps row {bad} of a chunk that has {} rows",
                self.rows
            )));
        }
        let rows = selection.len();
        let codes = selection.indices();
        let mut columns = Vec::with_capacity(self.columns.len());
        for column in self.columns {
            columns.push(Vector::dictionary(codes.to_vec(), column)?);
        }
        Self::with_rows(columns, rows)
    }

    /// The rows a selection kept, copied, so that nothing downstream reads through an indirection.
    ///
    /// The copying counterpart to [`Self::select`], and the two exist because neither one is right
    /// twice. Which one to call is measured rather than argued, and the measurement says something
    /// other than what the argument does, so here is both.
    ///
    /// The argument is that selecting pays nothing now and one redirection on every later read of
    /// every kept row, while compacting pays a copy now and nothing afterwards, so the deciding
    /// variable is selectivity: keep a few rows and select, keep most of them and compact. The
    /// measurement says the deciding variable is not selectivity at all, it is how many times the
    /// rows are read again afterwards, and selectivity barely moves the line. On server3, over a
    /// chunk of two integer columns, compacting loses to selecting at every selectivity from one
    /// percent to a hundred when there is one later pass over the kept rows, and beats it at every
    /// selectivity from one percent to a hundred when there are sixteen. With four later passes the
    /// two are within a few percent of each other everywhere. Put a varchar column in the chunk and
    /// compaction loses almost everywhere, because copying string bytes is most of what it costs and
    /// the dictionary it avoids is most of what it saves.
    ///
    /// Which is why nothing in the streaming pipeline calls this yet. A filter today feeds an
    /// aggregate or a projection and that is one pass or two, and end to end on two million rows
    /// `SELECT sum(a), sum(b), count(*) FROM t WHERE a > ?` measures the same either way at one
    /// percent selectivity and fifty percent slower compacting at fifty percent selectivity. The
    /// operators that will want this are the ones that hold chunks rather than pass them on, the
    /// hash join build side and the sort, because a chunk that is kept alive as a selection keeps
    /// the whole chunk it was selected from alive with it, and that is a hundred to one on memory
    /// rather than a few percent on time.
    ///
    /// Takes the chunk by value like [`Self::select`] does, even though the payload is copied rather
    /// than moved, because a caller that still wanted the original after compacting it would be
    /// holding both copies and should say so.
    ///
    /// # Errors
    ///
    /// If the selection points past the end of the chunk, or if a column has a type with no flat
    /// layout, which today means the nested types.
    pub fn compact(self, selection: &Selection) -> Result<Self> {
        if let Some(bad) = selection.iter().find(|&index| index >= self.rows) {
            return Err(Error::internal(format!(
                "a selection keeps row {bad} of a chunk that has {} rows",
                self.rows
            )));
        }
        let rows = selection.len();
        let indices = selection.indices();
        let mut columns = Vec::with_capacity(self.columns.len());
        for column in &self.columns {
            columns.push(column.gather(indices)?);
        }
        Self::with_rows(columns, rows)
    }

    /// The columns at the given positions, in that order.
    ///
    /// A position may appear twice, which is what `SELECT x, x FROM t` is, and the second one costs
    /// a copy. Every other position is moved.
    ///
    /// # Errors
    ///
    /// If a position is past the end of the chunk.
    pub fn project(self, positions: &[usize]) -> Result<Self> {
        let width = self.columns.len();
        if let Some(&bad) = positions.iter().find(|&&position| position >= width) {
            return Err(Error::internal(format!(
                "column {bad} of a chunk that has {width} columns"
            )));
        }
        let rows = self.rows;
        let mut sources: Vec<Option<Vector>> = self.columns.into_iter().map(Some).collect();
        let mut columns = Vec::with_capacity(positions.len());
        for (at, &position) in positions.iter().enumerate() {
            let last_use = !positions[at + 1..].contains(&position);
            let taken = if last_use { sources[position].take() } else { sources[position].clone() };
            match taken {
                Some(column) => columns.push(column),
                // Only reachable if the last-use bookkeeping above is wrong, since a position is
                // taken on its last appearance and cloned on every earlier one.
                None => {
                    return Err(Error::internal(format!("column {position} was taken twice")));
                }
            }
        }
        Self::with_rows(columns, rows)
    }

    /// The same rows with every column in flat form.
    ///
    /// Costs a copy per column that was not already flat. It is here for the result set at the top
    /// of a query, where the dictionary vectors a filter left behind would otherwise be handed to a
    /// caller who has to understand them.
    ///
    /// # Errors
    ///
    /// If a column has a type that cannot be stored flat yet, which today means the nested types.
    pub fn flatten(&self) -> Result<Self> {
        let mut columns = Vec::with_capacity(self.columns.len());
        for column in &self.columns {
            columns.push(column.flatten()?);
        }
        Self::with_rows(columns, self.rows)
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::*;
    use crate::vector::{Data, Form};

    fn integers(values: &[i32]) -> Vector {
        Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec()))
            .expect("integers are an i32 layout")
    }

    #[test]
    fn a_chunk_takes_its_length_from_its_columns() {
        let chunk = Chunk::new(vec![integers(&[1, 2, 3]), integers(&[4, 5, 6])])
            .expect("two columns of three");
        assert_eq!(chunk.len(), 3);
        assert_eq!(chunk.width(), 2);
        assert_eq!(chunk.value_at(2, 1), Value::Integer(6));
    }

    #[test]
    fn a_ragged_chunk_is_caught() {
        let error = Chunk::new(vec![integers(&[1, 2, 3]), integers(&[4])])
            .expect_err("a chunk is not ragged");
        assert!(error.message().contains("column 1"), "{error}");
    }

    /// `SELECT count(*) FROM t` scans no columns and the row count still has to survive, which is
    /// the reason the length is a field rather than the first column's length.
    #[test]
    fn a_chunk_with_no_columns_can_still_have_rows() {
        let chunk = Chunk::with_rows(Vec::new(), 900).expect("no columns and nine hundred rows");
        assert_eq!(chunk.len(), 900);
        assert_eq!(chunk.width(), 0);
        assert!(!chunk.is_empty(), "nine hundred rows is not empty");
    }

    #[test]
    fn a_chunk_longer_than_a_vector_is_caught() {
        let error = Chunk::with_rows(Vec::new(), VECTOR_SIZE + 1).expect_err("too long");
        assert!(error.message().contains("longer than"), "{error}");
    }

    #[test]
    fn an_empty_chunk_keeps_its_types() {
        let chunk = Chunk::empty(&[LogicalType::Integer, LogicalType::Varchar]);
        assert_eq!(chunk.len(), 0);
        assert_eq!(chunk.types(), vec![LogicalType::Integer, LogicalType::Varchar]);
    }

    #[test]
    fn selecting_keeps_the_rows_it_selected_and_no_others() {
        let chunk = Chunk::new(vec![integers(&[10, 20, 30, 40]), integers(&[1, 2, 3, 4])])
            .expect("four rows");
        let kept = Selection::from_predicate(4, |index| index % 2 == 1);
        let chunk = chunk.select(&kept).expect("rows one and three exist");
        assert_eq!(chunk.len(), 2);
        assert_eq!(chunk.row(0).collect::<Vec<_>>(), vec![Value::Integer(20), Value::Integer(2)]);
        assert_eq!(chunk.row(1).collect::<Vec<_>>(), vec![Value::Integer(40), Value::Integer(4)]);
    }

    /// The reason `select` takes the chunk by value. If it copied the payload then a filter would
    /// cost the same as a compaction and the selection would be a pure loss.
    #[test]
    fn selecting_leaves_the_values_where_they_were() {
        let chunk = Chunk::new(vec![integers(&[10, 20, 30, 40])]).expect("four rows");
        let kept = Selection::from_predicate(4, |index| index == 0);
        let chunk = chunk.select(&kept).expect("row zero exists");
        assert_eq!(chunk.column(0).expect("one column").form(), Form::Dictionary);
    }

    #[test]
    fn a_selection_past_the_end_is_caught() {
        let chunk = Chunk::new(vec![integers(&[1, 2])]).expect("two rows");
        let mut kept = Selection::empty();
        kept.push(7);
        let error = chunk.select(&kept).expect_err("row seven does not exist");
        assert!(error.message().contains("row 7"), "{error}");
    }

    /// The two halves of section 7.1's decision have to answer the same question the same way, or
    /// the threshold between them is a place where a query changes its answer.
    #[test]
    fn compacting_keeps_the_same_rows_selecting_does_and_leaves_no_indirection() {
        let chunk = Chunk::new(vec![integers(&[10, 20, 30, 40]), integers(&[1, 2, 3, 4])])
            .expect("four rows");
        let kept = Selection::from_predicate(4, |index| index % 2 == 1);
        let selected = chunk.clone().select(&kept).expect("rows one and three exist");
        let compacted = chunk.compact(&kept).expect("rows one and three exist");
        assert_eq!(compacted.len(), selected.len());
        for row in 0..compacted.len() {
            assert_eq!(
                compacted.row(row).collect::<Vec<_>>(),
                selected.row(row).collect::<Vec<_>>()
            );
        }
        assert_eq!(compacted.column(0).expect("one column").form(), Form::Flat);
    }

    #[test]
    fn a_selection_past_the_end_is_caught_by_compacting_too() {
        let chunk = Chunk::new(vec![integers(&[1, 2])]).expect("two rows");
        let mut kept = Selection::empty();
        kept.push(7);
        let error = chunk.compact(&kept).expect_err("row seven does not exist");
        assert!(error.message().contains("row 7"), "{error}");
    }

    #[test]
    fn projecting_reorders_and_can_repeat_a_column() {
        let chunk = Chunk::new(vec![integers(&[1, 2]), integers(&[3, 4])]).expect("two by two");
        let chunk = chunk.project(&[1, 0, 1]).expect("both columns exist");
        assert_eq!(chunk.width(), 3);
        assert_eq!(
            chunk.row(0).collect::<Vec<_>>(),
            vec![Value::Integer(3), Value::Integer(1), Value::Integer(3)]
        );
    }

    #[test]
    fn projecting_a_column_that_is_not_there_is_caught() {
        let chunk = Chunk::new(vec![integers(&[1, 2])]).expect("one column");
        let error = chunk.project(&[0, 4]).expect_err("there is no column four");
        assert!(error.message().contains("column 4"), "{error}");
    }

    #[test]
    fn flattening_a_selected_chunk_gives_the_same_values() {
        let chunk = Chunk::new(vec![integers(&[10, 20, 30])]).expect("three rows");
        let kept = Selection::from_predicate(3, |index| index != 1);
        let selected = chunk.select(&kept).expect("rows zero and two exist");
        let flat = selected.flatten().expect("integers flatten");
        assert_eq!(flat.column(0).expect("one column").form(), Form::Flat);
        for row in 0..flat.len() {
            assert_eq!(flat.value_at(row, 0), selected.value_at(row, 0), "row {row}");
        }
    }
}

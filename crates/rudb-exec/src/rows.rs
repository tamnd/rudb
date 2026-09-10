//! Turning chunks into rows and rows back into chunks.
//!
//! Every pipeline breaker here holds its input as `Vec<Vec<Value>>`. That is the slowest reasonable
//! layout and it is chosen knowingly: a boxed value per field per row is what section 7.4 replaces
//! with a fixed width row prefix and a payload beside it, and the reason to write the slow one
//! first is that the fast one has to produce the same answer and there has to be something to
//! compare it against. Sorting, grouping and joining are the three places a database is most often
//! subtly wrong, and none of those bugs are about the row layout.

use rudb_common::{LogicalType, Result, Value};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::operator::Operator;

/// Drains an operator into rows.
///
/// # Errors
///
/// Anything the operator reports while producing its input.
pub(crate) fn collect(input: &mut dyn Operator) -> Result<Vec<Vec<Value>>> {
    let mut rows = Vec::new();
    while let Some(chunk) = input.next()? {
        for row in 0..chunk.len() {
            rows.push(chunk.row(row).collect());
        }
    }
    Ok(rows)
}

/// Rows back into chunks of at most [`VECTOR_SIZE`], in the order given.
///
/// An operator with no columns keeps its row count, which is the `SELECT count(*)` case and the
/// reason this takes the types rather than deriving the width from the first row.
///
/// # Errors
///
/// If a value does not belong in the column it was placed in, or if a row is not as wide as the
/// type list.
pub(crate) fn chunks(types: &[LogicalType], rows: &[Vec<Value>]) -> Result<Vec<Chunk>> {
    let mut built = Vec::new();
    for batch in rows.chunks(VECTOR_SIZE) {
        let mut columns = Vec::with_capacity(types.len());
        for (position, ty) in types.iter().enumerate() {
            let down: Vec<Value> =
                batch.iter().map(|row| row.get(position).cloned().unwrap_or(Value::Null)).collect();
            columns.push(Vector::from_values(ty.clone(), &down)?);
        }
        built.push(Chunk::with_rows(columns, batch.len())?);
    }
    Ok(built)
}

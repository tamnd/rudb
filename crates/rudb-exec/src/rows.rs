//! Turning chunks into rows and rows back into chunks.
//!
//! Every pipeline breaker here holds its input as `Vec<Vec<Value>>`. That is the slowest reasonable
//! layout and it is chosen knowingly: a boxed value per field per row is what section 7.4 replaces
//! with a fixed width row prefix and a payload beside it, and the reason to write the slow one
//! first is that the fast one has to produce the same answer and there has to be something to
//! compare it against. Sorting, grouping and joining are the three places a database is most often
//! subtly wrong, and none of those bugs are about the row layout.
//!
//! It is also why the memory limit is charged here. These two functions are where an unbounded
//! amount of memory is taken, so they are where the limit has to be asked, and a limit charged in
//! one place per crate is one somebody can believe rather than one that has to be re-audited every
//! time an operator is added.

use rudb_common::{LogicalType, Reservation, Result, Value};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::operator::Operator;

/// What one buffered row costs, counting the values and the vector holding them.
pub(crate) fn footprint(row: &[Value]) -> u64 {
    let bytes = size_of::<Vec<Value>>() + row.iter().map(Value::footprint).sum::<usize>();
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// Drains an operator into rows, charging what they take against the budget.
///
/// The charge happens once per input chunk rather than once per row, so a query passes its limit by
/// up to a chunk of rows before it is told. That is the same granularity the cancellation check
/// runs at and for the same reason: a thousand rows is a bounded overshoot and a check per row is a
/// branch in the row loop.
///
/// # Errors
///
/// Anything the operator reports while producing its input, and
/// [`rudb_common::ErrorCode::OutOfMemory`] when the rows pass the limit the database was opened
/// with.
pub(crate) fn collect(input: &mut dyn Operator, held: &mut Reservation) -> Result<Vec<Vec<Value>>> {
    let mut rows = Vec::new();
    while let Some(chunk) = input.next()? {
        let mut taken = 0;
        for row in 0..chunk.len() {
            let values: Vec<Value> = chunk.row(row).collect();
            taken += footprint(&values);
            rows.push(values);
        }
        held.grow(taken)?;
    }
    Ok(rows)
}

/// Rows back into chunks of at most [`VECTOR_SIZE`], in the order given.
///
/// An operator with no columns keeps its row count, which is the `SELECT count(*)` case and the
/// reason this takes the types rather than deriving the width from the first row.
///
/// What the chunks take is charged as they are built, because this is the second copy of the data:
/// the rows that went in are still alive while it runs, and an operator that is about to hand out
/// the chunks is holding both.
///
/// # Errors
///
/// If a value does not belong in the column it was placed in, if a row is not as wide as the type
/// list, or if the chunks pass the limit the database was opened with.
pub(crate) fn chunks(
    types: &[LogicalType],
    rows: &[Vec<Value>],
    held: &mut Reservation,
) -> Result<Vec<Chunk>> {
    let mut built = Vec::new();
    for batch in rows.chunks(VECTOR_SIZE) {
        let mut columns = Vec::with_capacity(types.len());
        for (position, ty) in types.iter().enumerate() {
            let down: Vec<Value> =
                batch.iter().map(|row| row.get(position).cloned().unwrap_or(Value::Null)).collect();
            columns.push(Vector::from_values(ty.clone(), &down)?);
        }
        let chunk = Chunk::with_rows(columns, batch.len())?;
        held.grow(u64::try_from(chunk.footprint()).unwrap_or(u64::MAX))?;
        built.push(chunk);
    }
    Ok(built)
}

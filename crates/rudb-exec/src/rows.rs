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

use rudb_common::{ALLOCATION, LogicalType, Reservation, Result, Value};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::operator::Operator;

/// What one buffered row costs, counting the values and the vector holding them.
pub(crate) fn footprint(row: &[Value]) -> u64 {
    u64::try_from(size_of::<Vec<Value>>()).unwrap_or(u64::MAX) + heap(row)
}

/// What one buffered row owns away from itself, which is everything [`footprint`] counts except
/// the three words of the vector's own header.
///
/// Separate because a row sitting inside a container has those three words counted already, by
/// [`capacity`] over the container that holds it, and counting them again here would charge them
/// twice. Per #227 the containers are now charged for what they took rather than for what they are
/// using, and the two have to divide the row between them without overlapping.
///
/// A string is counted as the room it has and not as the bytes in it, for the same reason, so the
/// row this is asked about has to be the copy something kept rather than a buffer that is about to
/// be filled again. Grouping reads a row into one buffer and copies it out only when the group is
/// new, and that buffer keeps whatever the longest string it has held needed, so asking this about
/// it charges every row for the longest row.
pub(crate) fn heap(row: &[Value]) -> u64 {
    let bytes = row.iter().map(Value::footprint).sum::<usize>() + ALLOCATION_USIZE;
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// What one value owns away from itself, which is its footprint without its own bytes.
///
/// For a value going into room that has been charged already, where charging the footprint would
/// charge that room twice. A group by asks for the room its aggregate results need before it has
/// them, because by the time it has them it has taken the memory, and then it has only what each
/// result owns left to charge.
pub(crate) fn owned(value: &Value) -> u64 {
    u64::try_from(value.footprint() - size_of::<Value>()).unwrap_or(u64::MAX)
}

/// [`ALLOCATION`] in the type the sizes around it are in.
const ALLOCATION_USIZE: usize = ALLOCATION as usize;

/// Charges whatever a set of containers has grown by since it was last charged.
///
/// `now` is what they take today and `charged` is what they were last charged, which this updates.
///
/// A container's capacity is what it took from the allocator and its length is what it is using,
/// and the difference is not small. A `Vec` doubles, so it is between half empty and full, and a
/// `HashMap` fills to seven eighths and then doubles as well. Charging the entries alone charges
/// the used part of a structure that paid for all of it, which is the larger half of #227.
///
/// Asked once per input chunk, the same granularity everything else in here charges at, so the
/// overshoot before a query is told is bounded by one chunk of insertions.
///
/// # Errors
///
/// [`rudb_common::ErrorCode::OutOfMemory`] when the growth passes the limit.
pub(crate) fn capacity(now: u64, charged: &mut u64, held: &mut Reservation) -> Result<()> {
    let grown = now.saturating_sub(*charged);
    if grown > 0 {
        held.grow(grown)?;
        *charged = now;
    }
    Ok(())
}

/// How many buckets a hash table has to have to hold `entries` without growing again.
///
/// Both `HashMap` and `HashSet` are open addressed and refuse to fill past seven eighths, and they
/// report the seven rather than the eight, so the table on the heap is a bucket and a control byte
/// for each of eight sevenths of what `capacity` says.
pub(crate) fn buckets(entries: usize) -> u64 {
    u64::try_from(entries).unwrap_or(u64::MAX).saturating_mul(8).div_ceil(7)
}

/// Drains an operator into rows, charging what they take against the budget.
///
/// The charge happens once per input chunk rather than once per row, so a query passes its limit by
/// up to a chunk of rows before it is told. That is the same granularity the cancellation check
/// runs at and for the same reason: a thousand rows is a bounded overshoot and a check per row is a
/// branch in the row loop.
///
/// Two charges rather than one. Each row is charged what it owns, and the vector holding the rows is
/// charged what it has taken from the allocator rather than what it has put in it, which is up to
/// twice as much because a `Vec` doubles.
///
/// # Errors
///
/// Anything the operator reports while producing its input, and
/// [`rudb_common::ErrorCode::OutOfMemory`] when the rows pass the limit the database was opened
/// with.
pub(crate) fn collect(input: &mut dyn Operator, held: &mut Reservation) -> Result<Vec<Vec<Value>>> {
    let mut rows = Vec::new();
    let mut charged = 0;
    while let Some(chunk) = input.next()? {
        let mut taken = 0;
        for row in 0..chunk.len() {
            let values: Vec<Value> = chunk.row(row).collect();
            taken += heap(&values);
            rows.push(values);
        }
        held.grow(taken)?;
        let slots = u64::try_from(rows.capacity() * size_of::<Vec<Value>>()).unwrap_or(u64::MAX);
        capacity(slots, &mut charged, held)?;
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

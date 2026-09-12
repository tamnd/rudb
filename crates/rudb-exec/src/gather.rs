//! The sink that does nothing but hold its input.
//!
//! Some operators need one whole side of their input in hand before they can start, and the side
//! they need is not the side they are a sink for. A set operation reads the right side, counts it,
//! and then decides one left row at a time. A join builds from one side and probes with the other.
//! In push terms that is two pipelines and a dependency between them, and the pipeline that is
//! depended on ends in a sink that keeps its rows and does nothing else.
//!
//! This is that sink. The rows come out through a [`Rows`] handle, which is what the operator on
//! the other side of the dependency holds.

use std::sync::{Arc, Mutex};

use rudb_common::{Error, Memory, Reservation, Result, Value};
use rudb_pipeline::{Progress, Sink};
use rudb_vector::Chunk;

use crate::rows;

/// Rows somebody gathered, readable once the pipeline that filled them has finished.
///
/// Cloning one gives another handle on the same rows.
#[derive(Debug, Default, Clone)]
pub(crate) struct Rows {
    held: Arc<Mutex<Vec<Vec<Value>>>>,
}

impl Rows {
    /// Take the rows out, leaving nothing behind.
    ///
    /// Taking rather than borrowing, because the one caller is a `finalize` that is about to turn
    /// them into something else and holding two copies of a side of a join is the thing the memory
    /// budget exists to stop.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while holding
    /// them.
    pub(crate) fn take(&self) -> Result<Vec<Vec<Value>>> {
        Ok(std::mem::take(&mut *self.held.lock().map_err(poisoned)?))
    }
}

/// A sink that keeps every row it is given.
#[derive(Debug)]
pub(crate) struct Gather {
    memory: Memory,
    out: Rows,
    /// What the gathered rows are charged, kept until this sink is dropped, which is when the
    /// operator that depended on them is done with them.
    charged: Mutex<Vec<Reservation>>,
}

/// What one instance of a gather is holding.
#[derive(Debug)]
pub(crate) struct Gathering {
    rows: Vec<Vec<Value>>,
    charged: Reservation,
    /// What the vector holding the rows has been charged for, so that its doubling is charged once
    /// per growth rather than once per chunk. See [`rows::capacity`].
    counted: u64,
}

impl Gather {
    /// A gather and the handle its rows come out of.
    pub(crate) fn new(memory: &Memory) -> (Self, Rows) {
        let out = Rows::default();
        let gather =
            Self { memory: memory.clone(), out: out.clone(), charged: Mutex::new(Vec::new()) };
        (gather, out)
    }
}

impl Sink for Gather {
    type Local = Gathering;

    fn local(&self) -> Gathering {
        Gathering { rows: Vec::new(), charged: self.memory.reservation(), counted: 0 }
    }

    fn sink(&self, chunk: &Chunk, local: &mut Gathering) -> Result<Progress> {
        take(chunk, local)?;
        Ok(Progress::More)
    }

    fn combine(&self, local: Gathering) -> Result<()> {
        self.out.held.lock().map_err(poisoned)?.extend(local.rows);
        self.charged.lock().map_err(poisoned)?.push(local.charged);
        Ok(())
    }

    fn finalize(&self) -> Result<()> {
        Ok(())
    }
}

/// Reads one chunk into rows and charges what they took.
///
/// The charge happens once per chunk rather than once per row, so a query passes its limit by up to
/// a chunk of rows before it is told. That is the same granularity the cancellation check runs at
/// and for the same reason: a thousand rows is a bounded overshoot and a check per row is a branch
/// in the row loop.
///
/// Shared with the set operation, which gathers its left side the same way and then does something
/// with it, because two copies of this loop would be two places to get the charging wrong.
///
/// # Errors
///
/// [`rudb_common::ErrorCode::OutOfMemory`] when the rows pass the limit.
pub(crate) fn take(chunk: &Chunk, local: &mut Gathering) -> Result<()> {
    let mut taken = 0;
    // row at a time: this is the `Vec<Vec<Value>>` layout `rows` documents, and section 7.4's row
    // prefix with a payload beside it replaces it everywhere at once.
    for row in 0..chunk.len() {
        let values: Vec<Value> = chunk.row(row).collect();
        taken += rows::heap(&values);
        local.rows.push(values);
    }
    local.charged.grow(taken)?;
    let slots = u64::try_from(local.rows.capacity() * size_of::<Vec<Value>>()).unwrap_or(u64::MAX);
    rows::capacity(slots, &mut local.counted, &mut local.charged)
}

/// A fresh instance's state, for an operator that gathers one of its sides itself.
pub(crate) fn gathering(memory: &Memory) -> Gathering {
    Gathering { rows: Vec::new(), charged: memory.reservation(), counted: 0 }
}

/// What an instance gathered, and what it was charged for it.
pub(crate) fn into_parts(local: Gathering) -> (Vec<Vec<Value>>, Reservation) {
    (local.rows, local.charged)
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the rows an operator gathered")
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Memory, Value};
    use rudb_vector::{Data, Vector};

    use super::{Chunk, Gather, Sink};

    fn chunk(values: &[i32]) -> Chunk {
        let column = Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into()))
            .expect("integers are an i32 layout");
        Chunk::new(vec![column]).expect("one column is one length")
    }

    fn first(rows: &[Vec<Value>]) -> Vec<Value> {
        rows.iter().map(|row| row[0].clone()).collect()
    }

    #[test]
    fn what_goes_in_comes_out_in_the_order_it_was_combined() {
        let memory = Memory::unlimited();
        let (gather, rows) = Gather::new(&memory);

        let mut local = gather.local();
        gather.sink(&chunk(&[1, 2]), &mut local).expect("two rows");
        gather.sink(&chunk(&[3]), &mut local).expect("one more");
        gather.combine(local).expect("the one instance");
        gather.finalize().expect("nothing to do");

        assert_eq!(
            first(&rows.take().expect("readable")),
            [Value::Integer(1), Value::Integer(2), Value::Integer(3)]
        );
    }

    /// Every instance's rows end up in the one list. What order the instances combine in is what
    /// decides the order of the list, which is why the operator that reads it either does not care
    /// or has to sort it, and a set operation does not care.
    #[test]
    fn two_instances_both_end_up_in_the_one_list() {
        let memory = Memory::unlimited();
        let (gather, rows) = Gather::new(&memory);

        let mut left = gather.local();
        let mut right = gather.local();
        gather.sink(&chunk(&[1]), &mut left).expect("one row");
        gather.sink(&chunk(&[2]), &mut right).expect("one row");
        gather.combine(left).expect("the first instance");
        gather.combine(right).expect("the second instance");

        assert_eq!(first(&rows.take().expect("readable")), [Value::Integer(1), Value::Integer(2)]);
    }

    /// Taking leaves nothing behind, because the caller is about to turn the rows into something
    /// else and two copies of a side of a join is what the memory budget exists to stop.
    #[test]
    fn taking_the_rows_empties_them() {
        let memory = Memory::unlimited();
        let (gather, rows) = Gather::new(&memory);
        gather.combine(gather.local()).expect("an instance that saw nothing");

        assert!(rows.take().expect("readable").is_empty());
        assert!(rows.take().expect("readable").is_empty());
    }
}

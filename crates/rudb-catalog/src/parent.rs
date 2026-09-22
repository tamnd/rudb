//! One column of a table held end to end, which is what a link join gathers out of.
//!
//! spec/graph/05-execution.md section 5.2 says the parent columns of a link join come out as
//! `Gathered`, holding an `Arc` to the parent's column vector and the `rid`s to take from it. This
//! is where that vector comes from. A stored table is read a part at a time, so the whole of one
//! column is the parts of it laid end to end, and laying them end to end is
//! [`rudb_vector::concat`], which is the same call a row group of a stored table is already built
//! with.
//!
//! # Why the whole column and not a part at a time
//!
//! Because the `rid`s a child chunk carries are `rid`s of the parent *table*, and two thousand of
//! them out of a clustered child may still straddle a part boundary while two thousand out of an
//! unclustered child straddle the whole table. A gather whose source was one part would have to be
//! several gathers with the rows interleaved back together afterwards, which is an assembly per
//! chunk per column to answer something that is a shift and a load once the column is contiguous.
//!
//! The cost of that decision is stated rather than hidden: this holds the projected columns of the
//! parent in memory for as long as the query runs. That is the same thing a hash join's build side
//! does, minus the hash table, the tuple layout and the copy per matching child row, and it is why
//! section 6.4's rule is about the width of the parent's *projection* rather than about the width
//! of the parent.
//!
//! # A stored column does not arrive flat
//!
//! It arrives in whatever form the writer chose, which on real data is bit packed for an integer
//! and dictionary encoded for a low cardinality string, and almost never flat.
//! [`rudb_vector::concat`] lays flat runs end to end and declines everything else, on the argument
//! that a caller who gets a `None` has somewhere to put the pieces.
//!
//! This caller does not. The `rid` a child row carries names a row of the parent table, and a run
//! that is still in pieces has no row at that offset, so the pieces have to become one run before
//! anything can be taken out of them. So a piece that is not flat is flattened here.
//!
//! The cost of that is one decode of one column, once per query, and it is worth being explicit
//! that it is not new work: the hash join this replaces decodes the same values to build its table
//! over them, and then writes each of them into a tuple as well. What is given up is the encoding's
//! size in memory, which is why the budget below is measured after the flattening rather than
//! before it.
//!
//! An earlier version of this file did not flatten and passed the `None` on. Every link join over
//! every table anybody had written with rudb's own writer then failed, and it failed reporting that
//! it was out of memory, which it was not. The measurement that found it is `cargo xtask sections`.
//!
//! # The budget, and what a refusal means
//!
//! [`Parent::column`] answers `None` rather than an error when a column would not fit, which is a
//! memory limit being reached and not a bug. What the caller does with it is the caller's: the link
//! join operator reports it, on the argument in `LinkJoin::read_parent` that a parent whose
//! projection will not fit is one whose hash join would not have fit either.
//!
//! The budget is counted over what is held rather than estimated before the read, because a column
//! is compressed in the file and the number that matters is what it costs once it is a vector. A
//! column that turns out not to fit is dropped rather than kept, so the next query is not refused
//! because of a column this one could not use anyway.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rudb_common::{LogicalType, Result};
use rudb_vector::{Form, Vector, concat};

use crate::table::Rows;

/// The projected columns of one table, each held whole, each read at most once.
///
/// One of these per parent table per query. It is shared rather than cloned, because the point of
/// the whole exercise is that eight columns gathered off one parent are one parent between them,
/// which is also what [`Vector::footprint`] reports about the vectors that come out of it.
#[derive(Debug)]
pub struct Parent {
    rows: Rows,
    /// The columns already read, by column index. A `None` is a column that was asked for and did
    /// not fit, remembered so that a second ask does not read it again to refuse it again.
    held: Mutex<HashMap<usize, Option<Arc<Vector>>>>,
    /// What every column held here may cost together, in bytes.
    budget: usize,
}

impl Parent {
    /// A parent whose columns may cost `budget` bytes between them.
    #[must_use]
    pub fn new(rows: Rows, budget: usize) -> Self {
        Self { rows, held: Mutex::new(HashMap::new()), budget }
    }

    /// The whole of one column, or `None` if reading it would go past the budget.
    ///
    /// The type is the caller's because the table's field list is the caller's. Passing the wrong
    /// one is caught by [`rudb_vector::concat()`], which refuses pieces that do not agree with it.
    ///
    /// # Errors
    ///
    /// If a part of the column cannot be read, or if the parts do not lay end to end.
    pub fn column(&self, column: usize, ty: &LogicalType) -> Result<Option<Arc<Vector>>> {
        // The lock is held across the read, which serializes two threads that want the same column
        // of the same parent. That is the intended trade: the alternative is both of them reading
        // it, and the column is the expensive thing here while the wait is one read of it.
        let mut held = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(found) = held.get(&column) {
            return Ok(found.clone());
        }
        let read = self.read(column, ty, &held)?;
        held.insert(column, read.clone());
        Ok(read)
    }

    /// How many bytes the columns held here cost between them.
    ///
    /// For the metrics document, and for a test to assert that a refusal refused.
    #[must_use]
    pub fn footprint(&self) -> usize {
        let held = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        spent(&held)
    }

    /// Reads every part of one column and lays them end to end.
    fn read(
        &self,
        column: usize,
        ty: &LogicalType,
        held: &HashMap<usize, Option<Arc<Vector>>>,
    ) -> Result<Option<Arc<Vector>>> {
        let room = self.budget.saturating_sub(spent(held));
        let mut pieces = Vec::with_capacity(self.rows.chunk_count());
        let mut cost = 0usize;
        for part in 0..self.rows.chunk_count() {
            let chunk = self.rows.read(part, &[column])?;
            let piece = chunk.column(0)?;
            // A part with no rows in it contributes no rows to the run and would make `concat`
            // decline the whole of it, which is a column given up on over a part that says nothing.
            if piece.is_empty() {
                continue;
            }
            // flatten: the whole point of this type is a run the gather can index by a row id of
            // the parent table, and a row id has no meaning against a bit packed part that has not
            // been decoded. See the module doc for why this is a decode the hash join pays as well.
            let piece = if piece.form() == Form::Flat { piece.clone() } else { piece.flatten()? };
            // Measured after the flattening, because the number the budget is about is what the
            // column costs once it is a vector, and checked as the parts arrive rather than at the
            // end so that a column far past the budget is abandoned after one part instead of all
            // of them.
            cost = cost.saturating_add(piece.footprint());
            if cost > room {
                return Ok(None);
            }
            pieces.push(piece);
        }
        let Some(whole) = concat(ty, &pieces)? else {
            return Ok(None);
        };
        // Measured again on the result, because laying the pieces end to end is where a string
        // column's arena is sized and the answer is not the sum of the pieces.
        if whole.footprint() > room {
            return Ok(None);
        }
        Ok(Some(Arc::new(whole)))
    }
}

/// What the columns already held cost between them.
fn spent(held: &HashMap<usize, Option<Arc<Vector>>>) -> usize {
    held.values().flatten().map(|column| column.footprint()).sum()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::{LogicalType, Value};
    use rudb_storage::MemoryTable;
    use rudb_vector::{Chunk, Vector};

    use super::Parent;
    use crate::table::Rows;

    /// A table of one integer column, written `per` rows to a chunk, so that a column of it is
    /// genuinely several parts rather than one.
    fn table(values: &[i32], per: usize) -> Rows {
        let mut rows = MemoryTable::new(vec![LogicalType::Integer]);
        for group in values.chunks(per) {
            let held: Vec<Value> = group.iter().map(|&value| Value::Integer(value)).collect();
            let column = Vector::from_values(LogicalType::Integer, &held).expect("a column");
            rows.append(Chunk::new(vec![column]).expect("a chunk")).expect("appended");
        }
        Rows::Memory(rows)
    }

    /// The thing the whole module exists for: the parts of a column come back as one vector, in
    /// the order the rows are stored in, because a `rid` is an offset into that order.
    #[test]
    fn a_column_read_in_parts_comes_back_as_one_vector_in_row_order() {
        let values: Vec<i32> = (0..1000).collect();
        let parent = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        let column = parent.column(0, &LogicalType::Integer).expect("read").expect("it fits");
        assert_eq!(column.len(), 1000);
        assert_eq!(column.value_at(0), Value::Integer(0));
        assert_eq!(column.value_at(999), Value::Integer(999));
        assert_eq!(column.value_at(500), Value::Integer(500), "part boundaries do not renumber");
    }

    /// Asked twice, read once, and the same allocation both times. A link join asks per chunk of
    /// the child, so a column that were read per ask would be read a thousand times.
    #[test]
    fn a_column_asked_for_twice_is_the_same_allocation() {
        let parent = Parent::new(table(&(0..64).collect::<Vec<i32>>(), 16), 64 * 1024 * 1024);
        let first = parent.column(0, &LogicalType::Integer).expect("read").expect("it fits");
        let second = parent.column(0, &LogicalType::Integer).expect("read").expect("it fits");
        assert!(Arc::ptr_eq(&first, &second), "the second ask read the column again");
    }

    /// A refusal is `None` and not an error, because the caller's answer to `None` is to run the
    /// hash join, and the query answers the same either way.
    #[test]
    fn a_column_that_does_not_fit_the_budget_is_refused_rather_than_failed() {
        let values: Vec<i32> = (0..4096).collect();
        let parent = Parent::new(table(&values, 512), 64);
        assert_eq!(
            parent.column(0, &LogicalType::Integer).expect("no error"),
            None,
            "a column far past the budget is refused"
        );
        assert_eq!(parent.footprint(), 0, "and nothing is held on to afterwards");
    }

    /// The budget is over the columns together and not over each one, because what the gather
    /// costs is the parent's projection rather than any one column of it.
    #[test]
    fn the_budget_is_shared_between_the_columns_of_one_parent() {
        let mut rows = MemoryTable::new(vec![LogicalType::Integer, LogicalType::Integer]);
        let held: Vec<Value> = (0..2000).map(Value::Integer).collect();
        let first = Vector::from_values(LogicalType::Integer, &held).expect("a column");
        let second = first.clone();
        rows.append(Chunk::new(vec![first, second]).expect("a chunk")).expect("appended");
        let rows = Rows::Memory(rows);
        // Room for one column of eight thousand bytes and not for two.
        let parent = Parent::new(rows, 12 * 1024);
        assert!(parent.column(0, &LogicalType::Integer).expect("read").is_some());
        assert_eq!(
            parent.column(1, &LogicalType::Integer).expect("no error"),
            None,
            "the second column is refused because the first one is still held"
        );
    }
}

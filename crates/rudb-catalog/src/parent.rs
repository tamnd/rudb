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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rudb_common::{LogicalType, Result};
use rudb_vector::{Form, Vector, concat};

use crate::table::Rows;

/// Somewhere to run one piece of work per part, which is the thread lease of whoever is reading.
///
/// This crate is under the pipeline layer and has no threads to hand out, and reading a part of a
/// column is the most parallel work there is: a part depends on no other part and nothing is written
/// but that part's own slot. So a caller that does have threads passes them in as this, and a caller
/// that does not gets [`serially`], which is what [`Parent::column`] uses.
///
/// The contract is that every index below `count` is run exactly once and that all of them have
/// finished when this returns. How they are shared out is the caller's business, and the caller with
/// threads does it off a counter rather than by dealing ranges in advance, because the parts of a
/// real table are not the same size.
pub type Spread<'a> = dyn Fn(usize, &(dyn Fn(usize) + Sync)) -> Result<()> + 'a;

/// Every piece on the calling thread, in order.
///
/// # Errors
///
/// Never. The signature is [`Spread`]'s, and a caller with threads to lend has a thread that can
/// panic and so has something to report.
pub fn serially(count: usize, task: &(dyn Fn(usize) + Sync)) -> Result<()> {
    for at in 0..count {
        task(at);
    }
    Ok(())
}

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
        self.column_on(column, ty, &serially)
    }

    /// The same, reading the parts on whatever threads `spread` has.
    ///
    /// This is the one a link join calls, and the difference it makes is the whole reason it exists.
    /// A parent column is read once per query in [`Stream::prepare`], before any instance of the
    /// pipeline starts, so every nanosecond of it is on the pipeline's wall clock with nothing else
    /// happening. On TPC-H q12 at scale factor one that read was two thirds of the link plan's wall
    /// clock while the hash join it was being compared against built its table on the whole lease.
    ///
    /// [`Stream::prepare`]: https://docs.rs/rudb-pipeline
    ///
    /// # Errors
    ///
    /// If a part of the column cannot be read, or if the parts do not lay end to end. A part that
    /// failed is reported in part order rather than in the order the threads finished, so the same
    /// table reports the same error however the parts were shared out.
    pub fn column_on(
        &self,
        column: usize,
        ty: &LogicalType,
        spread: &Spread<'_>,
    ) -> Result<Option<Arc<Vector>>> {
        // The lock is held across the read, which serializes two threads that want the same column
        // of the same parent. That is the intended trade: the alternative is both of them reading
        // it, and the column is the expensive thing here while the wait is one read of it.
        let mut held = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(found) = held.get(&column) {
            return Ok(found.clone());
        }
        let read = self.read(column, ty, &held, spread)?;
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
        spread: &Spread<'_>,
    ) -> Result<Option<Arc<Vector>>> {
        let room = self.budget.saturating_sub(spent(held));
        let parts = self.rows.chunk_count();
        // A slot per part rather than one growing list, because the parts may be read in any order
        // and the run they make is in part order. A slot left empty is a part that said nothing or
        // one nobody reached, and the walk below tells those apart from a part that failed.
        let slots: Vec<Mutex<Option<Result<Option<Vector>>>>> =
            (0..parts).map(|_| Mutex::new(None)).collect();
        let cost = AtomicUsize::new(0);
        let over = AtomicBool::new(false);
        let task = |part: usize| {
            // Whoever went past the budget has already decided the answer, so there is no reason to
            // decode anything else. In a serial read that made the column cost one part; in a
            // parallel one it is one part per thread, because the threads already reading cannot be
            // called back. That is a bounded overshoot of a column that is being given up on.
            if over.load(Ordering::Relaxed) {
                return;
            }
            let read = self.piece(part, column);
            let footprint = match &read {
                Ok(Some(piece)) => piece.footprint(),
                Ok(None) | Err(_) => 0,
            };
            // Measured after the flattening, because the number the budget is about is what the
            // column costs once it is a vector, and added up as the parts arrive rather than at the
            // end so that a column far past the budget is given up on early.
            if cost.fetch_add(footprint, Ordering::Relaxed).saturating_add(footprint) > room {
                over.store(true, Ordering::Relaxed);
                return;
            }
            let mut slot = slots[part].lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(read);
        };
        spread(parts, &task)?;

        let mut pieces = Vec::with_capacity(parts);
        for slot in &slots {
            let taken = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
            match taken {
                // A part that failed fails the column, and the first one in part order is the one
                // reported, which is why the walk is a walk rather than a look at whoever finished
                // last.
                Some(Err(error)) => return Err(error),
                Some(Ok(piece)) => pieces.extend(piece),
                // Nothing here is not a failure. Either the part had no rows, or the budget went
                // while this one was still unread, and the test below is what tells those apart.
                None => {}
            }
        }
        if over.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let Some(whole) = concat(ty, &pieces)? else {
            return Ok(None);
        };
        // Measured again on the result, because laying the pieces end to end is where a string
        // column's arena is sized and the answer is not the sum of the pieces.
        if whole.footprint() > room {
            return Ok(None);
        }
        // The pieces go before the paging and not after it, and the order is the whole of whether
        // the paging happens. `into_pages` moves a string arena into a page only when the column it
        // is paging is the one holder of it, because the other way to do it is to copy the arena and
        // copying is what this is here to avoid. With one part, `concat` hands back that part's own
        // arena, so a `pieces` still in scope is a second holder and the paging quietly declines.
        drop(pieces);
        // Paged, because this is the definition of a column that is handed out many times: it is
        // read once here and then gathered from by every chunk of the child for the rest of the
        // query. The form that cares is the string body. A flatten of a gather over one takes a
        // handle to the arena when the arena is a page and copies the bytes of every string it
        // reached when it is not. Without this line every chunk copies, and on TPC-H q12 at scale
        // factor one that was fourteen hundred copies a query out of a column of five distinct
        // values.
        Ok(Some(Arc::new(whole.into_pages())))
    }

    /// One part of one column, decoded, or nothing when the part holds no rows.
    ///
    /// Everything expensive about reading a parent column is in here, which is why this is the unit
    /// the threads are shared out over: the read of the part and the decode of whatever encoding its
    /// writer chose. It touches nothing but its own part.
    fn piece(&self, part: usize, column: usize) -> Result<Option<Vector>> {
        let chunk = self.rows.read(part, &[column])?;
        let piece = chunk.column(0)?;
        // A part with no rows in it contributes no rows to the run and would make `concat` decline
        // the whole of it, which is a column given up on over a part that says nothing.
        if piece.is_empty() {
            return Ok(None);
        }
        // flatten: the whole point of this type is a run the gather can index by a row id of the
        // parent table, and a row id has no meaning against a bit packed part that has not been
        // decoded. See the module doc for why this is a decode the hash join pays as well.
        if piece.form() == Form::Flat {
            return Ok(Some(piece.clone()));
        }
        Ok(Some(piece.flatten()?))
    }
}

/// What the columns already held cost between them.
fn spent(held: &HashMap<usize, Option<Arc<Vector>>>) -> usize {
    held.values().flatten().map(|column| column.footprint()).sum()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::{LogicalType, Result, Value};
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

    /// The same table with one string column, which is the form the paging below is about.
    fn strings(values: &[&str], per: usize) -> Rows {
        let mut rows = MemoryTable::new(vec![LogicalType::Varchar]);
        for group in values.chunks(per) {
            let held: Vec<Value> =
                group.iter().map(|value| Value::Varchar((*value).to_string())).collect();
            let column = Vector::from_values(LogicalType::Varchar, &held).expect("a column");
            rows.append(Chunk::new(vec![column]).expect("a chunk")).expect("appended");
        }
        Rows::Memory(rows)
    }

    /// A string column comes back over a page, which is what keeps a link join from copying the
    /// bytes it gathers once per chunk of the child.
    ///
    /// Asserted here rather than left to the vector crate because the thing that can break it is
    /// local: `into_pages` declines an arena that has another holder, so a piece of the read left
    /// alive would turn this into a silent no change with every test still green and q12 still slow.
    ///
    /// Several parts, because that is the read that lays an arena out and the one a parent worth
    /// gathering from has. A column that arrived in one part is handed back as that part and keeps
    /// whatever form the part was in, which for a stored column is already over a page.
    #[test]
    fn a_string_column_comes_back_over_a_page_rather_than_an_arena_of_its_own() {
        let values = ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"];
        let held: Vec<&str> = (0..500).map(|row| values[row % values.len()]).collect();
        for per in [250, 64] {
            let parent = Parent::new(strings(&held, per), 64 * 1024 * 1024);
            let column = parent.column(0, &LogicalType::Varchar).expect("read").expect("it fits");
            let (_, arena) = column.shared_views().expect("a string column is string views");
            assert!(arena.is_shared(), "{per} rows a part came back over an arena of its own");
            assert_eq!(column.len(), 500);
            assert_eq!(column.value_at(499), Value::Varchar("5-LOW".into()));
        }
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
        // Two parts of a thousand rows each, so that each column is laid end to end into a run of
        // its own. A column of one part comes back as a share of the table's page, and since #1491
        // a share of a page the table is holding anyway is charged as the share it is.
        for part in 0..2 {
            let held: Vec<Value> = (part * 1000..part * 1000 + 1000).map(Value::Integer).collect();
            let first = Vector::from_values(LogicalType::Integer, &held).expect("a column");
            let second = Vector::from_values(LogicalType::Integer, &held).expect("a column");
            rows.append(Chunk::new(vec![first, second]).expect("a chunk")).expect("appended");
        }
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

    /// A [`Spread`] that runs each part on a thread of its own, in no particular order.
    ///
    /// Not what the engine passes, which shares the parts out over a fixed lease off a counter. This
    /// is the harsher version on purpose: a thread per part and nothing deciding who goes first is
    /// the widest the interleaving can get, so anything the read does that depends on part order
    /// happening to be arrival order shows up here.
    fn scattered(count: usize, task: &(dyn Fn(usize) + Sync)) -> Result<()> {
        std::thread::scope(|scope| {
            let running: Vec<_> =
                (0..count).rev().map(|at| scope.spawn(move || task(at))).collect();
            for thread in running {
                thread.join().expect("a part reader panicked");
            }
        });
        Ok(())
    }

    /// The point of the parallel read: the same column, whichever thread read which part.
    ///
    /// Row order is the whole of correctness here, because a `rid` is an offset into it, and the read
    /// no longer appends the parts in the order it read them. So this asserts the order rather than
    /// just the contents, at every part boundary and at both ends.
    #[test]
    fn a_column_read_on_many_threads_comes_back_in_the_same_order_as_one_read_on_one() {
        let values: Vec<i32> = (0..1000).collect();
        let one = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        let many = Parent::new(table(&values, 128), 64 * 1024 * 1024);
        let serial = one.column(0, &LogicalType::Integer).expect("read").expect("it fits");
        let parallel =
            many.column_on(0, &LogicalType::Integer, &scattered).expect("read").expect("it fits");
        assert_eq!(parallel.len(), serial.len());
        for row in 0..serial.len() {
            assert_eq!(parallel.value_at(row), serial.value_at(row), "row {row} moved");
        }
    }

    /// A string column too, because that one is laid out rather than copied and the arena is built
    /// from the pieces in the order the walk found them.
    #[test]
    fn a_string_column_read_on_many_threads_comes_back_in_row_order_and_over_a_page() {
        let values = ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"];
        let held: Vec<&str> = (0..500).map(|row| values[row % values.len()]).collect();
        let parent = Parent::new(strings(&held, 64), 64 * 1024 * 1024);
        let column =
            parent.column_on(0, &LogicalType::Varchar, &scattered).expect("read").expect("it fits");
        let (_, arena) = column.shared_views().expect("a string column is string views");
        assert!(arena.is_shared(), "a parallel read came back over an arena of its own");
        assert_eq!(column.len(), 500);
        for (row, want) in held.iter().enumerate() {
            assert_eq!(column.value_at(row), Value::Varchar((*want).into()), "row {row} moved");
        }
    }

    /// The budget still refuses, and still holds nothing afterwards, when the parts arrive at once.
    ///
    /// This is the case the parallel read changes the most. Serially the first part past the budget
    /// ends the read; here every thread may be holding a part by the time one of them notices. What
    /// has to survive is the answer, which is a refusal rather than a short column.
    #[test]
    fn a_column_past_the_budget_is_refused_however_many_threads_read_it() {
        let values: Vec<i32> = (0..4096).collect();
        let parent = Parent::new(table(&values, 512), 64);
        assert_eq!(
            parent.column_on(0, &LogicalType::Integer, &scattered).expect("no error"),
            None,
            "a column far past the budget is refused"
        );
        assert_eq!(parent.footprint(), 0, "and nothing is held on to afterwards");
    }
}

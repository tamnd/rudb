//! A table that lives in memory, which is what M0 stores rows in.
//!
//! This is not the storage format. There are no blocks, no compression and no buffer manager in
//! here, and every one of those is what the rest of this crate becomes at M2. What this is, is
//! somewhere for rows to be so that the binder and the executor can be written and tested against
//! something real, and a shape that the real thing can replace without the layers above it noticing:
//! a table is a sequence of chunks, a scan reads them in order, and a scan asks for the columns it
//! wants rather than all of them.
//!
//! The one thing it does get right on purpose is that a read is by chunk and by column, and not by
//! row. A row-at-a-time interface here would be an interface every operator above would grow
//! against, and unwinding that later is the rewrite this project exists to avoid.
//!
//! # Row groups
//!
//! What a scan reads and what the table stores are two different sizes, and they answer two
//! different questions. How many values an operator should work on at once is a question about L1
//! and L2, and the answer is [`VECTOR_SIZE`]. How many rows should sit together in one run of memory
//! is a question about how many places a scan has to jump to, and the answer is much larger.
//!
//! So the rows are held in groups of [`ROWS_PER_GROUP`], one page per column per group, and a chunk
//! is a window cut out of a page. A twenty million row table of two columns used to be 19,532 chunks
//! and about 39,000 buffers, and is now 163 groups and 326 pages. Nothing above the storage seam
//! sees it: the chunk numbering is what it always was, and [`MemoryTable::read`] still answers chunk
//! `n` with the same rows it used to. What changed is where those rows are, which is next to the
//! rows of the chunks either side of them.
//!
//! The directory that says where each chunk is, is kept rather than computed, because a chunk does
//! not have to arrive [`VECTOR_SIZE`] rows long and the arithmetic would be wrong the first time one
//! does not. It is three numbers a chunk and it is what lets a group that could not be laid end to
//! end sit next to one that could without anything else knowing the difference.
//!
//! # It does have statistics
//!
//! A zone map per chunk, and per chunk rather than per group on purpose. The group is the unit of
//! layout and the chunk is still the unit of pruning, because the whole value of a zone map is how
//! few rows it speaks for: the note at the top of `zone.rs` measures ClickBench query 37 skipping 75
//! percent of its chunks and running two and a half times faster, and a synopsis covering a hundred
//! and twenty times as many rows would skip almost nothing. A coarser second level on top of these,
//! so that a group can be ruled out without reading its chunks' zones at all, is worth having and is
//! its own piece of work.
//!
//! What they are is built on the way in, which is `zone.rs`. They were put here to skip a chunk
//! a filter rules out, and they hold more than that: an exact null count for every column whatever
//! form it arrived in, the two ends, and the total of an integer column. So a `COUNT`, a `MIN`, a
//! `MAX`, a `SUM` and an `AVG` over a whole table are questions this can answer out of numbers it
//! already has rather than by reading twenty million rows, which is what `null_count`,
//! `exact_extremes` and `exact_sum` are for and what a native file has always done from its
//! directory. The load already paid for them, and [`MemoryTable::stats_ns`] is what it paid.

use std::time::Instant;

use rudb_common::bounds::Bound;
use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::vector::VECTOR_SIZE;
use rudb_vector::{Chunk, Vector};

use crate::zone::{Probe, Range, Zone};

/// How many rows one row group holds.
///
/// DuckDB's number, and what `spec/storage-v2/` is designed around, so a table in memory and a table
/// in a file are cut the same way and a checkpoint is not also a re-layout. It is exactly 120
/// [`VECTOR_SIZE`] chunks, which is not required by anything here and does mean a group filled by a
/// loader that hands over full chunks holds a whole number of them.
pub const ROWS_PER_GROUP: usize = 122_880;

/// One group's columns, each the full height of the group.
#[derive(Debug, Clone)]
struct Group {
    /// One page per column, in the table's column order.
    columns: Vec<Vector>,
    /// How many rows every one of those columns has.
    rows: usize,
}

/// Where one chunk's rows are.
///
/// Two cases and not one, because a group is laid end to end when it seals and a table being written
/// to has rows in it that no group has claimed yet. A reader in the middle of an insert sees those
/// rows as the chunks they arrived as, which is what the table did for every chunk before there were
/// groups, so it is a layout the read path already knows how to answer.
#[derive(Debug, Clone, Copy)]
enum Slot {
    /// `len` rows starting at row `at` of group `group`.
    Window { group: usize, at: usize, len: usize },
    /// A chunk of the group still filling, at position `at` of the open run.
    Open { at: usize },
}

/// A table held in memory as row groups, read a chunk at a time.
#[derive(Debug, Clone)]
pub struct MemoryTable {
    types: Vec<LogicalType>,
    /// The sealed groups, one page per column each.
    groups: Vec<Group>,
    /// Where each chunk is, in the chunk numbering a scan reads.
    slots: Vec<Slot>,
    /// The chunks of the group still filling, in the order they arrived.
    open: Vec<Chunk>,
    /// How many rows are in `open`, which is what decides when it seals.
    open_rows: usize,
    /// One per chunk, in the same numbering as `slots`.
    zones: Vec<Zone>,
    rows: usize,
    stats_ns: u64,
}

impl MemoryTable {
    /// An empty table of the given column types.
    #[must_use]
    pub fn new(types: Vec<LogicalType>) -> Self {
        Self {
            types,
            groups: Vec::new(),
            slots: Vec::new(),
            open: Vec::new(),
            open_rows: 0,
            zones: Vec::new(),
            rows: 0,
            stats_ns: 0,
        }
    }

    /// The column types.
    #[must_use]
    pub fn types(&self) -> &[LogicalType] {
        &self.types
    }

    /// How many columns.
    #[must_use]
    pub fn width(&self) -> usize {
        self.types.len()
    }

    /// How many rows, across every chunk.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows
    }

    /// Whether the table has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// How many chunks a scan will read.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.slots.len()
    }

    /// How many groups the rows are laid out in, which is what a scan does not see.
    ///
    /// Here so that a test can say what the layout is rather than guess at it from the read path,
    /// and so that anything measuring the table can report the number that actually moved.
    #[must_use]
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Appends a chunk, which has to have the table's column types.
    ///
    /// An empty chunk is dropped rather than stored, because a scan that has to skip empty chunks
    /// is a scan with a branch in it that exists only because an operator upstream was sloppy.
    ///
    /// # Errors
    ///
    /// If the chunk's columns are not the table's columns.
    pub fn append(&mut self, chunk: Chunk) -> Result<()> {
        if chunk.width() != self.types.len() {
            return Err(Error::internal(format!(
                "a chunk of {} columns appended to a table of {}",
                chunk.width(),
                self.types.len()
            )));
        }
        for (index, (held, wanted)) in chunk.types().iter().zip(&self.types).enumerate() {
            if held != wanted {
                return Err(Error::internal(format!(
                    "column {index} of the chunk is {held} and the table's is {wanted}"
                )));
            }
        }
        if chunk.is_empty() {
            return Ok(());
        }
        let started = Instant::now();
        let zone = Zone::of(&chunk);
        self.stats_ns += started.elapsed().as_nanos() as u64;
        self.rows += chunk.len();
        self.zones.push(zone);
        self.open_rows += chunk.len();
        self.slots.push(Slot::Open { at: self.open.len() });
        // Stored as pages even before the group seals, because a chunk that goes through the
        // fallback is never laid end to end and this is what makes reading it a reference count bump
        // rather than a copy. A page laid end to end afterwards is copied out of once, here.
        self.open.push(chunk.into_pages());
        if self.open_rows >= ROWS_PER_GROUP {
            self.seal();
        }
        Ok(())
    }

    /// Lays the open chunks end to end into one group, or keeps them as the chunks they are.
    ///
    /// Every column has to lay for the group to lay, because a group holds one page per column of
    /// the same height and half of one is not a group. A column that will not lay is a column that
    /// arrived encoded, and encoded is the form worth keeping, so the answer is to leave the whole
    /// run as it came: each chunk becomes a group of its own, which is three numbers in the
    /// directory and is exactly what the table did before there were groups.
    ///
    /// Sealing is where the copy is. It is one pass over the rows of the group per column, and it
    /// buys every read afterwards a window into one run instead of a jump to one of a hundred and
    /// twenty allocations.
    fn seal(&mut self) {
        if self.open.is_empty() {
            return;
        }
        let first = self.slots.len() - self.open.len();
        match self.laid() {
            Some(columns) => {
                let group = self.groups.len();
                let mut at = 0;
                for (slot, chunk) in self.slots[first..].iter_mut().zip(&self.open) {
                    *slot = Slot::Window { group, at, len: chunk.len() };
                    at += chunk.len();
                }
                self.groups.push(Group { columns, rows: at });
            }
            None => {
                for (slot, chunk) in self.slots[first..].iter_mut().zip(self.open.drain(..)) {
                    *slot = Slot::Window { group: self.groups.len(), at: 0, len: chunk.len() };
                    let rows = chunk.len();
                    self.groups.push(Group { columns: chunk.into_columns(), rows });
                }
            }
        }
        self.open.clear();
        self.open_rows = 0;
    }

    /// The open chunks as one page per column, or `None` if any column will not lay end to end.
    ///
    /// Collected a column at a time rather than a chunk at a time because that is the direction the
    /// pages run in, and the borrow of each chunk's column ends inside the loop, so nothing is
    /// cloned to build the list handed to [`rudb_vector::concat()`].
    ///
    /// An error from the concatenation is treated as a run that will not lay. It means a piece said
    /// it was flat and did not hold a flat run of its own length, which the vector crate believes is
    /// impossible, and the safe thing for a table to do about a layout it cannot build is to keep
    /// the rows it was given rather than to fail an insert over it.
    fn laid(&self) -> Option<Vec<Vector>> {
        let mut pages = Vec::with_capacity(self.types.len());
        let mut pieces = Vec::with_capacity(self.open.len());
        for (column, ty) in self.types.iter().enumerate() {
            pieces.clear();
            for chunk in &self.open {
                pieces.push(chunk.column(column).ok()?.clone());
            }
            pages.push(rudb_vector::concat(ty, &pieces).ok()??);
        }
        Some(pages)
    }

    /// How long this table has spent building statistics, in nanoseconds.
    ///
    /// A load reports this next to its own wall time so that the price of the zone maps is a number
    /// somebody can argue with rather than something buried inside the load. See `zone.rs`.
    #[must_use]
    pub fn stats_ns(&self) -> u64 {
        self.stats_ns
    }

    /// The zone of one chunk, or `None` past the end.
    #[must_use]
    pub fn zone(&self, index: usize) -> Option<&Zone> {
        self.zones.get(index)
    }

    /// Whether the probes rule out every row of chunk `index`.
    ///
    /// A chunk with no zone is a chunk that is read, because saying nothing about a chunk has to
    /// mean keeping it. That is what makes this safe to ask about any index at all.
    #[must_use]
    pub fn skips(&self, index: usize, probes: &[Probe]) -> bool {
        self.zones.get(index).is_some_and(|zone| zone.skips(probes))
    }

    /// Whether the probes keep every row of chunk `index`.
    ///
    /// A chunk with no zone is a chunk that gets compared, for the same reason it is a chunk that
    /// gets read: saying nothing about a chunk has to mean doing the work on it.
    #[must_use]
    pub fn certain(&self, index: usize, probes: &[Probe]) -> bool {
        self.zones.get(index).is_some_and(|zone| zone.certain(probes))
    }

    /// How many rows of one column are null, added up over the chunks.
    ///
    /// Always an answer, because a zone's null count is the one number in it that never comes from a
    /// summary somebody else wrote: `Range::of` counts the validity mask whatever form the column
    /// arrived in. That is what makes it exact for a dictionary and a bit packed column too, where
    /// the two ends are allowed to be wider than the rows.
    ///
    /// # Errors
    ///
    /// If the column is outside the table.
    pub fn null_count(&self, column: usize) -> Result<usize> {
        let mut nulls = 0;
        for (range, _) in self.ranges(column)? {
            nulls += range.nulls;
        }
        Ok(nulls)
    }

    /// The smallest and the largest value of one column, when every chunk walked its rows.
    ///
    /// A zone's ends are allowed to be wider than the truth, because ends that rule out a chunk that
    /// could not match are still right when they rule out nothing. That is what makes them cheap for
    /// a form this cannot read, and it is also what stops them answering a `MIN`, so each range says
    /// which of the two it is and this answers only when all of them looked.
    ///
    /// A chunk of nothing but nulls has no ends and says nothing about the column's, so it is
    /// skipped rather than given up on. A chunk that has rows and still has no ends is a form this
    /// cannot see into, and answering from the other chunks would answer with ends that do not cover
    /// its rows, so that one gives up.
    ///
    /// # Errors
    ///
    /// If the column is outside the table.
    pub fn exact_extremes(&self, column: usize) -> Result<Option<(Bound, Bound)>> {
        let mut low: Option<Bound> = None;
        let mut high: Option<Bound> = None;
        for (range, rows) in self.ranges(column)? {
            if !range.exact {
                return Ok(None);
            }
            let (Some(small), Some(large)) = (range.low.as_ref(), range.high.as_ref()) else {
                if rows > range.nulls {
                    return Ok(None);
                }
                continue;
            };
            low = Some(low.map_or_else(|| small.clone(), |held| held.smaller(small.clone())));
            high = Some(high.map_or_else(|| large.clone(), |held| held.larger(large.clone())));
        }
        Ok(low.zip(high))
    }

    /// The total of one integer column and how many rows went into it, when every chunk has a total.
    ///
    /// The count beside the total is the rows that are not null, because that is what a `SUM` adds up
    /// and what an `AVG` divides by, and working it out from the row count and the null count
    /// afterwards would walk the same zones twice.
    ///
    /// `None` for a column no chunk of which could be added up, which is every column that is not an
    /// integer one, and for a table so large that adding the chunks together overflows an `i128`.
    ///
    /// # Errors
    ///
    /// If the column is outside the table.
    pub fn exact_sum(&self, column: usize) -> Result<Option<(i128, u64)>> {
        let mut total = 0_i128;
        let mut rows = 0_u64;
        for (range, held) in self.ranges(column)? {
            let Some(part) = range.sum else { return Ok(None) };
            let Some(sum) = total.checked_add(part) else { return Ok(None) };
            total = sum;
            rows = rows.saturating_add((held - range.nulls) as u64);
        }
        Ok(Some((total, rows)))
    }

    /// The range of one column of every chunk, beside how many rows that chunk has.
    ///
    /// Collected rather than returned as an iterator so that a zone narrower than the table is an
    /// error here instead of a chunk quietly dropped out of the middle of a fold, which would answer
    /// a total over some of the rows as though it were over all of them.
    ///
    /// # Errors
    ///
    /// If the column is outside the table, or if a chunk has no range for it, which `append` makes
    /// impossible by building the zone from the chunk it has already checked the width of.
    fn ranges(&self, column: usize) -> Result<Vec<(&Range, usize)>> {
        if column >= self.types.len() {
            return Err(Error::internal(format!(
                "column {column} of a table that has {}",
                self.types.len()
            )));
        }
        let mut found = Vec::with_capacity(self.zones.len());
        for (zone, slot) in self.zones.iter().zip(&self.slots) {
            let range = zone
                .column(column)
                .ok_or_else(|| Error::internal("a chunk's zone is narrower than the table"))?;
            found.push((range, self.rows_of(*slot)));
        }
        Ok(found)
    }

    /// How many rows one slot names.
    fn rows_of(&self, slot: Slot) -> usize {
        match slot {
            Slot::Window { len, .. } => len,
            Slot::Open { at } => self.open.get(at).map_or(0, Chunk::len),
        }
    }

    /// Appends rows given one at a time, splitting them into chunks.
    ///
    /// The slow way in, for an `INSERT` and for a test. It transposes, which is the whole cost:
    /// rows arrive across the columns and a chunk is down them.
    ///
    /// # Errors
    ///
    /// If a row is not as wide as the table, or if a value is not one its column can hold.
    pub fn append_rows(&mut self, rows: &[Vec<Value>]) -> Result<()> {
        for (index, row) in rows.iter().enumerate() {
            if row.len() != self.types.len() {
                return Err(Error::internal(format!(
                    "row {index} has {} values and the table has {} columns",
                    row.len(),
                    self.types.len()
                )));
            }
        }
        for batch in rows.chunks(VECTOR_SIZE) {
            let mut columns = Vec::with_capacity(self.types.len());
            for (position, ty) in self.types.iter().enumerate() {
                let down: Vec<Value> = batch.iter().map(|row| row[position].clone()).collect();
                columns.push(Vector::from_values(ty.clone(), &down)?);
            }
            self.append(Chunk::with_rows(columns, batch.len())?)?;
        }
        Ok(())
    }

    /// One chunk's worth of the named columns, in the order they are named.
    ///
    /// The columns are shared rather than copied. A chunk is a window into its group's pages, so the
    /// vector handed back here points at the stored values and the cost of this call is one atomic
    /// increment per column and whatever the cut has to rewrite. It used to copy, and
    /// `spec/perf/12-the-chunk-and-the-page.md` measured
    /// what that cost: 1,803 instructions a chunk of memcpy and 1,869 of malloc and free on a `sum`
    /// over a twenty million row table, which is 27 percent of the chunk and none of it the query's
    /// work.
    ///
    /// Nothing downstream can tell, because a write through a shared buffer copies it out first,
    /// which is `Buffer::to_mut`, so an operator that means to modify a column it was handed pays
    /// the copy there instead and one that does not pays nothing. Asking for the columns rather than
    /// taking them all still matters, because the atomic increments and the validity masks are per
    /// column, and it is the same reason projection pushdown exists.
    ///
    /// What is still copied is the parts a cut has to rewrite: a string column's views, a
    /// dictionary's codes, and a validity mask that is not all valid or all invalid. Those are
    /// sixteen, four and an eighth of a byte a row against the payload, and each of them is a `Vec`
    /// where the payload is a page.
    ///
    /// # Errors
    ///
    /// If there is no such chunk, or if a column is past the end of the table.
    pub fn read(&self, chunk: usize, columns: &[usize]) -> Result<Chunk> {
        let slot = *self.slots.get(chunk).ok_or_else(|| {
            Error::internal(format!(
                "chunk {chunk} of a table that has {} chunks",
                self.slots.len()
            ))
        })?;
        match slot {
            Slot::Window { group, at, len } => {
                let held = self
                    .groups
                    .get(group)
                    .ok_or_else(|| Error::internal("a chunk names a group that is not there"))?;
                // Checked once here rather than once per column, because a window past the end of
                // its group is the directory disagreeing with the pages and the message wanted is
                // that, not the same out of range cut reported by whichever column was read first.
                if at + len > held.rows {
                    return Err(Error::internal(format!(
                        "chunk {chunk} is rows {at} to {} of a group of {}",
                        at + len,
                        held.rows
                    )));
                }
                let mut picked = Vec::with_capacity(columns.len());
                for &column in columns {
                    let page = held.columns.get(column).ok_or_else(|| {
                        Error::internal(format!(
                            "column {column} of a table that has {}",
                            self.types.len()
                        ))
                    })?;
                    picked.push(page.slice(at, len)?);
                }
                Chunk::with_rows(picked, len)
            }
            Slot::Open { at } => {
                let held = self
                    .open
                    .get(at)
                    .ok_or_else(|| Error::internal("a chunk names an open chunk that is gone"))?;
                let mut picked = Vec::with_capacity(columns.len());
                for &column in columns {
                    picked.push(held.column(column)?.clone());
                }
                Chunk::with_rows(picked, held.len())
            }
        }
    }

    /// How many rows chunk `index` has, or `None` past the end.
    ///
    /// Answered out of the directory, which is why it is here rather than left to a caller that
    /// reads the chunk and asks how long it is. A caller walking the table to turn a row ordinal
    /// into a chunk and an offset wants the lengths and not the rows, and reading every column of
    /// every chunk to find out how many rows are in them is the cost this is for.
    #[must_use]
    pub fn chunk_len(&self, index: usize) -> Option<usize> {
        self.slots.get(index).map(|&slot| self.rows_of(slot))
    }

    /// One stored chunk, whole.
    ///
    /// Every column of it, which for a sealed group is a window per column and for an open one is a
    /// reference count bump per column. It comes back owned rather than borrowed because a chunk is
    /// something the table now builds when it is asked for rather than something it holds, and that
    /// is the whole point of the groups: the rows of a chunk are next to the rows of its neighbours
    /// instead of in an allocation of their own.
    #[must_use]
    pub fn chunk(&self, index: usize) -> Option<Chunk> {
        let columns: Vec<usize> = (0..self.types.len()).collect();
        self.read(index, &columns).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn people() -> MemoryTable {
        let mut table = MemoryTable::new(vec![LogicalType::Integer, LogicalType::Varchar]);
        table
            .append_rows(&[
                vec![Value::Integer(1), Value::Varchar("ada".to_string())],
                vec![Value::Integer(2), Value::Null],
                vec![Value::Integer(3), Value::Varchar("grace".to_string())],
            ])
            .expect("three rows of the table's own types");
        table
    }

    /// A table of one integer column over several chunks, so the folds have something to fold.
    fn counted(rows: usize) -> MemoryTable {
        let mut table = MemoryTable::new(vec![LogicalType::Integer]);
        let values: Vec<Vec<Value>> = (0..rows)
            .map(|row| {
                vec![if row % 5 == 0 { Value::Null } else { Value::Integer(row as i32 % 7) }]
            })
            .collect();
        table.append_rows(&values).expect("one integer a row");
        table
    }

    #[test]
    fn the_nulls_of_a_column_are_added_up_over_the_chunks() {
        let table = counted(VECTOR_SIZE * 2 + 10);
        assert!(table.chunk_count() > 1, "one chunk would not test the fold");
        let rows = table.len();
        let wanted = (0..rows).filter(|row| row % 5 == 0).count();
        assert_eq!(table.null_count(0).expect("the only column"), wanted);
        // A string column that never had a null still answers, with zero.
        let mut words = MemoryTable::new(vec![LogicalType::Varchar]);
        words.append_rows(&[vec![Value::Varchar("a".to_string())]]).expect("one string");
        assert_eq!(words.null_count(0).expect("the only column"), 0);
    }

    #[test]
    fn an_empty_table_has_no_nulls_no_ends_and_a_total_of_nothing() {
        let table = MemoryTable::new(vec![LogicalType::Integer]);
        assert_eq!(table.null_count(0).expect("the only column"), 0);
        assert_eq!(table.exact_extremes(0).expect("the only column"), None);
        // Zero over no rows rather than no answer, which is what a `SUM` of nothing finishes to
        // `NULL` from, because the count beside it is what says there were no rows.
        assert_eq!(table.exact_sum(0).expect("the only column"), Some((0, 0)));
    }

    #[test]
    fn the_ends_and_the_total_are_the_chunks_put_together() {
        let table = counted(VECTOR_SIZE * 2 + 10);
        let rows = table.len();
        let kept: Vec<i128> =
            (0..rows).filter(|row| row % 5 != 0).map(|row| (row % 7) as i128).collect();
        let (total, counted_rows) = table.exact_sum(0).expect("the only column").expect("integers");
        assert_eq!(total, kept.iter().sum::<i128>());
        assert_eq!(counted_rows as usize, kept.len());
        let (low, high) = table.exact_extremes(0).expect("the only column").expect("integers");
        assert_eq!(low, Bound::Int(*kept.iter().min().expect("some rows")));
        assert_eq!(high, Bound::Int(*kept.iter().max().expect("some rows")));
        // The nulls are left out of both, the same way `MIN` and `SUM` leave them out.
        assert_eq!(counted_rows as usize + table.null_count(0).expect("the column"), rows);
    }

    #[test]
    fn a_column_that_cannot_be_added_up_has_no_total_and_still_has_ends() {
        let mut table = MemoryTable::new(vec![LogicalType::Varchar]);
        table
            .append_rows(&[
                vec![Value::Varchar("pear".to_string())],
                vec![Value::Null],
                vec![Value::Varchar("apple".to_string())],
            ])
            .expect("three strings");
        assert_eq!(table.exact_sum(0).expect("the only column"), None);
        let (low, high) = table.exact_extremes(0).expect("the only column").expect("strings");
        assert_eq!(low, Bound::of_value(&Value::Varchar("apple".to_string())).expect("a bound"));
        assert_eq!(high, Bound::of_value(&Value::Varchar("pear".to_string())).expect("a bound"));
    }

    #[test]
    fn a_column_the_table_does_not_have_is_an_error_rather_than_an_empty_answer() {
        let table = people();
        // Two columns, so index two is one past the end. An answer of `None` here would read as the
        // column having no statistics, and the caller would go and read rows that are not there.
        assert!(table.null_count(2).is_err());
        assert!(table.exact_extremes(2).is_err());
        assert!(table.exact_sum(2).is_err());
    }

    #[test]
    fn rows_go_in_and_come_back_out() {
        let table = people();
        assert_eq!(table.len(), 3);
        assert_eq!(table.chunk_count(), 1);
        let chunk = table.read(0, &[0, 1]).expect("both columns of the only chunk");
        assert_eq!(chunk.value_at(0, 1), Value::Varchar("ada".to_string()));
        assert_eq!(chunk.value_at(1, 1), Value::Null);
        assert_eq!(chunk.value_at(2, 0), Value::Integer(3));
    }

    #[test]
    fn a_read_gives_back_only_the_columns_it_was_asked_for() {
        let table = people();
        let chunk = table.read(0, &[1]).expect("the second column");
        assert_eq!(chunk.width(), 1);
        assert_eq!(chunk.len(), 3);
        assert_eq!(chunk.value_at(2, 0), Value::Varchar("grace".to_string()));
    }

    /// `SELECT count(*) FROM t` reads no columns and still has to be told how many rows there were.
    #[test]
    fn a_read_of_no_columns_still_says_how_many_rows() {
        let table = people();
        let chunk = table.read(0, &[]).expect("no columns");
        assert_eq!(chunk.width(), 0);
        assert_eq!(chunk.len(), 3);
    }

    /// The point of storing a chunk as pages. A read points at the stored values rather than copying
    /// them, asserted on the address, because the values are the same either way.
    #[test]
    fn a_read_points_at_the_stored_values_rather_than_copying_them() {
        use rudb_vector::vector::Data;

        let mut table = MemoryTable::new(vec![LogicalType::BigInt]);
        let rows: Vec<Vec<Value>> = (0..64).map(|n| vec![Value::BigInt(n)]).collect();
        table.append_rows(&rows).expect("bigints");

        let address = |chunk: &Chunk| match chunk.column(0).expect("one column").data() {
            Some(Data::Int64(values)) => values.as_slice().as_ptr() as usize,
            _ => panic!("a BIGINT column is not a run of i64"),
        };
        let stored = address(&table.chunk(0).expect("the only chunk"));
        let first = table.read(0, &[0]).expect("the only chunk");
        let second = table.read(0, &[0]).expect("the only chunk again");
        assert_eq!(address(&first), stored, "the read copied the column out");
        assert_eq!(address(&second), stored, "the second read copied the column out");
        assert_eq!(first.value_at(7, 0), Value::BigInt(7));
        assert_eq!(second.value_at(63, 0), Value::BigInt(63));

        // Nothing can write through what it was handed, because a vector has no mutating method at
        // all and the only way at the values is `Buffer::to_mut`, which copies the page out first.
        // So the sharing is safe without a rule anybody has to remember.

        // The memory limit is not told about the page twice. Three holders of one 512 byte page add
        // up to the page rather than to three of it, which is the rule in `Buffer::footprint`.
        let charged = table.chunk(0).expect("the only chunk").footprint()
            + first.footprint()
            + second.footprint();
        assert!(charged < 512 * 2, "{charged} charged for one 512 byte page held three times");
    }

    #[test]
    fn more_rows_than_a_vector_become_more_than_one_chunk() {
        let mut table = MemoryTable::new(vec![LogicalType::BigInt]);
        let rows: Vec<Vec<Value>> =
            (0..VECTOR_SIZE + 5).map(|n| vec![Value::BigInt(n as i64)]).collect();
        table.append_rows(&rows).expect("bigints");
        assert_eq!(table.len(), VECTOR_SIZE + 5);
        assert_eq!(table.chunk_count(), 2);
        let last = table.read(1, &[0]).expect("the second chunk");
        assert_eq!(last.len(), 5);
        assert_eq!(last.value_at(4, 0), Value::BigInt((VECTOR_SIZE + 4) as i64));
    }

    #[test]
    fn a_chunk_of_the_wrong_types_is_caught() {
        let mut table = MemoryTable::new(vec![LogicalType::Integer]);
        let wrong = Chunk::new(vec![
            Vector::from_values(LogicalType::Varchar, &[Value::Varchar("x".to_string())])
                .expect("a string column"),
        ])
        .expect("one column");
        let error = table.append(wrong).expect_err("a varchar is not an integer");
        assert!(error.message().contains("column 0"), "{error}");
    }

    #[test]
    fn a_row_of_the_wrong_width_is_caught() {
        let mut table = MemoryTable::new(vec![LogicalType::Integer, LogicalType::Integer]);
        let error =
            table.append_rows(&[vec![Value::Integer(1)]]).expect_err("a row of one is not a row");
        assert!(error.message().contains("row 0"), "{error}");
    }

    /// A table of one bigint column, filled a full chunk at a time.
    fn filled(chunks: usize) -> MemoryTable {
        let mut table = MemoryTable::new(vec![LogicalType::BigInt]);
        for chunk in 0..chunks {
            let held: Vec<Value> = (0..VECTOR_SIZE)
                .map(|row| Value::BigInt((chunk * VECTOR_SIZE + row) as i64))
                .collect();
            let column = Vector::from_values(LogicalType::BigInt, &held).expect("bigints");
            table.append(Chunk::new(vec![column]).expect("one column")).expect("a full chunk");
        }
        table
    }

    /// Every row of every chunk, read back through the chunk numbering a scan uses.
    fn every_row(table: &MemoryTable) -> Vec<Value> {
        (0..table.chunk_count())
            .flat_map(|at| {
                let chunk = table.read(at, &[0]).expect("a chunk the table says it has");
                (0..chunk.len()).map(|row| chunk.value_at(row, 0)).collect::<Vec<_>>()
            })
            .collect()
    }

    /// The chunk numbering is what it was, and so are the rows in it. The layout is not.
    #[test]
    fn a_full_group_is_one_page_a_column_and_the_chunks_are_windows_into_it() {
        let chunks = ROWS_PER_GROUP / VECTOR_SIZE;
        let table = filled(chunks + 3);
        assert_eq!(table.chunk_count(), chunks + 3, "the chunk numbering moved");
        assert_eq!(table.group_count(), 1, "the full group did not seal");
        assert_eq!(table.len(), (chunks + 3) * VECTOR_SIZE);
        let wanted: Vec<Value> = (0..table.len()).map(|row| Value::BigInt(row as i64)).collect();
        assert_eq!(every_row(&table), wanted, "the rows moved when the layout did");
    }

    /// What a window is, said on the address, because the values are the same either way.
    #[test]
    fn two_chunks_of_one_group_read_out_of_the_same_run_of_memory() {
        use rudb_vector::vector::Data;

        let table = filled(ROWS_PER_GROUP / VECTOR_SIZE);
        assert_eq!(table.group_count(), 1, "one group to read two chunks out of");
        let address = |chunk: &Chunk| match chunk.column(0).expect("one column").data() {
            Some(Data::Int64(values)) => values.as_slice().as_ptr() as usize,
            _ => panic!("a BIGINT column is not a run of i64"),
        };
        let first = address(&table.read(0, &[0]).expect("the first chunk"));
        let second = address(&table.read(1, &[0]).expect("the second chunk"));
        assert_eq!(
            second - first,
            VECTOR_SIZE * size_of::<i64>(),
            "the second chunk is not the first one's neighbour, so the group did not lay"
        );
    }

    /// Rows a group has not claimed yet are still readable, which is what a reader mid insert sees.
    #[test]
    fn the_chunks_of_the_group_still_filling_read_back_before_it_seals() {
        let table = filled(3);
        assert_eq!(table.group_count(), 0, "three chunks is not a group yet");
        assert_eq!(table.chunk_count(), 3);
        let wanted: Vec<Value> = (0..table.len()).map(|row| Value::BigInt(row as i64)).collect();
        assert_eq!(every_row(&table), wanted);
        assert_eq!(table.chunk_len(1), Some(VECTOR_SIZE));
        assert_eq!(table.chunk_len(3), None, "a chunk past the end has no length");
    }

    /// The fallback. An encoded column is worth more than a laid out one, so the run is left alone.
    #[test]
    fn a_column_that_arrives_encoded_keeps_its_form_rather_than_being_flattened_into_a_page() {
        let mut table = MemoryTable::new(vec![LogicalType::BigInt]);
        let chunks = ROWS_PER_GROUP / VECTOR_SIZE;
        for _ in 0..chunks {
            let values = Vector::from_values(
                LogicalType::BigInt,
                &[Value::BigInt(7), Value::BigInt(8), Value::BigInt(9)],
            )
            .expect("three distinct values");
            let codes: Vec<u32> = (0..VECTOR_SIZE).map(|row| (row % 3) as u32).collect();
            let column = Vector::dictionary(codes, values).expect("a dictionary column");
            table.append(Chunk::new(vec![column]).expect("one column")).expect("a full chunk");
        }
        assert_eq!(table.group_count(), chunks, "the dictionaries were laid end to end after all");
        let chunk = table.read(0, &[0]).expect("the first chunk");
        assert_eq!(
            chunk.column(0).expect("one column").form(),
            rudb_vector::Form::Dictionary,
            "the fallback flattened the column it was there to protect"
        );
        let wanted: Vec<Value> =
            (0..VECTOR_SIZE).map(|row| Value::BigInt(7 + (row % 3) as i64)).collect();
        assert_eq!((0..chunk.len()).map(|row| chunk.value_at(row, 0)).collect::<Vec<_>>(), wanted);
    }

    /// The statistics are per chunk and stay per chunk, whatever the rows were laid out as.
    #[test]
    fn a_group_does_not_coarsen_the_zone_maps_of_the_chunks_in_it() {
        let table = filled(ROWS_PER_GROUP / VECTOR_SIZE + 1);
        assert_eq!(table.chunk_count(), table.zones.len(), "a chunk lost its zone map");
        let first = table.zone(0).expect("the first chunk's zone");
        let range = first.column(0).expect("its only column");
        assert_eq!(range.low, Some(Bound::Int(0)), "the first chunk's smallest value");
        assert_eq!(
            range.high,
            Some(Bound::Int(VECTOR_SIZE as i128 - 1)),
            "a zone map that speaks for the whole group rather than for the chunk"
        );
        // And the pruning that rests on them still answers, across the seal and outside it.
        let op = rudb_common::bounds::Op::Equal;
        let probes = [Probe { column: 0, op, value: Bound::Int(7) }];
        assert!(!table.skips(0, &probes), "the chunk holding the value was skipped");
        assert!(table.skips(1, &probes), "the chunk that cannot hold the value was read");
    }

    /// Nulls are the thing a laid out page can silently lose, since they live beside the values.
    #[test]
    fn the_nulls_of_a_column_survive_the_rows_being_laid_end_to_end() {
        let table = counted(ROWS_PER_GROUP + VECTOR_SIZE);
        assert!(table.group_count() > 0, "nothing sealed, so nothing was laid");
        let rows = table.len();
        let wanted = (0..rows).filter(|row| row % 5 == 0).count();
        assert_eq!(table.null_count(0).expect("the only column"), wanted);
        let read: Vec<Value> = (0..table.chunk_count())
            .flat_map(|at| {
                let chunk = table.read(at, &[0]).expect("a chunk");
                (0..chunk.len()).map(|row| chunk.value_at(row, 0)).collect::<Vec<_>>()
            })
            .collect();
        let expected: Vec<Value> = (0..rows)
            .map(|row| if row % 5 == 0 { Value::Null } else { Value::Integer(row as i32 % 7) })
            .collect();
        assert_eq!(read, expected, "the values or the nulls moved when the layout did");
    }

    /// A string column, which is the one whose page is views rather than a flat run.
    #[test]
    fn a_string_column_is_laid_into_one_arena_and_cut_back_out_of_it() {
        let mut table = MemoryTable::new(vec![LogicalType::Varchar]);
        let rows: Vec<Vec<Value>> = (0..ROWS_PER_GROUP + 5)
            .map(|row| vec![Value::Varchar(format!("a string of some length number {row}"))])
            .collect();
        table.append_rows(&rows).expect("strings");
        assert_eq!(table.group_count(), 1, "the strings were not laid end to end");
        let chunk = table.read(0, &[0]).expect("the first chunk");
        assert_eq!(
            chunk.column(0).expect("one column").form(),
            rudb_vector::Form::StringView,
            "a flat varchar page copies every byte of every long string in every cut"
        );
        assert_eq!(chunk.value_at(3, 0), rows[3][0]);
        let last = table.chunk_count() - 1;
        let tail = table.read(last, &[0]).expect("the last chunk");
        assert_eq!(tail.value_at(tail.len() - 1, 0), rows[ROWS_PER_GROUP + 4][0]);
    }

    #[test]
    fn an_empty_chunk_is_not_stored() {
        let mut table = MemoryTable::new(vec![LogicalType::Integer]);
        table.append(Chunk::empty(&[LogicalType::Integer])).expect("an empty chunk is allowed");
        assert_eq!(table.chunk_count(), 0);
        assert!(table.is_empty());
    }
}

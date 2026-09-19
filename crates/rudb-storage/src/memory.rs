//! A table that lives in memory, which is what M0 stores rows in.
//!
//! This is not the storage format. There are no blocks, no row groups, no compression and no buffer
//! manager in here, and every one of those is what the rest of this crate becomes at M2. What this
//! is, is somewhere for rows to be so that the binder and the executor can be written and tested
//! against something real, and a shape that the real thing can replace without the layers above it
//! noticing: a table is a sequence of chunks, a scan reads them in order, and a scan asks for the
//! columns it wants rather than all of them.
//!
//! The one thing it does get right on purpose is that a read is by chunk and by column, and not by
//! row. A row-at-a-time interface here would be an interface every operator above would grow
//! against, and unwinding that later is the rewrite this project exists to avoid.
//!
//! # It does have statistics
//!
//! A zone map per chunk, built on the way in, which is `zone.rs`. They were put here to skip a chunk
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

/// A table held in memory as a sequence of chunks.
#[derive(Debug, Clone)]
pub struct MemoryTable {
    types: Vec<LogicalType>,
    chunks: Vec<Chunk>,
    zones: Vec<Zone>,
    rows: usize,
    stats_ns: u64,
}

impl MemoryTable {
    /// An empty table of the given column types.
    #[must_use]
    pub fn new(types: Vec<LogicalType>) -> Self {
        Self { types, chunks: Vec::new(), zones: Vec::new(), rows: 0, stats_ns: 0 }
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
        self.chunks.len()
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
        // Stored as pages, because a stored chunk is read once per scan of the table and a page is
        // what makes that read a reference count bump rather than a copy. See `read`.
        self.chunks.push(chunk.into_pages());
        Ok(())
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
        for (zone, chunk) in self.zones.iter().zip(&self.chunks) {
            let range = zone
                .column(column)
                .ok_or_else(|| Error::internal("a chunk's zone is narrower than the table"))?;
            found.push((range, chunk.len()));
        }
        Ok(found)
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
    /// The columns are shared rather than copied. `append` stores every chunk as pages, so the
    /// vector handed back here points at the stored values and the cost of this call is one atomic
    /// increment per column. It used to copy, and `spec/perf/12-the-chunk-and-the-page.md` measured
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
        let held = self.chunks.get(chunk).ok_or_else(|| {
            Error::internal(format!(
                "chunk {chunk} of a table that has {} chunks",
                self.chunks.len()
            ))
        })?;
        let mut picked = Vec::with_capacity(columns.len());
        for &column in columns {
            picked.push(held.column(column)?.clone());
        }
        Chunk::with_rows(picked, held.len())
    }

    /// One stored chunk, whole.
    #[must_use]
    pub fn chunk(&self, index: usize) -> Option<&Chunk> {
        self.chunks.get(index)
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
        words
            .append_rows(&[vec![Value::Varchar("a".to_string())]])
            .expect("one string");
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
        let stored = address(table.chunk(0).expect("the only chunk"));
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

    #[test]
    fn an_empty_chunk_is_not_stored() {
        let mut table = MemoryTable::new(vec![LogicalType::Integer]);
        table.append(Chunk::empty(&[LogicalType::Integer])).expect("an empty chunk is allowed");
        assert_eq!(table.chunk_count(), 0);
        assert!(table.is_empty());
    }
}

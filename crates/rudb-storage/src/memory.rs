//! A table that lives in memory, which is what M0 stores rows in.
//!
//! This is not the storage format. There are no blocks, no row groups, no statistics, no
//! compression and no buffer manager in here, and every one of those is what the rest of this crate
//! becomes at M2. What this is, is somewhere for rows to be so that the binder and the executor can
//! be written and tested against something real, and a shape that the real thing can replace
//! without the layers above it noticing: a table is a sequence of chunks, a scan reads them in
//! order, and a scan asks for the columns it wants rather than all of them.
//!
//! The one thing it does get right on purpose is that a read is by chunk and by column, and not by
//! row. A row-at-a-time interface here would be an interface every operator above would grow
//! against, and unwinding that later is the rewrite this project exists to avoid.

use std::time::Instant;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::vector::VECTOR_SIZE;
use rudb_vector::{Chunk, Vector};

use crate::zone::{Probe, Zone};

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

//! A limit written as a share of the input rather than as a row count.
//!
//! `LIMIT 30 PERCENT` over ten rows is three rows. Thirty percent of how many is the whole point:
//! the share is of everything that arrives, so nothing can come out until the last row has gone in,
//! which makes this a pipeline breaker where a plain [`Limit`](crate::stream::Limit) is a stream
//! that hands each chunk on and stops the scan when it has enough.
//!
//! The count rounds down. Thirty five percent of ten rows is three and a half rows and comes out as
//! three, five percent of ten rows is nought, and a hundred percent is every row. The pinned binary
//! answers the same way, which is where the rule was read off rather than chosen.
//!
//! The offset is applied after the share has been worked out, and applied to the whole input rather
//! than to the share. `LIMIT 30 PERCENT OFFSET 2` over ten rows is three rows starting at the
//! third, so it is rows two, three and four, and not the three rows after the first three.
//!
//! # One instance
//!
//! [`Sink::parallel`] is false here for the reason it is false on the plain limit. Which rows a
//! limit with no `ORDER BY` under it returns is not settled by the query, so it has to be settled
//! by the engine, or the same query answers differently on two runs of the same build. One instance
//! takes the morsels in the order they were cut and keeps the chunks in the order they arrived,
//! which is the answer one thread would have given.

use std::sync::Mutex;

use rudb_common::{Error, Memory, Reservation, Result};
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_vector::{Chunk, Selection};

use crate::buffer::Buffered;

/// A share of the input, held until there is an input to take a share of.
#[derive(Debug)]
pub(crate) struct LimitPercent {
    percent: f64,
    offset: u64,
    memory: Memory,
    /// Every chunk that arrived, in the order it arrived.
    chunks: Mutex<Vec<Chunk>>,
    /// What those chunks are charged, given back once the kept ones are charged instead.
    charged: Mutex<Vec<Reservation>>,
    /// What the kept chunks are charged, held for as long as they are readable.
    held: Mutex<Reservation>,
    out: Buffered,
}

/// What one instance gathered before it combines.
#[derive(Debug)]
pub(crate) struct Gathered {
    chunks: Vec<Chunk>,
    charged: Reservation,
}

impl LimitPercent {
    /// The sink the input ends in, and the source the kept rows come out of.
    pub(crate) fn new(percent: f64, offset: u64, memory: &Memory) -> (Self, Buffered) {
        let out = Buffered::new();
        let limit = Self {
            percent,
            offset,
            memory: memory.clone(),
            chunks: Mutex::new(Vec::new()),
            charged: Mutex::new(Vec::new()),
            held: Mutex::new(memory.reservation()),
            out: out.clone(),
        };
        (limit, out)
    }

    /// How many rows a share of `rows` rows comes to, rounded down.
    ///
    /// The percentage was checked to be between nought and a hundred while the query was bound, so
    /// the share is never more than the input and the conversion back cannot saturate.
    fn taken(&self, rows: u64) -> u64 {
        (self.percent / 100.0 * rows as f64) as u64
    }
}

impl Sink for LimitPercent {
    type Local = Gathered;

    fn local(&self) -> Gathered {
        Gathered { chunks: Vec::new(), charged: self.memory.reservation() }
    }

    /// Never, and for the same reason the plain limit says never. See the module documentation.
    fn parallel(&self) -> bool {
        false
    }

    fn sink(&self, chunk: &Chunk, local: &mut Gathered) -> Result<Progress> {
        local.charged.grow(chunk.footprint() as u64)?;
        local.chunks.push(chunk.clone());
        Ok(Progress::More)
    }

    fn combine(&self, local: Gathered) -> Result<()> {
        self.chunks.lock().map_err(poisoned)?.extend(local.chunks);
        self.charged.lock().map_err(poisoned)?.push(local.charged);
        Ok(())
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        let gathered = std::mem::take(&mut *self.chunks.lock().map_err(poisoned)?);
        let rows: u64 = gathered.iter().map(|chunk| chunk.len() as u64).sum();
        let from = self.offset;
        let to = from.saturating_add(self.taken(rows));
        let kept = window(gathered, from, to)?;
        let mut held = self.held.lock().map_err(poisoned)?;
        held.grow(kept.iter().map(|chunk| chunk.footprint() as u64).sum())?;
        self.out.fill(kept)?;
        // The gathered chunks are gone and the kept ones are charged instead, so what the instance
        // took is given back here and not before.
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
    }
}

/// The rows from `from` up to `to`, counting across the whole list rather than within a chunk.
///
/// A chunk that falls entirely inside the window is kept as it is, which is the common case and
/// copies nothing. Only the chunk the window starts in and the one it ends in are narrowed, and a
/// chunk outside it is dropped without being looked at.
fn window(chunks: Vec<Chunk>, from: u64, to: u64) -> Result<Vec<Chunk>> {
    let mut kept = Vec::with_capacity(chunks.len());
    let mut seen = 0u64;
    for chunk in chunks {
        let rows = chunk.len() as u64;
        let start = seen;
        seen += rows;
        if seen <= from || start >= to {
            continue;
        }
        if start >= from && seen <= to {
            kept.push(chunk);
            continue;
        }
        let first = from.saturating_sub(start);
        let last = to.min(seen) - start;
        let mut selection = Selection::with_capacity((last - first) as usize);
        for row in first..last {
            selection.push(row as usize);
        }
        kept.push(chunk.select(&selection)?);
    }
    Ok(kept)
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the rows a percentage limit is gathering")
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::{Chunk, Data, Vector};

    use super::window;

    fn chunk(values: &[i32]) -> Chunk {
        let column = Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into()))
            .expect("integers are an i32 layout");
        Chunk::new(vec![column]).expect("one column is one length")
    }

    fn numbers(chunks: &[Chunk]) -> Vec<i32> {
        let mut found = Vec::new();
        for chunk in chunks {
            for row in 0..chunk.len() {
                match chunk.value_at(row, 0) {
                    Value::Integer(number) => found.push(number),
                    other => panic!("{other:?}"),
                }
            }
        }
        found
    }

    /// The window is counted across the list, so it can start in one chunk and end in another.
    #[test]
    fn a_window_that_crosses_chunks_takes_the_rows_between_its_ends() {
        let chunks = vec![chunk(&[0, 1, 2]), chunk(&[3, 4, 5]), chunk(&[6, 7, 8])];
        let kept = window(chunks, 2, 7).expect("a window");
        assert_eq!(numbers(&kept), vec![2, 3, 4, 5, 6]);
    }

    /// A chunk the window covers entirely is kept whole, which is what makes the common case free.
    #[test]
    fn a_window_over_everything_keeps_every_chunk_as_it_is() {
        let chunks = vec![chunk(&[0, 1]), chunk(&[2, 3])];
        let kept = window(chunks, 0, 4).expect("a window");
        assert_eq!(kept.len(), 2);
        assert_eq!(numbers(&kept), vec![0, 1, 2, 3]);
    }

    /// An offset past the end and a share of nothing both answer with no rows rather than an error.
    #[test]
    fn a_window_that_is_empty_keeps_nothing() {
        let chunks = vec![chunk(&[0, 1]), chunk(&[2, 3])];
        assert!(window(chunks.clone(), 9, 9).expect("a window").is_empty());
        assert!(window(chunks, 0, 0).expect("a window").is_empty());
    }
}

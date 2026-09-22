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
//! Neither end has to be a number the query wrote. `LIMIT (SELECT 30)% OFFSET (SELECT 2)` holds two
//! values that are settled while the query runs, so each arrives as a column of the input and is
//! read off the first row that got here. That is the same rule and the same [`Edge`] a plain limit
//! uses for its offset, and a [`Portion`] for the share, which differs only in being a fraction
//! rather than a row count. Read once rather than per chunk, because a volatile call would otherwise
//! answer a different share every chunk.
//!
//! A share read that way is checked here rather than in the binder, because here is where the value
//! turns up, and the two ways of being outside the range get two different sentences. A share above
//! a hundred is out of range, which is the same sentence a share written out gets in the binder. A
//! negative one says that a percentage cannot be negative and names the value, which is a sentence
//! nothing else says. Both of those are read off the pin, which splits them the same way and at the
//! same point in the query.
//!
//! # One instance
//!
//! [`Sink::parallel`] is false here for the reason it is false on the plain limit. Which rows a
//! limit with no `ORDER BY` under it returns is not settled by the query, so it has to be settled
//! by the engine, or the same query answers differently on two runs of the same build. One instance
//! takes the morsels in the order they were cut and keeps the chunks in the order they arrived,
//! which is the answer one thread would have given.

use std::sync::Mutex;

use rudb_common::{Error, Memory, Reservation, Result, Session};
use rudb_kernels::percentage;
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_vector::{Chunk, Selection};

use crate::buffer::Buffered;
use crate::prepared::{Prepared, Scratch};
use crate::stream::Edge;

/// The share while the query runs, which is the [`Edge`] of a plain limit with a fraction in it.
#[derive(Debug)]
pub(crate) enum Portion {
    /// A share the binder worked out, already checked to be between nought and a hundred.
    Percent(f64),
    /// An expression over the input, holding the share in every row.
    Read(Prepared),
}

/// A share of the input, held until there is an input to take a share of.
#[derive(Debug)]
pub(crate) struct LimitPercent {
    percent: Portion,
    offset: Edge,
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

impl Portion {
    /// Working space sized for this share, which is nothing at all unless it is read off the rows.
    fn scratch(&self) -> Scratch {
        match self {
            Self::Read(prepared) => prepared.scratch(),
            Self::Percent(_) => Scratch::default(),
        }
    }

    /// The share this asks for, reading the chunk when that is where the value is.
    ///
    /// `None` is every row, which is what a null share answers on the pin, the same way a null row
    /// count is every row. The range is checked here for a share that had to be read, because here
    /// is the first point anybody has the value. See the module documentation for the two
    /// sentences.
    fn share(&self, chunk: &Chunk, scratch: &mut Scratch) -> Result<Option<f64>> {
        let prepared = match self {
            Self::Percent(percent) => return Ok(Some(*percent)),
            Self::Read(prepared) => prepared,
        };
        let value = prepared.evaluate_one(chunk, scratch)?.value_at(0);
        if value.is_null() {
            return Ok(None);
        }
        let percent = percentage(&value)?;
        if percent < 0.0 {
            return Err(Error::binder(format!("Percentage value({percent:.6}) can't be negative")));
        }
        if percent > 100.0 || percent.is_nan() {
            return Err(Error::out_of_range(
                "Limit percent out of range, should be between 0% and 100%",
            ));
        }
        Ok(Some(percent))
    }
}

impl LimitPercent {
    /// The sink the input ends in, and the source the kept rows come out of.
    pub(crate) fn new(
        percent: Portion,
        offset: Edge,
        memory: &Memory,
        session: &Session,
    ) -> (Self, Buffered) {
        let out = Buffered::new();
        let limit = Self {
            percent: match percent {
                Portion::Read(prepared) => Portion::Read(prepared.in_session(session)),
                settled => settled,
            },
            offset: offset.in_session(session),
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
    /// The percentage is between nought and a hundred by the time it gets here, checked by the
    /// binder or by [`Portion::share`], so the share is never more than the input and the
    /// conversion back cannot saturate. No share at all is every row, which is what a null one is.
    fn taken(&self, share: Option<f64>, rows: u64) -> u64 {
        match share {
            Some(percent) => (percent / 100.0 * rows as f64) as u64,
            None => rows,
        }
    }

    /// How many rows the offset asks to skip, reading `first` when that is where the number is.
    ///
    /// The first chunk that arrived carries it, the same as it does for a plain limit, and every
    /// row of it carries the same value because the query that produced it was joined in as a
    /// single row. A null offset is nought, the way a null offset on a plain limit is.
    fn skipped(&self, first: Option<&Chunk>) -> Result<u64> {
        if let Edge::Rows(rows) = self.offset {
            return Ok(rows);
        }
        let Some(first) = first else {
            return Ok(0);
        };
        let mut scratch = self.offset.scratch();
        Ok(self.offset.rows(first, &mut scratch, "OFFSET")?.unwrap_or(0))
    }

    /// The share to take, reading `first` when that is where the value is.
    ///
    /// The same rule as the offset one line up and the same chunk. No rows at all is nothing to
    /// read it off and no rows to take a share of either, so the answer does not depend on the
    /// value and the pin does not read it there: an empty input with a share of minus one answers
    /// nothing rather than raising the error a row would have raised.
    fn share(&self, first: Option<&Chunk>) -> Result<Option<f64>> {
        let Some(first) = first else {
            return Ok(Some(0.0));
        };
        let mut scratch = self.percent.scratch();
        self.percent.share(first, &mut scratch)
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
        let first = gathered.iter().find(|chunk| !chunk.is_empty());
        let from = self.skipped(first)?;
        let share = self.share(first)?;
        let to = from.saturating_add(self.taken(share, rows));
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

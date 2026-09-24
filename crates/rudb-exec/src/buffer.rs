//! The source every pipeline breaker finalises into.
//!
//! An operator that cannot answer until it has seen all of its input finishes by building a list of
//! chunks, and something has to read that list back out. [`Sink::finalize`] is deliberately not the
//! thing that does it, for the reason its documentation gives: a hash aggregate finalises into a
//! structure and a separate source reads that structure out in parallel, and fusing the two would
//! make the parallel read impossible.
//!
//! So this is that separate source, and there is one of it rather than one per operator. A morsel
//! here is one chunk, which is the right granule when the chunks were built by the operator that
//! filled it rather than read off a disk, because they are already the size everything downstream
//! wants.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rudb_common::{Error, Reservation, Result};
use rudb_pipeline::{Morsel, Progress, Source};
use rudb_vector::Chunk;

/// A list of finished chunks.
#[derive(Debug, Default)]
struct Shared {
    chunks: Mutex<Vec<Chunk>>,
    /// What the chunks are charged, where the operator that kept them handed the charge on with
    /// them. It leaves with the chunks when they are taken, so the limit sees them go when they do.
    charged: Mutex<Vec<Reservation>>,
}

/// Chunks somebody else built, read back out one at a time.
///
/// Cloning one gives another handle on the same list and the same cursor, which is how the sink
/// half and the source half of a pipeline breaker end up looking at the same thing. Reading the
/// list a second time from the start is [`Buffered::reader`] instead.
#[derive(Debug, Default, Clone)]
pub(crate) struct Buffered {
    shared: Arc<Shared>,
    handed: Arc<AtomicU64>,
}

impl Buffered {
    /// Nothing yet.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The same chunks with a cursor of its own, so this handle sees all of them.
    ///
    /// A pipeline breaker wants the shared cursor, because the rows go past once and the threads
    /// reading them are splitting one pass between them. A materialised `WITH` is the other case:
    /// the rows are held so that every read of the name gets the whole list, and a read that only
    /// saw the chunks another read had not taken yet would be a wrong answer rather than a slower
    /// one.
    pub(crate) fn reader(&self) -> Self {
        Self { shared: Arc::clone(&self.shared), handed: Arc::new(AtomicU64::new(0)) }
    }

    /// Hand the finished chunks over, which is what a [`Sink::finalize`] does with its result.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while holding
    /// the list.
    pub(crate) fn fill(&self, chunks: Vec<Chunk>) -> Result<()> {
        *self.shared.chunks.lock().map_err(poisoned)? = chunks;
        Ok(())
    }

    /// Hold what the chunks are charged for as long as the chunks are here.
    ///
    /// # Errors
    ///
    /// The same as [`Buffered::fill`].
    pub(crate) fn charge(&self, charged: Vec<Reservation>) -> Result<()> {
        self.shared.charged.lock().map_err(poisoned)?.extend(charged);
        Ok(())
    }

    /// How many chunks are in there.
    ///
    /// # Errors
    ///
    /// The same as [`Buffered::fill`].
    pub(crate) fn len(&self) -> Result<usize> {
        Ok(self.shared.chunks.lock().map_err(poisoned)?.len())
    }

    /// Every chunk, leaving nothing behind for any handle on the same list.
    ///
    /// For the one reader that turns the whole list into something else and never reads it again,
    /// so that the list and what it was turned into are not both held. What the chunks are charged
    /// comes with them, for the caller to drop when it drops them.
    ///
    /// # Errors
    ///
    /// The same as [`Buffered::fill`].
    pub(crate) fn take(&self) -> Result<(Vec<Chunk>, Vec<Reservation>)> {
        let chunks = std::mem::take(&mut *self.shared.chunks.lock().map_err(poisoned)?);
        let charged = std::mem::take(&mut *self.shared.charged.lock().map_err(poisoned)?);
        Ok((chunks, charged))
    }

    /// One chunk by position, or `None` past the end.
    ///
    /// # Errors
    ///
    /// The same as [`Buffered::fill`].
    pub(crate) fn at(&self, index: usize) -> Result<Option<Chunk>> {
        Ok(self.shared.chunks.lock().map_err(poisoned)?.get(index).cloned())
    }
}

impl Source for Buffered {
    fn morsel(&self) -> Option<Morsel> {
        let chunks = self.shared.chunks.lock().ok()?.len() as u64;
        let index = self.handed.fetch_add(1, Ordering::Relaxed);
        if index >= chunks {
            return None;
        }
        Some(Morsel::new(index, index, index + 1))
    }

    fn morsels(&self, threads: usize, _weight: usize) -> Option<usize> {
        let chunks = self.shared.chunks.lock().ok()?;
        let rows = chunks.iter().map(Chunk::len).sum::<usize>();
        let useful = rows.div_ceil(50_000).clamp(1, 4);
        Some(chunks.len().min(threads).min(useful))
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let index = morsel.cursor() as usize;
        match self.at(index)? {
            Some(chunk) => {
                *out = chunk;
                morsel.advance(1);
                Ok(Progress::Done)
            }
            None => Err(Error::internal(format!("{morsel} asks for a chunk nobody built"))),
        }
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding an operator's finished chunks")
}

impl fmt::Display for Buffered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.len() {
            Ok(chunks) => write!(f, "{chunks} finished chunks"),
            Err(_) => write!(f, "finished chunks nobody can read"),
        }
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::{Data, Vector};

    use super::{Buffered, Chunk, Morsel, Progress, Source};

    fn chunk(values: &[i32]) -> Chunk {
        let column = Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into()))
            .expect("integers are an i32 layout");
        Chunk::new(vec![column]).expect("one column is one length")
    }

    #[test]
    fn a_morsel_is_one_chunk_and_they_run_out() {
        let buffered = Buffered::new();
        buffered.fill(vec![chunk(&[1]), chunk(&[2])]).expect("two chunks");

        let first = buffered.morsel().expect("a first chunk");
        assert_eq!(first.len(), 1);
        assert!(buffered.morsel().is_some(), "a second chunk");
        assert!(buffered.morsel().is_none(), "and no third");
    }

    #[test]
    fn reading_a_morsel_drains_it_in_one_call() {
        let buffered = Buffered::new();
        buffered.fill(vec![chunk(&[7, 8])]).expect("one chunk");

        let mut morsel = buffered.morsel().expect("the one chunk");
        let mut out = Chunk::empty(&[]);
        assert_eq!(buffered.read(&mut morsel, &mut out).expect("it is there"), Progress::Done);
        assert!(morsel.is_drained());
        assert_eq!(out.value_at(0, 0), Value::Integer(7));
        assert_eq!(out.value_at(1, 0), Value::Integer(8));
    }

    /// Nothing hands out a morsel for a chunk that does not exist, so this is a bug in a source
    /// rather than anything a query can cause, and it says so rather than answering with no rows.
    #[test]
    fn a_morsel_for_a_chunk_nobody_built_is_an_error() {
        let buffered = Buffered::new();
        buffered.fill(vec![chunk(&[1])]).expect("one chunk");

        let mut invented = Morsel::new(4, 4, 5);
        let mut out = Chunk::empty(&[]);
        let why = buffered.read(&mut invented, &mut out).expect_err("there is no chunk four");
        assert!(why.to_string().contains("a chunk nobody built"), "{why}");
    }

    /// Two reads of a materialised `WITH` both get the whole list, and a clone is still the same
    /// reader as the one it was cloned from.
    #[test]
    fn a_reader_starts_again_and_a_clone_carries_on() {
        let buffered = Buffered::new();
        buffered.fill(vec![chunk(&[1]), chunk(&[2])]).expect("two chunks");

        let one = buffered.reader();
        let two = buffered.reader();
        assert!(one.morsel().is_some(), "the first reader takes the first chunk");
        assert_eq!(two.morsel().expect("the second reader starts again").cursor(), 0);

        let along = one.clone();
        assert_eq!(along.morsel().expect("the clone carries on").cursor(), 1);
        assert!(one.morsel().is_none(), "and the pair of them have taken both");
    }

    #[test]
    fn an_empty_buffer_hands_out_nothing() {
        let buffered = Buffered::new();
        assert_eq!(buffered.len().expect("readable"), 0);
        assert!(buffered.morsel().is_none());
    }
}

//! The one pull left in the engine.
//!
//! Everything inside the engine pushes. The C API, the shell and the Arrow interface all pull,
//! because a caller that owns the loop is what an embedded library is. So there is exactly one
//! adapter, it sits at the root of the query, and this is it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rudb_common::{Error, Result};
use rudb_vector::Chunk;

use crate::progress::{Blocked, BufferId, Progress};
use crate::traits::Sink;

/// The queue between the last operator and whoever is pulling.
#[derive(Debug)]
struct Shared {
    chunks: Mutex<VecDeque<Chunk>>,
    finished: AtomicBool,
    capacity: Option<usize>,
    buffer: BufferId,
}

/// A sink that hands finished chunks to a caller outside the engine.
///
/// Taking the lock once per chunk at the root of the query is not a cost worth avoiding. The root
/// is the last operator, a chunk is a hundred and twenty thousand rows, and the alternative is a
/// per instance buffer that holds the entire result until the query ends, which is the thing an
/// adapter like this exists to avoid.
#[derive(Debug, Clone)]
pub struct RootSink {
    shared: Arc<Shared>,
}

/// The pulling half.
#[derive(Debug, Clone)]
pub struct RootReader {
    shared: Arc<Shared>,
}

/// A sink and the reader that drains it.
///
/// `capacity` is how many chunks may sit in the queue before the sink reports
/// [`Blocked::Downstream`], which is how backpressure from a slow consumer reaches the scheduler
/// through the same four reasons everything else uses. `None` is unbounded, and `None` is what the
/// serial driver needs, because the serial driver has nothing to run while a task is parked and
/// the caller does not start pulling until it returns.
#[must_use]
pub fn root(buffer: BufferId, capacity: Option<usize>) -> (RootSink, RootReader) {
    let shared = Arc::new(Shared {
        chunks: Mutex::new(VecDeque::new()),
        finished: AtomicBool::new(false),
        capacity,
        buffer,
    });
    (RootSink { shared: Arc::clone(&shared) }, RootReader { shared })
}

impl Sink for RootSink {
    type Local = ();

    fn local(&self) {}

    fn sink(&self, chunk: &Chunk, (): &mut ()) -> Result<Progress> {
        if chunk.is_empty() {
            return Ok(Progress::More);
        }
        let mut queue = self.shared.chunks.lock().map_err(poisoned)?;
        if self.shared.capacity.is_some_and(|capacity| queue.len() >= capacity) {
            return Ok(Progress::Blocked(Blocked::Downstream(self.shared.buffer)));
        }
        queue.push_back(chunk.clone());
        Ok(Progress::More)
    }

    fn combine(&self, (): ()) -> Result<()> {
        Ok(())
    }

    fn finalize(&self) -> Result<()> {
        self.shared.finished.store(true, Ordering::Release);
        Ok(())
    }
}

impl RootReader {
    /// The next chunk, or `None` when there is nothing queued right now.
    ///
    /// `None` does not mean the query is over. Ask [`RootReader::is_finished`] for that. The two
    /// questions are separate because with the parallel driver they have different answers, and a
    /// caller written against the serial driver that treats them as one would start silently
    /// truncating results at F4.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while
    /// holding the queue.
    pub fn next_chunk(&self) -> Result<Option<Chunk>> {
        let mut queue = self.shared.chunks.lock().map_err(poisoned)?;
        Ok(queue.pop_front())
    }

    /// Whether the pipeline that feeds this has finalised.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire)
    }

    /// How many chunks are waiting.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while
    /// holding the queue.
    pub fn queued(&self) -> Result<usize> {
        let queue = self.shared.chunks.lock().map_err(poisoned)?;
        Ok(queue.len())
    }

    /// Everything queued, which is what a caller that does not want to write a loop uses.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while
    /// holding the queue.
    pub fn drain(&self) -> Result<Vec<Chunk>> {
        let mut queue = self.shared.chunks.lock().map_err(poisoned)?;
        Ok(queue.drain(..).collect())
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the query result queue")
}

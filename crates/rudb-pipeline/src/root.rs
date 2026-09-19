//! The one pull left in the engine.
//!
//! Everything inside the engine pushes. The C API, the shell and the Arrow interface all pull,
//! because a caller that owns the loop is what an embedded library is. So there is exactly one
//! adapter, it sits at the root of the query, and this is it.
//!
//! # Order
//!
//! The root comes in two forms. [`root`] hands chunks out in whatever order they arrive, which is
//! what a query with an `ORDER BY` in it wants, because the operator below has already decided the
//! order and the root would only be getting in its way. [`root_in_order`] puts them back in the
//! order the source cut its morsels, which is what a query with no `ORDER BY` needs the moment more
//! than one thread reads the same file.
//!
//! Without that second form, running a scan on sixteen threads changes the answer to a query that
//! did not ask for an order. SQL does not promise one, so it is not wrong, but every engine people
//! actually use returns the file order for a plain `SELECT` and a user who gets a different order
//! on every run reports it as a bug. The point of having both is that the order becomes something
//! the planner picks, so the scheduler is free to hand morsels out however it likes and the choice
//! never turns into an answer change nobody meant to make.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rudb_common::{Error, Result};
use rudb_vector::Chunk;

use crate::morsel::Morsel;
use crate::pool::Lease;
use crate::progress::{Blocked, BufferId, Progress};
use crate::traits::Sink;

/// The queue between the last operator and whoever is pulling.
#[derive(Debug)]
struct Shared {
    queue: Mutex<Queue>,
    finished: AtomicBool,
    capacity: Option<usize>,
    buffer: BufferId,
    /// Whether this root restores the source order, kept out here so that asking costs no lock.
    ordered: bool,
}

/// Everything the root holds under one lock.
#[derive(Debug)]
struct Queue {
    /// Chunks the reader may take, in the order it will get them.
    ready: VecDeque<Chunk>,
    /// Set when this root restores the source order, `None` when it hands chunks on as they come.
    order: Option<Order>,
}

/// What an order restoring root holds while it waits for the morsels in front of a chunk.
///
/// This is DuckDB's minimum batch index scheme under another name. A chunk is keyed by the morsel it
/// came from and its place within that morsel, which totally orders the output in source order,
/// because every streaming operator transforms a chunk in place and none of them merges chunks
/// across morsels, so a chunk arriving at the sink came from exactly one morsel.
///
/// A chunk can be handed on once no instance is still reading a morsel cut before its own. While an
/// instance holds morsel `m`, more chunks of `m` can still turn up, so neither `m` nor anything
/// after it is settled.
#[derive(Debug, Default)]
struct Order {
    /// Chunks that have arrived and cannot go yet, by morsel and place within it.
    waiting: BTreeMap<(u64, u64), Chunk>,
    /// Morsels that have finished but have an earlier morsel still in flight.
    finished: BTreeSet<u64>,
    /// The first morsel that has not finished yet.
    next: u64,
}

impl Order {
    /// Records one completed morsel and advances across every contiguous completion.
    fn finish(&mut self, morsel: u64) {
        self.finished.insert(morsel);
        while self.finished.remove(&self.next) {
            self.next += 1;
        }
    }

    /// Move every chunk whose turn has come onto the ready queue.
    ///
    /// With nothing being read there is nothing left to arrive out of order, so everything goes. A
    /// morsel handed out later has a higher number than everything already waiting, which is the
    /// numbering [`Sink::at`] asks a source for and the reason this is safe rather than optimistic.
    fn release(&mut self, ready: &mut VecDeque<Chunk>) {
        let limit = self.next;
        while let Some((&key, _)) = self.waiting.iter().next() {
            if key.0 >= limit {
                break;
            }
            if let Some(chunk) = self.waiting.remove(&key) {
                ready.push_back(chunk);
            }
        }
    }
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

/// Where one instance of the root has got to.
///
/// An order restoring root needs this and a plain one ignores it. It is the same type either way
/// because a sink has one local state type and two roots that differ in a `bool` are not worth two.
#[derive(Debug, Default)]
pub struct RootPlace {
    /// The morsel this instance is reading, `None` before it has been given one.
    morsel: Option<u64>,
    /// How many chunks of that morsel have gone by, which is the second half of the key.
    at: u64,
}

/// The pulling half.
#[derive(Debug, Clone)]
pub struct RootReader {
    shared: Arc<Shared>,
}

/// A sink and the reader that drains it, handing chunks on as they arrive.
///
/// `capacity` is how many chunks may sit in the queue before the sink reports
/// [`Blocked::Downstream`], which is how backpressure from a slow consumer reaches the scheduler
/// through the same four reasons everything else uses. `None` is unbounded, and `None` is what the
/// serial driver needs, because the serial driver has nothing to run while a task is parked and
/// the caller does not start pulling until it returns.
#[must_use]
pub fn root(buffer: BufferId, capacity: Option<usize>) -> (RootSink, RootReader) {
    build(buffer, capacity, None)
}

/// A sink and the reader that drains it, putting chunks back in the order the morsels were cut.
///
/// For a query that did not ask for an order of its own and would notice losing the one the file
/// has. `capacity` means the same thing it does on [`root`], with one addition: an instance is told
/// to wait on chunks being held for ordering only when a morsel cut before its own is still being
/// read. The instance holding the earliest morsel is never told to wait, so there is always
/// somebody who can make progress and the bound cannot turn into a deadlock.
#[must_use]
pub fn root_in_order(buffer: BufferId, capacity: Option<usize>) -> (RootSink, RootReader) {
    build(buffer, capacity, Some(Order::default()))
}

fn build(
    buffer: BufferId,
    capacity: Option<usize>,
    order: Option<Order>,
) -> (RootSink, RootReader) {
    let shared = Arc::new(Shared {
        finished: AtomicBool::new(false),
        capacity,
        buffer,
        ordered: order.is_some(),
        queue: Mutex::new(Queue { ready: VecDeque::new(), order }),
    });
    (RootSink { shared: Arc::clone(&shared) }, RootReader { shared })
}

impl Sink for RootSink {
    type Local = RootPlace;

    fn local(&self) -> RootPlace {
        RootPlace::default()
    }

    /// Only the form that restores the source order.
    ///
    /// A plain root hands chunks on as they arrive, which on one thread is the order the morsels
    /// were cut and on several is the order the threads happened to finish. The engine reaches for
    /// the plain form when the operator below has already decided the order, and that operator is a
    /// sort or a top n whose finished rows are read back out of a buffer. Reading that buffer on
    /// four threads and handing the chunks on as they land would take the order the sort just
    /// produced and shuffle it, which is a wrong answer arrived at by going faster.
    fn parallel(&self) -> bool {
        self.shared.ordered
    }

    fn at(&self, morsel: &Morsel, place: &mut RootPlace) -> Result<()> {
        let mut queue = self.shared.queue.lock().map_err(poisoned)?;
        let Queue { ready, order } = &mut *queue;
        if let Some(order) = order.as_mut() {
            // Taking a morsel is also finishing the one before it, because an instance reads one at
            // a time, and finishing one is what lets the chunks behind it go.
            if let Some(done) = place.morsel {
                order.finish(done);
            }
            order.release(ready);
        }
        place.morsel = Some(morsel.index());
        place.at = 0;
        Ok(())
    }

    fn sink(&self, chunk: &Chunk, place: &mut RootPlace) -> Result<Progress> {
        if chunk.is_empty() {
            return Ok(Progress::More);
        }
        let mut queue = self.shared.queue.lock().map_err(poisoned)?;
        let full = |held: usize| self.shared.capacity.is_some_and(|capacity| held >= capacity);
        if full(queue.ready.len()) {
            return Ok(Progress::Blocked(Blocked::Downstream(self.shared.buffer)));
        }
        let Queue { ready, order } = &mut *queue;
        let Some(order) = order.as_mut() else {
            ready.push_back(chunk.clone());
            return Ok(Progress::More);
        };
        // An order restoring root whose driver never said which morsel this came from has nowhere to
        // put it, and picking a place would be a silently reordered answer rather than a slow one.
        // The driver calls `at` before it reads, so this is a driver that does not.
        let Some(morsel) = place.morsel else {
            return Err(Error::internal(
                "the root was told to keep the source order by a driver that does not say which morsel a chunk came from",
            ));
        };
        if full(order.waiting.len()) && order.next != morsel {
            return Ok(Progress::Blocked(Blocked::Downstream(self.shared.buffer)));
        }
        order.waiting.insert((morsel, place.at), chunk.clone());
        place.at += 1;
        Ok(Progress::More)
    }

    fn combine(&self, place: RootPlace) -> Result<()> {
        let mut queue = self.shared.queue.lock().map_err(poisoned)?;
        let Queue { ready, order } = &mut *queue;
        if let Some(order) = order.as_mut() {
            if let Some(done) = place.morsel {
                order.finish(done);
            }
            order.release(ready);
        }
        Ok(())
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        {
            // Every instance has combined by now, so nothing is being read and everything still
            // held is in order. Doing it here as well as in `combine` is what makes a root with no
            // instances at all, which is a pipeline over an empty file, come out empty rather than
            // holding something forever.
            let mut queue = self.shared.queue.lock().map_err(poisoned)?;
            let Queue { ready, order } = &mut *queue;
            if let Some(order) = order.as_mut() {
                order.release(ready);
            }
        }
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
    /// truncating results at F4. An order restoring root widens that gap, because it answers `None`
    /// while it holds chunks whose turn has not come.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while
    /// holding the queue.
    pub fn next_chunk(&self) -> Result<Option<Chunk>> {
        let mut queue = self.shared.queue.lock().map_err(poisoned)?;
        Ok(queue.ready.pop_front())
    }

    /// Whether the pipeline that feeds this has finalised.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire)
    }

    /// How many chunks the reader may take right now.
    ///
    /// Not how many the root is holding. An order restoring root that is waiting on an earlier
    /// morsel has chunks it cannot hand over, and they are not counted here because a caller asks
    /// this to size its next pull.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while
    /// holding the queue.
    pub fn queued(&self) -> Result<usize> {
        let queue = self.shared.queue.lock().map_err(poisoned)?;
        Ok(queue.ready.len())
    }

    /// Everything queued, which is what a caller that does not want to write a loop uses.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while
    /// holding the queue.
    pub fn drain(&self) -> Result<Vec<Chunk>> {
        let mut queue = self.shared.queue.lock().map_err(poisoned)?;
        Ok(queue.ready.drain(..).collect())
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the query result queue")
}

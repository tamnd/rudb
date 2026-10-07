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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

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
    /// Whether a chunk is flattened on its way into the queue, which is [`RootReader::flattening`].
    ///
    /// An atomic because the reader is the one that knows the answer and it is handed back after
    /// the sink is built. It is written once, before the query runs, and read once per chunk, so
    /// relaxed ordering is all it needs: there is no second thing whose visibility depends on it.
    flatten: AtomicBool,
    /// Nanoseconds spent flattening, summed over every thread that queued a chunk.
    ///
    /// Here rather than in a stage, because the root is not an operator and no operator's reading
    /// would see it. It is the last copy a query makes, and on a query with a large answer it is a
    /// real part of the time.
    flattened: AtomicU64,
    /// How many ready rows make a worker wait in the sink, or zero when no worker waits. See
    /// [`RootReader::streaming`].
    bound: AtomicUsize,
    /// Wakes the reader when half the bound is ready or the run ends.
    filled: Condvar,
    /// Wakes the workers that wait for room when the reader takes the ready rows or goes away.
    room: Condvar,
    /// The run of the query returned, with or without an error. See [`RootReader::end`].
    ended: AtomicBool,
    /// The reader will take no more chunks. See [`RootReader::close`].
    closed: AtomicBool,
}

/// Everything the root holds under one lock.
#[derive(Debug)]
struct Queue {
    /// Chunks the reader may take, in the order it will get them.
    ready: VecDeque<Chunk>,
    /// The rows in `ready`, counted only on a streaming root.
    rows: usize,
    /// The rows that wait in the order for the morsels in front of them, counted only on a
    /// streaming root.
    held: usize,
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

    /// Move the chunks of the first morsel that did not finish onto the ready queue, after the
    /// query failed while it read that morsel.
    ///
    /// The chunks of a later morsel stay, because the rows of the morsel in front of them did not
    /// all arrive. On one thread the morsel that failed is that first morsel, so the rows before
    /// the error all go.
    fn failed(&mut self, ready: &mut VecDeque<Chunk>) {
        self.release(ready);
        let first = self.next;
        while let Some((&key, _)) = self.waiting.iter().next() {
            if key.0 != first {
                break;
            }
            if let Some(chunk) = self.waiting.remove(&key) {
                ready.push_back(chunk);
            }
        }
    }

    /// Move everything still waiting onto the ready queue, in key order.
    ///
    /// For the end of a run, once every instance has combined and nothing is being read, so there
    /// is nothing left that could still arrive before any of it. [`Order::release`] cannot do this
    /// on its own because it stops at the first morsel that has not finished, and a chunk that came
    /// out of a drain rather than a morsel is keyed after every morsel a source will ever hand out.
    /// See `crate::serial::drain`.
    fn rest(&mut self, ready: &mut VecDeque<Chunk>) {
        ready.extend(std::mem::take(&mut self.waiting).into_values());
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
        flatten: AtomicBool::new(false),
        flattened: AtomicU64::new(0),
        bound: AtomicUsize::new(0),
        filled: Condvar::new(),
        room: Condvar::new(),
        ended: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        queue: Mutex::new(Queue { ready: VecDeque::new(), rows: 0, held: 0, order }),
    });
    (RootSink { shared: Arc::clone(&shared) }, RootReader { shared })
}

impl RootSink {
    /// The chunk as it goes into the queue.
    ///
    /// A copy of the handle either way, because the sink is handed a borrow and the queue holds a
    /// chunk of its own. A flattening root spends that copy on the flatten rather than on a clone
    /// of whatever form the chunk arrived in, so the caller outside the engine gets flat columns
    /// and the decode that produces them happens on the worker that produced the chunk instead of
    /// on the single thread that drains the queue afterwards.
    fn taken(&self, chunk: &Chunk) -> Result<Chunk> {
        if self.shared.flatten.load(Ordering::Relaxed) {
            let started = Instant::now();
            let flat = chunk.clone().into_flat();
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.shared.flattened.fetch_add(nanos, Ordering::Relaxed);
            return flat;
        }
        Ok(chunk.clone())
    }

    /// On a streaming root, counts the chunks that became ready after the first `before`, wakes
    /// the reader when half the bound is ready, and waits while the ready rows are at the bound.
    ///
    /// The wait is the backpressure of a caller that sends the rows on while the query runs: a
    /// worker does not make more rows until the reader took the ones that are ready. The reader is
    /// never a worker, so it can always take them and the wait cannot become a deadlock. The reader
    /// takes all the ready rows at once and wakes at half the bound, not at each chunk, because a
    /// query can give chunks of a few rows and a wake for each of them costs more than the rows.
    /// On a streaming root that restores the order, waits while the rows that are ready and the
    /// rows that wait in the order are at the bound. Without it the workers on the morsels behind
    /// a slow one fill the order with the whole result. The worker on the morsel in front never
    /// waits here, so the order always moves.
    fn held_back<'q>(
        &self,
        mut queue: MutexGuard<'q, Queue>,
        morsel: u64,
        bound: usize,
    ) -> Result<MutexGuard<'q, Queue>> {
        while queue.rows + queue.held >= bound
            && queue.order.as_ref().is_some_and(|order| order.next != morsel)
        {
            if self.shared.closed.load(Ordering::Acquire) {
                return Err(Error::interrupt("the reader of the result went away"));
            }
            queue = self.shared.room.wait(queue).map_err(poisoned)?;
        }
        Ok(queue)
    }

    fn settle(&self, mut queue: MutexGuard<'_, Queue>, before: usize) -> Result<()> {
        let bound = self.shared.bound.load(Ordering::Relaxed);
        if bound == 0 {
            return Ok(());
        }
        let added: usize = queue.ready.range(before..).map(Chunk::len).sum();
        queue.rows += added;
        if added > 0 && queue.rows >= bound / 2 {
            self.shared.filled.notify_one();
        }
        if queue.order.is_some() {
            // The rows came out of the order, and the morsel in front can be another one now, so
            // a worker that waits to put its rows in the order looks again.
            queue.held = queue.held.saturating_sub(added);
            self.shared.room.notify_all();
        }
        while queue.rows >= bound {
            if self.shared.closed.load(Ordering::Acquire) {
                return Err(Error::interrupt("the reader of the result went away"));
            }
            queue = self.shared.room.wait(queue).map_err(poisoned)?;
        }
        Ok(())
    }
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
        let before = queue.ready.len();
        let Queue { ready, order, .. } = &mut *queue;
        if let Some(order) = order.as_mut() {
            // Taking a morsel is also finishing the one before it, because an instance reads one at
            // a time, and finishing one is what lets the chunks behind it go.
            if let Some(done) = place.morsel {
                order.finish(done);
            }
            order.release(ready);
        }
        self.settle(queue, before)?;
        place.morsel = Some(morsel.index());
        place.at = 0;
        Ok(())
    }

    fn keeps_morsels(&self) -> bool {
        self.shared.ordered
    }

    fn sink(&self, chunk: &Chunk, place: &mut RootPlace) -> Result<Progress> {
        if chunk.is_empty() {
            return Ok(Progress::More);
        }
        // Before the lock and not after it. This is the one point every worker in the query queues
        // through, so work done holding it is work the rest of the pool waits out, and the flatten
        // is the most expensive thing that happens to a chunk on its way out of the engine. The
        // cost of doing it here is that a root with a capacity on it can decide below that the
        // queue is full and do it again on the retry. No root the engine builds has one.
        let taken = self.taken(chunk)?;
        let mut queue = self.shared.queue.lock().map_err(poisoned)?;
        let full = |held: usize| self.shared.capacity.is_some_and(|capacity| held >= capacity);
        if full(queue.ready.len()) {
            return Ok(Progress::Blocked(Blocked::Downstream(self.shared.buffer)));
        }
        let before = queue.ready.len();
        if queue.order.is_none() {
            queue.ready.push_back(taken);
            self.settle(queue, before)?;
            return Ok(Progress::More);
        }
        // An order restoring root whose driver never said which morsel this came from has nowhere to
        // put it, and picking a place would be a silently reordered answer rather than a slow one.
        // The driver calls `at` before it reads, so this is a driver that does not.
        let Some(morsel) = place.morsel else {
            return Err(Error::internal(
                "the root was told to keep the source order by a driver that does not say which morsel a chunk came from",
            ));
        };
        let bound = self.shared.bound.load(Ordering::Relaxed);
        if bound > 0 {
            queue = self.held_back(queue, morsel, bound)?;
            queue.held += taken.len();
        }
        let Some(order) = queue.order.as_mut() else {
            return Err(Error::internal("an order restoring root lost its order"));
        };
        if full(order.waiting.len()) && order.next != morsel {
            return Ok(Progress::Blocked(Blocked::Downstream(self.shared.buffer)));
        }
        order.waiting.insert((morsel, place.at), taken);
        place.at += 1;
        Ok(Progress::More)
    }

    fn combine(&self, place: RootPlace) -> Result<()> {
        let mut queue = self.shared.queue.lock().map_err(poisoned)?;
        let before = queue.ready.len();
        let Queue { ready, order, .. } = &mut *queue;
        if let Some(order) = order.as_mut() {
            if let Some(done) = place.morsel {
                order.finish(done);
            }
            order.release(ready);
        }
        self.settle(queue, before)
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        {
            // Every instance has combined by now, so nothing is being read and everything still
            // held is in order. Doing it here as well as in `combine` is what makes a root with no
            // instances at all, which is a pipeline over an empty file, come out empty rather than
            // holding something forever.
            let mut queue = self.shared.queue.lock().map_err(poisoned)?;
            let Queue { ready, order, .. } = &mut *queue;
            if let Some(order) = order.as_mut() {
                order.rest(ready);
            }
            self.shared.filled.notify_one();
        }
        self.shared.finished.store(true, Ordering::Release);
        Ok(())
    }
}

impl RootReader {
    /// Ask for every chunk to be in flat form by the time it is queued.
    ///
    /// For the caller outside the engine, which reads a value at a time and would otherwise have to
    /// understand a dictionary and a packed run to read a row. It is asked for here, on the reader,
    /// because the reader is the half the caller holds and the sink is already inside the query by
    /// the time there is anybody to ask.
    ///
    /// Call it before the query runs. Setting it while chunks are arriving is not unsafe and the
    /// answer is not wrong, it is just a result where the first chunks are in whatever form they
    /// were produced in and the rest are flat, which is not a thing any caller wants.
    pub fn flattening(&self) {
        self.shared.flatten.store(true, Ordering::Relaxed);
    }

    /// How long the flattening [`RootReader::flattening`] asked for took, summed over threads.
    ///
    /// Zero for a root nobody asked to flatten.
    #[must_use]
    pub fn flattened_ns(&self) -> u64 {
        self.shared.flattened.load(Ordering::Relaxed)
    }

    /// Ask the workers to wait in the sink while `bound` rows are ready and nobody took them.
    ///
    /// For a caller that reads with [`RootReader::wait_chunks`] on its own thread while the query
    /// runs on another, and sends the rows on before it takes more. Without the bound the query
    /// runs ahead of a slow caller and the queue holds the whole result. Call it before the query
    /// runs. A bound of zero is no bound, which is the default.
    pub fn streaming(&self, bound: usize) {
        self.shared.bound.store(bound, Ordering::Relaxed);
    }

    /// Moves the ready chunks into `into`, which must be empty, waiting until half the bound is
    /// ready or the run ended. False when the run ended and nothing is ready.
    ///
    /// An order restoring root can still hold chunks after a run that failed. See
    /// [`RootReader::failed`].
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while
    /// holding the queue.
    pub fn wait_chunks(&self, into: &mut VecDeque<Chunk>) -> Result<bool> {
        let half = self.shared.bound.load(Ordering::Relaxed) / 2;
        let mut queue = self.shared.queue.lock().map_err(poisoned)?;
        loop {
            let ended = self.shared.ended.load(Ordering::Acquire);
            if queue.rows >= half.max(1) || ended && !queue.ready.is_empty() {
                std::mem::swap(&mut queue.ready, into);
                queue.rows = 0;
                self.shared.room.notify_all();
                return Ok(true);
            }
            if ended {
                return Ok(false);
            }
            queue = self.shared.filled.wait(queue).map_err(poisoned)?;
        }
    }

    /// Says that the run of the query returned, so a reader that waits for a chunk stops waiting.
    pub fn end(&self) {
        self.shared.ended.store(true, Ordering::Release);
        // Under the lock, so a reader between its check and its wait does not miss the wake.
        let _queue = self.shared.queue.lock();
        self.shared.filled.notify_all();
    }

    /// Says that the reader takes no more chunks, so a worker that waits for room fails instead.
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        let _queue = self.shared.queue.lock();
        self.shared.room.notify_all();
    }

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

    /// Lets the reader take the chunks that arrived in source order before the query failed. See
    /// `Order::failed`. A root that does not restore the order queues each chunk as it comes and
    /// has nothing held.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while
    /// holding the queue.
    pub fn failed(&self) -> Result<()> {
        let mut queue = self.shared.queue.lock().map_err(poisoned)?;
        let Queue { ready, order, .. } = &mut *queue;
        if let Some(order) = order.as_mut() {
            order.failed(ready);
        }
        Ok(())
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

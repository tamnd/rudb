//! The threads a database runs queries on, parked between queries and handed work by the drivers.
//!
//! The thread count belongs to the database and not to the query. Two connections running two
//! queries on a machine with sixteen cores should use sixteen threads between them and not sixteen
//! each, and a pool that is made when a query starts cannot know about the other one. So there is
//! one of these per database and every query asks it, first for a number and then for the threads
//! themselves.
//!
//! # Why the threads are kept
//!
//! This used to lend a number and let the driver spawn scoped threads against it, which cost a
//! thread creation per pipeline per run and bought a borrow the compiler checked rather than one
//! this module promised. Its own documentation said the first measurement would decide whether that
//! trade needed revisiting, and the measurement came in. A scoped thread on the bench machine is
//! about sixteen microseconds to start and join, paid on the thread that starts it before that
//! thread does any work of its own, so a sixteen way scan spent a quarter of a millisecond before
//! it read a row. Over ClickBench that was most of the cost of every query that takes about a
//! millisecond, and it was the reason a scan was held to four instances no matter how many cores
//! the machine had.
//!
//! So the threads are kept. A worker parks on a condition variable, wakes when there is a job,
//! runs it, and parks again. The cost of using one is a lock, a push and a notify, which is
//! hundreds of nanoseconds rather than tens of microseconds.
//!
//! # The one unsafe block, and what makes it sound
//!
//! An operator borrows the plan and the catalog, so the work a driver wants to hand out borrows
//! them too, and a parked worker thread is `'static` as far as the compiler is concerned. Erasing
//! that lifetime needs `unsafe`, and the whole of this module's soundness is one invariant:
//!
//! **[`Lease::scatter`] does not return until every task it handed out has finished running.**
//!
//! That is the same invariant [`std::thread::scope`] has, and the same one `rayon` and `crossbeam`
//! have. It is upheld in three places and all three matter. The wait is in a [`Joined`] guard, so
//! an unwind out of the body still waits rather than skipping it. A worker counts its task down
//! whether the task returned or panicked, so a bug in an operator cannot turn into a hang. And if a
//! worker thread cannot be started at all, the job it would have run is run on the calling thread
//! instead, because a queued task nobody runs is a wait that never ends.
//!
//! What is not covered, and is not covered by `std::thread::scope` either, is leaking the guard.
//! There is no path here that can: the guard is a local of `scatter` and `scatter` is not generic
//! over anything that could hold on to it.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;

/// The thread budget of one database, and the threads themselves.
#[derive(Debug)]
pub struct Pool {
    threads: AtomicUsize,
    busy: AtomicUsize,
    shared: Arc<Shared>,
    hired: Mutex<Vec<JoinHandle<()>>>,
    live: AtomicUsize,
}

impl Pool {
    /// A pool that will lend up to `threads` at once, counting the thread that asks.
    ///
    /// Zero is read as one, because a database that may run a query on no threads at all cannot run
    /// a query, and the setting that feeds this is validated where a user sets it.
    ///
    /// No thread is started here. A database that never runs a parallel query never pays for one,
    /// which matters because a test or an embedded caller makes a lot of these.
    #[must_use]
    pub fn new(threads: usize) -> Self {
        Self {
            threads: AtomicUsize::new(threads.max(1)),
            busy: AtomicUsize::new(0),
            shared: Arc::new(Shared::default()),
            hired: Mutex::new(Vec::new()),
            live: AtomicUsize::new(0),
        }
    }

    /// The most that may run at once.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.threads.load(Ordering::Relaxed)
    }

    /// Change it, which is what `SET threads` does.
    ///
    /// Queries already running keep the threads they were lent. Nothing is taken back mid query,
    /// because a driver that lost a thread half way through a morsel would have to either abandon
    /// the rows it read or hold them, and neither is worth having for a setting somebody changed
    /// while a query was in flight.
    ///
    /// Workers already started are kept too. They are parked and cost a stack, and a setting that
    /// went down and will go up again should not have to pay to start them twice.
    pub fn resize(&self, threads: usize) {
        self.threads.store(threads.max(1), Ordering::Relaxed);
    }

    /// Borrow up to `want` threads, counting the caller's own as one of them.
    ///
    /// Never lends fewer than the caller, so a query always runs even when every other thread is
    /// spoken for. That is the difference between a governor and a semaphore, and it is deliberate:
    /// a query that waits for a thread it is not going to get is a query that hangs, and the worst
    /// a busy pool should do is make a query serial.
    #[must_use]
    pub fn lease(&self, want: usize) -> Lease<'_> {
        let wanted = want.saturating_sub(1);
        let mut busy = self.busy.load(Ordering::Relaxed);
        loop {
            let spare = self.threads().saturating_sub(1).saturating_sub(busy);
            let taken = wanted.min(spare);
            if taken == 0 {
                return Lease { pool: self, extra: 0 };
            }
            match self.busy.compare_exchange_weak(
                busy,
                busy + taken,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Lease { pool: self, extra: taken },
                Err(seen) => busy = seen,
            }
        }
    }

    /// How many workers exist right now, which is a test's way of seeing that they are reused.
    #[must_use]
    pub fn workers(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// Queue `tasks` copies of one batch and make sure something will run all of them.
    ///
    /// The shortfall is read under the same lock the jobs are pushed under, so a worker that is
    /// parked has already counted itself as idle and one that is running a job has not. Starting
    /// the threads happens after the lock is dropped, because starting a thread is the slow thing
    /// this module exists to stop doing and holding a lock across it would serialise every query in
    /// the database behind it.
    ///
    /// A worker that has finished a job but has not parked again yet counts as missing, which would
    /// start a thread that was not needed. So the count of threads that exist is the real ceiling,
    /// and it is one below the database's thread count because that is the most a lease can ever
    /// hand out. Once there are that many the shortfall is ignored, and nothing is lost by ignoring
    /// it: every worker looks at the queue again after each job, so a job that is in the queue while
    /// a worker is between jobs is a job that worker will find.
    fn enqueue(&self, batch: &Arc<Batch>, tasks: usize) {
        let shortfall = {
            let mut queue = self.shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
            for _ in 0..tasks {
                queue.jobs.push_back(Arc::clone(batch));
            }
            tasks.saturating_sub(queue.idle)
        };
        for _ in 0..tasks {
            self.shared.ready.notify_one();
        }
        for _ in 0..shortfall {
            if self.live.load(Ordering::Relaxed) >= self.threads().saturating_sub(1) {
                break;
            }
            if !self.hire() {
                self.inline();
            }
        }
    }

    /// Start one worker, reporting whether the operating system gave us one.
    fn hire(&self) -> bool {
        let shared = Arc::clone(&self.shared);
        let started =
            std::thread::Builder::new().name("rudb-worker".to_owned()).spawn(move || work(&shared));
        match started {
            Ok(handle) => {
                self.hired.lock().unwrap_or_else(PoisonError::into_inner).push(handle);
                self.live.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => false,
        }
    }

    /// Run one queued job on this thread, for when a worker could not be started.
    ///
    /// Any job will do rather than one of the caller's own. They are all work somebody is waiting
    /// on, and the queue is shortened either way, which is the only thing that matters here.
    fn inline(&self) {
        let job = {
            let mut queue = self.shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
            queue.jobs.pop_front()
        };
        if let Some(job) = job {
            job.run();
            job.finish();
        }
    }
}

impl Default for Pool {
    /// One thread, which is the pool a test or an embedded caller that never said otherwise gets.
    fn default() -> Self {
        Self::new(1)
    }
}

impl Drop for Pool {
    /// Tells the workers to stop and waits for them.
    ///
    /// Waiting is not politeness. A worker holds an `Arc` to the shared queue and nothing else, so
    /// it would not read freed memory, but a database that was dropped should not leave threads
    /// behind it, and a test that made a thousand pools should not end with a thousand threads.
    fn drop(&mut self) {
        {
            let mut queue = self.shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
            queue.closed = true;
        }
        self.shared.ready.notify_all();
        let hired = std::mem::take(&mut *self.hired.lock().unwrap_or_else(PoisonError::into_inner));
        for handle in hired {
            let _ = handle.join();
        }
    }
}

/// What the workers and the pool both hold.
#[derive(Debug, Default)]
struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

/// The jobs waiting to be picked up, and how many threads are waiting to pick one up.
#[derive(Debug, Default)]
struct Queue {
    jobs: VecDeque<Arc<Batch>>,
    /// Workers parked on `ready` right now, which is how a caller knows whether to start more.
    idle: usize,
    /// Set when the pool is going away, which is the only way a worker's loop ends.
    closed: bool,
}

/// One worker's whole life.
fn work(shared: &Shared) {
    let mut queue = shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if let Some(job) = queue.jobs.pop_front() {
            drop(queue);
            job.run();
            job.finish();
            queue = shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
            continue;
        }
        if queue.closed {
            return;
        }
        queue.idle += 1;
        queue = shared.ready.wait(queue).unwrap_or_else(PoisonError::into_inner);
        queue.idle -= 1;
    }
}

/// One task handed to several workers at once, and the count that says when they are all done.
#[derive(Debug)]
struct Batch {
    /// The work, with its lifetime erased. See the module documentation for what keeps it alive.
    task: *const (dyn Fn() + Sync + 'static),
    left: Mutex<usize>,
    done: Condvar,
    panicked: AtomicBool,
}

// SAFETY: the pointer is made from a `&(dyn Fn() + Sync)`, and a shared reference to a `Sync` value
// is `Send`, so passing it between threads is exactly what the original reference already allowed.
// What the raw pointer erases is the lifetime, not the thread safety, and `Lease::scatter` is what
// keeps the lifetime honest by not returning until the count in `left` has reached zero.
#[allow(unsafe_code)]
unsafe impl Send for Batch {}
// SAFETY: as above. The other fields are a mutex, a condition variable and an atomic.
#[allow(unsafe_code)]
unsafe impl Sync for Batch {}

impl Batch {
    /// Run the task once, turning a panic into a flag rather than an unwind.
    ///
    /// Catching is what stops a bug in an operator becoming a hang. The count has to come down
    /// whatever happened, and letting the panic through would kill this worker between running the
    /// task and counting it down, leaving the thread that is waiting to wait forever.
    fn run(&self) {
        // SAFETY: this pointer came from a reference that `Lease::scatter` still holds. The batch
        // is only reachable through the queue, `scatter` put it there, and `scatter` does not
        // return until `left` is zero, which cannot happen until every worker holding a copy has
        // called `finish` below. So the borrow this came from outlives every call to this.
        #[allow(unsafe_code)]
        let task = unsafe { &*self.task };
        if catch_unwind(AssertUnwindSafe(task)).is_err() {
            self.panicked.store(true, Ordering::Relaxed);
        }
    }

    /// Count one task down and wake the waiter if that was the last of them.
    fn finish(&self) {
        let mut left = self.left.lock().unwrap_or_else(PoisonError::into_inner);
        *left -= 1;
        if *left == 0 {
            self.done.notify_all();
        }
    }

    /// Block until every task in this batch has run.
    fn wait(&self) {
        let mut left = self.left.lock().unwrap_or_else(PoisonError::into_inner);
        while *left > 0 {
            left = self.done.wait(left).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// The wait, in a guard, so that an unwind out of the caller's own work still does it.
struct Joined<'a> {
    batch: &'a Batch,
}

impl Drop for Joined<'_> {
    fn drop(&mut self) {
        self.batch.wait();
    }
}

/// Threads borrowed from a pool, given back when this goes away.
#[derive(Debug)]
pub struct Lease<'a> {
    pool: &'a Pool,
    /// How many threads beyond the caller's own this lease covers.
    extra: usize,
}

impl Lease<'_> {
    /// How many instances may run at once, counting the thread that asked.
    #[must_use]
    pub fn degree(&self) -> usize {
        self.extra + 1
    }

    /// Run `task` on every borrowed thread and `body` on this one, returning once all have finished.
    ///
    /// The caller's thread runs work rather than waiting on the threads it woke, because a degree
    /// of two that keeps one thread idle is not a degree of two. `body` is therefore usually the
    /// same work as `task` written once more, and the two are separate parameters because the
    /// caller's instance is the one whose result can be handed straight back.
    ///
    /// Returns what `body` returned, and whether any of the borrowed threads panicked. A panic is
    /// reported rather than resumed: it is a bug in this engine, the thread it happened on has
    /// already printed it, and what is left to do is fail the query.
    pub fn scatter<R>(&self, task: &(dyn Fn() + Sync), body: impl FnOnce() -> R) -> (R, bool) {
        if self.extra == 0 {
            return (body(), false);
        }
        let borrowed: *const (dyn Fn() + Sync + '_) = task;
        // SAFETY: the lifetime is erased and then kept honest by the `Joined` guard below, which
        // does not let this function return until every worker that took a copy of this batch has
        // finished with it. `task` outlives this call, so it outlives every use of the pointer.
        // The module documentation has the whole argument and the three places it is upheld.
        #[allow(unsafe_code)]
        let erased = unsafe {
            std::mem::transmute::<*const (dyn Fn() + Sync + '_), *const (dyn Fn() + Sync + 'static)>(
                borrowed,
            )
        };
        let batch = Arc::new(Batch {
            task: erased,
            left: Mutex::new(self.extra),
            done: Condvar::new(),
            panicked: AtomicBool::new(false),
        });
        self.pool.enqueue(&batch, self.extra);
        let value = {
            let _joined = Joined { batch: &batch };
            body()
        };
        (value, batch.panicked.load(Ordering::Relaxed))
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.pool.busy.fetch_sub(self.extra, Ordering::Relaxed);
    }
}

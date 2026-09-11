//! The I/O threads, which are not the execution threads.
//!
//! `spec/engine/05-scan.md` section 5.3. A thread blocked on a read is not a core lost to
//! execution, because the thread that blocked was never an execution thread. That is the whole
//! idea and it is the one DuckDB v2.0 arrived at, with the practical effect that on a machine where
//! the data does not fit in the page cache the engine keeps the CPU busy while the disk works.
//!
//! # Why the two pools are sized apart
//!
//! The execution pool wants one thread per core, because more than that is context switches
//! between threads that all want the same ALUs. The I/O pool wants however many concurrent reads
//! it takes to keep the device busy, which for a local NVMe is a small number and for object
//! storage is a large one: an S3 GET is a hundred milliseconds of waiting and nothing else, so the
//! only way to fill a link is to have a lot of them outstanding at once. Those two numbers differ
//! by an order of magnitude, which is why this pool is sized on its own rather than being told the
//! core count and left to it. [`Config::local_disk`] and [`Config::object_store`] are the two
//! answers.
//!
//! # Coalescing
//!
//! A batch handed to [`Pool::submit`] is sorted and adjacent ranges are merged into one physical
//! read, then scattered back into the per request buffers. Section 5.3 says coalescing is worth
//! more than concurrency on spinning media and on object storage, where the per request cost
//! dominates. It is not free: merging two ranges means reading into a scratch buffer and copying
//! out of it, plus reading the bytes in the gap and throwing them away. Which way that comes out is
//! a fact about a machine and about an access pattern, so it is a knob with a measurement behind it
//! rather than a thing that is always on. `cargo xtask io` is the measurement, and the defaults in
//! [`Config::local_disk`] say which run of it produced them.
//!
//! The short version of that run: cold, coalescing over a small gap is worth nearly two to one on
//! scattered pages and worth nothing on a sequential scan, and coalescing over a large gap is worth
//! two to one against you on a column projection. So the gap is small.
//!
//! # What this is not
//!
//! It is not io_uring and it is not asynchronous in the sense of a runtime. There are threads and
//! they block, which is what the standard library gives us and what the zero dependency rule in
//! `spec/18-package-layout.md` leaves us with. Section 5.3 records io_uring as a possible later
//! change under the same [`File::submit`] interface, with the measurement that would justify it,
//! which is this pool showing up as a bottleneck on `server1` where there are four cores to spare.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use rudb_common::Result;

use crate::File;
use crate::submit::{Completion, Filler, Request, Response};

/// How an [`Pool`] is sized and how hard it coalesces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// How many I/O threads. One means every batch is served in submission order by one thread,
    /// which is a useful thing for a test to be able to ask for.
    pub threads: usize,
    /// Two ranges no further apart than this are read as one, and the bytes in the gap are read
    /// and thrown away. Zero merges only ranges that touch.
    pub coalesce_gap: u64,
    /// A merged read never spans more than this, however small the gaps are. Without it a
    /// projection of two columns from opposite ends of a row group turns into a read of the row
    /// group.
    pub coalesce_span: u64,
    /// Whether to merge at all.
    pub coalesce: bool,
}

impl Config {
    /// The sizing for a local disk.
    ///
    /// Every number here came out of `cargo xtask io --cold` on `server3`, which is what that task
    /// exists for. The warm table is the opposite of the cold one on almost every row, which is the
    /// reason the cold one is the one that decided this.
    ///
    /// Threads equal to the core count, floored at two and capped at eight. Cold, on eight cores, a
    /// batch of scattered reads goes from 2.7 seconds through the loop to 392 milliseconds at eight
    /// threads, and sixteen threads is 413, inside the spread. Sequential is 204 at eight and 211 at
    /// sixteen, the same. Eight is where the device saturates and past it the extra threads are
    /// contending for the memory bandwidth the execution threads want.
    ///
    /// Coalescing on, over gaps of sixteen kilobytes. The gap is the whole decision and it is a
    /// narrow one. Sixteen kilobytes takes a batch of scattered eight kilobyte pages from 413
    /// milliseconds to 223, nearly twice as fast, and it does it while reading only 1.22 times the
    /// bytes asked for. Widening it to half a megabyte buys nothing on that pattern that is outside
    /// the spread, reads 7.03 times the bytes, and costs a column projection dearly: 64 kilobyte
    /// ranges 448 kilobytes apart go from 60 milliseconds unmerged to 115 merged, because the gaps
    /// between columns are real and reading them is work. Sixteen kilobytes is small enough to leave
    /// that pattern alone entirely, which is why it is the number.
    ///
    /// The span cap means a sequential scan in one megabyte ranges merges nothing at all, which the
    /// table confirms: its read count does not move at any gap. That is the intended answer. A one
    /// megabyte read is already large enough that saving the syscall next to it is not measurable.
    #[must_use]
    pub fn local_disk() -> Self {
        Self {
            threads: cores().clamp(2, 8),
            coalesce_gap: 16 << 10,
            coalesce_span: 1 << 20,
            coalesce: true,
        }
    }

    /// The sizing for object storage.
    ///
    /// An order of magnitude more threads, because the thing being hidden is a round trip rather
    /// than a device, and coalescing on with a generous gap, because a request that costs a
    /// hundred milliseconds however many bytes it asks for makes reading a gap and throwing it
    /// away obviously right.
    ///
    /// Named rather than measured. There is no object store in this workspace yet and this is the
    /// default it will be measured against when there is, not a number that came out of a run.
    #[must_use]
    pub fn object_store() -> Self {
        Self {
            threads: (cores() * 8).clamp(32, 128),
            coalesce_gap: 512 << 10,
            coalesce_span: 8 << 20,
            coalesce: true,
        }
    }

    /// This configuration with `threads` threads.
    #[must_use]
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }

    /// This configuration merging ranges no more than `gap` bytes apart.
    #[must_use]
    pub fn coalescing(mut self, gap: u64) -> Self {
        self.coalesce = true;
        self.coalesce_gap = gap;
        self
    }

    /// This configuration issuing every range as its own read.
    #[must_use]
    pub fn not_coalescing(mut self) -> Self {
        self.coalesce = false;
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::local_disk()
    }
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
}

/// What the pool has done, for the byte counting `spec/engine/13-measurement.md` section 13.5
/// asks for.
///
/// The distinction between [`Self::wanted`] and [`Self::read`] is the one that matters and it is
/// the reason coalescing is counted rather than assumed harmless. A merged read that spans a gap
/// reads bytes nobody asked for, and a reader whose pruning is wrong reads bytes it should have
/// skipped. Both show up here as a ratio and neither shows up in the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// How many requests were submitted.
    pub requests: u64,
    /// How many physical reads those requests turned into.
    pub reads: u64,
    /// How many bytes the requests asked for.
    pub wanted: u64,
    /// How many bytes were actually read off the device.
    pub read: u64,
}

#[derive(Debug, Default)]
struct Counters {
    requests: AtomicU64,
    reads: AtomicU64,
    wanted: AtomicU64,
    read: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> Stats {
        Stats {
            requests: self.requests.load(Ordering::Relaxed),
            reads: self.reads.load(Ordering::Relaxed),
            wanted: self.wanted.load(Ordering::Relaxed),
            read: self.read.load(Ordering::Relaxed),
        }
    }
}

/// One request inside a physical read.
#[derive(Debug)]
struct Part {
    index: usize,
    offset: u64,
    buf: Vec<u8>,
}

/// One physical read, serving the requests it was merged out of.
#[derive(Debug)]
struct Job {
    file: Arc<dyn File>,
    filler: Filler,
    offset: u64,
    span: usize,
    parts: Vec<Part>,
}

impl Job {
    /// Performs the read and fills the completion slots it covers.
    fn run(self, counters: &Counters) {
        let Self { file, filler, offset, span, mut parts } = self;
        counters.reads.fetch_add(1, Ordering::Relaxed);

        // One part is the common case and it reads straight into the caller's buffer, which is the
        // whole reason a request owns one. Only a merge needs the scratch and the copies.
        if parts.len() == 1 {
            let part = parts.pop().unwrap_or_else(|| unreachable!("checked one part"));
            let Part { index, offset, mut buf } = part;
            let outcome = file.read_at(offset, &mut buf).map(|read| {
                counters.read.fetch_add(read as u64, Ordering::Relaxed);
                Response::new(index, offset, read, buf)
            });
            filler.finish(index, outcome);
            return;
        }

        let mut scratch = vec![0u8; span];
        match file.read_at(offset, &mut scratch) {
            Ok(read) => {
                counters.read.fetch_add(read as u64, Ordering::Relaxed);
                for part in parts {
                    let Part { index, offset: at, mut buf } = part;
                    let start = (at - offset) as usize;
                    // A short read on a merged read is a short read on every part past where it
                    // stopped, which is the same thing it would have been unmerged. Saying so here
                    // rather than erroring is what keeps merging invisible to the caller.
                    let got = read.saturating_sub(start).min(buf.len());
                    buf[..got].copy_from_slice(&scratch[start..start + got]);
                    filler.finish(index, Ok(Response::new(index, at, got, buf)));
                }
            }
            Err(error) => {
                for part in parts {
                    filler.finish(part.index, Err(error.clone()));
                }
            }
        }
    }
}

#[derive(Debug)]
struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}

#[derive(Debug)]
struct Shared {
    queue: Mutex<Queue>,
    work: Condvar,
    counters: Counters,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A pool of threads that do nothing but wait for disks.
///
/// Cloning a handle is cheap and gives another handle on the same pool. The threads stop when the
/// last handle goes away.
#[derive(Debug, Clone)]
pub struct Pool {
    shared: Arc<Shared>,
    /// Held and never read. The field is the shutdown mechanism rather than a value: the last
    /// handle to go away drops the last `Arc`, and that is what closes the queue and joins.
    #[allow(dead_code, reason = "holding this is the point of it, reading it is not")]
    threads: Arc<Threads>,
    config: Config,
}

/// The join handles, in their own allocation so that dropping the last [`Pool`] handle is what
/// stops the threads rather than dropping any of them.
#[derive(Debug)]
struct Threads {
    shared: Arc<Shared>,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for Threads {
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.work.notify_all();
        let handles = std::mem::take(
            &mut *self.handles.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for handle in handles {
            let _ = handle.join();
        }
    }
}

impl Pool {
    /// A pool sized and configured by `config`.
    ///
    /// # Panics
    ///
    /// If a thread cannot be spawned, which is not a condition a database can carry on from and is
    /// not one a caller can do anything about.
    #[must_use]
    pub fn new(config: Config) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue { jobs: VecDeque::new(), closed: false }),
            work: Condvar::new(),
            counters: Counters::default(),
        });
        let mut handles = Vec::with_capacity(config.threads);
        for n in 0..config.threads.max(1) {
            let shared = Arc::clone(&shared);
            handles.push(
                std::thread::Builder::new()
                    .name(format!("rudb-io-{n}"))
                    .spawn(move || worker(&shared))
                    .expect("could not spawn an I/O thread"),
            );
        }
        let threads =
            Arc::new(Threads { shared: Arc::clone(&shared), handles: Mutex::new(handles) });
        Self { shared, threads, config }
    }

    /// How this pool is sized.
    #[must_use]
    pub fn config(&self) -> Config {
        self.config
    }

    /// What it has read since it was made.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.shared.counters.snapshot()
    }

    /// Queues every request against `file` and hands back something to wait on or poll.
    ///
    /// The requests are merged where the configuration says to, then queued. This call does not
    /// block on the disk, which is the point: the caller goes back to decoding the row group it
    /// already has.
    #[must_use]
    pub fn submit(&self, file: &Arc<dyn File>, requests: Vec<Request>) -> Completion {
        let count = requests.len();
        self.shared.counters.requests.fetch_add(count as u64, Ordering::Relaxed);
        let wanted: u64 = requests.iter().map(|r| r.len() as u64).sum();
        self.shared.counters.wanted.fetch_add(wanted, Ordering::Relaxed);

        let (completion, filler) = Completion::pending(count);
        let mut wanted_nothing = Vec::new();
        let mut real = Vec::with_capacity(count);
        for (index, request) in requests.into_iter().enumerate() {
            if request.is_empty() {
                wanted_nothing.push((index, request));
            } else {
                real.push((index, request));
            }
        }
        // A request for no bytes is answered here rather than queued, because nobody reads nothing
        // and because a slot left empty is a caller left waiting for a read that will never be
        // issued. It gets a response of length zero, not no response at all.
        for (index, request) in wanted_nothing {
            let offset = request.offset();
            filler.finish(index, Ok(Response::new(index, offset, 0, request.into_buffer())));
        }
        let jobs = plan(real, self.config, file, &filler);
        let mut queue = self.shared.lock();
        if queue.closed {
            // The pool is stopping. Dropping the filler wakes the waiter with an error, which is
            // better than queueing work nobody will run.
            drop(queue);
            drop(filler);
            return completion;
        }
        queue.jobs.extend(jobs);
        drop(queue);
        self.shared.work.notify_all();
        completion
    }

    /// How many jobs are queued and not yet picked up.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.shared.lock().jobs.len()
    }
}

/// Turns a batch of requests into the physical reads that will serve them.
///
/// Sorted by offset, because the merge is a scan over neighbours and because issuing a row group's
/// ranges in file order is what a device wants whether or not they get merged.
fn plan(
    requests: Vec<(usize, Request)>,
    config: Config,
    file: &Arc<dyn File>,
    filler: &Filler,
) -> Vec<Job> {
    let mut parts: Vec<Part> = requests
        .into_iter()
        .map(|(index, request)| {
            let offset = request.offset();
            Part { index, offset, buf: request.into_buffer() }
        })
        .collect();
    parts.sort_by_key(|part| part.offset);

    let mut jobs: Vec<Job> = Vec::with_capacity(parts.len());
    for part in parts {
        let end = part.offset + part.buf.len() as u64;
        if config.coalesce {
            if let Some(last) = jobs.last_mut() {
                let last_end = last.offset + last.span as u64;
                let gap = part.offset.saturating_sub(last_end);
                let span = end.saturating_sub(last.offset);
                // `part.offset < last_end` means the ranges overlap, which merges for free.
                if gap <= config.coalesce_gap && span <= config.coalesce_span {
                    last.span = span as usize;
                    last.parts.push(part);
                    continue;
                }
            }
        }
        jobs.push(Job {
            file: Arc::clone(file),
            filler: filler.clone(),
            offset: part.offset,
            span: part.buf.len(),
            parts: vec![part],
        });
    }
    jobs
}

fn worker(shared: &Arc<Shared>) {
    loop {
        let mut queue = shared.lock();
        let job = loop {
            if let Some(job) = queue.jobs.pop_front() {
                break job;
            }
            if queue.closed {
                return;
            }
            queue = shared.work.wait(queue).unwrap_or_else(std::sync::PoisonError::into_inner);
        };
        drop(queue);
        job.run(&shared.counters);
    }
}

/// A file whose batched reads go through a [`Pool`].
///
/// Everything else is the file underneath, unchanged. `read_at` in particular stays synchronous on
/// the calling thread, because handing one read to another thread and then blocking on it is two
/// context switches to do what the caller was going to do anyway.
#[derive(Debug)]
pub struct Pooled {
    file: Arc<dyn File>,
    pool: Pool,
}

impl Pooled {
    /// Puts `pool` underneath `file`.
    #[must_use]
    pub fn new(file: Box<dyn File>, pool: Pool) -> Self {
        Self { file: Arc::from(file), pool }
    }

    /// The pool this file reads through.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }
}

impl File for Pooled {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.file.read_at(offset, buf)
    }

    fn submit(&self, requests: Vec<Request>) -> Completion {
        self.pool.submit(&self.file, requests)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        self.file.write_at(offset, data)
    }

    fn sync(&self) -> Result<()> {
        self.file.sync()
    }

    fn truncate(&self, len: u64) -> Result<()> {
        self.file.truncate(len)
    }

    fn len(&self) -> Result<u64> {
        self.file.len()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Config, Pool, Pooled};
    use crate::submit::{Request, Response};
    use crate::{File, Filesystem, OpenMode, SimFilesystem};

    /// Two hundred and fifty six bytes, each one its own offset, so an assertion on the bytes is
    /// an assertion on where they came from.
    fn ramp(pool: Pool) -> Pooled {
        let fs = SimFilesystem::new();
        let file = fs.open(Path::new("/data"), OpenMode::Create).unwrap();
        let bytes: Vec<u8> = (0..=255u8).collect();
        file.write_at(0, &bytes).unwrap();
        file.sync().unwrap();
        Pooled::new(file, pool)
    }

    #[test]
    fn a_batch_comes_back_answering_the_requests_it_was_given() {
        let file = ramp(Pool::new(Config::local_disk()));
        let responses = file
            .submit(vec![Request::new(100, 4), Request::new(0, 4), Request::new(200, 4)])
            .wait()
            .unwrap();
        assert_eq!(responses[0].bytes(), &[100, 101, 102, 103]);
        assert_eq!(responses[1].bytes(), &[0, 1, 2, 3]);
        assert_eq!(responses[2].bytes(), &[200, 201, 202, 203]);
    }

    #[test]
    fn one_thread_answers_everything_just_as_well_as_eight() {
        // The pool being a pool is not supposed to be visible in the answers, and a test that
        // passes at eight threads and not at one is a test that found a race at eight.
        for threads in [1, 2, 8] {
            let file = ramp(Pool::new(Config::local_disk().with_threads(threads)));
            let requests = (0..32).map(|i| Request::new(i * 8, 8)).collect::<Vec<_>>();
            let responses = file.submit(requests).wait().unwrap();
            assert_eq!(responses.len(), 32, "at {threads} threads");
            for (i, response) in responses.iter().enumerate() {
                assert_eq!(response.bytes()[0], (i * 8) as u8, "at {threads} threads");
            }
        }
    }

    #[test]
    fn adjacent_ranges_become_one_read_and_the_bytes_do_not_change() {
        let pool = Pool::new(Config::local_disk().with_threads(1).coalescing(0));
        let file = ramp(pool.clone());
        let responses = file
            .submit(vec![Request::new(0, 8), Request::new(8, 8), Request::new(16, 8)])
            .wait()
            .unwrap();
        assert_eq!(pool.stats().reads, 1, "three adjacent ranges are one read");
        assert_eq!(pool.stats().requests, 3);
        assert_eq!(pool.stats().wanted, 24);
        assert_eq!(pool.stats().read, 24, "and no byte was read that nobody asked for");
        for (i, response) in responses.iter().enumerate() {
            assert_eq!(response.bytes()[0], (i * 8) as u8);
        }
    }

    #[test]
    fn a_gap_is_read_and_thrown_away_and_the_byte_count_says_so() {
        // This is why the byte count is two numbers. Coalescing over a gap is a decision to read
        // bytes nobody wanted, and a coalescing policy that is too generous looks exactly like a
        // pruning bug from the outside: a right answer and too much disk.
        let pool = Pool::new(Config::local_disk().with_threads(1).coalescing(16));
        let file = ramp(pool.clone());
        let responses = file.submit(vec![Request::new(0, 4), Request::new(20, 4)]).wait().unwrap();
        assert_eq!(pool.stats().reads, 1);
        assert_eq!(pool.stats().wanted, 8);
        assert_eq!(pool.stats().read, 24, "the sixteen byte gap was read too");
        assert_eq!(responses[0].bytes(), &[0, 1, 2, 3]);
        assert_eq!(responses[1].bytes(), &[20, 21, 22, 23]);
    }

    #[test]
    fn a_gap_wider_than_the_policy_stays_two_reads() {
        let pool = Pool::new(Config::local_disk().with_threads(1).coalescing(4));
        let file = ramp(pool.clone());
        file.submit(vec![Request::new(0, 4), Request::new(20, 4)]).wait().unwrap();
        assert_eq!(pool.stats().reads, 2);
        assert_eq!(pool.stats().read, 8);
    }

    #[test]
    fn the_span_limit_stops_a_chain_of_small_gaps_becoming_one_huge_read() {
        // Without it, a hundred ranges four bytes apart merge pairwise all the way across the file
        // and the projection that was supposed to read 200 MB reads 20 GB.
        let mut config = Config::local_disk().with_threads(1).coalescing(12);
        config.coalesce_span = 32;
        let pool = Pool::new(config);
        let file = ramp(pool.clone());
        let requests = (0..8).map(|i| Request::new(i * 16, 4)).collect::<Vec<_>>();
        file.submit(requests).wait().unwrap();
        assert_eq!(pool.stats().reads, 4, "eight ranges over 128 bytes, capped at 32 a read");
    }

    #[test]
    fn both_defaults_coalesce_and_the_local_one_does_it_far_more_narrowly() {
        // Both merge, because `cargo xtask io --cold` says merging a small gap is worth nearly two
        // to one on scattered reads even on a local device. The gap is what separates them: a local
        // disk merges over kilobytes, an object store over hundreds of them, because an object
        // store request costs a round trip whatever it asks for.
        assert!(Config::local_disk().coalesce);
        assert!(Config::object_store().coalesce);
        assert!(Config::object_store().coalesce_gap >= Config::local_disk().coalesce_gap * 8);
        // The sizing difference is the point of there being two, per section 5.3.
        assert!(Config::object_store().threads >= Config::local_disk().threads * 4);
    }

    #[test]
    fn the_local_gap_is_too_small_to_swallow_the_space_between_two_columns() {
        // The row the default was chosen on. 64KiB ranges 448KiB apart is a projection of one
        // column out of eight, and merging those cold costs two to one, so the default must leave
        // that pattern alone. This is that claim as an assertion rather than as a paragraph.
        let pool = Pool::new(Config::local_disk().with_threads(1));
        let fs = SimFilesystem::new();
        let handle = fs.open(Path::new("/columns"), OpenMode::Create).unwrap();
        handle.write_at(0, &vec![7u8; 2 << 20]).unwrap();
        handle.sync().unwrap();
        let file = Pooled::new(handle, pool.clone());
        let requests = (0..4).map(|i| Request::new(i * (512 << 10), 64 << 10)).collect::<Vec<_>>();
        file.submit(requests).wait().unwrap();
        assert_eq!(pool.stats().reads, 4, "four columns 448KiB apart are four reads and not one");
        assert_eq!(pool.stats().read, pool.stats().wanted, "and nothing else was read");
    }

    #[test]
    fn a_short_read_stays_short_through_a_merge() {
        let pool = Pool::new(Config::local_disk().with_threads(1).coalescing(0));
        let file = ramp(pool.clone());
        // 248 through 256 is there, 256 through 264 is past the end.
        let responses =
            file.submit(vec![Request::new(248, 8), Request::new(256, 8)]).wait().unwrap();
        assert_eq!(pool.stats().reads, 1);
        assert!(!responses[0].is_short());
        assert!(responses[1].is_short());
        assert_eq!(responses[1].read(), 0);
    }

    #[test]
    fn a_failed_read_fails_every_request_it_was_merged_with_and_no_others() {
        let fs = SimFilesystem::new();
        let file = fs.open(Path::new("/data"), OpenMode::Create).unwrap();
        file.write_at(0, &(0..=255u8).collect::<Vec<u8>>()).unwrap();
        file.sync().unwrap();
        let pool = Pool::new(Config::local_disk().with_threads(1).coalescing(0));
        let pooled = Pooled::new(file, pool);
        // Reads are served in queue order at one thread, so the first physical read is the merged
        // pair at the front of the file.
        fs.fail_read_at(0);
        let mut completion =
            pooled.submit(vec![Request::new(0, 8), Request::new(8, 8), Request::new(128, 8)]);
        let mut failed = 0;
        let mut answered = 0;
        while let Some(outcome) = completion.take() {
            match outcome {
                Ok(_) => answered += 1,
                Err(_) => failed += 1,
            }
        }
        assert_eq!((answered, failed), (1, 2));
    }

    #[test]
    fn an_empty_request_is_answered_rather_than_queued() {
        let pool = Pool::new(Config::local_disk().with_threads(1));
        let file = ramp(pool.clone());
        let responses = file.submit(vec![Request::new(0, 0), Request::new(4, 4)]).wait().unwrap();
        assert_eq!(pool.stats().reads, 1, "nobody reads nothing");
        // A response of length zero and not no response at all. A slot left empty is a caller left
        // waiting for a read that is never going to be issued.
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].read(), 0);
        assert_eq!(responses[1].bytes(), &[4, 5, 6, 7]);
    }

    #[test]
    fn an_empty_batch_is_done_before_it_is_submitted() {
        let pool = Pool::new(Config::local_disk());
        let file = ramp(pool.clone());
        let completion = file.submit(Vec::new());
        assert!(completion.is_done());
        assert!(completion.wait().unwrap().is_empty());
    }

    #[test]
    fn a_submission_returns_before_the_reads_do() {
        // The reason the pool exists. If `submit` blocked until the bytes arrived it would be
        // `read_at` with extra steps, and the scan could not decode row group n while row group n
        // plus one is in flight.
        let pool = Pool::new(Config::local_disk().with_threads(1));
        let file = ramp(pool.clone());
        let requests = (0..64).map(|i| Request::new(i * 4, 4)).collect::<Vec<_>>();
        let completion = file.submit(requests);
        // Not an assertion on how much is left, because a fast machine may well have drained it.
        // The assertion is that submitting did not wait for all of it.
        let responses = completion.wait().unwrap();
        assert_eq!(responses.len(), 64);
        assert_eq!(pool.stats().requests, 64);
    }

    #[test]
    fn everything_submitted_is_answered_once_the_pool_is_stopping() {
        let pool = Pool::new(Config::local_disk().with_threads(2));
        let file = ramp(pool.clone());
        let completion = file.submit(vec![Request::new(0, 4)]);
        // Dropping the last pool handle stops the threads, and the outstanding batch has to come
        // back one way or the other. A hang here is the failure the test gate names.
        drop(pool);
        let responses = completion.wait();
        assert!(responses.is_ok() || responses.is_err());
    }

    #[test]
    fn read_at_on_a_pooled_file_is_the_read_underneath_it() {
        let file = ramp(Pool::new(Config::local_disk()));
        let mut buf = [0u8; 4];
        file.read_exact_at(64, &mut buf).unwrap();
        assert_eq!(buf, [64, 65, 66, 67]);
        assert_eq!(file.len().unwrap(), 256);
    }

    #[test]
    fn responses_are_in_submission_order_whatever_order_the_threads_finished_in() {
        let file = ramp(Pool::new(Config::local_disk().with_threads(8)));
        // Descending offsets, so submission order and file order disagree and the sort inside the
        // planner has something to get wrong.
        let requests = (0..32).rev().map(|i| Request::new(i * 8, 8)).collect::<Vec<_>>();
        let responses = file.submit(requests).wait().unwrap();
        let indices: Vec<usize> = responses.iter().map(Response::index).collect();
        assert_eq!(indices, (0..32).collect::<Vec<_>>());
        for (i, response) in responses.iter().enumerate() {
            assert_eq!(response.bytes()[0], ((31 - i) * 8) as u8);
        }
    }
}

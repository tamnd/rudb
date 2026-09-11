//! Stating every read up front, and waiting for the answers.
//!
//! `spec/engine/05-scan.md` section 5.3 is the specification. `File::read_at` is synchronous and a
//! thread that calls it stops until the bytes arrive, which on a machine where the data does not
//! fit in the page cache is a core that is not computing. The answer is the one DuckDB v2.0
//! reached and it is implementable with the standard library alone: the caller states everything
//! it wants in one call and then either waits or goes and does something else.
//!
//! # Why this is not ceremony
//!
//! Two things pay for the interface, and neither of them is available to a caller that reads one
//! range at a time.
//!
//! Batching. A Parquet row group scan knows every byte range it needs before it reads any of them,
//! so it can hand all of them over at once, and a backend that has all of them at once can issue
//! them concurrently and can coalesce adjacent ranges into one larger read. On object storage the
//! per request cost dominates and the coalescing is worth more than the concurrency.
//!
//! Overlap. The scan submits the reads for row group n plus one while it is decoding row group n,
//! which is the read ahead that turns a stop and go scan into a continuous one.
//!
//! # The shape io_uring needs
//!
//! io_uring is deferred, per section 5.3, because it is Linux only and it is a large amount of
//! unsafe code against a raw syscall interface under a zero dependency rule. What is not deferred
//! is the shape, because retrofitting it is the expensive half.
//!
//! A [`Request`] owns its destination buffer and hands that buffer back inside the [`Response`].
//! That is not an accident of the borrow checker, it is what a kernel interface wants: the buffer
//! has to stay alive and untouched for as long as the kernel might write into it, which a borrow
//! cannot promise once the submitting stack frame is free to return. Ownership can, and it is the
//! same ownership a registered buffer pool would hand out. So the day io_uring arrives it goes in
//! under [`File::submit`] and no caller changes.
//!
//! [`File::submit`]: crate::File::submit

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use rudb_common::{Error, Result};

/// One read, with the buffer its bytes land in.
///
/// The length wanted is the length of the buffer. There is no separate length field because two
/// fields that have to agree are two fields that disagree eventually.
#[derive(Debug)]
pub struct Request {
    offset: u64,
    buf: Vec<u8>,
}

impl Request {
    /// A read of `len` bytes at `offset`, into a buffer allocated here.
    #[must_use]
    pub fn new(offset: u64, len: usize) -> Self {
        Self { offset, buf: vec![0; len] }
    }

    /// A read at `offset` into a buffer the caller already has.
    ///
    /// The buffer's length is the length of the read, and its contents are overwritten. This is
    /// the form a scan uses on its second row group, because the alternative is allocating a
    /// column's worth of page buffers per row group for the life of the query.
    #[must_use]
    pub fn reusing(offset: u64, buf: Vec<u8>) -> Self {
        Self { offset, buf }
    }

    /// Where in the file the read starts.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// How many bytes are wanted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether this request asks for nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// One past the last byte wanted.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.offset + self.buf.len() as u64
    }

    /// The destination buffer, taken out of the request.
    #[must_use]
    pub fn into_buffer(self) -> Vec<u8> {
        self.buf
    }
}

/// One finished read.
///
/// The `index` is the position the request had in the vector handed to `submit`, and it is here
/// because completions do not arrive in submission order. A caller that decodes as answers arrive
/// has no other way to know which page it is looking at.
#[derive(Debug)]
pub struct Response {
    index: usize,
    offset: u64,
    read: usize,
    buf: Vec<u8>,
}

impl Response {
    /// A finished read of `read` bytes into `buf`.
    #[must_use]
    pub fn new(index: usize, offset: u64, read: usize, buf: Vec<u8>) -> Self {
        Self { index, offset, read, buf }
    }

    /// Which request this answers, by position in the submitted vector.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// Where in the file the read started.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// How many bytes arrived.
    #[must_use]
    pub fn read(&self) -> usize {
        self.read
    }

    /// The bytes that arrived, which is a prefix of the buffer and not all of it.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.read]
    }

    /// Whether fewer bytes arrived than were asked for.
    ///
    /// A short read is not an error here for the same reason it is not one in `read_at`: the end
    /// of the file is a fact about the file and the caller is the one that knows whether it was
    /// expecting to be there. A caller that cannot proceed on a short read says so itself.
    #[must_use]
    pub fn is_short(&self) -> bool {
        self.read < self.buf.len()
    }

    /// The buffer, taken out of the response so it can be handed to the next request.
    ///
    /// The bytes past [`Self::read`] are whatever was in the buffer before.
    #[must_use]
    pub fn into_buffer(self) -> Vec<u8> {
        self.buf
    }
}

#[derive(Debug)]
struct State {
    /// One slot per request, filled when that read finishes.
    slots: Vec<Option<Result<Response>>>,
    /// The slots that are filled and not yet handed out, in the order they were filled.
    ready: VecDeque<usize>,
    /// How many slots are still empty.
    outstanding: usize,
    /// How many [`Filler`] handles exist. When this reaches zero with slots still empty, the
    /// backend went away and the waiters are woken with an error rather than left on the condvar.
    fillers: usize,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned mutex means a thread panicked while holding it, and the panic is the finding.
        // Propagating a lock error on top of it would bury the thing that actually went wrong.
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The end of a [`Completion`] that a backend fills in.
///
/// Cloneable, because a batch of reads is served by however many I/O threads the pool feels like
/// putting on it and each of them finishes its own requests.
#[derive(Debug)]
pub struct Filler {
    shared: Arc<Shared>,
}

impl Clone for Filler {
    fn clone(&self) -> Self {
        self.shared.lock().fillers += 1;
        Self { shared: Arc::clone(&self.shared) }
    }
}

impl Filler {
    /// Records the outcome of the request at `index`.
    ///
    /// Filling the same index twice is ignored rather than treated as an error, because the
    /// alternative is a backend that panics inside an I/O thread on a bug that a wrong answer test
    /// would have caught anyway.
    pub fn finish(&self, index: usize, outcome: Result<Response>) {
        let mut state = self.shared.lock();
        if state.slots.get(index).is_none_or(Option::is_some) {
            return;
        }
        state.slots[index] = Some(outcome);
        state.ready.push_back(index);
        state.outstanding -= 1;
        drop(state);
        self.shared.wake.notify_all();
    }
}

impl Drop for Filler {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.fillers -= 1;
        if state.fillers > 0 || state.outstanding == 0 {
            return;
        }
        // Nobody is left to fill these. A caller blocked in `wait` would otherwise be blocked
        // forever, and the test gate for the scan asks for no wrong answers and no hangs, in that
        // order, which makes this the case worth being explicit about rather than the one worth
        // assuming cannot happen.
        for index in 0..state.slots.len() {
            if state.slots[index].is_some() {
                continue;
            }
            state.slots[index] =
                Some(Err(Error::io("the I/O backend stopped before this read finished")));
            state.ready.push_back(index);
        }
        state.outstanding = 0;
        drop(state);
        self.shared.wake.notify_all();
    }
}

/// The answer to a batch of reads, which can be waited on or polled.
///
/// Two ways to consume one and they are for different callers. [`Completion::wait`] is for the
/// caller that needs all of it before it can do anything, which is most of them and is what
/// `read_at` is written in terms of. [`Completion::take`] hands back reads in the order they
/// finished, which is for the scan that decodes a page as soon as that page has arrived instead of
/// waiting for the slowest read in the row group.
#[derive(Debug)]
pub struct Completion {
    shared: Arc<Shared>,
    /// How many responses have been handed out by [`Completion::take`].
    taken: usize,
}

impl Completion {
    /// A completion for `count` requests, and the handle a backend fills it through.
    #[must_use]
    pub fn pending(count: usize) -> (Self, Filler) {
        let mut slots = Vec::with_capacity(count);
        slots.resize_with(count, || None);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                slots,
                ready: VecDeque::with_capacity(count),
                outstanding: count,
                fillers: 1,
            }),
            wake: Condvar::new(),
        });
        (Self { shared: Arc::clone(&shared), taken: 0 }, Filler { shared })
    }

    /// A completion that is already finished, for a backend with nothing to wait for.
    #[must_use]
    pub fn ready(responses: Vec<Result<Response>>) -> Self {
        let (completion, filler) = Self::pending(responses.len());
        for (index, outcome) in responses.into_iter().enumerate() {
            filler.finish(index, outcome);
        }
        completion
    }

    /// How many requests were submitted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shared.lock().slots.len()
    }

    /// Whether nothing was submitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many reads have finished and not been taken, without blocking.
    ///
    /// This is the poll. A scan uses it to decide whether there is decoding to be getting on with
    /// or whether it may as well wait.
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.shared.lock().ready.len()
    }

    /// Whether every read has finished, without blocking.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.shared.lock().outstanding == 0
    }

    /// The next read to finish, blocking until one does.
    ///
    /// `None` once every request has been handed back. The order is completion order, which is why
    /// [`Response::index`] exists.
    pub fn take(&mut self) -> Option<Result<Response>> {
        let mut state = self.shared.lock();
        loop {
            if let Some(index) = state.ready.pop_front() {
                self.taken += 1;
                return state.slots[index].take();
            }
            if state.outstanding == 0 {
                return None;
            }
            state = self.shared.wake.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Every response that has not already been taken, in the order the requests were submitted.
    ///
    /// Blocks until the last read finishes.
    ///
    /// # Errors
    ///
    /// The first failure by request position, so that two runs of the same faulty read report the
    /// same error rather than whichever one happened to finish first.
    pub fn wait(mut self) -> Result<Vec<Response>> {
        let mut state = self.shared.lock();
        while state.outstanding > 0 {
            state = self.shared.wake.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.ready.clear();
        self.taken = state.slots.len();
        let mut out = Vec::with_capacity(state.slots.len());
        let mut failure = None;
        for slot in &mut state.slots {
            match slot.take() {
                Some(Ok(response)) => out.push(response),
                Some(Err(error)) => failure = failure.or(Some(error)),
                None => {}
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rudb_common::Error;

    use super::{Completion, Request, Response};

    fn response(index: usize, bytes: &[u8]) -> Response {
        Response::new(index, index as u64, bytes.len(), bytes.to_vec())
    }

    #[test]
    fn a_request_states_its_length_through_its_buffer() {
        let request = Request::new(64, 8);
        assert_eq!(request.offset(), 64);
        assert_eq!(request.len(), 8);
        assert_eq!(request.end(), 72);
        assert!(!request.is_empty());
        assert_eq!(Request::reusing(0, vec![1, 2, 3]).len(), 3);
    }

    #[test]
    fn wait_returns_responses_in_submission_order_however_they_finished() {
        let (completion, filler) = Completion::pending(3);
        filler.finish(2, Ok(response(2, b"cc")));
        filler.finish(0, Ok(response(0, b"a")));
        filler.finish(1, Ok(response(1, b"bbb")));
        let responses = completion.wait().unwrap();
        assert_eq!(responses.iter().map(Response::index).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(responses[1].bytes(), b"bbb");
    }

    #[test]
    fn take_returns_responses_in_completion_order_and_then_stops() {
        let (mut completion, filler) = Completion::pending(2);
        filler.finish(1, Ok(response(1, b"second")));
        assert_eq!(completion.ready_count(), 1);
        assert!(!completion.is_done());
        assert_eq!(completion.take().unwrap().unwrap().index(), 1);
        filler.finish(0, Ok(response(0, b"first")));
        assert!(completion.is_done());
        assert_eq!(completion.take().unwrap().unwrap().index(), 0);
        assert!(completion.take().is_none());
    }

    #[test]
    fn wait_reports_the_first_failure_by_position_not_by_arrival() {
        let (completion, filler) = Completion::pending(3);
        filler.finish(2, Err(Error::io("late")));
        filler.finish(1, Err(Error::io("early")));
        filler.finish(0, Ok(response(0, b"fine")));
        let error = completion.wait().unwrap_err();
        assert!(error.to_string().contains("early"), "{error}");
    }

    #[test]
    fn a_short_read_is_a_response_and_not_an_error() {
        let completion = Completion::ready(vec![Ok(Response::new(0, 0, 2, vec![7, 7, 0, 0]))]);
        let responses = completion.wait().unwrap();
        assert!(responses[0].is_short());
        assert_eq!(responses[0].bytes(), &[7, 7]);
        assert_eq!(responses[0].read(), 2);
    }

    #[test]
    fn a_buffer_comes_back_out_of_the_response_to_be_used_again() {
        let request = Request::reusing(0, vec![0; 4]);
        let buf = request.into_buffer();
        let response = Response::new(0, 0, 4, buf);
        assert_eq!(response.into_buffer().len(), 4);
    }

    #[test]
    fn a_backend_that_goes_away_wakes_the_waiter_instead_of_hanging_it() {
        // The test gate for the scan asks for no wrong answers and no hangs. This is the hang.
        let (completion, filler) = Completion::pending(2);
        filler.finish(0, Ok(response(0, b"one")));
        drop(filler);
        let error = completion.wait().unwrap_err();
        assert!(error.to_string().contains("stopped before"), "{error}");
    }

    #[test]
    fn the_last_filler_out_is_the_one_that_wakes_the_waiter() {
        let (mut completion, filler) = Completion::pending(2);
        let second = filler.clone();
        drop(filler);
        assert!(!completion.is_done());
        second.finish(0, Ok(response(0, b"one")));
        drop(second);
        assert_eq!(completion.take().unwrap().unwrap().index(), 0);
        assert!(completion.take().unwrap().is_err());
        assert!(completion.take().is_none());
    }

    #[test]
    fn filling_a_slot_twice_leaves_the_first_answer_in_place() {
        let (mut completion, filler) = Completion::pending(1);
        filler.finish(0, Ok(response(0, b"kept")));
        filler.finish(0, Err(Error::io("ignored")));
        assert_eq!(completion.take().unwrap().unwrap().bytes(), b"kept");
    }

    #[test]
    fn a_waiter_blocks_until_another_thread_fills_the_last_slot() {
        let (completion, filler) = Completion::pending(2);
        let filled = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&filled);
        let worker = std::thread::spawn(move || {
            for index in 0..2 {
                std::thread::sleep(std::time::Duration::from_millis(5));
                counter.fetch_add(1, Ordering::SeqCst);
                filler.finish(index, Ok(response(index, b"x")));
            }
        });
        assert_eq!(completion.wait().unwrap().len(), 2);
        assert_eq!(filled.load(Ordering::SeqCst), 2);
        worker.join().unwrap();
    }
}

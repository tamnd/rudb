//! Stopping a query that is already running.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// A reason for a running query to stop, which is either somebody asking or a clock running out.
///
/// Cheap to clone, and a clone shares the flag with the token it came from, so one thread can stop
/// a query another thread is running. The deadline is not shared, because a deadline belongs to one
/// statement and the flag belongs to whoever is allowed to interrupt: see [`Cancel::restart`].
///
/// # How a query notices
///
/// [`Cancel::check`] between chunks, and nowhere else, which is what
/// `spec/engine/10-scheduler.md` section 10.9 asks for. That is a real limit and it is the one worth
/// having: an operator cannot notice halfway through a chunk without a branch in the inner loop over
/// values, and a thousand rows of arithmetic is microseconds, so the response time this gives is
/// already below anything a person can see. What it does not cover is an operator that spends an
/// unbounded time inside one call without pulling a chunk from below it, and there is none, because
/// every loop in `rudb-exec` that can run long runs by pulling chunks.
///
/// # Why the flag is read relaxed
///
/// The only thing a reader does with it is stop, and nothing it reads afterwards has to have been
/// written before the flag was set. A relaxed load on one location is still coherent, so a query
/// that is running when the flag is set sees it on the next chunk or the one after, and a query that
/// has already finished does not care. An acquire load here would cost a fence per chunk to order
/// writes that do not exist.
#[derive(Debug, Clone)]
pub struct Cancel {
    stopped: Arc<AtomicBool>,
    started: Instant,
    limit: Option<Duration>,
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

impl Cancel {
    /// A token nothing stops on its own, for a query with no time limit on it.
    #[must_use]
    pub fn new() -> Self {
        Self { stopped: Arc::new(AtomicBool::new(false)), started: Instant::now(), limit: None }
    }

    /// A token that stops itself after this long.
    #[must_use]
    pub fn after(timeout: Duration) -> Self {
        Self {
            stopped: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
            limit: Some(timeout),
        }
    }

    /// The same flag, a fresh clock, and this time limit.
    ///
    /// What a connection does at the top of each statement. The flag is shared, so a caller holding
    /// the connection's token can still stop the new statement, and the clock starts now, so a
    /// timeout is a limit on this statement rather than on the connection's whole life.
    ///
    /// It clears the flag as well, which means an interrupt that arrives between two statements is
    /// dropped rather than killing the next one. That is the same thing DuckDB does and it is the
    /// right way round: an interrupt is about the query the person is watching, and carrying one
    /// forward would stop a statement nobody asked to stop.
    #[must_use]
    pub fn restart(&self, timeout: Option<Duration>) -> Self {
        self.stopped.store(false, Ordering::Relaxed);
        Self { stopped: Arc::clone(&self.stopped), started: Instant::now(), limit: timeout }
    }

    /// Stop whatever is running on this token.
    ///
    /// Returns immediately. The query stops at its next chunk boundary, so a caller that wants to
    /// know it has stopped waits for the query's own thread to return an error.
    pub fn cancel(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// Whether the query should stop.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.stopped.load(Ordering::Relaxed) || self.expired()
    }

    /// How long this token has been running.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The time limit on it, if it has one.
    #[must_use]
    pub fn limit(&self) -> Option<Duration> {
        self.limit
    }

    /// An error when the query should stop, and nothing when it should keep going.
    ///
    /// The two reasons produce different sentences, because they are different things to a person
    /// reading a log: one of them says somebody pressed a key and the other says the query needed
    /// more time than it was given, and a timeout reported as an interrupt sends whoever reads it
    /// looking for a person who was not there.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorCode::Interrupt`] when the flag is set or the time is up.
    pub fn check(&self) -> Result<()> {
        if self.stopped.load(Ordering::Relaxed) {
            return Err(Error::interrupt("Interrupted!"));
        }
        if self.expired() {
            let limit = self.limit.unwrap_or_default();
            return Err(Error::interrupt(format!(
                "query took longer than the {} millisecond limit it was given",
                limit.as_millis()
            )));
        }
        Ok(())
    }

    /// Whether the clock has run out, which is false when there is no clock.
    ///
    /// The call to read the time is behind the `Some`, so a query with no limit on it does not read
    /// the clock once per chunk for an answer that cannot change.
    fn expired(&self) -> bool {
        self.limit.is_some_and(|limit| self.started.elapsed() >= limit)
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::Cancel;

    #[test]
    fn a_fresh_token_lets_the_query_run() {
        let cancel = Cancel::new();
        assert!(!cancel.is_cancelled());
        assert!(cancel.check().is_ok());
        assert_eq!(cancel.limit(), None);
    }

    #[test]
    fn cancelling_one_handle_stops_the_query_holding_another() {
        let cancel = Cancel::new();
        let other = cancel.clone();
        other.cancel();
        assert!(cancel.is_cancelled());
        let error = cancel.check().expect_err("it was cancelled");
        assert_eq!(error.code().duckdb_name(), "Interrupt Error");
        assert_eq!(error.message(), "Interrupted!");
    }

    #[test]
    fn a_token_another_thread_cancels_is_seen_by_the_one_running_the_query() {
        let cancel = Cancel::new();
        let other = cancel.clone();
        let stopper = thread::spawn(move || other.cancel());
        stopper.join().expect("the thread ran");
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn a_time_limit_runs_out_on_its_own() {
        let cancel = Cancel::after(Duration::from_millis(1));
        assert_eq!(cancel.limit(), Some(Duration::from_millis(1)));
        thread::sleep(Duration::from_millis(5));
        assert!(cancel.is_cancelled());
        let error = cancel.check().expect_err("the time is up");
        assert_eq!(error.code().duckdb_name(), "Interrupt Error");
        assert!(error.message().contains("longer than the 1 millisecond limit"), "{error}");
    }

    #[test]
    fn a_timeout_and_an_interrupt_say_different_things() {
        // A timeout reported as an interrupt sends whoever reads the log looking for a person who
        // was not there.
        let interrupted = Cancel::new();
        interrupted.cancel();
        let timed_out = Cancel::after(Duration::from_millis(0));
        assert_ne!(
            interrupted.check().expect_err("cancelled").message(),
            timed_out.check().expect_err("timed out").message()
        );
    }

    #[test]
    fn restarting_shares_the_flag_and_starts_the_clock_again() {
        let connection = Cancel::new();
        let statement = connection.restart(Some(Duration::from_secs(60)));
        assert!(!statement.is_cancelled());
        connection.cancel();
        assert!(statement.is_cancelled(), "the flag is shared");
    }

    #[test]
    fn an_interrupt_between_two_statements_does_not_stop_the_next_one() {
        let connection = Cancel::new();
        connection.cancel();
        let statement = connection.restart(None);
        assert!(!statement.is_cancelled());
        assert!(!connection.is_cancelled(), "and the connection is usable again");
    }

    #[test]
    fn a_limit_is_on_the_statement_rather_than_on_the_connection() {
        let connection = Cancel::after(Duration::from_millis(1));
        thread::sleep(Duration::from_millis(5));
        assert!(connection.is_cancelled());
        let statement = connection.restart(Some(Duration::from_secs(60)));
        assert!(!statement.is_cancelled(), "the clock started again");
    }
}

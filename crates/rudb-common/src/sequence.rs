//! The counter behind a `CREATE SEQUENCE`, and the registry the kernels find one in by number.
//!
//! The pin keeps a sequence's state on its catalog entry and changes it in place, so a value handed
//! out by `nextval` stays handed out when the transaction that asked for it rolls back, and
//! `currval` reads the last value the entry gave to anyone rather than one kept per session. The
//! counter here is shared the same way. The catalog holds it behind an [`Arc`], the copy of the
//! catalog a transaction keeps to roll back to holds the same one, and the binder turns
//! `nextval('seq')` into a call that carries the counter's number rather than its name, which is
//! what lets a kernel that knows nothing about catalogs reach it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use crate::{Error, Result};

/// What a `CREATE SEQUENCE` settled, with every default already filled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// Added to the counter by every `nextval`, and never zero.
    pub increment: i64,
    /// The smallest value the sequence hands out.
    pub min: i64,
    /// The largest value the sequence hands out.
    pub max: i64,
    /// The first value handed out.
    pub start: i64,
    /// Whether running past one end starts again from the other rather than failing.
    pub cycle: bool,
}

/// The part of a counter that changes.
#[derive(Debug, Clone, Copy)]
struct State {
    /// The value the next `nextval` hands out.
    counter: i64,
    /// The value the last `nextval` or `setval` handed out.
    last: Option<i64>,
    /// How many values have been handed out.
    uses: u64,
    /// How many of the uses the file and the log of the database already make durable. A use past
    /// this needs a log record before the commit that wrote it is durable, see [`Counter::ahead`].
    durable: u64,
}

/// How many values one log record of a counter covers, as `SEQ_LOG_VALS` does on the pin. After a
/// crash the counter starts after the values the last record covered, so up to this many values
/// are never handed out.
pub const PREFETCH: i64 = 32;

/// One sequence's counter.
#[derive(Debug)]
pub struct Counter {
    /// The number the registry knows it by.
    id: u64,
    /// The sequence's name, for the messages.
    name: String,
    /// What it was created with.
    options: Options,
    /// Where it has got to.
    state: Mutex<State>,
}

/// Every counter alive, by number.
static COUNTERS: LazyLock<Mutex<HashMap<u64, Weak<Counter>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The number the next counter gets.
static NEXT: AtomicU64 = AtomicU64::new(1);

impl Counter {
    /// A new counter at its start value, registered so [`lookup`] finds it.
    #[must_use]
    pub fn register(name: &str, options: Options) -> Arc<Self> {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let counter = Arc::new(Self {
            id,
            name: name.to_owned(),
            options,
            state: Mutex::new(State { counter: options.start, last: None, uses: 0, durable: 0 }),
        });
        let mut all = COUNTERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        all.retain(|_, counter| counter.strong_count() > 0);
        all.insert(id, Arc::downgrade(&counter));
        counter
    }

    /// A counter that a database file kept, at the place it had got to when the file was written.
    #[must_use]
    pub fn resume(name: &str, options: Options, counter: i64, uses: u64) -> Arc<Self> {
        let resumed = Self::register(name, options);
        *resumed.lock() = State { counter, last: None, uses, durable: uses };
        resumed
    }

    /// The value the next `nextval` hands out and how many values have been handed out, read
    /// together.
    #[must_use]
    pub fn snapshot(&self) -> (i64, u64) {
        let state = self.lock();
        (state.counter, state.uses)
    }

    /// Says that a checkpoint wrote the counter down after `uses` values, so that is what the
    /// file makes durable. The log records before the checkpoint are not read again.
    pub fn covered(&self, uses: u64) {
        self.lock().durable = uses;
    }

    /// Whether the counter handed out a value the last log record does not cover, so the next
    /// commit stages a record for it, see [`Counter::ahead`].
    pub fn moved(&self) -> bool {
        let state = self.lock();
        state.uses > state.durable
    }

    /// The `setval` that a log record must carry so that a replay of the log starts the counter
    /// after every value it has handed out, or `None` when the last record still covers them.
    ///
    /// The record goes past the counter by [`PREFETCH`] values, so most commits do not need one.
    /// The answer is the value and whether `setval` hands it out as called.
    pub fn ahead(&self) -> Option<(i64, bool)> {
        let mut state = self.lock();
        if state.uses <= state.durable {
            return None;
        }
        let Options { increment, min, max, cycle, .. } = self.options;
        let counter = state.counter;
        let target = counter.saturating_add(increment.saturating_mul(PREFETCH));
        let (value, called, covers) = if cycle {
            (counter, false, 0)
        } else if increment > 0 && counter > max {
            (max, true, 0)
        } else if increment < 0 && counter < min {
            (min, true, 0)
        } else if increment > 0 {
            let value = target.min(max);
            (value, false, (value - counter) / increment)
        } else {
            let value = target.max(min);
            (value, false, (counter - value) / -increment)
        };
        state.durable = state.uses + covers.unsigned_abs();
        Some((value, called))
    }

    /// The number [`lookup`] finds this counter by.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// What it was created with.
    #[must_use]
    pub fn options(&self) -> Options {
        self.options
    }

    /// The value the next `nextval` hands out, which is what the pin writes as `START` when it
    /// prints the sequence back.
    #[must_use]
    pub fn counter(&self) -> i64 {
        self.lock().counter
    }

    /// The value last handed out, if any has been.
    #[must_use]
    pub fn last(&self) -> Option<i64> {
        self.lock().last
    }

    /// How many values have been handed out.
    #[must_use]
    pub fn uses(&self) -> u64 {
        self.lock().uses
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `nextval`: the counter's value, moving the counter on by the increment.
    ///
    /// # Errors
    ///
    /// A sequence without `CYCLE` that has run past an end. The counter has moved all the same,
    /// which is what the pin does too.
    pub fn next(&self) -> Result<i64> {
        let mut state = self.lock();
        self.advance(&mut state)
    }

    fn advance(&self, state: &mut State) -> Result<i64> {
        let Options { increment, min, max, cycle, .. } = self.options;
        let result = state.counter;
        let (moved, overflow) = state.counter.overflowing_add(increment);
        if !overflow {
            state.counter = moved;
        }
        if cycle {
            if overflow {
                state.counter = if increment < 0 { max } else { min };
            } else if state.counter < min {
                state.counter = max;
            } else if state.counter > max {
                state.counter = min;
            }
        } else {
            if result < min || (overflow && increment < 0) {
                return Err(Error::sequence(format!(
                    "nextval: reached minimum value of sequence \"{}\" ({min})",
                    self.name
                )));
            }
            if result > max || overflow {
                return Err(Error::sequence(format!(
                    "nextval: reached maximum value of sequence \"\"{}\"\" ({max})",
                    self.name
                )));
            }
        }
        state.last = Some(result);
        state.uses += 1;
        Ok(result)
    }

    /// `currval`: the value last handed out.
    ///
    /// # Errors
    ///
    /// None has been yet.
    pub fn current(&self) -> Result<i64> {
        self.lock()
            .last
            .ok_or_else(|| Error::sequence("currval: sequence is not yet defined in this session"))
    }

    /// `setval`: puts the counter at `value`, and with `called` hands that value out as `nextval`
    /// would, so the next call gives the one after it.
    ///
    /// # Errors
    ///
    /// A value outside the sequence's bounds.
    pub fn set(&self, value: i64, called: bool) -> Result<i64> {
        let Options { min, max, .. } = self.options;
        if value < min || value > max {
            return Err(Error::sequence(format!(
                "setval: value {value} is out of bounds for sequence \"{}\" ({min}..{max})",
                self.name
            )));
        }
        let mut state = self.lock();
        state.counter = value;
        // The values the last log record covered say nothing about where the counter is now.
        state.durable = 0;
        if !called {
            state.uses += 1;
            return Ok(value);
        }
        self.advance(&mut state)
    }
}

/// The counter with this number.
///
/// # Errors
///
/// None is alive with it, which means the sequence was dropped after the statement was bound.
pub fn lookup(id: u64) -> Result<Arc<Counter>> {
    let all = COUNTERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    all.get(&id)
        .and_then(Weak::upgrade)
        .ok_or_else(|| Error::catalog("The sequence this statement uses no longer exists"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(increment: i64, min: i64, max: i64, start: i64, cycle: bool) -> Options {
        Options { increment, min, max, start, cycle }
    }

    #[test]
    fn a_counter_counts_and_stops_at_its_maximum() {
        let seq = Counter::register("seq", options(1, 1, 2, 1, false));
        assert_eq!(seq.next().unwrap(), 1);
        assert_eq!(seq.next().unwrap(), 2);
        let error = seq.next().unwrap_err().to_string();
        assert!(error.contains("reached maximum value of sequence \"\"seq\"\" (2)"), "{error}");
        assert_eq!(seq.current().unwrap(), 2);
        assert!(Arc::ptr_eq(&lookup(seq.id()).unwrap(), &seq));
    }

    #[test]
    fn a_cycle_starts_again_from_the_other_end() {
        let seq = Counter::register("seq", options(-1, 1, 3, 2, true));
        let got: Vec<i64> = (0..4).map(|_| seq.next().unwrap()).collect();
        assert_eq!(got, [2, 1, 3, 2]);
    }

    #[test]
    fn setval_hands_out_the_value_when_called() {
        let seq = Counter::register("seq", options(1, 1, i64::MAX, 1, false));
        assert!(seq.current().is_err());
        assert_eq!(seq.set(10, true).unwrap(), 10);
        assert_eq!(seq.next().unwrap(), 11);
        assert_eq!(seq.set(5, false).unwrap(), 5);
        assert_eq!(seq.current().unwrap(), 11);
        assert_eq!(seq.next().unwrap(), 5);
        assert!(seq.set(0, true).is_err());
    }

    #[test]
    fn a_log_record_covers_the_values_ahead_of_the_counter() {
        let seq = Counter::register("seq", options(1, 1, 40, 1, false));
        assert_eq!(seq.ahead(), None, "nothing is handed out yet");
        assert_eq!(seq.next().unwrap(), 1);
        assert_eq!(seq.ahead(), Some((34, false)), "the record covers 2 to 33");
        for _ in 0..32 {
            seq.next().unwrap();
        }
        assert_eq!(seq.ahead(), None, "the record still covers 33");
        assert_eq!(seq.next().unwrap(), 34);
        assert_eq!(seq.ahead(), Some((40, false)), "the record stops at the maximum");
        let resumed = Counter::resume("seq", seq.options(), 7, 6);
        assert_eq!(resumed.next().unwrap(), 7);
        resumed.covered(7);
        assert_eq!(resumed.ahead(), None);
        resumed.set(3, false).unwrap();
        assert_eq!(resumed.ahead(), Some((35, false)));
    }
}

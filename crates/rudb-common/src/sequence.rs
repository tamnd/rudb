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
}

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
            state: Mutex::new(State { counter: options.start, last: None, uses: 0 }),
        });
        let mut all = COUNTERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        all.retain(|_, counter| counter.strong_count() > 0);
        all.insert(id, Arc::downgrade(&counter));
        counter
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
}

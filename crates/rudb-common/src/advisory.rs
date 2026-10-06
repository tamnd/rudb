//! The advisory locks of PostgreSQL, which `pg_advisory_lock` and the functions with it take and
//! release.
//!
//! The table is one for the process, as the lock table of PostgreSQL is one for the server. A
//! session is known by its backend number, which the server gives to the engine in
//! [`crate::session::Postgres`] and the binder gives to the kernel as an argument. A kernel knows
//! nothing about sessions, so this is the place where both of them can see the locks, for the
//! reason that [`crate::sequence`] is here.
//!
//! A lock is on a key of the database and two 32-bit numbers, and the form of the call is part of
//! the key, so `pg_advisory_lock(1)` and `pg_advisory_lock(0, 1)` are two locks, as in
//! PostgreSQL. A session holds a lock in the exclusive mode or in the share mode, at the session
//! level or at the transaction level, and it can hold it more than once. The locks of one session
//! never block the same session.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::{Cancel, Error, ErrorCode, Result, SqlState};

/// How long a session waits for a lock before it looks for a deadlock, as `deadlock_timeout` of
/// PostgreSQL at its default.
const DEADLOCK_TIMEOUT: Duration = Duration::from_secs(1);

/// How often a waiting session looks at its cancel flag.
const POLL: Duration = Duration::from_millis(20);

/// The key of an advisory lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    /// The OID of the database of the session.
    pub database: u32,
    /// The high 32 bits of the `bigint` key, or the first `integer` key.
    pub first: u32,
    /// The low 32 bits of the `bigint` key, or the second `integer` key.
    pub second: u32,
    /// True for the form with two `integer` keys.
    pub pair: bool,
}

impl Key {
    /// The key of `pg_advisory_lock(key bigint)`.
    #[must_use]
    pub fn single(database: u32, key: i64) -> Key {
        let key = key as u64;
        Key { database, first: (key >> 32) as u32, second: key as u32, pair: false }
    }

    /// The key of `pg_advisory_lock(key1 integer, key2 integer)`.
    #[must_use]
    pub fn pair(database: u32, first: i32, second: i32) -> Key {
        Key { database, first: first as u32, second: second as u32, pair: true }
    }

    /// The key as `pg_locks` and the deadlock message of PostgreSQL write it.
    fn tag(&self) -> String {
        let kind = if self.pair { 2 } else { 1 };
        format!("[{},{},{},{kind}]", self.database, self.first, self.second)
    }
}

/// The mode of a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Blocks every lock of another session on the key.
    Exclusive,
    /// Blocks only an exclusive lock of another session on the key.
    Share,
}

impl Mode {
    /// The name of the mode in the messages of PostgreSQL.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Mode::Exclusive => "ExclusiveLock",
            Mode::Share => "ShareLock",
        }
    }
}

/// The level of a lock: until the session releases it, or until the end of the transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Released by `pg_advisory_unlock` or at the end of the session.
    Session,
    /// Released at the end of the transaction.
    Transaction,
}

/// A request for a lock.
#[derive(Debug, Clone, Copy)]
pub struct Request {
    /// The backend number of the session.
    pub owner: i32,
    pub key: Key,
    pub mode: Mode,
    pub level: Level,
}

/// The locks that one session holds on one key in one mode.
#[derive(Debug, Clone, Copy)]
struct Hold {
    owner: i32,
    mode: Mode,
    /// How many times the session holds it at the session level.
    session: u32,
    /// How many times the session holds it at the transaction level.
    transaction: u32,
}

#[derive(Default)]
struct Table {
    held: HashMap<Key, Vec<Hold>>,
    /// The request that each waiting session waits for.
    waiting: HashMap<i32, (Key, Mode)>,
    /// The warnings for each session that the server did not send yet.
    warnings: HashMap<i32, Vec<String>>,
    /// The cancel flag of each session, which stops a wait.
    cancels: HashMap<i32, Cancel>,
}

impl Table {
    /// The sessions that hold a lock on `key` that blocks a lock of `owner` in `mode`.
    fn blockers(&self, key: &Key, owner: i32, mode: Mode) -> Vec<i32> {
        let Some(holds) = self.held.get(key) else {
            return Vec::new();
        };
        let mut found: Vec<i32> = holds
            .iter()
            .filter(|hold| {
                hold.owner != owner && (mode == Mode::Exclusive || hold.mode == Mode::Exclusive)
            })
            .map(|hold| hold.owner)
            .collect();
        found.dedup();
        found
    }

    fn grant(&mut self, request: &Request) {
        let holds = self.held.entry(request.key).or_default();
        let at = match holds
            .iter()
            .position(|hold| hold.owner == request.owner && hold.mode == request.mode)
        {
            Some(at) => at,
            None => {
                let hold =
                    Hold { owner: request.owner, mode: request.mode, session: 0, transaction: 0 };
                holds.push(hold);
                holds.len() - 1
            }
        };
        match request.level {
            Level::Session => holds[at].session += 1,
            Level::Transaction => {
                holds[at].transaction += 1;
                TRANSACTION_HOLDS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Removes the holds that `keep` gives false for, after it changed them.
    fn release(&mut self, mut keep: impl FnMut(&mut Hold) -> bool) -> bool {
        let mut freed = false;
        self.held.retain(|_, holds| {
            holds.retain_mut(|hold| {
                let kept = keep(hold);
                freed |= !kept;
                kept
            });
            !holds.is_empty()
        });
        freed
    }

    /// The chain of waits from `owner` back to `owner`, if there is one.
    fn deadlock(&self, owner: i32) -> Option<Vec<(i32, Key, Mode, i32)>> {
        let mut path = Vec::new();
        let mut seen = HashSet::new();
        self.cycle(owner, owner, &mut path, &mut seen).then_some(path)
    }

    fn cycle(
        &self,
        start: i32,
        at: i32,
        path: &mut Vec<(i32, Key, Mode, i32)>,
        seen: &mut HashSet<i32>,
    ) -> bool {
        let Some(&(key, mode)) = self.waiting.get(&at) else {
            return false;
        };
        if !seen.insert(at) {
            return false;
        }
        for blocker in self.blockers(&key, at, mode) {
            path.push((at, key, mode, blocker));
            if blocker == start || self.cycle(start, blocker, path, seen) {
                return true;
            }
            path.pop();
        }
        false
    }
}

struct Locks {
    table: Mutex<Table>,
    /// Signaled when a lock is released.
    freed: Condvar,
}

/// How many locks of the transaction level all the sessions hold, and how many warnings wait for
/// the server. The server looks at both after each statement, and with no such lock and no such
/// warning it does not take the mutex of the table.
static TRANSACTION_HOLDS: AtomicUsize = AtomicUsize::new(0);
static WARNINGS: AtomicUsize = AtomicUsize::new(0);

static LOCKS: LazyLock<Locks> =
    LazyLock::new(|| Locks { table: Mutex::new(Table::default()), freed: Condvar::new() });

fn table() -> MutexGuard<'static, Table> {
    LOCKS.table.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Gives the cancel flag of a session, which stops the wait of `pg_advisory_lock` as it stops a
/// statement.
pub fn register(owner: i32, cancel: Cancel) {
    table().cancels.insert(owner, cancel);
}

/// Takes a lock, and waits until no other session blocks it.
///
/// # Errors
///
/// `55P03` when the wait is longer than `timeout`, `40P01` when the wait is part of a deadlock,
/// and the interrupt error when the statement is cancelled.
pub fn lock(request: &Request, timeout: Option<Duration>) -> Result<()> {
    let started = Instant::now();
    let mut table = table();
    let mut looked = false;
    loop {
        if table.blockers(&request.key, request.owner, request.mode).is_empty() {
            table.waiting.remove(&request.owner);
            table.grant(request);
            return Ok(());
        }
        table.waiting.insert(request.owner, (request.key, request.mode));
        let stop = |table: &mut Table, error: Error| {
            table.waiting.remove(&request.owner);
            Err(error)
        };
        if let Some(cancel) = table.cancels.get(&request.owner)
            && let Err(error) = cancel.check()
        {
            return stop(&mut table, error);
        }
        let waited = started.elapsed();
        if timeout.is_some_and(|timeout| waited >= timeout) {
            let error =
                Error::new(ErrorCode::Transaction, "canceling statement due to lock timeout")
                    .state(SqlState::LOCK_NOT_AVAILABLE);
            return stop(&mut table, error);
        }
        if !looked && waited >= DEADLOCK_TIMEOUT {
            looked = true;
            if let Some(path) = table.deadlock(request.owner) {
                return stop(&mut table, deadlock(&path));
            }
        }
        let mut wait = POLL;
        if let Some(timeout) = timeout {
            wait = wait.min(timeout.saturating_sub(waited));
        }
        table = match LOCKS.freed.wait_timeout(table, wait) {
            Ok((table, _)) => table,
            Err(poisoned) => poisoned.into_inner().0,
        };
    }
}

/// The error of a deadlock, with the waits in the detail as PostgreSQL writes them.
fn deadlock(path: &[(i32, Key, Mode, i32)]) -> Error {
    let detail = path
        .iter()
        .map(|(waiter, key, mode, blocker)| {
            format!(
                "Process {waiter} waits for {} on advisory lock {}; blocked by process {blocker}.",
                mode.name(),
                key.tag()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Error::new(ErrorCode::Transaction, "deadlock detected")
        .state(SqlState::T_R_DEADLOCK_DETECTED)
        .detail(detail)
        .hint("See server log for query details.")
}

/// Takes a lock when no other session blocks it, and gives whether it did.
pub fn try_lock(request: &Request) -> bool {
    let mut table = table();
    if !table.blockers(&request.key, request.owner, request.mode).is_empty() {
        return false;
    }
    table.grant(request);
    true
}

/// Releases a lock that the session holds at the session level once, and gives whether it held
/// one. A session that does not hold the lock gets the warning of PostgreSQL.
pub fn unlock(owner: i32, key: &Key, mode: Mode) -> bool {
    let mut table = table();
    let held = table.held.get_mut(key).and_then(|holds| {
        holds.iter_mut().find(|hold| hold.owner == owner && hold.mode == mode && hold.session > 0)
    });
    let Some(hold) = held else {
        let warning = format!("you don't own a lock of type {}", mode.name());
        table.warnings.entry(owner).or_default().push(warning);
        WARNINGS.fetch_add(1, Ordering::Relaxed);
        return false;
    };
    hold.session -= 1;
    if hold.session == 0 && hold.transaction == 0 {
        table.release(|hold| hold.session > 0 || hold.transaction > 0);
        LOCKS.freed.notify_all();
    }
    true
}

/// Releases the locks that the session holds at the session level, as `pg_advisory_unlock_all`.
pub fn unlock_all(owner: i32) {
    let mut table = table();
    let freed = table.release(|hold| {
        if hold.owner == owner {
            hold.session = 0;
        }
        hold.session > 0 || hold.transaction > 0
    });
    if freed {
        LOCKS.freed.notify_all();
    }
}

/// Releases the locks that the session holds at the transaction level, at the end of its
/// transaction.
pub fn end_transaction(owner: i32) {
    if TRANSACTION_HOLDS.load(Ordering::Relaxed) == 0 {
        return;
    }
    let mut table = table();
    let mut released = 0;
    let freed = table.release(|hold| {
        if hold.owner == owner {
            released += hold.transaction as usize;
            hold.transaction = 0;
        }
        hold.session > 0 || hold.transaction > 0
    });
    TRANSACTION_HOLDS.fetch_sub(released, Ordering::Relaxed);
    if freed {
        LOCKS.freed.notify_all();
    }
}

/// Releases every lock of the session and forgets it, at the end of the session.
pub fn end_session(owner: i32) {
    let mut table = table();
    let mut released = 0;
    let freed = table.release(|hold| {
        if hold.owner == owner {
            released += hold.transaction as usize;
        }
        hold.owner != owner
    });
    TRANSACTION_HOLDS.fetch_sub(released, Ordering::Relaxed);
    table.waiting.remove(&owner);
    if let Some(warnings) = table.warnings.remove(&owner) {
        WARNINGS.fetch_sub(warnings.len(), Ordering::Relaxed);
    }
    table.cancels.remove(&owner);
    if freed {
        LOCKS.freed.notify_all();
    }
}

/// Takes the warnings of the session that the server did not send yet.
#[must_use]
pub fn warnings(owner: i32) -> Vec<String> {
    if WARNINGS.load(Ordering::Relaxed) == 0 {
        return Vec::new();
    }
    let warnings = table().warnings.remove(&owner).unwrap_or_default();
    WARNINGS.fetch_sub(warnings.len(), Ordering::Relaxed);
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    // The backend numbers of the tests are their own, because the table is one for the process.
    fn request(owner: i32, key: i64, mode: Mode, level: Level) -> Request {
        Request { owner, key: Key::single(1, key), mode, level }
    }

    #[test]
    fn a_lock_blocks_another_session_and_not_its_own() {
        let a = request(-10, 7, Mode::Exclusive, Level::Session);
        let b = Request { owner: -11, ..a };
        assert!(try_lock(&a));
        assert!(try_lock(&a));
        assert!(!try_lock(&b));
        assert!(unlock(-10, &a.key, Mode::Exclusive));
        assert!(!try_lock(&b));
        assert!(unlock(-10, &a.key, Mode::Exclusive));
        assert!(try_lock(&b));
        assert!(!unlock(-10, &a.key, Mode::Exclusive));
        assert_eq!(warnings(-10), ["you don't own a lock of type ExclusiveLock"]);
        end_session(-11);
        assert!(try_lock(&a));
        end_session(-10);
    }

    #[test]
    fn share_locks_go_together_and_block_an_exclusive_lock() {
        let a = request(-20, 8, Mode::Share, Level::Session);
        let b = Request { owner: -21, ..a };
        let c = Request { owner: -22, mode: Mode::Exclusive, ..a };
        assert!(try_lock(&a));
        assert!(try_lock(&b));
        assert!(!try_lock(&c));
        unlock_all(-20);
        unlock_all(-21);
        assert!(try_lock(&c));
        end_session(-22);
    }

    #[test]
    fn the_two_forms_of_the_key_are_two_locks() {
        let single =
            Request { key: Key::single(1, 1), ..request(-30, 0, Mode::Exclusive, Level::Session) };
        let pair = Request { owner: -31, key: Key::pair(1, 0, 1), ..single };
        let other = Request { owner: -31, key: Key::single(2, 1), ..single };
        assert!(try_lock(&single));
        assert!(try_lock(&pair));
        assert!(try_lock(&other));
        end_session(-30);
        end_session(-31);
    }

    #[test]
    fn a_transaction_lock_goes_at_the_end_of_the_transaction() {
        let a = request(-40, 9, Mode::Exclusive, Level::Transaction);
        let b = Request { owner: -41, level: Level::Session, ..a };
        assert!(try_lock(&a));
        assert!(!unlock(-40, &a.key, Mode::Exclusive));
        assert!(!try_lock(&b));
        end_transaction(-40);
        assert!(try_lock(&b));
        end_session(-40);
        end_session(-41);
    }

    #[test]
    fn a_wait_ends_at_the_release_at_the_timeout_and_at_a_deadlock() {
        let a = request(-50, 10, Mode::Exclusive, Level::Session);
        let b = Request { owner: -51, ..a };
        assert!(try_lock(&a));
        let error = lock(&b, Some(Duration::from_millis(30))).unwrap_err();
        assert_eq!(error.sqlstate(), Some(SqlState::LOCK_NOT_AVAILABLE));
        let waiter = std::thread::spawn(move || lock(&b, None));
        std::thread::sleep(Duration::from_millis(50));
        unlock_all(-50);
        waiter.join().unwrap().unwrap();
        // -51 holds 10 and -50 holds 11, and each one waits for the lock of the other. The session
        // that waits first looks for a deadlock first, and it gets the error.
        let c = request(-50, 11, Mode::Exclusive, Level::Session);
        assert!(try_lock(&c));
        let d = Request { owner: -51, ..c };
        let first = std::thread::spawn(move || lock(&d, None));
        std::thread::sleep(Duration::from_millis(50));
        let second = std::thread::spawn(move || lock(&a, None));
        let error = first.join().unwrap().unwrap_err();
        assert_eq!(error.sqlstate(), Some(SqlState::T_R_DEADLOCK_DETECTED));
        end_session(-51);
        second.join().unwrap().unwrap();
        end_session(-50);
        end_session(-51);
    }
}

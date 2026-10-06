//! The advisory lock functions of PostgreSQL, such as `pg_advisory_lock`, which take and release
//! the locks of [`rudb_common::advisory`].
//!
//! The binder makes these calls only in a PostgreSQL session. It puts three constants before the
//! keys: the backend number of the session, the OID of its database and its `lock_timeout` in
//! milliseconds, with 0 for no limit. A call takes or releases a lock once for each row, so these
//! are answered before the constant path in [`crate::scalar::call`], as the sequence functions
//! are. The functions are strict, so a null key gives null and takes no lock.

use std::time::Duration;

use rudb_common::advisory::{self, Key, Level, Mode, Request};
use rudb_common::{LogicalType, Result, Value};
use rudb_vector::Vector;

/// What a function does.
#[derive(Clone, Copy)]
enum Action {
    /// Waits for the lock.
    Lock(Mode, Level),
    /// Takes the lock if it is free, and gives whether it did.
    Try(Mode, Level),
    /// Releases a lock of the session level, and gives whether the session held it.
    Unlock(Mode),
    /// Releases all the locks of the session level.
    UnlockAll,
}

/// The names of the functions.
pub const FUNCTIONS: [&str; 11] = [
    "pg_advisory_lock",
    "pg_advisory_lock_shared",
    "pg_advisory_unlock",
    "pg_advisory_unlock_all",
    "pg_advisory_unlock_shared",
    "pg_advisory_xact_lock",
    "pg_advisory_xact_lock_shared",
    "pg_try_advisory_lock",
    "pg_try_advisory_lock_shared",
    "pg_try_advisory_xact_lock",
    "pg_try_advisory_xact_lock_shared",
];

fn action(name: &str) -> Option<Action> {
    use Level::{Session, Transaction};
    use Mode::{Exclusive, Share};
    Some(match name {
        "pg_advisory_lock" => Action::Lock(Exclusive, Session),
        "pg_advisory_lock_shared" => Action::Lock(Share, Session),
        "pg_advisory_xact_lock" => Action::Lock(Exclusive, Transaction),
        "pg_advisory_xact_lock_shared" => Action::Lock(Share, Transaction),
        "pg_try_advisory_lock" => Action::Try(Exclusive, Session),
        "pg_try_advisory_lock_shared" => Action::Try(Share, Session),
        "pg_try_advisory_xact_lock" => Action::Try(Exclusive, Transaction),
        "pg_try_advisory_xact_lock_shared" => Action::Try(Share, Transaction),
        "pg_advisory_unlock" => Action::Unlock(Exclusive),
        "pg_advisory_unlock_shared" => Action::Unlock(Share),
        "pg_advisory_unlock_all" => Action::UnlockAll,
        _ => return None,
    })
}

/// Whether the function gives `void`, which goes to the client as an empty value.
#[must_use]
pub fn gives_void(name: &str) -> bool {
    matches!(action(name), Some(Action::Lock(..) | Action::UnlockAll))
}

/// The answer for a call to one of the functions, or `None` for any other name.
pub(crate) fn call<V: AsRef<Vector>>(
    name: &str,
    args: &[V],
    rows: usize,
) -> Result<Option<Vector>> {
    let Some(action) = action(name) else {
        return Ok(None);
    };
    let constant = |at: usize| -> Result<i64> {
        Ok(args
            .get(at)
            .map(|arg| arg.as_ref().try_value_at(0))
            .transpose()?
            .and_then(|v| v.as_i64())
            .unwrap_or(0))
    };
    let owner = i32::try_from(constant(0)?).unwrap_or(0);
    let database = u32::try_from(constant(1)?).unwrap_or(0);
    let timeout = u64::try_from(constant(2)?).ok().filter(|&ms| ms > 0).map(Duration::from_millis);
    let keys = &args[3.min(args.len())..];
    let returns = match action {
        Action::Try(..) | Action::Unlock(_) => LogicalType::Boolean,
        Action::Lock(..) | Action::UnlockAll => LogicalType::Varchar,
    };
    let void = || Value::Varchar(String::new());
    let mut values = Vec::with_capacity(rows);
    for row in 0..rows {
        let mode = match action {
            Action::UnlockAll => {
                advisory::unlock_all(owner);
                values.push(void());
                continue;
            }
            Action::Lock(mode, _) | Action::Try(mode, _) | Action::Unlock(mode) => mode,
        };
        let key = match keys {
            [key] => key.as_ref().try_value_at(row)?.as_i64().map(|key| Key::single(database, key)),
            [first, second] => {
                let first = first.as_ref().try_value_at(row)?.as_i64();
                let second = second.as_ref().try_value_at(row)?.as_i64();
                first
                    .zip(second)
                    .map(|(first, second)| Key::pair(database, first as i32, second as i32))
            }
            _ => None,
        };
        let Some(key) = key else {
            values.push(Value::Null);
            continue;
        };
        values.push(match action {
            Action::Lock(_, level) => {
                advisory::lock(&Request { owner, key, mode, level }, timeout)?;
                void()
            }
            Action::Try(_, level) => {
                Value::Boolean(advisory::try_lock(&Request { owner, key, mode, level }))
            }
            Action::Unlock(_) | Action::UnlockAll => {
                Value::Boolean(advisory::unlock(owner, &key, mode))
            }
        });
    }
    Vector::from_values(returns, &values).map(Some)
}

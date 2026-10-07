//! `pg_sleep(seconds)` of a PostgreSQL session, which waits and gives `void`.
//!
//! The binder adds the backend number of the session before the seconds. The wait stops when the
//! statement is cancelled, the same way the wait for an advisory lock stops.

use std::time::{Duration, Instant};

use rudb_common::{LogicalType, Result, Value, advisory};
use rudb_vector::Vector;

/// The longest wait between two looks at the cancel flag.
const STEP: Duration = Duration::from_millis(10);

/// The answer for a call to `pg_sleep`, or `None` for any other name.
pub(crate) fn call<V: AsRef<Vector>>(
    name: &str,
    args: &[V],
    rows: usize,
) -> Result<Option<Vector>> {
    let ("pg_sleep", [owner, seconds]) = (name, args) else {
        return Ok(None);
    };
    let owner = owner.as_ref().try_value_at(0)?.as_i64().unwrap_or(0);
    let cancel = advisory::cancel_of(i32::try_from(owner).unwrap_or(0));
    // row at a time: each row waits its own time, one after the other, as in PostgreSQL.
    for row in 0..rows {
        // A null, a negative and a NaN wait do not wait, as in PostgreSQL.
        let Value::Double(seconds) = seconds.as_ref().try_value_at(row)? else {
            continue;
        };
        let Ok(wait) = Duration::try_from_secs_f64(seconds) else {
            continue;
        };
        let started = Instant::now();
        loop {
            if let Some(cancel) = &cancel {
                cancel.check()?;
            }
            let left = wait.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(STEP));
        }
    }
    let values = vec![Value::Varchar(String::new()); rows];
    Vector::from_values(LogicalType::Varchar, &values).map(Some)
}

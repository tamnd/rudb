//! `nextval`, `currval` and `setval`, which move a sequence's counter once per row.
//!
//! The binder has already swapped the sequence's name for the number of its counter, so the first
//! argument here is a BIGINT the registry in [`rudb_common::sequence`] knows. These are the one
//! kind of call where every argument being a constant does not mean one answer for the batch,
//! because each row takes a value of its own, so they are answered before the constant path in
//! [`crate::scalar::call`] gets a chance to call them once.

use rudb_common::sequence::lookup;
use rudb_common::{LogicalType, Result, Value};
use rudb_vector::Vector;

/// The answer for a call to one of the three, or `None` for any other name.
pub(crate) fn call<V: AsRef<Vector>>(
    name: &str,
    args: &[V],
    rows: usize,
) -> Result<Option<Vector>> {
    if !matches!(name, "nextval" | "currval" | "setval") {
        return Ok(None);
    }
    let mut values = Vec::with_capacity(rows);
    // row at a time: every row moves the counter once, in order, so there is no batch answer.
    for row in 0..rows {
        let id = args[0].as_ref().try_value_at(row)?;
        let Some(id) = id.as_i64() else {
            values.push(Value::Null);
            continue;
        };
        let counter = lookup(id as u64)?;
        let value = match name {
            "nextval" => Value::BigInt(counter.next()?),
            "currval" => Value::BigInt(counter.current()?),
            _ => {
                let Some(value) = args[1].as_ref().try_value_at(row)?.as_i64() else {
                    values.push(Value::Null);
                    continue;
                };
                let called = match args.get(2) {
                    Some(called) => match called.as_ref().try_value_at(row)? {
                        Value::Boolean(called) => called,
                        _ => {
                            values.push(Value::Null);
                            continue;
                        }
                    },
                    None => true,
                };
                Value::BigInt(counter.set(value, called)?)
            }
        };
        values.push(value);
    }
    Vector::from_values(LogicalType::BigInt, &values).map(Some)
}

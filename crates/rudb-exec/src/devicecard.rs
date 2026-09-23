//! `rudb_device_card(path)`, the rows of the device card for the directory it names.
//!
//! The measuring is `rudb_io::device` and this is only the table. It is measured while the operator
//! is built, like every other metadata table here reads its facts then, so the rows are fixed
//! before the first chunk goes out. That also means `EXPLAIN` alone does not probe: it never builds
//! the operator.
//!
//! The card is not written into the database yet. `engine-v4/09-the-log.md` keeps it next to the
//! log so that opening a database does not measure again, and that lands with the log in W3. Until
//! then a process measures a device once and every later call on that device gets the kept card
//! back, and a call that names an iteration count measures again at that count.

use std::path::Path;

use rudb_common::{Error, Result, Value};
use rudb_functions::device_card_fields;
use rudb_io::device::{Card, card};
use rudb_plan::{Expr, Plan, Slice};

use crate::metadata::{Metadata, text};

/// The card for the directory the first argument names, one row per sync call.
///
/// # Errors
///
/// When the path is null or is not a directory that can be written to, when the iteration count is
/// not a positive number that fits in 32 bits, or when the plan asks for a column this table does
/// not have.
pub(crate) fn device_card(
    plan: &Plan,
    args: Slice,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let values: Vec<&Value> = plan
        .expr_list(args)
        .iter()
        .map(|argument| match plan.expr(*argument) {
            Expr::Constant(reference) => Ok(plan.value(*reference)),
            _ => Err(Error::not_implemented(
                "rudb_device_card() given an argument that is not a constant",
            )),
        })
        .collect::<Result<_>>()?;
    let path = match values.first() {
        Some(Value::Varchar(path)) => path.clone(),
        _ => return Err(Error::invalid_input("rudb_device_card() needs the path of a directory")),
    };
    let iterations = match values.get(1) {
        None => None,
        Some(Value::BigInt(n)) => {
            Some(u32::try_from(*n).ok().filter(|n| *n > 0).ok_or_else(|| {
                Error::invalid_input(format!("rudb_device_card() cannot run {n} iterations"))
            })?)
        }
        Some(other) => {
            return Err(Error::internal(format!(
                "an iteration count bound as BIGINT arrived as {other}"
            )));
        }
    };
    let measured = card(Path::new(&path), iterations)?;
    Metadata::new("rudb_device_card", &device_card_fields(), &rows(&measured), plan, index, columns)
}

/// The card as rows, with the device wide columns repeated on each.
#[expect(
    clippy::cast_precision_loss,
    reason = "nanosecond latencies and byte rates on a real device are far under 2^53"
)]
fn rows(card: &Card) -> Vec<Vec<Value>> {
    let micros = |ns: u64| Value::Double(ns as f64 / 1_000.0);
    let count = |n: u64| Value::BigInt(i64::try_from(n).unwrap_or(i64::MAX));
    card.probes
        .iter()
        .enumerate()
        .map(|(at, probe)| {
            vec![
                text(&card.path.display().to_string()),
                text(&card.device),
                text(&card.filesystem),
                text(probe.call.name()),
                Value::Boolean(at == 0),
                micros(probe.p50_4k_ns),
                micros(probe.p99_4k_ns),
                micros(probe.p50_64k_ns),
                micros(probe.p99_64k_ns),
                Value::Boolean(probe.plausible),
                Value::Double(card.write_bytes_per_s as f64 / f64::from(1 << 20)),
                count(card.syncs_per_s[0]),
                count(card.syncs_per_s[1]),
                count(card.syncs_per_s[2]),
                count(card.syncs_per_s[3]),
                Value::Double(card.scaling),
                text(card.plp.name()),
                Value::Boolean(card.memory_backed),
                Value::Integer(i32::try_from(card.lanes).unwrap_or(i32::MAX)),
                Value::Integer(i32::try_from(card.iterations).unwrap_or(i32::MAX)),
            ]
        })
        .collect()
}

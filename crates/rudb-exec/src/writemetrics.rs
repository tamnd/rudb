//! `rudb_write_metrics()`, the load profile of the recent bulk loads.
//!
//! The counting is in `rudb_metrics::LoadProfile` and the writer and the sink that charge it. What
//! is here is turning the loads the process kept into rows, which is one row for each stage a load
//! ran and one `total` row after them.
//!
//! A stage nothing charged is left out rather than printed as zeros. Split, the structural index,
//! transcoding and the extent allocator are stages of the v4 bulk path that the writer does not
//! have yet, and a row of zeros for them would read as a stage that ran and cost nothing.

use rudb_common::{Result, Value};
use rudb_functions::write_metric_fields;
use rudb_metrics::{LoadProfile, Stage, StageTotals, recent_loads};
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// Every stage of every kept load, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn write_metrics(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let mut rows = Vec::new();
    for load in recent_loads() {
        let mut total = StageTotals::default();
        for stage in Stage::ALL {
            let spent = load.stage(stage);
            if spent.charged == 0 && spent.waits == 0 {
                continue;
            }
            total.cpu_ns = total.cpu_ns.saturating_add(spent.cpu_ns);
            total.waits = total.waits.saturating_add(spent.waits);
            total.wait_ns = total.wait_ns.saturating_add(spent.wait_ns);
            rows.push(row(&load, stage.name(), &spent));
        }
        // The load's rows are the rows that reached the writer, and its bytes are what went in
        // to the page builder and what the file grew by, which is every stage that wrote.
        let convert = load.stage(Stage::Convert);
        let pages = load.stage(Stage::Pages);
        total.wall_ns = load.elapsed_ns();
        total.rows = convert.rows.max(pages.rows);
        total.bytes_in = pages.bytes_in;
        total.bytes_out = [Stage::Dictionary, Stage::Write, Stage::Publish]
            .into_iter()
            .map(|stage| load.stage(stage).bytes_out)
            .fold(0_u64, u64::saturating_add);
        rows.push(row(&load, "total", &total));
    }
    Metadata::new("rudb_write_metrics", &write_metric_fields(), &rows, plan, index, columns)
}

fn row(load: &LoadProfile, stage: &str, spent: &StageTotals) -> Vec<Value> {
    vec![
        Value::BigInt(signed(load.id())),
        text(load.target()),
        text(stage),
        Value::Double(millis(spent.wall_ns)),
        Value::Double(millis(spent.cpu_ns)),
        Value::BigInt(signed(spent.bytes_in)),
        Value::BigInt(signed(spent.bytes_out)),
        Value::BigInt(signed(spent.rows)),
        Value::BigInt(signed(spent.waits)),
        Value::Double(millis(spent.wait_ns)),
        Value::Boolean(load.finished()),
    ]
}

#[expect(clippy::cast_precision_loss, reason = "a millisecond reading does not need 53 bits")]
fn millis(nanos: u64) -> f64 {
    nanos as f64 / 1e6
}

fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

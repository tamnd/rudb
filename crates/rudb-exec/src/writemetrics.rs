//! `rudb_write_metrics()`, the load profile of the recent bulk loads.
//!
//! The counting is in `rudb_metrics::LoadProfile` and the writer and the sink that charge it. What
//! is here is turning the loads the process kept into rows, which is one row for each stage a load
//! ran and one `total` row after them. The `total` row also carries the load's two memory peaks.
//!
//! A stage nothing charged is left out rather than printed as zeros. Split, the structural index,
//! transcoding and the extent allocator are stages of the v4 bulk path that the writer does not
//! have yet, and a row of zeros for them would read as a stage that ran and cost nothing.
//!
//! `rudb_codec_metrics()` is here too, since it is the page builder's row of the same profile split
//! by codec. The counting is in `rudb_encoding::tally`.
//!
//! So is `rudb_statement_metrics()`, the phases of the recent statements, which is the same kind of
//! table over a ring `rudb_metrics` keeps. The counting is on the statement path in `rudb`.

use rudb_common::{Result, Value};
use rudb_encoding::tally;
use rudb_functions::{codec_metric_fields, statement_metric_fields, write_metric_fields};
use rudb_metrics::{LoadProfile, Stage, StageTotals, recent_loads, recent_statements};
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
            rows.push(row(&load, stage.name(), &spent, false));
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
        rows.push(row(&load, "total", &total, true));
    }
    Metadata::new("rudb_write_metrics", &write_metric_fields(), &rows, plan, index, columns)
}

/// Every codec of both families with what it cost and how often it was kept.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn codec_metrics(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let codecs = tally::codecs();
    let family_nanos = |family: &str| -> u64 {
        codecs
            .iter()
            .filter(|codec| codec.family == family)
            .map(|codec| codec.nanos)
            .fold(0, u64::saturating_add)
    };
    let rows: Vec<Vec<Value>> = codecs
        .iter()
        .map(|codec| {
            vec![
                text(codec.family),
                text(codec.name),
                Value::BigInt(signed(codec.offers)),
                Value::BigInt(signed(codec.kept)),
                ratio(codec.kept, codec.offers),
                Value::Double(millis(codec.nanos)),
                ratio(codec.nanos, family_nanos(codec.family)),
            ]
        })
        .collect();
    Metadata::new("rudb_codec_metrics", &codec_metric_fields(), &rows, plan, index, columns)
}

/// Every kept statement with what each of its phases cost.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn statement_metrics(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let rows: Vec<Vec<Value>> = recent_statements()
        .iter()
        .map(|statement| {
            let count = |nanos: u64| Value::BigInt(signed(nanos));
            let mut row = vec![
                count(statement.id),
                text(&statement.sql),
                count(statement.parse_ns),
                count(statement.bind_ns),
                count(statement.rewrite_ns),
                count(statement.optimize_ns),
                count(statement.frontend_ns()),
                count(statement.physical_ns),
                count(statement.codegen_ns),
                count(statement.execute_ns),
                count(statement.total_ns),
                count(statement.cpu_ns),
            ];
            row.extend(statement.split.parts().iter().map(|(_, nanos)| count(*nanos)));
            row
        })
        .collect();
    Metadata::new("rudb_statement_metrics", &statement_metric_fields(), &rows, plan, index, columns)
}

#[expect(clippy::cast_precision_loss, reason = "a share does not need 53 bits")]
fn ratio(part: u64, whole: u64) -> Value {
    if whole == 0 { Value::Null } else { Value::Double(part as f64 / whole as f64) }
}

fn row(load: &LoadProfile, stage: &str, spent: &StageTotals, total: bool) -> Vec<Value> {
    let (accounted, resident) = if total {
        (
            Value::BigInt(signed(load.accounted_peak())),
            load.peak_rss().map_or(Value::Null, |peak| Value::BigInt(signed(peak))),
        )
    } else {
        (Value::Null, Value::Null)
    };
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
        accounted,
        resident,
    ]
}

#[expect(clippy::cast_precision_loss, reason = "a millisecond reading does not need 53 bits")]
fn millis(nanos: u64) -> f64 {
    nanos as f64 / 1e6
}

fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

//! `rudb_strategies()`, the table that says what the engine can be made of.
//!
//! One row per registered implementation of a seam, and one row for every seam that has no
//! implementations yet, which today is twenty six of the twenty seven. A reader who wants to know
//! what rudb will let them swap runs this, and a reader who wants to know what it lets them swap
//! now reads the same table and finds the implementation columns null.
//!
//! The operator is [`Metadata`], which every table of this kind shares. What is here is the list of
//! rows and nothing else.

use rudb_common::{Result, Value};
use rudb_functions::strategy_fields;
use rudb_plan::{Plan, Slice};
use rudb_seam::{SeamId, StrategyRow};

use crate::metadata::{Metadata, text};
use crate::register::registries;

/// Every seam and everything registered against it, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn strategies(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let registered = registries().rows();
    let mut rows = Vec::new();
    for seam in SeamId::ALL.iter().copied() {
        let mut any = false;
        for row in registered.iter().filter(|row| row.seam == seam) {
            rows.push(implemented(row));
            any = true;
        }
        if !any {
            rows.push(planned(seam));
        }
    }
    Metadata::new("rudb_strategies", &strategy_fields(), &rows, plan, index, columns)
}

/// The row an implementation produces.
fn implemented(row: &StrategyRow) -> Vec<Value> {
    vec![
        text(row.seam.name()),
        text(row.seam.milestone()),
        text(row.seam.describe()),
        text(row.name),
        text(row.describe),
        text(&row.provenance.to_string()),
        text(&row.determinism.to_string()),
        Value::Boolean(row.is_reference),
        Value::Boolean(row.is_default),
    ]
}

/// The row a seam with no registry produces.
///
/// The three columns that describe the seam are filled and the six that describe an implementation
/// are null, rather than the seam being left out of the table. A seam nobody has built is a
/// commitment somebody has made, the milestone column says who owes it, and a table that listed
/// only what exists would make the engine look finished.
fn planned(seam: SeamId) -> Vec<Value> {
    vec![
        text(seam.name()),
        text(seam.milestone()),
        text(seam.describe()),
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
    ]
}

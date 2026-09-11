//! The rewrite passes, cardinality estimation, join ordering, predicate transfer and layout adaptation.
//!
//! Rank 11 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! One pass so far, which is column pruning. `spec/09-optimizer.md` section 9.1 describes a sequence
//! and this is the first of it, chosen because it is the one whose absence is measured in gigabytes:
//! a scan that reads 105 columns to answer a question about three is the whole of the difference on
//! ClickBench, and the Parquet reader has been able to read a subset since M1 with nothing able to
//! tell it which subset.

#![forbid(unsafe_code)]

pub mod columns;

use rudb_common::Result;
use rudb_plan::Plan;

/// The crate this rank belongs to, so that the layer check has something to read.
pub const RANK: u8 = 11;

/// Rewrites a bound plan into the plan that runs.
///
/// Every pass preserves the plan invariant, which is what [`Plan::validate`] checks, so this checks
/// it once at the end rather than each pass checking itself. In a release build it does not, because
/// a pass that breaks the invariant breaks it the same way in both builds and the debug build is
/// where that gets found.
///
/// # Errors
///
/// If a pass left the plan malformed, which is a bug in the pass and not in the query.
pub fn optimize(plan: &mut Plan) -> Result<()> {
    columns::prune(plan);
    if cfg!(debug_assertions) {
        plan.validate()?;
    }
    Ok(())
}

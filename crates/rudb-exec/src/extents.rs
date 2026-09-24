//! A turned around semi or anti join whose one leftover condition compares a column on each side.
//!
//! TPC-H q21 asks, for each late line of an order, whether some other line of the same order came
//! from another supplier: `EXISTS (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey
//! AND l2.l_suppkey <> l1.l_suppkey)`. The plan gathers `l1`, the smaller side, and marks each of
//! its rows that some row of `l2` matches, see [`crate::join::Marking`]. With the `<>` left over
//! that meant a chain walk per driving row, every pair gathered into a chunk, the condition
//! evaluated over it and the answer read back, which on SF1 was about a billion instructions for
//! 740 thousand driving rows, three times what DuckDB spends on the whole subquery.
//!
//! None of that is needed. Whether some driving row of a key has a value `d` with `g <> d` depends
//! only on the smallest and the largest `d` of that key: some `d` differs from `g` exactly when the
//! two are not both `g`. The same holds for the four orderings, which want only one of the two:
//! `g < d` for some `d` exactly when the largest is above `g`. So the driving side keeps two numbers
//! per key and no pair is ever made. A driving row costs the lookup it already cost and two
//! compares, and at the end each gathered row is compared once against its key's two numbers.
//!
//! This is the rewrite Seshadri et al. call magic decorrelation turned into an aggregate, done
//! where the table on the key is already built rather than as a plan with a group by in it.
//!
//! # Nulls
//!
//! A comparison with a null is null and a null never marks, so a driving row whose value is null
//! is not counted in its key's range and a gathered row whose value is null is never marked. A key
//! that only null driving values reached has no range, which is the same as no driving row at all.
//!
//! # What is not here
//!
//! Anything but one integer column against another of the same type, with nothing else left over.
//! A float has `NaN`, which the orderings do not place where the smallest and the largest would
//! need it, and a string would be the same argument over bytes that nothing here needs yet.

use std::sync::atomic::{AtomicI64, Ordering};

use rudb_common::{LogicalType, Result};
use rudb_plan::{CompareOp, Expr, ExprRef, Plan};
use rudb_vector::Vector;

use crate::lookup::{Lookup, MISS};
use crate::schema::Schema;

/// The one comparison a marking join's leftover condition makes, in terms of the two sides.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Spread {
    /// The driving column, by position in a driving chunk.
    pub(crate) driving: usize,
    /// The gathered column, by position in the gathered side.
    pub(crate) gathered: usize,
    /// The comparison, written with the gathered value on the left: `g op d`.
    op: CompareOp,
}

impl Spread {
    /// The comparison `residual` makes, or nothing when it is not one this answers.
    ///
    /// `combined` is the driving schema and then the gathered one, which is how the join holds a
    /// pair, and `width` is where the first ends.
    pub(crate) fn of(
        plan: &Plan,
        residual: &[ExprRef],
        combined: &Schema,
        width: usize,
    ) -> Option<Self> {
        let [expr] = residual else { return None };
        let Expr::Compare { op, left, right } = *plan.expr(*expr) else { return None };
        let place = |expr: ExprRef| match *plan.expr(expr) {
            Expr::Column(binding) => combined.position_of(binding),
            _ => None,
        };
        let (left, right) = (place(left)?, place(right)?);
        let types = combined.types();
        let (Some(left_type), Some(right_type)) = (types.get(left), types.get(right)) else {
            return None;
        };
        if left_type != right_type || !ordered(left_type) {
            return None;
        }
        let (driving, gathered, op) = match (left < width, right < width) {
            (false, true) => (right, left - width, op),
            (true, false) => (left, right - width, flipped(op)?),
            _ => return None,
        };
        matches!(
            op,
            CompareOp::NotEqual
                | CompareOp::Less
                | CompareOp::LessOrEqual
                | CompareOp::Greater
                | CompareOp::GreaterOrEqual
        )
        .then_some(Self { driving, gathered, op })
    }

    /// Whether a gathered value `g` meets some driving value of a key whose range is `low..=high`.
    fn meets(self, g: i64, low: i64, high: i64) -> bool {
        match self.op {
            CompareOp::NotEqual => low != g || high != g,
            CompareOp::Less => high > g,
            CompareOp::LessOrEqual => high >= g,
            CompareOp::Greater => low < g,
            CompareOp::GreaterOrEqual => low <= g,
            _ => false,
        }
    }
}

/// The types whose values read as `i64` in the order they compare in.
fn ordered(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::Date
    )
}

/// The same comparison with its operands swapped, so `a < b` becomes `b > a`.
fn flipped(op: CompareOp) -> Option<CompareOp> {
    Some(match op {
        CompareOp::NotEqual => CompareOp::NotEqual,
        CompareOp::Less => CompareOp::Greater,
        CompareOp::LessOrEqual => CompareOp::GreaterOrEqual,
        CompareOp::Greater => CompareOp::Less,
        CompareOp::GreaterOrEqual => CompareOp::LessOrEqual,
        _ => return None,
    })
}

/// The smallest and the largest driving value seen per key, shared by every instance.
///
/// Shared rather than one copy per instance and merged, because a copy is sixteen bytes a key per
/// thread and the gathered side can hold millions of keys. The updates are a load and a compare,
/// and an atomic write only when the value moves the range, which after the first few rows of a
/// key it rarely does, so two instances almost never write the same place.
#[derive(Debug)]
pub(crate) struct Extents {
    low: Vec<AtomicI64>,
    high: Vec<AtomicI64>,
    /// The gathered column, read once up front so that the answer never depends on a read that
    /// could fail after the driving side has been spent on this.
    values: Vec<i64>,
    /// Which gathered rows are null, empty when none are.
    nulls: Vec<bool>,
}

impl Extents {
    /// Nothing seen, for a table of `slots` keys over the gathered column `column`.
    ///
    /// Nothing when that column does not read as a run of integers.
    pub(crate) fn new(slots: usize, column: &Vector) -> Option<Self> {
        let mut values = Vec::new();
        if !column.signed_block(&mut values) {
            return None;
        }
        let nulls = if column.none_null() {
            Vec::new()
        } else {
            (0..values.len()).map(|row| column.is_null_at(row)).collect()
        };
        Some(Self {
            low: (0..slots).map(|_| AtomicI64::new(i64::MAX)).collect(),
            high: (0..slots).map(|_| AtomicI64::new(i64::MIN)).collect(),
            values,
            nulls,
        })
    }

    /// What this holds, for the reservation.
    pub(crate) fn footprint(&self) -> u64 {
        let bytes = self.low.len() * 2 * size_of::<AtomicI64>()
            + self.values.len() * size_of::<i64>()
            + self.nulls.len();
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }

    /// Widens each key's range by the driving column's values, `slots` being the key of each row.
    ///
    /// `block` is the caller's buffer. A column in a form with no run to read, a gather through a
    /// link for one, is flattened first. `false`, with nothing seen, for a column that does not
    /// read as a run of integers even then, which the caller answers the way it would without this.
    ///
    /// # Errors
    ///
    /// Whatever flattening the column raises.
    pub(crate) fn widen(
        &self,
        column: &Vector,
        slots: &[usize],
        block: &mut Vec<i64>,
    ) -> Result<bool> {
        if column.signed_block(block) {
            return Ok(self.read(column, slots, block));
        }
        // flatten: a gather through a link has no run to read and the loop below wants one, and
        // the copy is one chunk of one column against a lookup per row that is being saved.
        let flat = column.flatten()?;
        Ok(flat.signed_block(block) && self.read(&flat, slots, block))
    }

    /// The same, once `block` holds the column's values.
    fn read(&self, column: &Vector, slots: &[usize], block: &[i64]) -> bool {
        if block.len() < slots.len() {
            return false;
        }
        let nullable = !column.none_null();
        for (row, (&slot, &value)) in slots.iter().zip(block.iter()).enumerate() {
            if slot == MISS || nullable && column.is_null_at(row) {
                continue;
            }
            let (Some(low), Some(high)) = (self.low.get(slot), self.high.get(slot)) else {
                continue;
            };
            if value < low.load(Ordering::Relaxed) {
                low.fetch_min(value, Ordering::Relaxed);
            }
            if value > high.load(Ordering::Relaxed) {
                high.fetch_max(value, Ordering::Relaxed);
            }
        }
        true
    }

    /// Sets the bit of every gathered row whose value meets its key's range.
    ///
    /// Every instance has finished by now, so the relaxed loads read every write.
    pub(crate) fn mark(&self, spread: Spread, index: &Lookup, bits: &mut [u64]) {
        let mut chain = Vec::new();
        for (slot, (low, high)) in self.low.iter().zip(&self.high).enumerate() {
            let (low, high) = (low.load(Ordering::Relaxed), high.load(Ordering::Relaxed));
            if low > high {
                continue;
            }
            index.matches(slot, &mut chain);
            for &row in &chain {
                let at = row as usize;
                let Some(&g) = self.values.get(at) else { continue };
                if self.nulls.get(at).copied().unwrap_or(false) || !spread.meets(g, low, high) {
                    continue;
                }
                if let Some(word) = bits.get_mut(at / u64::BITS as usize) {
                    *word |= 1 << (at % u64::BITS as usize);
                }
            }
        }
    }
}

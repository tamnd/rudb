//! `RUDBCS1`, the column summary of `spec/stats/03-the-file-format.md` section 3.3.
//!
//! One per column, a few hundred bytes, and the first thing a planner reads. Everything in it is
//! either a count that adds, an extreme that folds, or a flag that is dropped when it cannot be
//! proved to survive a fold, which is what the crate doc means by mergeable or re-derivable.
//!
//! # What is not in here
//!
//! The generation the summary describes. That is the section table entry's stamp, one level up, and
//! writing it here as well would be two places for one fact to be wrong. What is here is
//! [`Summary::newest`], the generation of the newest data the summary has seen, because that one
//! has nowhere else to live and it is the half of the pair that says how far behind a stale summary
//! is rather than only that it is behind.
//!
//! The column's identity. The section entry's `id` names the column, so a summary that also named
//! one could name a different one.

use std::cmp::Ordering;

use rudb_common::bounds::{self, Bound};
use rudb_common::stat::{Class, Direction};
use rudb_common::{Error, Result};

/// What layout the bytes are in.
///
/// One, and a reader that meets a number it does not know declines the summary rather than guessing
/// at it. That is cheap here in a way it is not in a column: section 3.1 says a summary that is not
/// read changes no answer, so declining one is a slower query and never a wrong one.
const LAYOUT: u8 = 1;

/// How a column's values are ordered, as far as the writer could tell in one pass.
///
/// `Neither` is the ordinary answer and is not a failure. It is what a column that is genuinely
/// unordered says, and it is also what a fold of two ordered parts says when the parts overlap,
/// because a pair of ascending runs that interleave is not an ascending run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Every value is at least the one before it.
    Ascending,
    /// Every value is at most the one before it.
    Descending,
    /// Neither, which is most columns.
    Neither,
}

impl Order {
    /// The tag this takes on disk.
    const fn tag(self) -> u8 {
        match self {
            Self::Neither => 0,
            Self::Ascending => 1,
            Self::Descending => 2,
        }
    }

    fn of_tag(tag: u8) -> Result<Self> {
        Ok(match tag {
            0 => Self::Neither,
            1 => Self::Ascending,
            2 => Self::Descending,
            _ => return Err(torn(format!("an order tag of {tag}"))),
        })
    }

    /// The word a report prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ascending => "ascending",
            Self::Descending => "descending",
            Self::Neither => "neither",
        }
    }
}

/// Everything one column of one table says about itself.
///
/// The fields are `pub` because this is a record and not an abstraction. What it means to fold two
/// of them is the one piece of behaviour it has, and that is [`Summary::widen`].
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// How many rows the column has. Exact.
    pub rows: u64,
    /// How many of them are null. Exact.
    pub nulls: u64,
    /// The smallest value, or `None` when the column has no ordered bound.
    pub low: Option<Bound>,
    /// The largest value, same.
    pub high: Option<Bound>,
    /// Whether the two ends are values the rows really hold rather than bounds that are allowed to
    /// be wider.
    ///
    /// A bound that is too wide skips a stripe correctly and answers a `MIN` wrongly, so this is the
    /// difference between the two, and it is the same distinction `rudb_storage::zone::Range` draws
    /// for the same reason.
    pub ends_exact: bool,
    /// How many distinct non-null values there are.
    pub distinct: u64,
    /// How much of `distinct` is knowledge.
    ///
    /// Exact when the sketch never overflowed or the column is a dictionary with no nulls, a
    /// certified lower bound after a fold, estimated otherwise. Stored rather than inferred because
    /// the difference matters at the consuming end: an exact distinct count lets `COUNT(DISTINCT c)`
    /// be answered out of metadata and an estimate does not.
    pub distinct_class: Class,
    /// Whether every non-null value is unique, which is exact and which is what makes a column a key
    /// candidate.
    ///
    /// False means not known to be unique rather than known to repeat, which is the direction that
    /// costs a key map and never costs an answer.
    pub unique: bool,
    /// Which way the values run, if either.
    pub order: Order,
    /// How many ascending runs there are, which is one for a sorted column and about the row count
    /// for a shuffled one.
    pub runs: u64,
    /// Whether the ranges of the stripes this covers overlap each other.
    ///
    /// Separate from `order` because they answer different questions. A column can be unordered
    /// inside every stripe and still have stripes that do not overlap, which is the case that makes
    /// a range predicate skip nearly everything, and `order` alone would call it unordered and say
    /// nothing about the skipping.
    pub overlapping: bool,
    /// Total bytes the values take. Exact.
    pub bytes: u64,
    /// The widest single value in bytes. Exact.
    pub widest: u64,
    /// The generation of the newest data this summary has seen.
    pub newest: u64,
}

impl Default for Summary {
    /// A summary of nothing, which is the identity [`Summary::widen`] folds onto.
    ///
    /// Not a summary that says a column is empty. Those are the same bytes and they differ in how
    /// they got here, which is why folding an empty one onto a real one has to leave the real one
    /// alone, and does.
    fn default() -> Self {
        Self {
            rows: 0,
            nulls: 0,
            low: None,
            high: None,
            ends_exact: true,
            distinct: 0,
            distinct_class: Class::Exact,
            unique: true,
            order: Order::Ascending,
            runs: 0,
            overlapping: false,
            bytes: 0,
            widest: 0,
            newest: 0,
        }
    }
}

impl Summary {
    /// Folds `other` into this one, so that the result describes both stretches of rows.
    ///
    /// This is the mergeable half of the crate doc's rule, and the two facts that do not merge are
    /// where the care is.
    ///
    /// A distinct count does not add. Two parts holding a thousand values each hold between a
    /// thousand and two thousand between them, and which of those it is depends on how much they
    /// overlap, which nothing in a summary can say. So the fold keeps the larger of the two and
    /// marks it [`Direction::AtLeast`], which is true of the union whatever the overlap turns out to
    /// be. The merged sketch beside it is what turns the bound back into a number, and when the fold
    /// is of two exact counts the bound is still exact in the one direction it claims.
    ///
    /// Distinctness does not survive a fold either, for the same reason pointing the other way: two
    /// parts can each hold no repeats and still repeat each other. It survives only when the two
    /// ranges are exact and provably apart, which is the ordinary case for a sorted key column split
    /// across parts and is the case worth keeping, and it is dropped otherwise.
    ///
    /// Order is folded the same way. Two ascending stretches make an ascending stretch when the
    /// first ends at or before the second begins, and make an unordered one when they interleave.
    pub fn widen(&mut self, other: &Self) {
        if other.rows == 0 && other.bytes == 0 && other.low.is_none() && other.high.is_none() {
            return;
        }
        if self.rows == 0 && self.bytes == 0 && self.low.is_none() && self.high.is_none() {
            *self = other.clone();
            return;
        }

        let apart = self.below(other) || other.below(self);
        let after = self.runs_after(other);

        self.rows += other.rows;
        self.nulls += other.nulls;
        self.bytes += other.bytes;
        self.widest = self.widest.max(other.widest);
        self.newest = self.newest.max(other.newest);
        self.overlapping = self.overlapping || other.overlapping || !apart;

        self.distinct = self.distinct.max(other.distinct);
        self.distinct_class = at_least(self.distinct_class, other.distinct_class);
        self.unique = self.unique && other.unique && apart;
        self.order = after;
        self.runs += other.runs;

        self.ends_exact &= other.ends_exact;
        self.low = match (self.low.take(), other.low.clone()) {
            (Some(mine), Some(theirs)) => Some(mine.smaller(theirs)),
            _ => None,
        };
        self.high = match (self.high.take(), other.high.clone()) {
            (Some(mine), Some(theirs)) => Some(mine.larger(theirs)),
            _ => None,
        };
    }

    /// Whether every value of this stretch is strictly below every value of `other`.
    ///
    /// Three ways to answer no. Either stretch having ends that are allowed to be wider than its
    /// rows is one, because a wide bound can say two stretches are apart when they are not, and both
    /// of the things that lean on this, distinctness and ascending order, are wrong answers rather
    /// than slow ones when it does. Either end being absent is another. And two bounds from
    /// different domains is the third, which `Bound::order` answers `None` to and which a comparison
    /// written with `smaller` would quietly have called apart.
    fn below(&self, other: &Self) -> bool {
        if !self.ends_exact || !other.ends_exact {
            return false;
        }
        let (Some(high), Some(low)) = (self.high.as_ref(), other.low.as_ref()) else {
            return false;
        };
        high.order(low) == Some(Ordering::Less)
    }

    /// What the order of the two stretches laid end to end is.
    ///
    /// This one is directional where apartness is not: two ascending stretches make an ascending
    /// stretch only when the one that comes first in the file is the one that holds the smaller
    /// values, and two descending ones only when it is the other way round.
    fn runs_after(&self, other: &Self) -> Order {
        match (self.order, other.order) {
            (Order::Ascending, Order::Ascending) if self.below(other) => Order::Ascending,
            (Order::Descending, Order::Descending) if other.below(self) => Order::Descending,
            _ => Order::Neither,
        }
    }

    /// How many non-null rows there are, which is the count every ratio in a planner divides by.
    #[must_use]
    pub fn present(&self) -> u64 {
        self.rows.saturating_sub(self.nulls)
    }

    /// Appends the summary's bytes.
    ///
    /// # Errors
    ///
    /// If a bound is longer than a `u32` can count, which is the one thing here that is not a fixed
    /// width field.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.push(LAYOUT);
        out.extend_from_slice(&self.rows.to_le_bytes());
        out.extend_from_slice(&self.nulls.to_le_bytes());
        bounds::put(out, self.low.as_ref())?;
        bounds::put(out, self.high.as_ref())?;
        let flags = u8::from(self.ends_exact)
            | (u8::from(self.unique) << 1)
            | (u8::from(self.overlapping) << 2);
        out.push(flags);
        out.push(self.order.tag());
        out.extend_from_slice(&self.runs.to_le_bytes());
        out.extend_from_slice(&self.distinct.to_le_bytes());
        put_class(out, self.distinct_class);
        out.extend_from_slice(&self.bytes.to_le_bytes());
        out.extend_from_slice(&self.widest.to_le_bytes());
        out.extend_from_slice(&self.newest.to_le_bytes());
        Ok(())
    }

    /// Reads a summary written by [`Summary::encode`].
    ///
    /// # Errors
    ///
    /// If the layout number is one this build does not write, if the bytes run out part way
    /// through, or if a tag is not one of the ones defined. Every one of those is answered by not
    /// having a summary, which section 3.1 says is a correct state.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut at = 0;
        let layout = take(bytes, &mut at, 1)?[0];
        if layout != LAYOUT {
            return Err(torn(format!("layout {layout} and this build writes {LAYOUT}")));
        }
        let rows = eight(bytes, &mut at)?;
        let nulls = eight(bytes, &mut at)?;
        let low = bounds::get(bytes, &mut at)?;
        let high = bounds::get(bytes, &mut at)?;
        let flags = take(bytes, &mut at, 1)?[0];
        let order = Order::of_tag(take(bytes, &mut at, 1)?[0])?;
        let runs = eight(bytes, &mut at)?;
        let distinct = eight(bytes, &mut at)?;
        let distinct_class = get_class(bytes, &mut at)?;
        let byte_total = eight(bytes, &mut at)?;
        let widest = eight(bytes, &mut at)?;
        let newest = eight(bytes, &mut at)?;
        Ok(Self {
            rows,
            nulls,
            low,
            high,
            ends_exact: flags & 1 != 0,
            distinct,
            distinct_class,
            unique: flags & 2 != 0,
            order,
            runs,
            overlapping: flags & 4 != 0,
            bytes: byte_total,
            widest,
            newest,
        })
    }
}

/// The class a fold of two distinct counts has.
///
/// The number kept is the larger of the two, which the union is at least as large as whatever the
/// two stretches share, so the class says [`Direction::AtLeast`] whatever the two sides said. An
/// exact count folded with an exact count is still a certified bound and not an exact answer, which
/// is the point: the two counts were exact about their own stretches and neither of them was ever
/// about the union.
fn at_least(left: Class, right: Class) -> Class {
    let bound = match (left, right) {
        (Class::Exact, Class::Exact) => 0.0,
        _ => {
            let each = |class: Class| match class {
                Class::Exact => 0.0,
                Class::Certified { bound, .. } => bound,
                Class::Estimated => f64::INFINITY,
            };
            each(left).max(each(right))
        }
    };
    if bound.is_finite() {
        Class::Certified { bound, direction: Direction::AtLeast }
    } else {
        Class::Estimated
    }
}

fn put_class(out: &mut Vec<u8>, class: Class) {
    match class {
        Class::Exact => out.push(0),
        Class::Certified { bound, direction } => {
            out.push(1);
            out.extend_from_slice(&bound.to_le_bytes());
            out.push(match direction {
                Direction::AtMost => 0,
                Direction::AtLeast => 1,
                Direction::Within => 2,
            });
        }
        Class::Estimated => out.push(2),
    }
}

fn get_class(bytes: &[u8], at: &mut usize) -> Result<Class> {
    Ok(match take(bytes, at, 1)?[0] {
        0 => Class::Exact,
        1 => {
            let bound = f64::from_bits(eight(bytes, at)?);
            let direction = match take(bytes, at, 1)?[0] {
                0 => Direction::AtMost,
                1 => Direction::AtLeast,
                2 => Direction::Within,
                other => return Err(torn(format!("a direction tag of {other}"))),
            };
            Class::Certified { bound, direction }
        }
        2 => Class::Estimated,
        other => return Err(torn(format!("a class tag of {other}"))),
    })
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = at.checked_add(len).ok_or_else(|| torn("a length that overflows"))?;
    let taken = bytes.get(*at..end).ok_or_else(|| torn("fewer bytes than it names"))?;
    *at = end;
    Ok(taken)
}

fn eight(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let held: [u8; 8] = take(bytes, at, 8)?.try_into().map_err(|_| torn("a short field"))?;
    Ok(u64::from_le_bytes(held))
}

fn torn(what: impl Into<String>) -> Error {
    Error::invalid_input(format!("a stored column summary has {}", what.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(low: i128, high: i128, rows: u64) -> Summary {
        Summary {
            rows,
            nulls: 0,
            low: Some(Bound::Int(low)),
            high: Some(Bound::Int(high)),
            ends_exact: true,
            distinct: rows,
            distinct_class: Class::Exact,
            unique: true,
            order: Order::Ascending,
            runs: 1,
            overlapping: false,
            bytes: rows * 8,
            widest: 8,
            newest: 3,
        }
    }

    #[test]
    fn a_summary_survives_being_written_down_and_read_back() {
        let mut one = part(1, 100, 100);
        one.low = Some(Bound::Bytes(b"aardvark".to_vec()));
        one.high = Some(Bound::Bytes(b"zebra".to_vec()));
        one.distinct_class = Class::Certified { bound: 0.02, direction: Direction::AtLeast };
        one.order = Order::Descending;
        one.overlapping = true;
        one.unique = false;
        let mut bytes = Vec::new();
        one.encode(&mut bytes).expect("encode");
        assert_eq!(Summary::decode(&bytes).expect("decode"), one);
    }

    #[test]
    fn a_summary_of_a_column_with_no_ends_round_trips() {
        // A column of nothing but nulls, which has counts and no bounds, and which is the case a
        // layout that assumed two bounds would get wrong.
        let empty = Summary { rows: 40, nulls: 40, ..Summary::default() };
        let mut bytes = Vec::new();
        empty.encode(&mut bytes).expect("encode");
        assert_eq!(Summary::decode(&bytes).expect("decode"), empty);
        assert_eq!(empty.present(), 0);
    }

    #[test]
    fn a_summary_is_a_few_hundred_bytes() {
        // Section 3.3 says small and always present, and the always present half is only affordable
        // because of the small half. Sixteen columns of an SF100 lineitem is what this is a
        // sixteenth of.
        let mut bytes = Vec::new();
        part(1, 1000, 1000).encode(&mut bytes).expect("encode");
        assert!(bytes.len() < 256, "{} bytes", bytes.len());
    }

    #[test]
    fn a_truncated_summary_is_refused_at_every_length() {
        let mut bytes = Vec::new();
        part(1, 1000, 1000).encode(&mut bytes).expect("encode");
        for short in 0..bytes.len() {
            assert!(Summary::decode(&bytes[..short]).is_err(), "{short} bytes");
        }
    }

    #[test]
    fn a_layout_this_build_does_not_write_is_declined() {
        let mut bytes = Vec::new();
        part(1, 10, 10).encode(&mut bytes).expect("encode");
        bytes[0] = LAYOUT + 1;
        assert!(Summary::decode(&bytes).is_err());
    }

    #[test]
    fn counts_and_extremes_fold() {
        let mut left = part(1, 100, 100);
        left.nulls = 5;
        let mut right = part(101, 200, 100);
        right.nulls = 7;
        right.widest = 12;
        right.newest = 9;
        left.widen(&right);
        assert_eq!(left.rows, 200);
        assert_eq!(left.nulls, 12);
        assert_eq!(left.present(), 188);
        assert_eq!(left.low, Some(Bound::Int(1)));
        assert_eq!(left.high, Some(Bound::Int(200)));
        assert_eq!(left.bytes, 1600);
        assert_eq!(left.widest, 12);
        assert_eq!(left.newest, 9, "the newer of the two generations");
        assert_eq!(left.runs, 2);
    }

    #[test]
    fn a_distinct_count_folds_to_a_lower_bound_and_not_to_a_sum() {
        // The fact this whole module is careful about. Two parts of a thousand values each hold
        // between a thousand and two thousand between them, and adding them would claim the top of
        // that range as a fact.
        let mut left = part(1, 1000, 1000);
        let right = part(500, 1500, 1000);
        left.widen(&right);
        assert_eq!(left.distinct, 1000, "the larger of the two, not the sum");
        assert_eq!(
            left.distinct_class,
            Class::Certified { bound: 0.0, direction: Direction::AtLeast },
            "exact about each stretch is a bound about the union"
        );
    }

    #[test]
    fn an_estimated_distinct_count_stays_estimated_through_a_fold() {
        let mut left = part(1, 1000, 1000);
        left.distinct_class = Class::Estimated;
        let right = part(1001, 2000, 1000);
        left.widen(&right);
        assert_eq!(left.distinct_class, Class::Estimated);
    }

    #[test]
    fn distinctness_survives_a_fold_only_when_the_two_ranges_are_apart() {
        let mut apart = part(1, 100, 100);
        apart.widen(&part(101, 200, 100));
        assert!(apart.unique, "two unique stretches that cannot share a value are unique");
        assert!(!apart.overlapping);
        assert_eq!(apart.order, Order::Ascending, "and they are still in order");

        let mut touching = part(1, 100, 100);
        touching.widen(&part(100, 200, 100));
        assert!(!touching.unique, "sharing one end is enough to repeat a value");
        assert!(touching.overlapping);
        assert_eq!(touching.order, Order::Neither);

        let mut crossing = part(1, 100, 100);
        crossing.widen(&part(50, 60, 10));
        assert!(!crossing.unique);
        assert!(crossing.overlapping);
    }

    #[test]
    fn a_fold_of_two_stretches_with_wide_ends_keeps_neither_distinctness_nor_order() {
        // A bound that is allowed to be wider than the rows can say two stretches are apart when
        // they are not, and both of the things that lean on apartness are wrong answers rather than
        // slow ones when it does.
        let mut left = part(1, 100, 100);
        left.ends_exact = false;
        let mut right = part(101, 200, 100);
        right.ends_exact = false;
        left.widen(&right);
        assert!(!left.unique);
        assert_eq!(left.order, Order::Neither);
        assert!(!left.ends_exact);
    }

    #[test]
    fn descending_stretches_fold_the_other_way_round() {
        let mut left = Summary { order: Order::Descending, ..part(101, 200, 100) };
        let right = Summary { order: Order::Descending, ..part(1, 100, 100) };
        left.widen(&right);
        assert_eq!(left.order, Order::Descending);
        assert!(left.unique);
    }

    #[test]
    fn folding_a_summary_of_nothing_changes_nothing() {
        // The identity, and the reason `Default` is a summary of nothing rather than a summary of an
        // empty column. A fold that starts from one and walks the parts has to come out where a
        // fold that started from the first part would.
        let one = part(1, 100, 100);
        let mut from_nothing = Summary::default();
        from_nothing.widen(&one);
        assert_eq!(from_nothing, one);

        let mut onto_nothing = one.clone();
        onto_nothing.widen(&Summary::default());
        assert_eq!(onto_nothing, one);
    }

    #[test]
    fn a_fold_does_not_care_which_order_the_parts_arrive_in() {
        // Two parts folded either way round have to agree, because the writer walks them in file
        // order and a reader that walked them differently would get a different answer to the same
        // question.
        let (left, right) = (part(1, 100, 100), part(101, 200, 100));
        let mut forwards = left.clone();
        forwards.widen(&right);
        let mut backwards = right.clone();
        backwards.widen(&left);
        assert_eq!(forwards.rows, backwards.rows);
        assert_eq!(forwards.low, backwards.low);
        assert_eq!(forwards.high, backwards.high);
        assert_eq!(forwards.distinct, backwards.distinct);
        assert_eq!(forwards.unique, backwards.unique, "apartness does not have a direction");
        assert_eq!(forwards.overlapping, backwards.overlapping);
    }
}

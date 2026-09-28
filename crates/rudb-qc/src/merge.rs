//! Folding one worker's groups into another's, the merge step of section 5.4 of
//! `spec/compiler/05-pipelines-and-state.md` for a thread-local aggregation table.
//!
//! The generated body updates a group's accumulators in place and the table does not know what
//! they are, so the fold is written here from the [`Grouping`] the generator described them with.
//! Each one does to two partial accumulators what the body does to an accumulator and a row, so a
//! query answers the same on any number of workers, with one exception that cannot be helped:
//! a float total is added up in a different order.

use rudb_common::Result;
use rudb_qc_gen::{AccOp, Grouping, qir_type, unsigned};
use rudb_qc_ir::Ty;
use rudb_qc_rt::table::read_u128;
use rudb_qc_rt::text;

/// How to fold one accumulator.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fold {
    /// Where it is in the row.
    at: usize,
    how: How,
}

#[derive(Clone, Copy, Debug)]
enum How {
    /// An `i64` count.
    Count,
    /// An `i128` total and a seen byte.
    SumInt,
    /// An `i128` total and an `i64` count.
    AvgInt,
    /// An `f64` total and a seen byte.
    SumFloat,
    /// An `f64` total and an `i64` count.
    AvgFloat,
    /// A value of `width` bytes and a seen byte.
    Extreme { width: usize, keep: Keep, order: Order },
    /// A `str16` and a seen byte.
    Text { least: bool },
    /// Kept outside the row.
    Outside,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keep {
    Least,
    Greatest,
    Any,
}

#[derive(Clone, Copy, Debug)]
enum Order {
    F32,
    F64,
    Unsigned,
    Signed,
}

/// The folds of every accumulator of `g`, in the order they sit in the row.
pub(crate) fn folds(g: &Grouping) -> Result<Vec<Fold>> {
    let mut folds = Vec::with_capacity(g.accs.len());
    for acc in &g.accs {
        let how = match acc.op {
            AccOp::CountStar | AccOp::Count => How::Count,
            AccOp::SumInt => How::SumInt,
            AccOp::AvgInt => How::AvgInt,
            AccOp::SumFloat => How::SumFloat,
            AccOp::AvgFloat => How::AvgFloat,
            AccOp::Min | AccOp::Max | AccOp::AnyValue => {
                let ty = qir_type(&acc.arg)
                    .map_err(|r| rudb_common::Error::internal(r.to_string()))?;
                let order = match ty {
                    Ty::F32 => Order::F32,
                    Ty::F64 => Order::F64,
                    _ if unsigned(&acc.arg) => Order::Unsigned,
                    _ => Order::Signed,
                };
                let keep = match acc.op {
                    AccOp::Min => Keep::Least,
                    AccOp::Max => Keep::Greatest,
                    _ => Keep::Any,
                };
                How::Extreme { width: ty.bytes() as usize, keep, order }
            }
            AccOp::MinStr => How::Text { least: true },
            AccOp::MaxStr => How::Text { least: false },
            AccOp::Distinct(_) => How::Outside,
        };
        folds.push(Fold { at: (g.acc_offset + acc.offset) as usize, how });
    }
    Ok(folds)
}

/// The handles of the distinct sets of `g`.
pub(crate) fn sets(g: &Grouping) -> Vec<u64> {
    g.accs
        .iter()
        .filter_map(|acc| match acc.op {
            AccOp::Distinct(h) => Some(h),
            _ => None,
        })
        .collect()
}

/// Folds the accumulators of the row `src` into those of the row `dst`.
pub(crate) fn fold(folds: &[Fold], dst: &mut [u8], src: &[u8]) {
    for f in folds {
        let (d, s) = (&mut dst[f.at..], &src[f.at..]);
        match f.how {
            How::Count => put_i64(d, 0, i64_at(d, 0).wrapping_add(i64_at(s, 0))),
            How::SumInt => {
                put_i128(d, 0, i128_at(d, 0).wrapping_add(i128_at(s, 0)));
                d[16] |= s[16];
            }
            How::AvgInt => {
                put_i128(d, 0, i128_at(d, 0).wrapping_add(i128_at(s, 0)));
                put_i64(d, 16, i64_at(d, 16).wrapping_add(i64_at(s, 16)));
            }
            // The body only ever adds to a total that starts at zero, so adding a worker's total is
            // the same sum in another order. One with nothing in it is skipped, so that a total of
            // negative zero stays one.
            How::SumFloat => {
                if s[8] != 0 {
                    put_f64(d, f64_at(d) + f64_at(s));
                    d[8] = 1;
                }
            }
            How::AvgFloat => {
                if i64_at(s, 8) != 0 {
                    put_f64(d, f64_at(d) + f64_at(s));
                    put_i64(d, 8, i64_at(d, 8).wrapping_add(i64_at(s, 8)));
                }
            }
            How::Extreme { width, keep, order } => {
                if s[width] == 0 {
                    continue;
                }
                let replace = d[width] == 0
                    || match keep {
                        Keep::Any => false,
                        Keep::Least => less(order, &s[..width], &d[..width]),
                        Keep::Greatest => less(order, &d[..width], &s[..width]),
                    };
                if replace {
                    d[..=width].copy_from_slice(&s[..=width]);
                }
            }
            How::Text { least } => {
                if s[16] == 0 {
                    continue;
                }
                let replace = d[16] == 0 || {
                    let (old, new) = (read_u128(&d[..16]), read_u128(&s[..16]));
                    // SAFETY: both accumulators hold strings that are inline or in a heap the
                    // query's runtime keeps, its own or a worker's it holds.
                    let ord = unsafe { text::bytes(&new).cmp(text::bytes(&old)) };
                    if least { ord.is_lt() } else { ord.is_gt() }
                };
                if replace {
                    d[..17].copy_from_slice(&s[..17]);
                }
            }
            How::Outside => {}
        }
    }
}

/// Whether `a` is less than `b`, both values of the same width, the way the body compares them.
fn less(order: Order, a: &[u8], b: &[u8]) -> bool {
    match order {
        Order::F32 => f32::from_le_bytes(four(a)) < f32::from_le_bytes(four(b)),
        Order::F64 => f64_at(a) < f64_at(b),
        Order::Unsigned => wide(a) < wide(b),
        Order::Signed => {
            let shift = 128 - 8 * a.len() as u32;
            ((wide(a) << shift) as i128 >> shift) < ((wide(b) << shift) as i128 >> shift)
        }
    }
}

/// Up to sixteen little endian bytes as a number.
fn wide(b: &[u8]) -> u128 {
    let mut w = [0u8; 16];
    w[..b.len()].copy_from_slice(b);
    u128::from_le_bytes(w)
}

fn four(b: &[u8]) -> [u8; 4] {
    let mut w = [0u8; 4];
    w.copy_from_slice(&b[..4]);
    w
}

fn i64_at(b: &[u8], at: usize) -> i64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    i64::from_le_bytes(w)
}

fn f64_at(b: &[u8]) -> f64 {
    f64::from_bits(i64_at(b, 0) as u64)
}

fn i128_at(b: &[u8], at: usize) -> i128 {
    read_u128(&b[at..at + 16]) as i128
}

fn put_i64(b: &mut [u8], at: usize, v: i64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_i128(b: &mut [u8], at: usize, v: i128) {
    b[at..at + 16].copy_from_slice(&v.to_le_bytes());
}

fn put_f64(b: &mut [u8], v: f64) {
    b[..8].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signed_minimum_compares_below_zero() {
        let (a, b) = ((-3i32).to_le_bytes(), 2i32.to_le_bytes());
        assert!(less(Order::Signed, &a, &b));
        assert!(!less(Order::Unsigned, &a, &b));
        let (a, b) = ((-1i8).to_le_bytes(), 0i8.to_le_bytes());
        assert!(less(Order::Signed, &a, &b));
    }

    #[test]
    fn counts_and_totals_add_and_extremes_keep_the_better() {
        let folds = [
            Fold { at: 0, how: How::Count },
            Fold { at: 8, how: How::SumInt },
            Fold { at: 32, how: How::Extreme { width: 8, keep: Keep::Least, order: Order::Signed } },
            Fold { at: 48, how: How::Extreme { width: 8, keep: Keep::Any, order: Order::Signed } },
        ];
        let row = |count: i64, sum: Option<i128>, min: Option<i64>, any: Option<i64>| {
            let mut r = vec![0u8; 64];
            put_i64(&mut r, 0, count);
            if let Some(s) = sum {
                put_i128(&mut r, 8, s);
                r[24] = 1;
            }
            if let Some(m) = min {
                put_i64(&mut r, 32, m);
                r[40] = 1;
            }
            if let Some(a) = any {
                put_i64(&mut r, 48, a);
                r[56] = 1;
            }
            r
        };
        let mut d = row(2, Some(5), Some(4), None);
        fold(&folds, &mut d, &row(3, Some(-7), Some(-1), Some(9)));
        assert_eq!(i64_at(&d, 0), 5);
        assert_eq!(i128_at(&d, 8), -2);
        assert_eq!(i64_at(&d, 32), -1);
        assert_eq!((i64_at(&d, 48), d[56]), (9, 1));
        fold(&folds, &mut d, &row(1, None, None, Some(1)));
        assert_eq!(i128_at(&d, 8), -2);
        assert_eq!(d[24], 1);
        assert_eq!(i64_at(&d, 48), 9);
    }
}

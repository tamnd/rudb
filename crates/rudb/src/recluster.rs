//! The reclusterer, `engine-v4/14-reads-stay-fast.md` section 14.6, for the tables this engine has.
//!
//! A table declared with `SET cluster_by` is sorted on its way in, a statement at a time, so a big
//! load lands in order and its parts line up with the buckets of the declaration. Rows that arrive
//! later a few at a time land in the parts a checkpoint appends after the table's, in the order
//! they came, and each of those parts covers whatever range its rows did. A query the declaration
//! lets skip all but one bucket then reads every one of those parts as well. Clustering decays.
//!
//! So a checkpoint that writes the file before the commit returns measures each clustered table
//! first, and one that has decayed far enough is put back in order and written again whole. The
//! measure is the section's overlap depth, taken over rows rather than parts: at the low end of each
//! part, how many rows a scan of that one bucket reads against how many the parts that are in place
//! give it. A part is out of place when another part lies strictly inside its range, which no two
//! parts of a sorted table do. A table loaded in order measures 1, and one whose bucket scans
//! read twice the rows they should measures 2, which is where `recluster_depth` starts a rewrite.
//!
//! The section rewrites the smallest set of stripes that brings the depth back under 1.5. Here the
//! parts out of place cover the whole range as often as not and a file cannot put rows between two
//! parts it already holds, so the whole table is written again. That costs the table's rows, which
//! is why the rows out of place have to be a tenth of it as well before it happens: each row that
//! arrived out of order is then written at most ten times more over, and a table taking a trickle
//! of rows is not written again at every checkpoint.
//!
//! The rows move to new numbers, which is a new frame for the table, the way a delete that keeps
//! the rows it does not take is. A checkpoint on a thread of its own cannot do that, because the
//! commits logged while it writes name rows by the numbers they had, so a commit that finds a
//! clustered table decayed checkpoints before it returns rather than start one, see
//! [`any_decayed`].

use std::cmp::Ordering;

use rudb_catalog::{Catalog, QualifiedName, Rows, Table};
use rudb_common::bounds::{Bound, scale_of};
use rudb_common::value::civil_from_days;
use rudb_common::{LogicalType, Result, Width};
use rudb_vector::{Chunk, VECTOR_SIZE};

/// How many times the rows out of place a table has to hold before it is written again, see the
/// module doc.
const SHARE: usize = 10;

/// What a table was before [`reorder`] put it back in order, for a write of the file that failed.
pub(crate) type Before = Vec<(QualifiedName, Table)>;

/// Whether some clustered table of `database` has decayed past `depth`.
pub(crate) fn any_decayed(catalog: &Catalog, database: &str, depth: f64) -> bool {
    catalog.stored_tables_in(database).any(|table| decayed(table, depth).is_some())
}

/// Puts each clustered table of `database` that has decayed past `depth` back in the order its
/// declaration asks for, and hands back what each one was.
///
/// # Errors
///
/// If the rows of one cannot be read or put back, which leaves every table as it was.
pub(crate) fn reorder(catalog: &mut Catalog, database: &str, depth: f64) -> Result<Before> {
    let due = catalog
        .stored_tables_in(database)
        .filter_map(|table| Some((table.name().clone(), decayed(table, depth)?)))
        .collect::<Vec<_>>();
    let workers = std::thread::available_parallelism().map_or(1, usize::from);
    let mut before = Vec::with_capacity(due.len());
    for (name, width) in due {
        let table = catalog.table(&name)?;
        let Some(clustering) = table.clustering() else { continue };
        before.push((name.clone(), table.clone()));
        let done = sorted(table, clustering.columns(), width)
            .and_then(|chunks| catalog.table_mut(&name)?.replace_all(chunks, workers));
        if let Err(error) = done {
            restore(catalog, before);
            return Err(error);
        }
    }
    Ok(before)
}

/// Puts back what [`reorder`] took, after a write of the file that failed, so the catalog numbers
/// the rows the way the file and the log still do.
pub(crate) fn restore(catalog: &mut Catalog, before: Before) {
    for (name, was) in before {
        if let Ok(table) = catalog.table_mut(&name) {
            *table = was;
        }
    }
}

/// The width a decayed clustered table is sorted by, or `None` for a table that is fine as it is,
/// is not clustered or is not in the file yet.
fn decayed(table: &Table, depth: f64) -> Option<Width> {
    if depth.is_nan() || depth <= 0.0 {
        return None;
    }
    let clustering = table.clustering()?;
    let reader = match table.rows() {
        Rows::Native(reader) | Rows::Grown(reader, _) | Rows::Masked(reader, ..) => reader,
        Rows::Memory(_) => return None,
    };
    let column = clustering.partition() as usize;
    let ty = &table.columns().get(column)?.ty;
    let ends = reader.part_ends(column);
    if ends.len() < 2 {
        return None;
    }
    let width = match clustering.width() {
        Width::Auto => auto(&ends, ty),
        width => width,
    };
    let parts = ends
        .into_iter()
        .filter_map(|(rows, ends)| {
            let (low, high) = ends?;
            Some(Part { low: bucket(&low, width, ty)?, high: bucket(&high, width, ty)?, rows })
        })
        .collect::<Vec<_>>();
    let decay = measure(&parts);
    (decay.depth > depth && decay.out.saturating_mul(SHARE) >= decay.rows).then_some(width)
}

/// The width an automatic declaration comes to over the rows the file holds, the way the binder
/// fits one to the rows of a load.
fn auto(ends: &[(usize, Option<(Bound, Bound)>)], ty: &LogicalType) -> Width {
    let rows = ends.iter().map(|(rows, _)| *rows as u64).sum::<u64>();
    let days = ends.iter().filter_map(|(_, ends)| ends.as_ref()).fold(None, |span, (low, high)| {
        let (low, high) = (day(low, ty), day(high, ty));
        match (span, low, high) {
            (None, Some(low), Some(high)) => Some((low, high)),
            (Some((first, last)), Some(low), Some(high)) => Some((low.min(first), high.max(last))),
            (span, ..) => span,
        }
    });
    let days = days.map_or(0, |(first, last)| u64::try_from(last - first).unwrap_or(0));
    Width::for_span(rows, days)
}

/// One part of a table, its two ends already put in the buckets of the declaration.
#[derive(Debug, Clone)]
struct Part {
    low: Bound,
    high: Bound,
    rows: usize,
}

/// How far a table's parts have drifted out of order.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Decay {
    /// The mean over the low ends of the parts of the rows a scan of that bucket reads, over the
    /// rows of the parts in place it reads. 1 for a table in order.
    depth: f64,
    /// The rows of the parts out of place.
    out: usize,
    /// The rows of every part.
    rows: usize,
}

/// The order of two bounds of one column, where the two are always of one domain.
fn order(one: &Bound, other: &Bound) -> Ordering {
    one.order(other).unwrap_or(Ordering::Equal)
}

/// See [`Decay`].
fn measure(parts: &[Part]) -> Decay {
    let rows = parts.iter().map(|part| part.rows).sum::<usize>();
    let mut by_low = parts.iter().collect::<Vec<_>>();
    by_low.sort_by(|one, other| order(&one.low, &other.low));
    // Out of place is holding another part strictly inside this one's range, which no two parts
    // of a sorted table do: they meet at most at an end. The parts that start after this one are
    // a run of `by_low`, and the one of them that ends first is the one to ask.
    let mut least: Vec<&Bound> = Vec::with_capacity(by_low.len());
    for part in by_low.iter().rev() {
        let high = match least.last() {
            Some(&held) if order(held, &part.high) == Ordering::Less => held,
            _ => &part.high,
        };
        least.push(high);
    }
    least.reverse();
    let placed = parts
        .iter()
        .map(|part| {
            let past =
                by_low.partition_point(|other| order(&other.low, &part.low) != Ordering::Greater);
            least.get(past).is_none_or(|high| order(high, &part.high) != Ordering::Less)
        })
        .collect::<Vec<_>>();
    let chosen = |wanted: bool| {
        parts
            .iter()
            .zip(&placed)
            .filter(move |(_, placed)| **placed == wanted)
            .map(|(part, _)| part)
    };
    let inside = Cover::of(chosen(true).collect());
    let outside = Cover::of(chosen(false).collect());
    let out = outside.rows;
    let (mut total, mut points) = (0.0, 0usize);
    for part in parts {
        let held = inside.at(&part.low);
        if held == 0 {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let ratio = (held + outside.at(&part.low)) as f64 / held as f64;
        total += ratio;
        points += 1;
    }
    #[allow(clippy::cast_precision_loss)]
    let depth = match points {
        0 if out > 0 => f64::INFINITY,
        0 => 1.0,
        points => total / points as f64,
    };
    Decay { depth, out, rows }
}

/// The rows of a set of parts that hold a point, by two sorted lists of their ends.
struct Cover<'a> {
    /// The low ends, sorted, each with the rows of every part up to and including it.
    lows: Vec<(&'a Bound, usize)>,
    /// The high ends, the same.
    highs: Vec<(&'a Bound, usize)>,
    rows: usize,
}

impl<'a> Cover<'a> {
    fn of(parts: Vec<&'a Part>) -> Self {
        let summed = |mut ends: Vec<(&'a Bound, usize)>| {
            ends.sort_by(|one, other| order(one.0, other.0));
            let mut running = 0;
            for end in &mut ends {
                running += end.1;
                end.1 = running;
            }
            ends
        };
        Self {
            lows: summed(parts.iter().map(|part| (&part.low, part.rows)).collect()),
            highs: summed(parts.iter().map(|part| (&part.high, part.rows)).collect()),
            rows: parts.iter().map(|part| part.rows).sum(),
        }
    }

    /// The rows of the parts that start at or before `point`, less those that end before it.
    fn at(&self, point: &Bound) -> usize {
        let upto = |ends: &[(&Bound, usize)], past: Ordering| {
            let count = ends.partition_point(|(end, _)| {
                let side = order(end, point);
                side == Ordering::Less || side == past
            });
            count.checked_sub(1).map_or(0, |last| ends[last].1)
        };
        upto(&self.lows, Ordering::Equal) - upto(&self.highs, Ordering::Less)
    }
}

/// The bucket `bound` falls in, as an integer that orders the buckets, for a column of `ty`.
///
/// The calendar is UTC's. A declaration on a zoned timestamp sorts a load by the session's, and
/// the two differ by a few hours at the edges of a bucket, which moves a row to the next part and
/// no answer.
fn bucket(bound: &Bound, width: Width, ty: &LogicalType) -> Option<Bound> {
    if width == Width::Exact {
        return Some(bound.clone());
    }
    let (year, month, _) = civil_from_days(i32::try_from(day(bound, ty)?).ok()?);
    let months = i128::from(year) * 12 + i128::from(month) - 1;
    Some(Bound::Int(match width {
        Width::Quarter | Width::Auto => months.div_euclid(3),
        Width::Year => months.div_euclid(12),
        _ => months,
    }))
}

/// The day `bound` falls on, counted from 1970, for a date or a timestamp of `ty`.
fn day(bound: &Bound, ty: &LogicalType) -> Option<i64> {
    let per_day = |unscaled: i128, scale: u8| {
        let day = 86_400_i128.checked_mul(10_i128.checked_pow(u32::from(scale))?)?;
        i64::try_from(unscaled.div_euclid(day)).ok()
    };
    match (bound, ty) {
        (Bound::Int(days), LogicalType::Date) => i64::try_from(*days).ok(),
        (Bound::Int(unscaled), _) => per_day(*unscaled, scale_of(ty)?),
        (Bound::Scaled { unscaled, scale }, _) => per_day(*unscaled, *scale),
        _ => None,
    }
}

/// The rows of `table` in the order `columns` declares, the first of them bucketed by `width`, a
/// vector's worth to a chunk. Rows with the same key keep the order they had, and nulls go last,
/// the way the binder sorts a load.
fn sorted(table: &Table, columns: &[u32], width: Width) -> Result<Vec<Chunk>> {
    let rows = table.rows();
    let fields = table.columns();
    let every = (0..fields.len()).collect::<Vec<_>>();
    let mut chunks = Vec::with_capacity(rows.chunk_count());
    for at in 0..rows.chunk_count() {
        chunks.push(rows.read(at, &every)?.settled()?);
    }
    let keyed = columns.iter().map(|&column| column as usize).collect::<Vec<_>>();
    let mut keys: Vec<Vec<Option<Bound>>> = Vec::with_capacity(rows.len());
    for chunk in &chunks {
        for row in 0..chunk.len() {
            let mut key = Vec::with_capacity(keyed.len());
            for (at, &column) in keyed.iter().enumerate() {
                let bound = Bound::of_value(&chunk.try_value_at(row, column)?);
                key.push(match (at, bound) {
                    (0, Some(bound)) => bucket(&bound, width, &fields[column].ty),
                    (_, bound) => bound,
                });
            }
            keys.push(key);
        }
    }
    let mut placed = (0..keys.len()).collect::<Vec<_>>();
    placed.sort_by(|&one, &other| {
        for (left, right) in keys[one].iter().zip(&keys[other]) {
            let side = match (left, right) {
                (Some(left), Some(right)) => order(left, right),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            if side != Ordering::Equal {
                return side;
            }
        }
        Ordering::Equal
    });
    drop(keys);
    let mut laid = Vec::with_capacity(fields.len());
    for (column, field) in fields.iter().enumerate() {
        let pieces =
            chunks.iter().map(|chunk| chunk.column(column).cloned()).collect::<Result<Vec<_>>>()?;
        laid.push(rudb_vector::interleave(&field.ty, &pieces, &placed)?);
    }
    drop(chunks);
    let total = placed.len();
    let mut out = Vec::with_capacity(total.div_ceil(VECTOR_SIZE));
    let mut at = 0;
    while at < total {
        let len = VECTOR_SIZE.min(total - at);
        let cut = laid.iter().map(|vector| vector.slice(at, len)).collect::<Result<Vec<_>>>()?;
        out.push(Chunk::with_rows(cut, len)?);
        at += len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(low: i128, high: i128, rows: usize) -> Part {
        Part { low: Bound::Int(low), high: Bound::Int(high), rows }
    }

    #[test]
    fn parts_in_order_measure_one_with_nothing_out_of_place() {
        // Ten buckets of a hundred rows each, in parts of fifty, with a part across every edge.
        let mut parts = Vec::new();
        for bucket in 0..10 {
            parts.push(part(bucket, bucket, 50));
            parts.push(part(bucket, bucket + 1, 50));
        }
        let decay = measure(&parts);
        assert_eq!(decay.out, 0);
        assert_eq!(decay.rows, 1000);
        assert!((decay.depth - 1.0).abs() < 1e-9, "{decay:?}");

        // A key with no two rows alike sorts into parts that only ever meet at an end.
        let parts = (0..20).map(|at| part(at * 100, at * 100 + 99, 100)).collect::<Vec<_>>();
        let decay = measure(&parts);
        assert_eq!((decay.out, decay.depth), (0, 1.0));
    }

    #[test]
    fn parts_across_the_whole_range_are_out_of_place_and_raise_the_depth() {
        let mut parts = (0..20).map(|at| part(at * 100, at * 100 + 99, 100)).collect::<Vec<_>>();
        parts.push(part(3, 1990, 150));
        parts.push(part(0, 1999, 150));
        let decay = measure(&parts);
        assert_eq!(decay.out, 300);
        assert_eq!(decay.rows, 2300);
        // A scan of a bucket reads its own hundred rows and both wide parts, except at 0, where
        // the part from 3 has not started yet. The part from 0 to 99 holds the start of the one
        // from 3 and is still in place, since nothing is inside it whole.
        let want = (2.0 * 2.5 + 20.0 * 4.0) / 22.0;
        assert!((decay.depth - want).abs() < 1e-9, "{decay:?} against {want}");
    }

    #[test]
    fn a_table_of_nothing_but_parts_out_of_place_measures_past_any_depth() {
        let parts = [part(0, 100, 10), part(5, 90, 10), part(1, 99, 10)];
        let decay = measure(&parts);
        // The part from 5 to 90 holds no other part inside it and stands in place, alone.
        assert_eq!(decay.out, 20);
        assert!(decay.depth > 2.0, "{decay:?}");
        assert_eq!(measure(&[part(0, 100, 10), part(0, 100, 10)]).out, 0);
    }

    #[test]
    fn buckets_follow_the_calendar_of_a_date_and_a_timestamp() {
        let date = LogicalType::Date;
        // 1970-01-31, 1970-02-01 and 1969-12-31.
        let month = |days: i128| bucket(&Bound::Int(days), Width::Month, &date);
        assert_eq!(month(30), Some(Bound::Int(1970 * 12)));
        assert_eq!(month(31), Some(Bound::Int(1970 * 12 + 1)));
        assert_eq!(month(-1), Some(Bound::Int(1969 * 12 + 11)));
        assert_eq!(bucket(&Bound::Int(-1), Width::Year, &date), Some(Bound::Int(1969)));
        assert_eq!(
            bucket(&Bound::Int(31 + 28 + 31), Width::Quarter, &date),
            Some(Bound::Int(1970 * 4 + 1))
        );
        let micros = Bound::Scaled { unscaled: 86_400_000_000 * 59 - 1, scale: 6 };
        assert_eq!(
            bucket(&micros, Width::Month, &LogicalType::Timestamp),
            Some(Bound::Int(1970 * 12 + 1))
        );
        let seconds = Bound::Int(86_400 * 59);
        assert_eq!(
            bucket(&seconds, Width::Month, &LogicalType::TimestampS),
            Some(Bound::Int(1970 * 12 + 2))
        );
        assert_eq!(
            bucket(&Bound::Int(7), Width::Exact, &LogicalType::Integer),
            Some(Bound::Int(7))
        );
    }
}

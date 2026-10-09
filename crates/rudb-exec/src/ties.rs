//! The order PostgreSQL's in-memory sort leaves the rows in when their keys tie.
//!
//! PostgreSQL's sort is not stable. Rows that tie come out in the order its quicksort leaves them
//! in, or its radix sort when the first key compares as an integer, and that order depends only on
//! the order the rows arrived in. A window function that numbers the rows or reads a neighbor sees
//! that order, and so does a query that sorts on fewer columns than it shows. So under
//! [`TieOrder::Postgres`](rudb_common::TieOrder) a sort first finds its answer the way it always
//! does. Then it runs the pin's algorithm over the rows in the order they arrived, and compares
//! only which group of tied rows each one is in. That puts the rows of each group where the pin
//! puts them, and it moves nothing else, because the groups are already in order.
//!
//! The two sorts are ports of `sort_template.h` and of `radix_sort_tuple` in `tuplesort.c` of the
//! pin, step for step. An arrangement of tied rows is only right if every swap is the same.
//!
//! Two things are not here. A sort with a `LIMIT` that keeps a bounded heap, and a sort that spills
//! and merges its runs, arrange their ties in other ways.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};

use rudb_common::{LogicalType, Value};
use rudb_plan::SortKey;

use crate::sort::Arrival;

/// How many rows a sort needs before the pin sorts them with a radix sort, `QSORT_THRESHOLD`.
pub(crate) const RADIX: usize = 40;

/// Days from 1970-01-01, where a date of the engine counts from, to 2000-01-01, where one of the
/// pin counts from.
const DAYS_TO_2000: i32 = 10_957;

/// The same distance in microseconds, for a timestamp.
const MICROS_TO_2000: i64 = 946_684_800_000_000;

/// How the pin holds the first key of a sort beside each row, when the key compares as an integer.
///
/// That is what decides between the two sorts. A key of any other type is compared through a
/// function and always takes the quicksort, and so has no `Leading`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Leading {
    held: Held,
    descending: bool,
    nulls_first: bool,
}

/// What the first key is held as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// `integer` and `date`, compared as a signed 32 bit integer.
    Int32,
    /// `bigint`, `timestamp` and `timestamptz`, compared as a signed 64 bit integer.
    Int64,
    /// `text` in the C collation, abbreviated to its first eight bytes and compared unsigned,
    /// unless the pin gives the abbreviation up.
    Text,
    /// `uuid`, abbreviated to its first eight bytes and compared unsigned.
    Uuid,
}

impl Leading {
    /// How the pin holds a first key of this type, or `None` when it compares it through a
    /// function.
    pub(crate) fn of(ty: &LogicalType, key: SortKey) -> Option<Self> {
        let held = match ty {
            LogicalType::Integer | LogicalType::Date => Held::Int32,
            LogicalType::BigInt | LogicalType::Timestamp | LogicalType::TimestampTz => Held::Int64,
            LogicalType::Varchar => Held::Text,
            LogicalType::Uuid => Held::Uuid,
            _ => return None,
        };
        Some(Self { held, descending: key.descending, nulls_first: key.nulls_first })
    }

    /// The first key of each row as the pin's radix sort reads it, in the order given.
    pub(crate) fn datums<'v>(self, values: impl Iterator<Item = &'v Value>) -> Datums {
        let mut held = Vec::new();
        let mut full = Vec::new();
        for value in values {
            held.push(self.datum(value).map(|datum| self.normalized(datum)));
            if self.held == Held::Text {
                full.push(match value {
                    Value::Varchar(text) => whole(text.as_bytes()),
                    _ => 0,
                });
            }
        }
        Datums { held, full, abbreviated: matches!(self.held, Held::Text), leading: self }
    }

    /// The datum the pin holds for a value, before it is normalized, or `None` for a null.
    fn datum(self, value: &Value) -> Option<u64> {
        let datum = match (self.held, value) {
            (Held::Int32, Value::Integer(value)) => u64::from(*value as u32),
            (Held::Int32, Value::Date(days)) => {
                // The engine's infinities are at plus and minus `i32::MAX`, the pin's at the two
                // ends of the `i32`.
                let days = match *days {
                    i32::MAX => i32::MAX,
                    days if days == -i32::MAX => i32::MIN,
                    days => days.wrapping_sub(DAYS_TO_2000),
                };
                u64::from(days as u32)
            }
            (Held::Int64, Value::BigInt(value)) => *value as u64,
            (Held::Int64, Value::Timestamp(micros) | Value::TimestampTz(micros)) => {
                let micros = match *micros {
                    i64::MAX => i64::MAX,
                    micros if micros == -i64::MAX => i64::MIN,
                    micros => micros.wrapping_sub(MICROS_TO_2000),
                };
                micros as u64
            }
            (Held::Text, Value::Varchar(text)) => {
                let mut prefix = [0_u8; 8];
                let bytes = text.as_bytes();
                let len = bytes.len().min(8);
                prefix[..len].copy_from_slice(&bytes[..len]);
                u64::from_be_bytes(prefix)
            }
            (Held::Uuid, Value::Uuid(held)) => (rudb_common::uuid::to_number(*held) >> 64) as u64,
            _ => return None,
        };
        Some(datum)
    }

    /// A datum turned into one whose unsigned order is the key's order, `normalize_datum`.
    fn normalized(self, datum: u64) -> u64 {
        let normal = match self.held {
            Held::Int32 => u64::from((datum as u32).wrapping_add(1 << 31)),
            Held::Int64 => datum.wrapping_add(1 << 63),
            Held::Text | Held::Uuid => datum,
        };
        if self.descending { !normal } else { normal }
    }
}

/// What the pin keeps of a whole string for its estimate of how many distinct strings there are:
/// the first 64 bytes, and the length when the string is longer.
fn whole(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes[..bytes.len().min(64)].hash(&mut hasher);
    if bytes.len() > 64 {
        bytes.len().hash(&mut hasher);
    }
    hasher.finish()
}

/// The first key of every row, in sorted order, as the pin's radix sort reads it.
#[derive(Debug)]
pub(crate) struct Datums {
    leading: Leading,
    /// The normalized datum of each row, or `None` where the key is null.
    held: Vec<Option<u64>>,
    /// For an abbreviated string, what [`whole`] keeps of each one.
    full: Vec<u64>,
    abbreviated: bool,
}

impl Datums {
    /// Whether the pin still holds the key as an integer once every row has arrived.
    ///
    /// An abbreviated string is given up when the abbreviations turn out to be much less distinct
    /// than the strings, which `varstr_abbrev_abort` checks each time the count of rows doubles
    /// from ten. The pin estimates both counts with a HyperLogLog. They are counted exactly here,
    /// which only decides differently when the two are close to the line.
    fn kept(&self, arrived: &[u32]) -> bool {
        if !self.abbreviated {
            return true;
        }
        let mut next = 10;
        let mut proportion = 0.20;
        let mut abbreviations = HashSet::new();
        let mut strings = HashSet::new();
        for (count, &row) in arrived.iter().enumerate() {
            let Some(datum) = self.held[row as usize] else { continue };
            if count >= next {
                next *= 2;
                if count >= 100 {
                    let abbreviated = abbreviations.len().max(1) as f64;
                    let whole = strings.len().max(1) as f64;
                    if abbreviated <= whole * proportion {
                        return false;
                    }
                    if count > 10_000 {
                        proportion *= 0.65;
                    }
                }
            }
            let datum = if self.leading.descending { !datum } else { datum };
            abbreviations.insert((datum as u32) ^ ((datum >> 32) as u32));
            strings.insert(self.full[row as usize]);
        }
        true
    }
}

/// A group number for each row in sorted order, where a row that ties the one before it takes its
/// number.
pub(crate) fn groups<T>(rows: &[T], mut tie: impl FnMut(&T, &T) -> bool) -> Vec<u32> {
    let mut groups = Vec::with_capacity(rows.len());
    let mut group = 0_u32;
    for (at, row) in rows.iter().enumerate() {
        if at > 0 && !tie(&rows[at - 1], row) {
            group += 1;
        }
        groups.push(group);
    }
    groups
}

/// Where each row of a sorted answer goes when its ties are put in the pin's order.
///
/// `groups` is from [`groups`] and `arrivals` is where each row arrived, both in sorted order.
/// `first` is the first key of each row when the key compares as an integer. The answer is the
/// sorted rows as positions in that order, or `None` when no two rows tie and nothing moves.
pub(crate) fn arrangement(
    groups: &[u32],
    arrivals: &[Arrival],
    first: Option<&Datums>,
) -> Option<Vec<usize>> {
    if groups.windows(2).all(|pair| pair[0] != pair[1]) {
        return None;
    }
    let mut arrived: Vec<u32> = (0..u32::try_from(groups.len()).ok()?).collect();
    arrived.sort_unstable_by_key(|&row| arrivals[row as usize]);
    let compare = |left: u32, right: u32| groups[left as usize].cmp(&groups[right as usize]);
    match first {
        Some(first) if arrived.len() >= RADIX && first.kept(&arrived) => {
            radix_sort_tuple(&mut arrived, first, &compare);
        }
        _ => quicksort(&mut arrived, &compare),
    }
    Some(arrived.into_iter().map(|row| row as usize).collect())
}

/// `qsort` of `sort_template.h`: Bentley and McIlroy's quicksort, with an insertion sort under
/// seven rows, a check for rows already in order, and a median of three, or of nine over forty.
fn quicksort(rows: &mut [u32], compare: &impl Fn(u32, u32) -> Ordering) {
    let (mut lo, mut n) = (0, rows.len());
    loop {
        if n < 7 {
            for m in lo + 1..lo + n {
                let mut l = m;
                while l > lo && compare(rows[l - 1], rows[l]) == Ordering::Greater {
                    rows.swap(l, l - 1);
                    l -= 1;
                }
            }
            return;
        }
        if (lo + 1..lo + n).all(|m| compare(rows[m - 1], rows[m]) != Ordering::Greater) {
            return;
        }
        let mut pm = lo + n / 2;
        if n > 7 {
            let mut pl = lo;
            let mut pn = lo + n - 1;
            if n > 40 {
                let d = n / 8;
                pl = median(rows, pl, pl + d, pl + 2 * d, compare);
                pm = median(rows, pm - d, pm, pm + d, compare);
                pn = median(rows, pn - 2 * d, pn - d, pn, compare);
            }
            pm = median(rows, pl, pm, pn, compare);
        }
        rows.swap(lo, pm);
        let (mut pa, mut pb) = (lo + 1, lo + 1);
        let (mut pc, mut pd) = (lo + n - 1, lo + n - 1);
        loop {
            while pb <= pc {
                let order = compare(rows[pb], rows[lo]);
                if order == Ordering::Greater {
                    break;
                }
                if order == Ordering::Equal {
                    rows.swap(pa, pb);
                    pa += 1;
                }
                pb += 1;
            }
            while pb <= pc {
                let order = compare(rows[pc], rows[lo]);
                if order == Ordering::Less {
                    break;
                }
                if order == Ordering::Equal {
                    rows.swap(pc, pd);
                    pd -= 1;
                }
                pc -= 1;
            }
            if pb > pc {
                break;
            }
            rows.swap(pb, pc);
            pb += 1;
            pc -= 1;
        }
        let pn = lo + n;
        let d1 = (pa - lo).min(pb - pa);
        swap_many(rows, lo, pb - d1, d1);
        let d1 = (pd - pc).min(pn - pd - 1);
        swap_many(rows, pb, pn - d1, d1);
        let d1 = pb - pa;
        let d2 = pd - pc;
        // The smaller side is sorted first and the larger one is the next turn of the loop, the
        // order the pin takes them in.
        if d1 <= d2 {
            quicksort(&mut rows[lo..lo + d1], compare);
            lo = pn - d2;
            n = d2;
        } else {
            quicksort(&mut rows[pn - d2..pn], compare);
            n = d1;
        }
    }
}

/// `med3` of `sort_template.h`.
fn median(
    rows: &[u32],
    a: usize,
    b: usize,
    c: usize,
    compare: &impl Fn(u32, u32) -> Ordering,
) -> usize {
    if compare(rows[a], rows[b]) == Ordering::Less {
        if compare(rows[b], rows[c]) == Ordering::Less {
            b
        } else if compare(rows[a], rows[c]) == Ordering::Less {
            c
        } else {
            a
        }
    } else if compare(rows[b], rows[c]) == Ordering::Greater {
        b
    } else if compare(rows[a], rows[c]) == Ordering::Less {
        a
    } else {
        c
    }
}

/// `swapn` of `sort_template.h`.
fn swap_many(rows: &mut [u32], a: usize, b: usize, n: usize) {
    for at in 0..n {
        rows.swap(a + at, b + at);
    }
}

/// `radix_sort_tuple` of `tuplesort.c`: the rows with a null first key are moved to their end, and
/// the rest are sorted by a radix sort when there are enough of them.
///
/// The comparisons of the pin that only reach past the first key, on the rows with a null one and
/// on rows whose datums are the same, are made here with the whole comparison. Among rows that
/// already agree on the first key the two are the same answer.
fn radix_sort_tuple(rows: &mut [u32], first: &Datums, compare: &impl Fn(u32, u32) -> Ordering) {
    let n = rows.len();
    let nulls_first = first.leading.nulls_first;
    let left = |row: u32| first.held[row as usize].is_none() == nulls_first;
    let mut d1 = 0;
    while d1 < n && left(rows[d1]) {
        d1 += 1;
    }
    // The branchless cyclic Lomuto partition of the pin, which is not stable either.
    if d1 + 1 < n {
        let (mut i, mut j) = (d1, d1);
        let gap = rows[d1];
        while j < n - 1 {
            rows[j] = rows[i];
            j += 1;
            rows[i] = rows[j];
            i += usize::from(left(rows[i]));
        }
        rows[j] = rows[i];
        rows[i] = gap;
        i += usize::from(left(rows[i]));
        d1 = i;
    }
    let (nulls, values) = if nulls_first {
        rows.split_at_mut(d1)
    } else {
        let (values, nulls) = rows.split_at_mut(d1);
        (nulls, values)
    };
    quicksort(nulls, compare);
    if values.len() < RADIX {
        quicksort(values, compare);
    } else if values.windows(2).any(|pair| compare(pair[0], pair[1]) == Ordering::Greater) {
        let mut bytes = vec![0_u8; first.held.len()];
        radix_sort_recursive(values, 0, first, &mut bytes, compare);
    }
}

/// `radix_sort_recursive` of `tuplesort.c`, an American flag sort a byte at a time from the most
/// significant one, with the pin's way of moving rows into place.
fn radix_sort_recursive(
    rows: &mut [u32],
    level: usize,
    first: &Datums,
    bytes: &mut [u8],
    compare: &impl Fn(u32, u32) -> Ordering,
) {
    let datum = |row: u32| first.held[row as usize].unwrap_or_default();
    let byte = |datum: u64| (datum >> ((7 - level) * 8)) as u8;
    let mut count = [0_usize; 256];
    let reference = datum(rows[0]);
    let mut differing = 0_u64;
    for &row in rows.iter() {
        let this = datum(row);
        differing |= reference ^ this;
        let at = byte(this);
        bytes[row as usize] = at;
        count[usize::from(at)] += 1;
    }
    let mut offset = [0_usize; 256];
    let mut next = [0_usize; 256];
    let mut partitions = Vec::with_capacity(256);
    let mut total = 0;
    for at in 0..256 {
        if count[at] != 0 {
            offset[at] = total;
            total += count[at];
            partitions.push(at);
        }
        next[at] = total;
    }
    let mut remaining = partitions.len();
    while remaining > 1 {
        remaining = partitions.len();
        for &at in &partitions {
            let mut row = offset[at];
            while row < next[at] {
                let into = usize::from(bytes[rows[row] as usize]);
                let to = offset[into];
                offset[into] += 1;
                rows.swap(row, to);
                row += 1;
            }
            if offset[at] == next[at] {
                remaining -= 1;
            }
        }
    }
    let next_level = if partitions.len() == 1 {
        if differing == 0 { 8 } else { 7 - (63 - differing.leading_zeros() as usize) / 8 }
    } else {
        level + 1
    };
    let mut start = 0;
    for &at in &partitions {
        let end = next[at];
        let part = &mut rows[start..end];
        if part.len() > 1 {
            if next_level < 8 && part.len() >= RADIX {
                radix_sort_recursive(part, next_level, first, bytes, compare);
            } else {
                quicksort(part, compare);
            }
        }
        start = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `generate_series(1, n)` sorted by `key`, ascending or descending, with the order the pin
    /// leaves the rows that tie in. `first` is the type the pin holds the key as, if any.
    fn series(
        n: i64,
        key: impl Fn(i64) -> i64,
        descending: bool,
        first: Option<LogicalType>,
    ) -> String {
        let rows: Vec<i64> = (1..=n).collect();
        let mut sorted = rows.clone();
        sorted.sort_by_key(|&x| if descending { -key(x) } else { key(x) });
        let groups = groups(&sorted, |&left, &right| key(left) == key(right));
        let arrivals: Vec<Arrival> = sorted.iter().map(|&x| (0, x as u64)).collect();
        let values: Vec<Value> = sorted.iter().map(|&x| Value::BigInt(key(x))).collect();
        let sort = SortKey { expr: 0, descending, nulls_first: false };
        let datums =
            first.and_then(|ty| Leading::of(&ty, sort)).map(|first| first.datums(values.iter()));
        let order = arrangement(&groups, &arrivals, datums.as_ref())
            .unwrap_or_else(|| (0..sorted.len()).collect());
        order.iter().map(|&at| sorted[at].to_string()).collect::<Vec<_>>().join(",")
    }

    #[test]
    fn rows_that_never_tie_do_not_move() {
        assert_eq!(arrangement(&[0, 1, 2], &[(0, 2), (0, 0), (0, 1)], None), None);
    }

    #[test]
    fn a_quicksort_moves_tied_rows_out_of_their_arrival_order() {
        // `SELECT x FROM generate_series(1, 10) x ORDER BY x % 3` on the pin.
        assert_eq!(series(10, |x| x % 3, false, None), "9,6,3,10,4,7,1,8,5,2");
        // The same over fifty rows by a `numeric`, which is never a radix sort.
        assert_eq!(
            series(50, |x| x % 4, false, None),
            "4,24,20,16,48,40,36,8,44,32,28,12,1,5,9,13,17,21,25,29,33,37,41,45,49,26,50,30,10,46,\
             34,38,6,2,42,18,22,14,27,11,39,15,31,43,3,23,35,7,47,19"
        );
    }

    #[test]
    fn forty_rows_or_more_with_an_integer_key_take_the_radix_sort() {
        // `ORDER BY (x % 4)::bigint` and `ORDER BY (x % 4)::bigint DESC` on the pin.
        assert_eq!(
            series(50, |x| x % 4, false, Some(LogicalType::BigInt)),
            "4,8,12,16,20,24,32,36,48,28,40,44,1,5,9,17,21,25,33,37,49,41,29,45,13,2,6,10,18,22,\
             34,38,46,50,26,14,30,42,3,7,11,19,23,31,35,47,39,43,27,15"
        );
        assert_eq!(
            series(50, |x| x % 4, true, Some(LogicalType::BigInt)),
            "3,7,11,19,23,35,47,15,27,31,39,43,2,6,10,18,22,34,38,50,26,14,42,46,30,1,5,9,17,21,\
             25,33,37,49,41,29,45,13,4,8,12,16,20,24,32,36,48,40,28,44"
        );
    }
}

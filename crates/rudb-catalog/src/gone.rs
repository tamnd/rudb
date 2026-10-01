//! The rows a `DELETE` took out of a committed file, `engine-v4/03-the-shape.md` section 3.5.
//!
//! A file is never changed in place, so a delete on a table that lives in one is recorded beside
//! it rather than by writing the rest of the table again. [`Gone`] is that record: one bit a row
//! for each part that lost any, and nothing at all for a part that lost none, which for a table
//! that was loaded once and trimmed a little is almost every part.
//!
//! An `UPDATE` is recorded here too, as a [`Patch`] per part it changed: the rows it wrote, kept
//! at the place in the part of the rows they replace, so the rows keep their numbers and a read of
//! the part lays the new rows over the old.

use std::sync::Arc;

use rudb_common::LogicalType;
use rudb_common::{Error, Result};
use rudb_vector::{Chunk, Vector, assemble};

/// The rows an `UPDATE` wrote into one part of a file.
#[derive(Debug)]
pub struct Patch {
    /// The rows of the part that hold new values, rising, counted the way the file counts them.
    slots: Vec<u32>,
    /// The new rows, every column of the table, one for each of `slots` in the same order.
    rows: Chunk,
}

impl Patch {
    /// The rows of the part that hold new values, rising.
    #[must_use]
    pub fn slots(&self) -> &[u32] {
        &self.slots
    }

    /// The new rows, every column, in the order of [`Self::slots`].
    #[must_use]
    pub fn rows(&self) -> &Chunk {
        &self.rows
    }

    /// Column `column` of a part of `rows` rows read from the file, with the new rows laid over
    /// the ones they replace.
    ///
    /// # Errors
    ///
    /// If the column is not one of the patch's or a slot is past the part.
    pub fn over(&self, ty: &LogicalType, read: &Vector, column: usize) -> Result<Vector> {
        let rows = read.len();
        let mut order = (0..rows).collect::<Vec<_>>();
        for (at, &slot) in self.slots.iter().enumerate() {
            *order
                .get_mut(slot as usize)
                .ok_or_else(|| Error::internal("an update names a row past its part"))? = rows + at;
        }
        let new = self.rows.column(column)?.clone();
        Ok(assemble::interleave(ty, &[read.clone(), new], &order)?.loosened())
    }
}

/// The deleted rows of a file, by part, and the rows an update wrote over.
#[derive(Debug, Clone, Default)]
pub struct Gone {
    /// For each part, a bit for each of its rows that is gone, or `None` when none is.
    parts: Vec<Option<Arc<[u64]>>>,
    /// How many rows each part lost, so a part's live length is a subtraction.
    counts: Vec<u32>,
    /// How many rows are gone in all.
    total: usize,
    /// For each part, the rows an update wrote into it, or `None` when it wrote none.
    patches: Vec<Option<Arc<Patch>>>,
    /// For each column, whether an update wrote it, so what the file says about the others still
    /// holds. Empty when nothing was updated.
    touched: Vec<bool>,
}

/// Two records are the same when they mark the same rows and hold the same patches, which are
/// compared by what they point at because nothing reads a patch's rows to compare them.
impl PartialEq for Gone {
    fn eq(&self, other: &Self) -> bool {
        self.parts == other.parts
            && self.total == other.total
            && self.touched == other.touched
            && self.patches.len() == other.patches.len()
            && self.patches.iter().zip(&other.patches).all(|pair| match pair {
                (Some(one), Some(two)) => Arc::ptr_eq(one, two),
                (None, None) => true,
                _ => false,
            })
    }
}

impl Gone {
    /// No row gone from a file of `parts` parts.
    #[must_use]
    pub fn none(parts: usize) -> Self {
        Self {
            parts: vec![None; parts],
            counts: vec![0; parts],
            total: 0,
            patches: vec![None; parts],
            touched: Vec::new(),
        }
    }

    /// The rows a file of `parts` parts records as gone, see [`rudb_native::GoneRows`].
    ///
    /// # Errors
    ///
    /// If the record names a part past the file's.
    pub fn stored(parts: usize, stored: &rudb_native::GoneRows) -> Result<Self> {
        let mut gone = Self::none(parts);
        for (part, bits) in &stored.parts {
            let held = gone
                .parts
                .get_mut(*part)
                .ok_or_else(|| Error::internal("gone rows name a part the file does not have"))?;
            let lost = bits.iter().map(|word| word.count_ones()).sum::<u32>();
            *held = Some(Arc::from(&bits[..]));
            gone.counts[*part] = lost;
            gone.total += lost as usize;
        }
        Ok(gone)
    }

    /// The record of these rows the file keeps, see [`rudb_native::GoneRows`].
    #[must_use]
    pub fn marks(&self) -> rudb_native::GoneRows {
        let parts = self
            .parts
            .iter()
            .enumerate()
            .filter_map(|(part, bits)| bits.as_ref().map(|bits| (part, Box::from(&bits[..]))))
            .collect();
        rudb_native::GoneRows { parts, total: self.total, sums: Vec::new() }
    }

    /// How many rows are gone in all.
    #[must_use]
    pub fn total(&self) -> usize {
        self.total
    }

    /// Whether no row is gone.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Whether an update wrote any row.
    #[must_use]
    pub fn is_patched(&self) -> bool {
        !self.touched.is_empty()
    }

    /// Whether an update wrote column `column` of any row.
    #[must_use]
    pub fn touched(&self, column: usize) -> bool {
        self.touched.get(column).copied().unwrap_or(false)
    }

    /// The rows an update wrote into part `part`, if it wrote any.
    #[must_use]
    pub fn patch(&self, part: usize) -> Option<&Patch> {
        self.patches.get(part).and_then(Option::as_deref)
    }

    /// Whether an update wrote one of `columns` in part `part`, which is when what the file says
    /// about that part and those columns stops being true.
    #[must_use]
    pub fn changes(&self, part: usize, columns: impl IntoIterator<Item = usize>) -> bool {
        self.patch(part).is_some() && columns.into_iter().any(|column| self.touched(column))
    }

    /// Writes new rows over rows of one part, `slots` being the rows, rising, by the file's count,
    /// `rows` every column of the new rows in the same order, and `columns` the columns the update
    /// set. A row written before keeps the newer of its two.
    ///
    /// # Errors
    ///
    /// If the part is not one of the file's, the slots do not rise, or the rows do not match them.
    pub fn put(
        &mut self,
        part: usize,
        slots: Vec<u32>,
        rows: Chunk,
        columns: &[usize],
    ) -> Result<()> {
        let held = self
            .patches
            .get_mut(part)
            .ok_or_else(|| Error::internal("an update names a part the file does not have"))?;
        if rows.len() != slots.len() || slots.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(Error::internal("an update's rows do not match the rows it names"));
        }
        let rows = rows.settled()?;
        let patch = match held.as_deref() {
            None => {
                // Copied out, so the patch holds its own rows and not the statement's chunks.
                let order = (0..rows.len()).collect::<Vec<_>>();
                let columns = rows
                    .columns()
                    .iter()
                    .map(|column| {
                        assemble::interleave(column.logical_type(), &[column.clone()], &order)
                    })
                    .collect::<Result<Vec<_>>>()?;
                Patch { slots, rows: Chunk::with_rows(columns, order.len())? }
            }
            Some(old) => {
                let before = old.slots.len();
                let mut merged = Vec::with_capacity(before + slots.len());
                let mut order = Vec::with_capacity(before + slots.len());
                let (mut one, mut two) = (0, 0);
                while one < before || two < slots.len() {
                    let take_new =
                        two < slots.len() && (one == before || slots[two] <= old.slots[one]);
                    if take_new {
                        if one < before && slots[two] == old.slots[one] {
                            one += 1;
                        }
                        merged.push(slots[two]);
                        order.push(before + two);
                        two += 1;
                    } else {
                        merged.push(old.slots[one]);
                        order.push(one);
                        one += 1;
                    }
                }
                let mut columns = Vec::with_capacity(rows.width());
                for (old, new) in old.rows.columns().iter().zip(rows.columns()) {
                    columns.push(assemble::interleave(
                        old.logical_type(),
                        &[old.clone(), new.clone()],
                        &order,
                    )?);
                }
                Patch { slots: merged, rows: Chunk::with_rows(columns, order.len())? }
            }
        };
        *held = Some(Arc::new(patch));
        let width = rows.width().max(self.touched.len());
        self.touched.resize(width, false);
        for &column in columns {
            *self.touched.get_mut(column).ok_or_else(|| {
                Error::internal("an update names a column the table does not have")
            })? = true;
        }
        Ok(())
    }

    /// How many rows part `part` lost.
    #[must_use]
    pub fn lost(&self, part: usize) -> usize {
        self.counts.get(part).map_or(0, |&count| count as usize)
    }

    /// Whether row `row` of part `part` is gone.
    #[must_use]
    pub fn contains(&self, part: usize, row: usize) -> bool {
        self.parts.get(part).and_then(Option::as_ref).is_some_and(|bits| {
            bits.get(row / 64).is_some_and(|word| word & (1 << (row % 64)) != 0)
        })
    }

    /// The rows of part `part`, which has `rows` of them, that are still there, or `None` when
    /// every one of them is.
    #[must_use]
    pub fn live(&self, part: usize, rows: usize) -> Option<Vec<u32>> {
        let bits = self.parts.get(part)?.as_ref()?;
        let mut live = Vec::with_capacity(rows.saturating_sub(self.lost(part)));
        for row in 0..rows {
            if bits[row / 64] & (1 << (row % 64)) == 0 {
                live.push(row as u32);
            }
        }
        Some(live)
    }

    /// The rows of part `part` that are gone, rising.
    #[must_use]
    pub fn slots(&self, part: usize) -> Vec<u32> {
        let Some(bits) = self.parts.get(part).and_then(Option::as_ref) else { return Vec::new() };
        let mut out = Vec::with_capacity(self.lost(part));
        for (at, &word) in bits.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                out.push(at as u32 * 64 + word.trailing_zeros());
                word &= word - 1;
            }
        }
        out
    }

    /// Marks rows of one part gone, `rows` being how many the part has and `slots` its rows to
    /// take out, which may include rows already gone. Hands back how many were not gone before.
    ///
    /// # Errors
    ///
    /// If the part is not one of the file's or a slot is past its rows.
    pub fn take(&mut self, part: usize, rows: usize, slots: &[u32]) -> Result<usize> {
        let held = self
            .parts
            .get_mut(part)
            .ok_or_else(|| Error::internal("a delete names a part the file does not have"))?;
        let mut bits =
            held.as_deref().map_or_else(|| vec![0_u64; rows.div_ceil(64)], <[u64]>::to_vec);
        let mut taken = 0;
        for &slot in slots {
            let slot = slot as usize;
            if slot >= rows {
                return Err(Error::internal("a delete names a row past its part"));
            }
            let bit = 1 << (slot % 64);
            if bits[slot / 64] & bit == 0 {
                bits[slot / 64] |= bit;
                taken += 1;
            }
        }
        if taken > 0 {
            *held = Some(bits.into());
            self.counts[part] += taken as u32;
            self.total += taken;
        }
        Ok(taken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_keeps_the_rows_nobody_took() {
        let mut gone = Gone::none(3);
        assert_eq!(gone.take(1, 100, &[3, 70, 3]).unwrap(), 2);
        assert_eq!(gone.take(1, 100, &[70, 99]).unwrap(), 1);
        assert_eq!(gone.total(), 3);
        assert_eq!(gone.lost(1), 3);
        assert_eq!(gone.lost(0), 0);
        assert!(gone.live(0, 10).is_none());
        let live = gone.live(1, 100).unwrap();
        assert_eq!(live.len(), 97);
        assert!(!live.contains(&3) && !live.contains(&70) && !live.contains(&99));
        assert_eq!(gone.slots(1), vec![3, 70, 99]);
        assert!(gone.contains(1, 70) && !gone.contains(1, 71) && !gone.contains(2, 70));
    }

    #[test]
    fn the_record_a_file_keeps_reads_back_the_same() {
        let mut gone = Gone::none(4);
        gone.take(0, 8192, &[0, 63, 64, 8191]).unwrap();
        gone.take(3, 100, &[99]).unwrap();
        let marks = gone.marks();
        assert_eq!(marks.total, 5);
        assert_eq!(marks.parts.iter().map(|(part, _)| *part).collect::<Vec<_>>(), vec![0, 3]);
        assert_eq!(Gone::stored(4, &marks).unwrap(), gone);
        assert!(Gone::stored(3, &marks).is_err());
    }

    fn ints(values: &[i64]) -> Chunk {
        let values =
            values.iter().map(|&value| rudb_common::Value::BigInt(value)).collect::<Vec<_>>();
        Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &values).unwrap()]).unwrap()
    }

    fn laid(gone: &Gone, part: usize, file: &[i64]) -> Vec<rudb_common::Value> {
        let read = ints(file).column(0).unwrap().clone();
        let laid = gone.patch(part).unwrap().over(&LogicalType::BigInt, &read, 0).unwrap();
        (0..laid.len()).map(|row| laid.value_at(row)).collect()
    }

    #[test]
    fn a_patch_lays_its_rows_over_the_part_and_the_newer_row_wins() {
        let mut gone = Gone::none(2);
        assert!(!gone.is_patched());
        gone.put(1, vec![1, 3], ints(&[10, 30]), &[0]).unwrap();
        assert!(gone.is_patched() && gone.touched(0) && !gone.touched(1));
        assert!(gone.patch(0).is_none() && gone.changes(1, [0]) && !gone.changes(0, [0]));
        gone.put(1, vec![0, 3], ints(&[0, 33]), &[0]).unwrap();
        assert_eq!(gone.patch(1).unwrap().slots(), &[0, 1, 3]);
        let want = [0, 10, 2, 33, 4].map(rudb_common::Value::BigInt);
        assert_eq!(laid(&gone, 1, &[0, 1, 2, 3, 4]), want);
        assert!(gone.put(1, vec![2, 2], ints(&[1, 1]), &[0]).is_err());
        assert!(gone.put(2, vec![0], ints(&[1]), &[0]).is_err());
    }

    #[test]
    fn a_slot_past_the_part_is_refused() {
        let mut gone = Gone::none(1);
        assert!(gone.take(0, 10, &[10]).is_err());
        assert!(gone.take(1, 10, &[0]).is_err());
        assert!(gone.is_empty());
    }
}

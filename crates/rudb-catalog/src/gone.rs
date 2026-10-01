//! The rows a `DELETE` took out of a committed file, `engine-v4/03-the-shape.md` section 3.5.
//!
//! A file is never changed in place, so a delete on a table that lives in one is recorded beside
//! it rather than by writing the rest of the table again. [`Gone`] is that record: one bit a row
//! for each part that lost any, and nothing at all for a part that lost none, which for a table
//! that was loaded once and trimmed a little is almost every part.

use std::sync::Arc;

use rudb_common::{Error, Result};

/// The deleted rows of a file, by part.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gone {
    /// For each part, a bit for each of its rows that is gone, or `None` when none is.
    parts: Vec<Option<Arc<[u64]>>>,
    /// How many rows each part lost, so a part's live length is a subtraction.
    counts: Vec<u32>,
    /// How many rows are gone in all.
    total: usize,
}

impl Gone {
    /// No row gone from a file of `parts` parts.
    #[must_use]
    pub fn none(parts: usize) -> Self {
        Self { parts: vec![None; parts], counts: vec![0; parts], total: 0 }
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
        rudb_native::GoneRows { parts, total: self.total }
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

    #[test]
    fn a_slot_past_the_part_is_refused() {
        let mut gone = Gone::none(1);
        assert!(gone.take(0, 10, &[10]).is_err());
        assert!(gone.take(1, 10, &[0]).is_err());
        assert!(gone.is_empty());
    }
}

//! How the encoder decides which candidate to keep.
//!
//! The encoders in [`crate::string`] and [`crate::integer`] are the work. This is the search. They
//! are separate things and until now they were the same thing, because `encode` both offered every
//! candidate and encoded every candidate it offered, and there was no way to have one without the
//! other.
//!
//! # Why it is worth separating
//!
//! `cargo xtask encode` over a million rows of ClickBench `hits` says where the encoder's seconds
//! go. String `FRONT` is 32.2 percent of them and is kept once in 142 chunks. String `FSST` is 11.2
//! percent and is kept twice in 223. Integer `DELTA` is 10.8 percent and is kept never in 607.
//! String `PLAIN` is 8.7 percent and is kept four times in 223. Those four are 62.9 percent of the
//! encoder's time and they were kept seven times out of 1,195 offers.
//!
//! That is not a bug in any encoder. It is what an exhaustive search costs, and the search is worth
//! something: the shapes it arrives at are five to one on `hits` and nobody wrote them down in
//! advance. The question is how much of the search is needed, which is a question about the data
//! and therefore a question to measure rather than argue about. F2 asks for exactly this, as "the
//! encoder chooser as a seam, with exhaustive and sampled implementations", with the ablation being
//! how much size the sampled one gives up.
//!
//! # What a chooser sees and what it does not
//!
//! A chooser is asked once per chunk per level of the cascade, never once per value. It is handed
//! the values and the candidates that apply and it returns the ones worth encoding in full. It
//! cannot invent a candidate that does not apply, so nothing it does can produce a chunk that will
//! not decode, and the worst a bad chooser can do is pick a bigger encoding than another one would
//! have. That is the property that makes this safe to swap.
//!
//! # Not a `rudb-seam` seam yet, and why
//!
//! `SeamId::StorageEncoder` exists and says "how a block of values is encoded on the way to disk",
//! and this is what belongs behind it. It cannot be registered here: `rudb-seam` is rank 2 and so is
//! this crate, so the `Strategy` supertrait every seam trait needs is not visible from here. The
//! registry goes in `rudb-storage` at rank 5, next to the write path, and there is no write path
//! yet. Until there is, this is a plain trait with two implementations and an ablation, which is
//! the part that can be measured today.

use crate::{integer, string};

/// Which of the candidates that apply are worth encoding in full.
///
/// Crossed once per chunk per level of the cascade. No method here sees a single value on its own,
/// which is the rule that lets the decision be indirect at all.
pub trait Chooser: std::fmt::Debug + Sync {
    /// The name that goes in a report.
    fn name(&self) -> &'static str;

    /// Which of `offered` to encode in full, for a chunk of strings at `depth`.
    ///
    /// `offered` is what applies, in the order the exhaustive chooser would try them. The return
    /// has to be a subset of it and has to be non empty, because a chunk with no candidate is a
    /// chunk that cannot be written.
    fn narrow_strings(
        &self,
        values: &[&[u8]],
        offered: &[string::Kind],
        depth: u8,
    ) -> Vec<string::Kind>;

    /// Which of `offered` to encode in full, for a chunk of integers at `depth`.
    fn narrow_integers(
        &self,
        values: &[i64],
        offered: &[integer::Kind],
        depth: u8,
    ) -> Vec<integer::Kind>;
}

/// Encode every candidate that applies and keep the smallest.
///
/// The reference, and what `encode` has always done. It is the thing to beat rather than the thing
/// to ship: every size this crate has ever reported came out of it, so an alternative's ablation is
/// against this and a build that wants the old bytes exactly asks for this.
#[derive(Debug, Clone, Copy, Default)]
pub struct Exhaustive;

/// The one of these that does not have to be constructed, since it holds nothing.
pub const EXHAUSTIVE: Exhaustive = Exhaustive;

impl Chooser for Exhaustive {
    fn name(&self) -> &'static str {
        "exhaustive"
    }

    fn narrow_strings(
        &self,
        _values: &[&[u8]],
        offered: &[string::Kind],
        _depth: u8,
    ) -> Vec<string::Kind> {
        offered.to_vec()
    }

    fn narrow_integers(
        &self,
        _values: &[i64],
        offered: &[integer::Kind],
        _depth: u8,
    ) -> Vec<integer::Kind> {
        offered.to_vec()
    }
}

/// Encode every candidate on a sample, then encode only the winner on the whole chunk.
///
/// The bet is that a chunk of 122,880 values and a sample of 8,192 drawn from it agree about which
/// encoding suits them, which is a bet about the data and is what the ablation settles. Where it is
/// wrong the cost is size and never correctness, because the winner still has to apply to the whole
/// chunk and is still encoded over all of it.
///
/// The sample is windows of consecutive values rather than values picked one at a time, because
/// three of the candidates are about what a value has in common with the value before it. A sample
/// of scattered singletons would show `FRONT` and `RLE` nothing to find and would rule them out on
/// every column, which is the wrong answer arrived at quickly.
#[derive(Debug, Clone, Copy)]
pub struct Sampled {
    window: usize,
    regions: usize,
}

/// How many consecutive values one window of the sample holds.
///
/// The tile, which is what a bit packing kernel works in and is the smallest run of a column that
/// has the column's local structure in it rather than one value's worth of accident.
const WINDOW: usize = 1024;

/// How many windows the sample is drawn from.
///
/// Eight windows of a tile each is 8,192 values, a fifteenth of a chunk. Spread across the chunk
/// rather than taken off the front, because the front of a sorted column is one value repeated and
/// a chooser that saw only that would pick `CONSTANT` for everything.
const REGIONS: usize = 8;

impl Default for Sampled {
    fn default() -> Self {
        Self { window: WINDOW, regions: REGIONS }
    }
}

impl Sampled {
    /// The default sample, which is eight windows of 1,024 values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A sample of a size somebody else picked, which is what the ablation sweeps.
    #[must_use]
    pub fn over(window: usize, regions: usize) -> Self {
        Self { window: window.max(1), regions: regions.max(1) }
    }

    /// How many values the sample holds, which is what decides whether sampling is worth doing.
    #[must_use]
    pub fn size(self) -> usize {
        self.window * self.regions
    }
}

impl Chooser for Sampled {
    fn name(&self) -> &'static str {
        "sampled"
    }

    fn narrow_strings(
        &self,
        values: &[&[u8]],
        offered: &[string::Kind],
        depth: u8,
    ) -> Vec<string::Kind> {
        if offered.len() < 2 || values.len() <= self.size() {
            return offered.to_vec();
        }
        let sample = sample(values, self.window, self.regions);
        let mut best: Option<(string::Kind, usize)> = None;
        for &kind in offered {
            let Ok(Some(size)) = string::size_as(kind, &sample, depth) else {
                continue;
            };
            if best.is_none_or(|(_, smallest)| size < smallest) {
                best = Some((kind, size));
            }
        }
        // Nothing applied to the sample, which should not happen and is not worth a wrong answer
        // if it does. Hand back everything and let the exhaustive path sort it out.
        best.map_or_else(|| offered.to_vec(), |(kind, _)| vec![kind])
    }

    fn narrow_integers(
        &self,
        values: &[i64],
        offered: &[integer::Kind],
        depth: u8,
    ) -> Vec<integer::Kind> {
        if offered.len() < 2 || values.len() <= self.size() {
            return offered.to_vec();
        }
        let sample = sample(values, self.window, self.regions);
        let mut best: Option<(integer::Kind, usize)> = None;
        for &kind in offered {
            let Ok(Some(size)) = integer::size_as(kind, &sample, depth) else {
                continue;
            };
            if best.is_none_or(|(_, smallest)| size < smallest) {
                best = Some((kind, size));
            }
        }
        best.map_or_else(|| offered.to_vec(), |(kind, _)| vec![kind])
    }
}

/// `regions` windows of `window` consecutive values each, spread evenly across the input.
///
/// The starts are spread over the whole range a window can start at, so the first window begins at
/// the first value and the last one ends at the last value. A chunk of 122,880 values sampled at
/// eight windows of 1,024 gives windows starting at 0, 17,408, 34,816 and so on up to 121,856, which
/// crosses every part of the chunk including both ends of it.
///
/// Spreading to the end rather than striding by `len / regions` matters on the columns this is for.
/// A stride would leave the last stride minus one window of the chunk unsampled, and the tail of a
/// chunk is exactly where a column that is sorted or clustered stops looking like its front.
pub(crate) fn sample<T: Copy>(values: &[T], window: usize, regions: usize) -> Vec<T> {
    let wanted = window * regions;
    if values.len() <= wanted {
        return values.to_vec();
    }
    let last = values.len() - window;
    let mut out = Vec::with_capacity(wanted);
    for region in 0..regions {
        let from = if regions == 1 { 0 } else { region * last / (regions - 1) };
        out.extend_from_slice(&values[from..from + window]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Chooser, EXHAUSTIVE, Sampled, sample};
    use crate::{integer, string};

    #[test]
    fn a_sample_covers_the_whole_input_and_not_one_end_of_it() {
        let values: Vec<i64> = (0..8000).collect();
        let taken = sample(&values, 10, 4);
        assert_eq!(taken.len(), 40);
        assert_eq!(taken[0], 0);
        assert_eq!(taken[10], 2663);
        assert_eq!(taken[20], 5326);
        assert_eq!(taken[30], 7990);
        assert_eq!(taken[39], 7999);
    }

    #[test]
    fn an_input_no_bigger_than_the_sample_is_the_sample() {
        let values: Vec<i64> = (0..30).collect();
        assert_eq!(sample(&values, 10, 4), values);
    }

    #[test]
    fn the_last_window_does_not_run_off_the_end() {
        // Two windows of 40 over 100 values puts the second one at 60, which is the last start that
        // fits. Windows that overlap because there are more of them than the input has room for is
        // fine and double counts a few values. Reading past the end is not.
        let values: Vec<i64> = (0..100).collect();
        let taken = sample(&values, 40, 2);
        assert_eq!(taken.len(), 80);
        assert_eq!(*taken.last().expect("the sample is not empty"), 99);
    }

    #[test]
    fn the_exhaustive_chooser_hands_back_exactly_what_it_was_offered() {
        let offered = [string::Kind::Plain, string::Kind::Fsst, string::Kind::Dict];
        assert_eq!(EXHAUSTIVE.narrow_strings(&[b"a".as_slice()], &offered, 0), offered);
        let offered = [integer::Kind::Packed, integer::Kind::Delta];
        assert_eq!(EXHAUSTIVE.narrow_integers(&[1, 2], &offered, 0), offered);
    }

    #[test]
    fn a_chunk_no_bigger_than_the_sample_is_not_narrowed_at_all() {
        // Sampling a chunk that is smaller than the sample would encode every candidate on
        // something the size of the chunk and then encode the winner on the chunk, which is more
        // work than the exhaustive chooser for the same answer.
        let sampled = Sampled::over(4, 2);
        let values: Vec<i64> = (0..8).collect();
        let offered = [integer::Kind::Packed, integer::Kind::Delta];
        assert_eq!(sampled.narrow_integers(&values, &offered, 0), offered);
    }

    #[test]
    fn a_sampled_chooser_returns_one_of_what_it_was_offered() {
        let sampled = Sampled::over(16, 2);
        let values: Vec<i64> = (0..4000).map(|index| index / 200).collect();
        let offered = [integer::Kind::Packed, integer::Kind::Rle, integer::Kind::Dict];
        let narrowed = sampled.narrow_integers(&values, &offered, 0);
        assert_eq!(narrowed.len(), 1);
        assert!(offered.contains(&narrowed[0]), "{narrowed:?}");
    }
}

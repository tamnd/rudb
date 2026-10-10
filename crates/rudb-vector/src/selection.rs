//! Selection vectors.
//!
//! `spec/07-execution.md` section 7.1: a filter produces a `u32` selection vector rather than
//! compacting. Compaction happens when a measured selectivity threshold is crossed and the
//! downstream operator is one that benefits, and the threshold is per operator and measured rather
//! than one global constant somebody picked.
//!
//! The reason not to compact by default is that a filter over five columns which compacts has
//! copied five columns to save the next operator a redirection. On a query that filters and then
//! projects two of those columns, three of the copies were free work.

use std::sync::OnceLock;

/// Which positions of a vector are still in play, as indices into it.
///
/// An empty selection means nothing survived, which is different from no selection at all. The
/// distinction is why this is a type rather than an `Option<Vec<u32>>` that everybody interprets
/// slightly differently.
///
/// A filter that compares a column with a literal works out a mask word for every 64 rows, and a
/// selection made from one keeps the words and lists the positions only when something asks for
/// them. Plenty of readers never do. A `count(*)` behind a filter wants how many rows were kept, and
/// a chunk with no columns left to cut wants nothing else either. On `SELECT count(*) FROM lineitem
/// WHERE l_shipdate` in a year, listing the rows of the mask was 28% of the query.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// The positions, worked out of `mask` the first time they are asked for when there is one.
    list: OnceLock<Vec<u32>>,
    /// A bit for each position, position `i` in bit `i % 64` of word `i / 64`, for a selection a
    /// filter made out of a mask. It goes once the positions are changed one at a time.
    mask: Option<Box<[u64]>>,
    /// How many bits of `mask` are set.
    kept: usize,
}

impl PartialEq for Selection {
    fn eq(&self, other: &Self) -> bool {
        self.indices() == other.indices()
    }
}

impl Eq for Selection {}

impl Selection {
    /// A selection of nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// A selection of nothing, with room for `capacity` positions.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::from_indices(Vec::with_capacity(capacity))
    }

    /// A selection of the first `len` positions in order.
    ///
    /// Materialized rather than represented as an absent selection, so this is what a caller uses
    /// when it genuinely wants the identity written down. A scan that has not filtered anything
    /// carries no selection at all, which is cheaper and is the common case.
    #[must_use]
    pub fn identity(len: usize) -> Self {
        Self::from_indices((0..len as u32).collect())
    }

    /// A selection from positions a caller has already worked out, in order.
    ///
    /// For a kernel that fills a buffer of its own and counts as it goes, which is how a selection
    /// loop is written without a branch in it: every row writes its index at the current length and
    /// only a row that is kept moves the length on. Pushing one at a time would put a capacity check
    /// and a conversion on a loop whose whole point is that it has neither.
    #[must_use]
    pub fn from_indices(indices: Vec<u32>) -> Self {
        Self { list: OnceLock::from(indices), mask: None, kept: 0 }
    }

    /// A selection of the positions whose bits are set in `words`, `kept` of them, with position
    /// `i` in bit `i % 64` of word `i / 64`.
    ///
    /// The positions are not listed until something asks for them, see [`Selection`].
    #[must_use]
    pub fn from_mask(words: Vec<u64>, kept: usize) -> Self {
        debug_assert_eq!(
            words.iter().map(|word| word.count_ones() as usize).sum::<usize>(),
            kept,
            "a mask and its count disagree"
        );
        Self { list: OnceLock::new(), mask: Some(words.into_boxed_slice()), kept }
    }

    /// The mask this selection was made from, when it was made from one.
    #[must_use]
    pub fn mask(&self) -> Option<&[u64]> {
        self.mask.as_deref()
    }

    /// Whether every position is below `len`, which a mask answers from its words past `len`
    /// without listing the positions.
    #[must_use]
    pub fn below(&self, len: usize) -> bool {
        match (&self.mask, self.list.get()) {
            (Some(words), None) => {
                let (whole, part) = (len / 64, len % 64);
                words
                    .iter()
                    .enumerate()
                    .skip(whole)
                    .all(|(at, &word)| if at == whole { word >> part == 0 } else { word == 0 })
            }
            _ => crate::vector::below(self.indices(), len),
        }
    }

    /// The positions as a list the caller owns.
    #[must_use]
    pub fn into_indices(self) -> Vec<u32> {
        let _ = self.indices();
        self.list.into_inner().unwrap_or_default()
    }

    /// A selection of the positions a predicate accepts.
    ///
    /// Written the way [`Selection::from_indices`] asks a kernel to be written: every position is
    /// stored at the current length and only one that is kept moves the length on. A push for each
    /// kept row was a capacity check, a conversion and a branch the predictor gets wrong whenever
    /// the rows kept are mixed, which on q09 was 13 million instructions for a link join that
    /// keeps every row it sees.
    ///
    /// # Panics
    ///
    /// If `len` does not fit in a `u32`, for the reason [`Selection::push`] gives.
    pub fn from_predicate(len: usize, keep: impl Fn(usize) -> bool) -> Self {
        assert!(u32::try_from(len).is_ok(), "a position past four billion");
        let mut indices = vec![0_u32; len];
        let mut kept = 0;
        for index in 0..len {
            // `kept` is never past `index`, so the store is always in bounds.
            indices[kept] = index as u32;
            kept += usize::from(keep(index));
        }
        indices.truncate(kept);
        Self::from_indices(indices)
    }

    /// Adds a position to the end.
    ///
    /// # Panics
    ///
    /// If the index does not fit in a `u32`. A vector holds 1024 values and a row group holds
    /// 122,880, so an index that large is a bug several layers up rather than a large query.
    pub fn push(&mut self, index: usize) {
        let index = u32::try_from(index).expect("a position past four billion");
        if self.mask.is_some() {
            let _ = self.indices();
            self.mask = None;
        }
        match self.list.get_mut() {
            Some(list) => list.push(index),
            None => self.list = OnceLock::from(vec![index]),
        }
    }

    /// How many positions survived.
    #[must_use]
    pub fn len(&self) -> usize {
        self.list.get().map_or(self.kept, Vec::len)
    }

    /// Whether nothing survived.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The position at `slot`, where `slot` counts through the survivors.
    #[must_use]
    pub fn get(&self, slot: usize) -> Option<usize> {
        self.indices().get(slot).map(|&index| index as usize)
    }

    /// The positions, in order.
    #[must_use]
    pub fn indices(&self) -> &[u32] {
        self.list.get_or_init(|| {
            self.mask.as_deref().map_or_else(Vec::new, listed)
        })
    }

    /// The positions as `usize`, in order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.indices().iter().map(|&index| index as usize)
    }

    /// What fraction of `len` positions survived.
    ///
    /// This is the number the compaction decision is made on, and it is measured rather than
    /// assumed, per section 7.1. Zero length reports 1.0, because a filter over nothing has not
    /// rejected anything.
    #[must_use]
    pub fn selectivity(&self, len: usize) -> f64 {
        if len == 0 { 1.0 } else { self.len() as f64 / len as f64 }
    }

    /// This selection composed with an earlier one, so that filtering twice does not need the
    /// intermediate to be materialized.
    ///
    /// `self` indexes into `earlier`, and the result indexes into whatever `earlier` indexed into.
    /// Getting this backwards produces a query that returns the wrong rows rather than an error,
    /// which is why the direction is spelled out here and tested below.
    #[must_use]
    pub fn compose(&self, earlier: &Self) -> Self {
        let indices =
            self.indices().iter().filter_map(|&slot| earlier.indices().get(slot as usize).copied());
        Self::from_indices(indices.collect())
    }

    /// The positions this selection holds that `taken` does not.
    ///
    /// Both sides have to be in ascending order, which every selection in this engine is: a kernel
    /// fills one by walking the rows upward and a composed one keeps that order. So this is one
    /// merge over the pair rather than a search per position.
    ///
    /// This is what a threaded `OR` narrows its work with. Each branch is given the rows no branch
    /// before it accepted, and the rows it accepts come out of that set for the branch after.
    ///
    /// The branch in the middle of this is one no processor can predict, since a disjunction whose
    /// first branch takes about half the rows mispredicts on roughly every other one. Writing both
    /// answers and moving the length by whether the row was kept gets rid of it, and was tried and
    /// measured at no difference on ClickBench 40, so it is not here. The cost that looked like this
    /// was the membership kernel reading a packed column a value at a time.
    #[must_use]
    pub fn without(&self, taken: &Self) -> Self {
        let mut indices = Vec::with_capacity(self.len().saturating_sub(taken.len()));
        let mut next = taken.indices().iter().copied().peekable();
        for &index in self.indices() {
            while next.peek().is_some_and(|&other| other < index) {
                next.next();
            }
            if next.peek() == Some(&index) {
                next.next();
            } else {
                indices.push(index);
            }
        }
        Self::from_indices(indices)
    }

    /// The positions either selection holds, each once and in order.
    ///
    /// What a threaded `OR` adds up while its operands all run over the same rows, so the two sides
    /// can hold the same position. Both are in ascending order, as for [`Self::without`].
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        let (mut left, mut right) = (self.indices(), other.indices());
        let mut indices = Vec::with_capacity(left.len() + right.len());
        while let (Some(&a), Some(&b)) = (left.first(), right.first()) {
            indices.push(a.min(b));
            if a <= b {
                left = &left[1..];
            }
            if b <= a {
                right = &right[1..];
            }
        }
        indices.extend_from_slice(left);
        indices.extend_from_slice(right);
        Self::from_indices(indices)
    }

    /// The positions below `len` that this selection does not hold.
    ///
    /// The other half of a threaded `OR`. What the branches leave behind is the rows none of them
    /// accepted, and the rows the filter keeps are all the others.
    ///
    /// Writing the gaps between the positions instead, as runs of consecutive numbers with no branch
    /// per row, is the same idea as above and measured the same way, so it is not here either.
    #[must_use]
    pub fn complement(&self, len: usize) -> Self {
        let mut indices = Vec::with_capacity(len.saturating_sub(self.len()));
        let mut next = self.indices().iter().copied().peekable();
        for index in 0..len as u32 {
            while next.peek().is_some_and(|&held| held < index) {
                next.next();
            }
            if next.peek() == Some(&index) {
                next.next();
            } else {
                indices.push(index);
            }
        }
        Self::from_indices(indices)
    }
}

/// How many dropped rows a word may have and still be walked by the rows it drops.
const DENSE_WORD: u32 = 8;

/// How many rows of a sparse word are written before asking whether it holds more.
///
/// A filter that keeps a few rows in a hundred leaves most words with none, one or two, so the
/// first four are written whether the word has them or not and the answer only counts the ones it
/// has. What that saves is the branch on whether a word is empty and the branch that ends the walk
/// of its rows, and with words that are empty about as often as not, both were taken at random.
const SPARSE_WORD: usize = 4;

/// The positions whose bits are set in `words`, in order.
///
/// Every word is written into room the answer already has rather than pushed. A sparse word writes
/// [`SPARSE_WORD`] rows whatever it holds, so an empty word costs a few stores past the end of the
/// answer, which the next word writes over, and no branch. On TPC-H q14, whose filter keeps one
/// row in eighty of `lineitem`, the branchy walk this replaces was 5 percent of the query.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a mask is over a chunk, whose rows fit in a u32"
)]
#[allow(unsafe_code)]
fn listed(words: &[u64]) -> Vec<u32> {
    // Counted from the words rather than taken from the selection's count, because the room below
    // is written into unchecked and a count that came out wrong somewhere else must not matter.
    let total: usize = words.iter().map(|word| word.count_ones() as usize).sum();
    // Room for every row, and for the 64 a word may write past the last of them.
    let mut out = Vec::with_capacity(total + 2 * 64);
    let room = out.spare_capacity_mut();
    let mut len = 0;
    for (block, &word) in words.iter().enumerate() {
        let base = (block * 64) as u32;
        // A word that keeps most of its rows is walked by the rows it drops. Each run between two
        // of them is written as a whole 64 rows and cut back to its own, so it is a copy of fixed
        // width with no tail to finish a row at a time, and what it writes past its rows is
        // written over by the next run. A filter that keeps nearly every row would otherwise pay
        // a step for every row it keeps.
        if word.count_zeros() <= DENSE_WORD {
            let mut dropped = !word;
            let mut from = 0;
            loop {
                let at = if dropped == 0 { 64 } else { dropped.trailing_zeros() };
                for (slot, row) in room[len..len + 64].iter_mut().zip(base + from..) {
                    slot.write(row);
                }
                len += (at - from) as usize;
                if dropped == 0 {
                    break;
                }
                from = at + 1;
                dropped &= dropped - 1;
            }
            continue;
        }
        let count = word.count_ones() as usize;
        let mut rest = word;
        for slot in &mut room[len..len + SPARSE_WORD] {
            slot.write(base + rest.trailing_zeros());
            rest &= rest.wrapping_sub(1);
        }
        if count > SPARSE_WORD {
            for slot in &mut room[len + SPARSE_WORD..len + count] {
                slot.write(base + rest.trailing_zeros());
                rest &= rest - 1;
            }
        }
        len += count;
    }
    // SAFETY: `len` is the rows listed, and every slot under it was written above, since each word
    // writes the slots from the `len` it found up to the `len` it leaves.
    unsafe { out.set_len(len) };
    out
}

#[cfg(test)]
mod tests {
    use super::Selection;

    /// A selection made from a mask lists the same positions a list of them would, counts them
    /// without listing them, and turns into a list when one is pushed onto it.
    #[test]
    fn a_selection_made_from_a_mask_is_the_positions_its_bits_set() {
        let words = vec![u64::MAX, 0, !1, !(1 << 63), !0xff, !0x1ff, 0x8000_0000_0000_0001, 0x0f0f];
        let kept = words.iter().map(|word| word.count_ones() as usize).sum();
        let wanted: Vec<u32> = (0..words.len() * 64)
            .filter(|&at| words[at / 64] >> (at % 64) & 1 == 1)
            .map(|at| at as u32)
            .collect();
        let selection = Selection::from_mask(words.clone(), kept);
        assert_eq!(selection.len(), wanted.len());
        assert_eq!(selection.mask(), Some(words.as_slice()));
        assert!(selection.below(words.len() * 64 - 52));
        assert!(!selection.below(words.len() * 64 - 53));
        assert!(Selection::from_mask(vec![1, 0, 0], 1).below(1));
        assert!(!Selection::from_mask(vec![1, 0, 2], 2).below(129));
        assert_eq!(selection.indices(), wanted.as_slice());
        assert_eq!(selection, Selection::from_indices(wanted.clone()));
        let mut pushed = selection.clone();
        pushed.push(4096);
        assert_eq!(pushed.mask(), None);
        assert_eq!(pushed.len(), wanted.len() + 1);
        assert_eq!(selection.into_indices(), wanted);
    }

    /// The listing writes past its rows and counts only the ones it has, so every way a word can
    /// hold rows is tried: empty, one to five rows, about half, and dense with a few dropped.
    #[test]
    fn a_mask_of_any_density_lists_its_positions() {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for keep in [0, 1, 3, 9, 30, 64, 128, 192, 250, 255, 256] {
            let words: Vec<u64> = (0..128)
                .map(|_| (0..64).fold(0, |word, bit| word | u64::from(next() % 256 < keep) << bit))
                .collect();
            let kept = words.iter().map(|word| word.count_ones() as usize).sum();
            let wanted: Vec<u32> = (0..words.len() * 64)
                .filter(|&at| words[at / 64] >> (at % 64) & 1 == 1)
                .map(|at| at as u32)
                .collect();
            assert_eq!(Selection::from_mask(words, kept).indices(), wanted.as_slice(), "{keep}");
        }
    }

    #[test]
    fn nothing_selected_is_not_the_same_as_no_selection() {
        // The reason this is a type. An operator that treats an empty selection as "everything"
        // returns every row for a predicate that matched none, which is a wrong answer and the
        // worst thing this project can ship.
        let none = Selection::empty();
        assert_eq!(none.len(), 0);
        assert!(none.is_empty());
        assert_eq!(none.selectivity(1024), 0.0);
    }

    #[test]
    fn a_predicate_selection_keeps_the_positions_in_order() {
        let selection = Selection::from_predicate(10, |i| i % 3 == 0);
        assert_eq!(selection.indices(), &[0, 3, 6, 9]);
        assert_eq!(selection.get(2), Some(6));
        assert_eq!(selection.get(4), None);
        assert!((selection.selectivity(10) - 0.4).abs() < f64::EPSILON);
    }

    #[test]
    fn composing_two_filters_indexes_all_the_way_back() {
        // First filter keeps the even positions of sixteen. Second keeps every third survivor,
        // meaning slots 0, 3 and 6 of the first result, which are positions 0, 6 and 12.
        let first = Selection::from_predicate(16, |i| i % 2 == 0);
        let second = Selection::from_predicate(first.len(), |i| i % 3 == 0);
        assert_eq!(second.compose(&first).indices(), &[0, 6, 12]);
    }

    #[test]
    fn composing_with_the_identity_changes_nothing() {
        let selection = Selection::from_predicate(8, |i| i > 4);
        assert_eq!(selection.compose(&Selection::identity(8)), selection);
    }

    #[test]
    fn taking_rows_out_of_a_selection_leaves_the_rest_in_order() {
        let live = Selection::from_indices(vec![1, 4, 5, 9, 12]);
        let taken = Selection::from_indices(vec![4, 9]);
        assert_eq!(live.without(&taken).indices(), &[1, 5, 12]);
        // Taking nothing and taking everything are the two ends a threaded `OR` hits on its first
        // branch, and neither of them is allowed to be a special case at the call site.
        assert_eq!(live.without(&Selection::empty()), live);
        assert!(live.without(&live).is_empty());
    }

    #[test]
    fn taking_rows_that_are_not_there_changes_nothing() {
        let live = Selection::from_indices(vec![2, 6]);
        assert_eq!(live.without(&Selection::from_indices(vec![0, 3, 7])), live);
    }

    #[test]
    fn a_union_holds_each_position_of_either_side_once_and_in_order() {
        let left = Selection::from_indices(vec![1, 4, 5, 9]);
        let right = Selection::from_indices(vec![0, 4, 9, 12]);
        assert_eq!(left.union(&right).indices(), &[0, 1, 4, 5, 9, 12]);
        assert_eq!(right.union(&left), left.union(&right));
        assert_eq!(left.union(&Selection::empty()), left);
        assert_eq!(Selection::empty().union(&left), left);
        assert_eq!(left.union(&left), left);
    }

    #[test]
    fn the_complement_is_every_position_the_selection_left_out() {
        let selection = Selection::from_indices(vec![0, 2, 3]);
        assert_eq!(selection.complement(6).indices(), &[1, 4, 5]);
        assert_eq!(Selection::empty().complement(3), Selection::identity(3));
        assert!(Selection::identity(3).complement(3).is_empty());
        assert!(Selection::empty().complement(0).is_empty());
    }

    /// A position past the length asked about, which is not in the complement and does not stop the
    /// positions after it being in it.
    ///
    /// The walk above never reaches such a position, so it gets this right without trying. Written
    /// down because any faster way of doing this reads the positions rather than the length, and
    /// then this is the case it has to be told about.
    #[test]
    fn a_position_past_the_length_is_not_in_the_complement_and_does_not_swallow_what_follows() {
        let selection = Selection::from_indices(vec![1, 9]);
        assert_eq!(selection.complement(4).indices(), &[0, 2, 3]);
        assert_eq!(Selection::from_indices(vec![7]).complement(3), Selection::identity(3));
    }

    /// Both set operations against the obvious slow way of getting the same answer.
    ///
    /// A disjunction runs both of these on every chunk it touches, so they are the two functions
    /// here most likely to be rewritten for speed, and the way that goes wrong is an off by one on a
    /// boundary the handful of cases above happen not to cover. So the cases are generated instead:
    /// every pattern of eight rows against every other one, checked against the answer a set gives.
    #[test]
    fn the_set_operations_agree_with_the_slow_way_of_working_them_out() {
        const ROWS: u32 = 8;
        for left in 0..1u32 << ROWS {
            let live: Vec<u32> = (0..ROWS).filter(|bit| left >> bit & 1 == 1).collect();
            let selection = Selection::from_indices(live.clone());
            for len in 0..=ROWS as usize {
                let wanted: Vec<u32> =
                    (0..len as u32).filter(|index| !live.contains(index)).collect();
                assert_eq!(selection.complement(len).indices(), wanted, "{live:?} under {len}");
            }
            for right in 0..1u32 << ROWS {
                let taken: Vec<u32> = (0..ROWS).filter(|bit| right >> bit & 1 == 1).collect();
                let wanted: Vec<u32> =
                    live.iter().copied().filter(|index| !taken.contains(index)).collect();
                let answered = selection.without(&Selection::from_indices(taken.clone()));
                assert_eq!(answered.indices(), wanted, "{live:?} without {taken:?}");
            }
        }
    }

    #[test]
    fn selectivity_over_nothing_is_one_rather_than_a_division_by_zero() {
        assert!((Selection::empty().selectivity(0) - 1.0).abs() < f64::EPSILON);
    }
}

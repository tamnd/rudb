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

/// Which positions of a vector are still in play, as indices into it.
///
/// An empty selection means nothing survived, which is different from no selection at all. The
/// distinction is why this is a type rather than an `Option<Vec<u32>>` that everybody interprets
/// slightly differently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    indices: Vec<u32>,
}

impl Selection {
    /// A selection of nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// A selection of nothing, with room for `capacity` positions.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { indices: Vec::with_capacity(capacity) }
    }

    /// A selection of the first `len` positions in order.
    ///
    /// Materialized rather than represented as an absent selection, so this is what a caller uses
    /// when it genuinely wants the identity written down. A scan that has not filtered anything
    /// carries no selection at all, which is cheaper and is the common case.
    #[must_use]
    pub fn identity(len: usize) -> Self {
        Self { indices: (0..len as u32).collect() }
    }

    /// A selection from positions a caller has already worked out, in order.
    ///
    /// For a kernel that fills a buffer of its own and counts as it goes, which is how a selection
    /// loop is written without a branch in it: every row writes its index at the current length and
    /// only a row that is kept moves the length on. Pushing one at a time would put a capacity check
    /// and a conversion on a loop whose whole point is that it has neither.
    #[must_use]
    pub fn from_indices(indices: Vec<u32>) -> Self {
        Self { indices }
    }

    /// A selection of the positions a predicate accepts.
    pub fn from_predicate(len: usize, keep: impl Fn(usize) -> bool) -> Self {
        let mut selection = Self::with_capacity(len);
        for index in 0..len {
            if keep(index) {
                selection.push(index);
            }
        }
        selection
    }

    /// Adds a position to the end.
    ///
    /// # Panics
    ///
    /// If the index does not fit in a `u32`. A vector holds 1024 values and a row group holds
    /// 122,880, so an index that large is a bug several layers up rather than a large query.
    pub fn push(&mut self, index: usize) {
        self.indices.push(u32::try_from(index).expect("a position past four billion"));
    }

    /// How many positions survived.
    #[must_use]
    pub fn len(&self) -> usize {
        self.indices.len()
    }

    /// Whether nothing survived.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// The position at `slot`, where `slot` counts through the survivors.
    #[must_use]
    pub fn get(&self, slot: usize) -> Option<usize> {
        self.indices.get(slot).map(|&index| index as usize)
    }

    /// The positions, in order.
    #[must_use]
    pub fn indices(&self) -> &[u32] {
        &self.indices
    }

    /// The positions as `usize`, in order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.indices.iter().map(|&index| index as usize)
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
            self.indices.iter().filter_map(|&slot| earlier.indices.get(slot as usize).copied());
        Self { indices: indices.collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::Selection;

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
    fn selectivity_over_nothing_is_one_rather_than_a_division_by_zero() {
        assert!((Selection::empty().selectivity(0) - 1.0).abs() < f64::EPSILON);
    }
}

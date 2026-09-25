//! The delete vector of a frozen stripe, `engine-v4/03-the-shape.md` section 3.5.
//!
//! A frozen stripe is never changed in place, so a delete is recorded beside it. The record has
//! two layers. The **base** is the set of slots deleted as of the stripe's last checkpoint, kept as
//! whichever of three containers is smallest: a sorted array for up to 4,096 slots, a 64 KiB bitmap
//! beyond that, or a list of runs when the deletes come in ranges. It is what the file stores. On
//! top of it is a short list of `(slot, ts)` for deletes since that checkpoint, sorted by slot,
//! where `ts` is the commit timestamp, or the deleting transaction's id with the top bit set until
//! it commits. A checkpoint folds the committed entries into the base.
//!
//! A scan asks for one part at a time and gets the deleted slots of that part as 128 words, or
//! nothing at all when the stripe has no deletes, which for a table loaded once is always, and
//! then the scan is the scan it was before any of this.

use rudb_common::{Error, Result};

/// Rows in a stripe, and so the number of slots a delete vector covers.
pub const STRIPE_ROWS: u32 = 524_288;

/// Rows in a part, the unit a scan builds a mask for.
pub const PART_ROWS: u32 = 8_192;

/// Words in one part's mask.
pub const PART_WORDS: usize = PART_ROWS as usize / 64;

/// The bit that marks a timestamp as a transaction id that has not committed yet.
pub const UNCOMMITTED: u64 = 1 << 63;

/// The most slots an array container holds before a bitmap is used instead.
const ARRAY_MAX: usize = 4_096;

/// Words in a whole stripe's bitmap, 64 KiB of them.
const BITMAP_WORDS: usize = STRIPE_ROWS as usize / 64;

/// The deletes a stripe had at its last checkpoint, in the smallest of three shapes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Base {
    /// No slot is deleted.
    #[default]
    Empty,
    /// The deleted slots, sorted, at most 4,096 of them.
    Array(Vec<u32>),
    /// One bit a slot.
    Bitmap(Box<[u64]>),
    /// Runs of deleted slots as `(first, last)`, both included, sorted and apart.
    Runs(Vec<(u32, u32)>),
}

impl Base {
    /// The base for `slots`, which are sorted and distinct, in the smallest container.
    ///
    /// A run of `n` slots costs 8 bytes however long it is, an array 4 bytes a slot, and a bitmap
    /// 64 KiB whatever it holds, and on a tie the array wins because it is the cheapest to probe.
    #[must_use]
    pub fn of(slots: &[u32]) -> Self {
        debug_assert!(slots.windows(2).all(|pair| pair[0] < pair[1]), "sorted and distinct");
        if slots.is_empty() {
            return Self::Empty;
        }
        let runs = runs_of(slots);
        let array = if slots.len() <= ARRAY_MAX { slots.len() * 4 } else { usize::MAX };
        let bitmap = BITMAP_WORDS * 8;
        let run = runs.len() * 8;
        if run < array.min(bitmap) {
            Self::Runs(runs)
        } else if array <= bitmap {
            Self::Array(slots.to_vec())
        } else {
            let mut words = vec![0_u64; BITMAP_WORDS].into_boxed_slice();
            for &slot in slots {
                words[slot as usize / 64] |= 1 << (slot % 64);
            }
            Self::Bitmap(words)
        }
    }

    /// Whether `slot` is deleted.
    #[must_use]
    pub fn contains(&self, slot: u32) -> bool {
        match self {
            Self::Empty => false,
            Self::Array(slots) => slots.binary_search(&slot).is_ok(),
            Self::Bitmap(words) => {
                words.get(slot as usize / 64).is_some_and(|word| word & (1 << (slot % 64)) != 0)
            }
            Self::Runs(runs) => {
                let at = runs.partition_point(|&(_, last)| last < slot);
                runs.get(at).is_some_and(|&(first, _)| first <= slot)
            }
        }
    }

    /// How many slots are deleted.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Array(slots) => slots.len(),
            Self::Bitmap(words) => words.iter().map(|word| word.count_ones() as usize).sum(),
            Self::Runs(runs) => runs.iter().map(|&(first, last)| (last - first) as usize + 1).sum(),
        }
    }

    /// Whether no slot is deleted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The deleted slots, sorted.
    #[must_use]
    pub fn slots(&self) -> Vec<u32> {
        match self {
            Self::Empty => Vec::new(),
            Self::Array(slots) => slots.clone(),
            Self::Bitmap(words) => {
                let mut out = Vec::new();
                for (at, &word) in words.iter().enumerate() {
                    let mut bits = word;
                    while bits != 0 {
                        out.push(at as u32 * 64 + bits.trailing_zeros());
                        bits &= bits - 1;
                    }
                }
                out
            }
            Self::Runs(runs) => runs.iter().flat_map(|&(first, last)| first..=last).collect(),
        }
    }

    /// Sets the bit of every deleted slot of part `part` in `mask`, bit `i` for the part's slot
    /// `i`, and says whether it set any.
    fn mark(&self, part: u32, mask: &mut [u64; PART_WORDS]) -> bool {
        let start = part * PART_ROWS;
        let end = start + PART_ROWS;
        match self {
            Self::Empty => false,
            Self::Array(slots) => {
                let from = slots.partition_point(|&slot| slot < start);
                let mut any = false;
                for &slot in slots[from..].iter().take_while(|&&slot| slot < end) {
                    set(mask, slot - start);
                    any = true;
                }
                any
            }
            Self::Bitmap(words) => {
                let first = start as usize / 64;
                let mut any = 0;
                for (out, &word) in mask.iter_mut().zip(&words[first..first + PART_WORDS]) {
                    *out |= word;
                    any |= word;
                }
                any != 0
            }
            Self::Runs(runs) => {
                let from = runs.partition_point(|&(_, last)| last < start);
                let mut any = false;
                for &(first, last) in runs[from..].iter().take_while(|&&(first, _)| first < end) {
                    for slot in first.max(start)..=last.min(end - 1) {
                        set(mask, slot - start);
                    }
                    any = true;
                }
                any
            }
        }
    }

    /// The container as the file stores it: a kind byte, three zero bytes, a count, and the
    /// container's words, all little endian. The count is of slots, words or runs.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let (kind, count, words): (u8, usize, Vec<u32>) = match self {
            Self::Empty => (0, 0, Vec::new()),
            Self::Array(slots) => (1, slots.len(), slots.clone()),
            Self::Bitmap(words) => (
                2,
                words.len(),
                words.iter().flat_map(|&word| [word as u32, (word >> 32) as u32]).collect(),
            ),
            Self::Runs(runs) => {
                (3, runs.len(), runs.iter().flat_map(|&(first, last)| [first, last]).collect())
            }
        };
        let mut out = Vec::with_capacity(8 + words.len() * 4);
        out.extend_from_slice(&[kind, 0, 0, 0]);
        out.extend_from_slice(&(count as u32).to_le_bytes());
        for word in words {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out
    }

    /// A container read back from what [`Self::encode`] wrote.
    ///
    /// # Errors
    ///
    /// If the bytes are not a container: an unknown kind, a length that does not match the count,
    /// slots out of order or past the stripe, or a bitmap of the wrong size.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let bad = |why: &str| Error::internal(format!("a delete vector does not read back: {why}"));
        if bytes.len() < 8 || bytes[1..4] != [0; 3] {
            return Err(bad("the header is damaged"));
        }
        let count = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        let words: Vec<u32> = bytes[8..]
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
            .collect();
        if !(bytes.len() - 8).is_multiple_of(4) {
            return Err(bad("the length is not whole words"));
        }
        let inside = |slot: u32| slot < STRIPE_ROWS;
        match bytes[0] {
            0 if count == 0 && words.is_empty() => Ok(Self::Empty),
            1 if words.len() == count
                && count <= ARRAY_MAX
                && words.windows(2).all(|pair| pair[0] < pair[1])
                && words.iter().all(|&slot| inside(slot)) =>
            {
                Ok(Self::Array(words))
            }
            2 if count == BITMAP_WORDS && words.len() == 2 * count => Ok(Self::Bitmap(
                words
                    .chunks_exact(2)
                    .map(|pair| u64::from(pair[0]) | (u64::from(pair[1]) << 32))
                    .collect(),
            )),
            3 if words.len() == 2 * count => {
                let runs: Vec<(u32, u32)> =
                    words.chunks_exact(2).map(|pair| (pair[0], pair[1])).collect();
                let sound = runs.iter().all(|&(first, last)| first <= last && inside(last))
                    && runs.windows(2).all(|pair| pair[0].1 + 1 < pair[1].0);
                if sound { Ok(Self::Runs(runs)) } else { Err(bad("the runs overlap or touch")) }
            }
            _ => Err(bad("the kind or the count is wrong")),
        }
    }
}

/// The runs of consecutive slots in `slots`, which are sorted and distinct.
fn runs_of(slots: &[u32]) -> Vec<(u32, u32)> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &slot in slots {
        match runs.last_mut() {
            Some((_, last)) if *last + 1 == slot => *last = slot,
            _ => runs.push((slot, slot)),
        }
    }
    runs
}

fn set(mask: &mut [u64; PART_WORDS], bit: u32) {
    mask[bit as usize / 64] |= 1 << (bit % 64);
}

/// Why a delete was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The row was already deleted as the deleting transaction sees it, by itself or by a commit
    /// in its snapshot. The statement would not have found the row, so this is a caller's bug
    /// rather than a thing to tell the user.
    Gone,
    /// Another transaction deleted the row first: it holds an uncommitted delete of it, or
    /// committed one after this transaction's snapshot. First writer wins, so this one aborts.
    Conflict,
}

/// A frozen stripe's deletes: the base from its last checkpoint and the list since.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeleteVector {
    base: Base,
    /// `(slot, ts)` for every delete since the base, sorted by slot, one entry a slot.
    recent: Vec<(u32, u64)>,
}

impl DeleteVector {
    /// A vector with no deletes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A vector whose base is `base`, as a checkpoint left it, with nothing since.
    #[must_use]
    pub fn from_base(base: Base) -> Self {
        Self { base, recent: Vec::new() }
    }

    /// The base the file stores.
    #[must_use]
    pub fn base(&self) -> &Base {
        &self.base
    }

    /// How many deletes are in the list above the base, committed or not.
    #[must_use]
    pub fn recent(&self) -> usize {
        self.recent.len()
    }

    /// Whether nothing was ever deleted, which lets a scan skip building masks altogether.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self.base, Base::Empty) && self.recent.is_empty()
    }

    /// Whether `slot` is deleted for a reader at `snapshot` running as transaction `txn`, which
    /// sees its own uncommitted deletes and nobody else's.
    #[must_use]
    pub fn is_deleted(&self, slot: u32, snapshot: u64, txn: u64) -> bool {
        self.base.contains(slot)
            || self
                .recent
                .binary_search_by_key(&slot, |&(at, _)| at)
                .is_ok_and(|at| visible(self.recent[at].1, snapshot, txn))
    }

    /// Records that transaction `txn`, reading at `snapshot`, deletes `slot`. It stays invisible
    /// to everyone else until [`Self::commit`].
    ///
    /// # Errors
    ///
    /// [`Refusal::Gone`] when the row is already deleted as `txn` sees it, and
    /// [`Refusal::Conflict`] when someone else deleted it first.
    ///
    /// # Panics
    ///
    /// If `slot` is past the end of a stripe or `txn` already has the top bit set.
    pub fn delete(
        &mut self,
        slot: u32,
        snapshot: u64,
        txn: u64,
    ) -> std::result::Result<(), Refusal> {
        assert!(slot < STRIPE_ROWS, "slot {slot} is past the end of a stripe");
        assert_eq!(txn & UNCOMMITTED, 0, "a transaction id is under 2^63");
        if self.base.contains(slot) {
            return Err(Refusal::Gone);
        }
        match self.recent.binary_search_by_key(&slot, |&(at, _)| at) {
            Ok(at) => {
                let ts = self.recent[at].1;
                if visible(ts, snapshot, txn) { Err(Refusal::Gone) } else { Err(Refusal::Conflict) }
            }
            Err(at) => {
                self.recent.insert(at, (slot, txn | UNCOMMITTED));
                Ok(())
            }
        }
    }

    /// Stamps every delete of `txn` with its commit timestamp `ts`, and says how many there were.
    ///
    /// # Panics
    ///
    /// If `ts` has the top bit set.
    pub fn commit(&mut self, txn: u64, ts: u64) -> usize {
        assert_eq!(ts & UNCOMMITTED, 0, "a commit timestamp is under 2^63");
        let mut stamped = 0;
        for entry in &mut self.recent {
            if entry.1 == txn | UNCOMMITTED {
                entry.1 = ts;
                stamped += 1;
            }
        }
        stamped
    }

    /// Takes back every delete of `txn`, and says how many there were.
    pub fn abort(&mut self, txn: u64) -> usize {
        let before = self.recent.len();
        self.recent.retain(|&(_, ts)| ts != txn | UNCOMMITTED);
        before - self.recent.len()
    }

    /// Sets the bit of every slot of part `part` that is deleted for a reader at `snapshot`
    /// running as `txn`, over whatever `mask` held, and says whether it set any. A scan that gets
    /// `false` reads the part as though the stripe had no delete vector.
    ///
    /// # Panics
    ///
    /// If `part` is past the end of a stripe.
    pub fn mask(&self, part: u32, snapshot: u64, txn: u64, mask: &mut [u64; PART_WORDS]) -> bool {
        assert!(part < STRIPE_ROWS / PART_ROWS, "part {part} is past the end of a stripe");
        let mut any = self.base.mark(part, mask);
        let start = part * PART_ROWS;
        let from = self.recent.partition_point(|&(slot, _)| slot < start);
        for &(slot, ts) in
            self.recent[from..].iter().take_while(|&&(slot, _)| slot < start + PART_ROWS)
        {
            if visible(ts, snapshot, txn) {
                set(mask, slot - start);
                any = true;
            }
        }
        any
    }

    /// Moves every delete committed at or before `horizon` into the base, which is what a
    /// checkpoint at `horizon` writes, and says how many moved. Uncommitted deletes and later
    /// ones stay in the list.
    ///
    /// The base keeps no timestamps, so after this a reader at a snapshot before `horizon` would
    /// see the moved deletes as though they had always been there. The caller folds only up to
    /// the oldest snapshot still in use.
    pub fn fold(&mut self, horizon: u64) -> usize {
        let folded: Vec<u32> = self
            .recent
            .iter()
            .filter(|&&(_, ts)| ts & UNCOMMITTED == 0 && ts <= horizon)
            .map(|&(slot, _)| slot)
            .collect();
        if folded.is_empty() {
            return 0;
        }
        self.recent.retain(|&(_, ts)| ts & UNCOMMITTED != 0 || ts > horizon);
        let held = self.base.slots();
        let mut slots = Vec::with_capacity(held.len() + folded.len());
        let (mut left, mut right) = (held.iter().peekable(), folded.iter().peekable());
        loop {
            let next = match (left.peek(), right.peek()) {
                (Some(&&a), Some(&&b)) if a <= b => left.next().copied(),
                (Some(_), Some(_)) | (None, Some(_)) => right.next().copied(),
                (Some(_), None) => left.next().copied(),
                (None, None) => None,
            };
            let Some(slot) = next else { break };
            slots.push(slot);
        }
        self.base = Base::of(&slots);
        folded.len()
    }
}

/// Whether a delete stamped `ts` is visible to a reader at `snapshot` running as `txn`.
fn visible(ts: u64, snapshot: u64, txn: u64) -> bool {
    if ts & UNCOMMITTED != 0 { ts == txn | UNCOMMITTED } else { ts <= snapshot }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{Base, DeleteVector, PART_ROWS, PART_WORDS, Refusal, STRIPE_ROWS, UNCOMMITTED};

    fn every_part(vector: &DeleteVector, snapshot: u64, txn: u64) -> Vec<u32> {
        let mut out = Vec::new();
        for part in 0..STRIPE_ROWS / PART_ROWS {
            let mut mask = [0_u64; PART_WORDS];
            let any = vector.mask(part, snapshot, txn, &mut mask);
            assert_eq!(any, mask.iter().any(|&word| word != 0), "part {part}");
            for (at, &word) in mask.iter().enumerate() {
                let mut bits = word;
                while bits != 0 {
                    out.push(part * PART_ROWS + at as u32 * 64 + bits.trailing_zeros());
                    bits &= bits - 1;
                }
            }
        }
        out
    }

    #[test]
    fn the_smallest_container_is_chosen_and_each_reads_back() {
        let cases: Vec<(Vec<u32>, &str)> = vec![
            (vec![], "empty"),
            (vec![3, 9000, 524_287], "array"),
            ((1000..200_000).collect(), "runs"),
            ((0..STRIPE_ROWS).step_by(3).collect(), "bitmap"),
            ((0..4096).map(|n| n * 100).collect(), "array"),
            // Past 4,096 slots an array is not allowed, and apart as these are, runs at 8 bytes
            // each still come to less than the 64 KiB bitmap.
            ((0..4097).map(|n| n * 100).collect(), "runs"),
            ((0..10_000).map(|n| n * 50).collect(), "bitmap"),
            ((0..40).flat_map(|run| run * 1000..run * 1000 + 3).collect(), "runs"),
        ];
        for (slots, shape) in cases {
            let base = Base::of(&slots);
            let got = match &base {
                Base::Empty => "empty",
                Base::Array(_) => "array",
                Base::Bitmap(_) => "bitmap",
                Base::Runs(_) => "runs",
            };
            assert_eq!(got, shape, "{} slots", slots.len());
            assert_eq!(base.slots(), slots);
            assert_eq!(base.len(), slots.len());
            assert_eq!(Base::decode(&base.encode()).expect("reads back"), base);
            for &slot in slots.iter().take(50) {
                assert!(base.contains(slot));
            }
            let vector = DeleteVector::from_base(base);
            assert_eq!(every_part(&vector, 0, 1), slots);
        }
    }

    #[test]
    fn a_damaged_container_is_refused() {
        let base = Base::of(&[5, 6, 7, 100]);
        let bytes = base.encode();
        assert!(Base::decode(&bytes[..6]).is_err());
        let mut kind = bytes.clone();
        kind[0] = 9;
        assert!(Base::decode(&kind).is_err());
        let mut count = bytes.clone();
        count[4] += 1;
        assert!(Base::decode(&count).is_err());
        let unsorted = Base::Array(vec![9, 3]).encode();
        assert!(Base::decode(&unsorted).is_err());
        let touching = Base::Runs(vec![(1, 4), (5, 9)]).encode();
        assert!(Base::decode(&touching).is_err());
        let past = Base::Array(vec![STRIPE_ROWS]).encode();
        assert!(Base::decode(&past).is_err());
    }

    #[test]
    fn a_delete_is_seen_by_its_own_transaction_then_by_snapshots_after_its_commit() {
        let mut vector = DeleteVector::new();
        assert!(vector.is_empty());
        vector.delete(10, 5, 1).expect("delete");
        assert!(vector.is_deleted(10, 5, 1), "its own transaction");
        assert!(!vector.is_deleted(10, 100, 2), "another one before the commit");
        assert_eq!(vector.delete(10, 5, 1), Err(Refusal::Gone));
        assert_eq!(vector.delete(10, 5, 2), Err(Refusal::Conflict));
        assert_eq!(vector.commit(1, 7), 1);
        assert!(!vector.is_deleted(10, 6, 2), "a snapshot before the commit");
        assert!(vector.is_deleted(10, 7, 2), "a snapshot at the commit");
        assert_eq!(vector.delete(10, 6, 3), Err(Refusal::Conflict), "committed after its snapshot");
        assert_eq!(vector.delete(10, 8, 3), Err(Refusal::Gone));
    }

    #[test]
    fn an_abort_takes_back_only_its_own_deletes() {
        let mut vector = DeleteVector::new();
        vector.delete(1, 0, 1).expect("delete");
        vector.delete(2, 0, 2).expect("delete");
        vector.delete(3, 0, 1).expect("delete");
        assert_eq!(vector.abort(1), 2);
        assert_eq!(vector.recent(), 1);
        vector.delete(1, 0, 3).expect("the slot is free again");
    }

    #[test]
    fn a_fold_moves_what_the_checkpoint_covers_and_leaves_the_rest() {
        let mut vector = DeleteVector::from_base(Base::of(&[1, 2, 3]));
        vector.delete(5, 0, 1).expect("delete");
        vector.commit(1, 10);
        vector.delete(4, 0, 2).expect("delete");
        vector.commit(2, 20);
        vector.delete(0, 0, 3).expect("delete");
        assert_eq!(vector.fold(15), 1);
        assert_eq!(vector.base().slots(), vec![1, 2, 3, 5]);
        assert_eq!(vector.recent(), 2);
        assert!(vector.is_deleted(4, 20, 9));
        assert!(!vector.is_deleted(0, 20, 9));
        assert_eq!(vector.fold(15), 0);
        vector.commit(3, 30);
        assert_eq!(vector.fold(100), 2);
        assert_eq!(vector.base().slots(), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(vector.base(), &Base::Runs(vec![(0, 5)]));
    }

    #[test]
    fn it_answers_as_a_plain_map_does() {
        // A model of every delete as `slot -> ts`, driven by a fixed pseudo random sequence of
        // deletes, commits, aborts and folds, and compared at snapshots either side of each commit.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut vector = DeleteVector::new();
        let mut model: BTreeMap<u32, u64> = BTreeMap::new();
        let mut clock = 1_u64;
        let mut floor = 0_u64;
        for round in 0..400_u64 {
            let txn = 1_000 + round;
            let snapshot = clock;
            let mut mine = Vec::new();
            for _ in 0..next() % 40 {
                // Clustered so that runs, arrays and bitmaps all turn up across the rounds.
                let slot = if round % 3 == 0 {
                    (next() % u64::from(STRIPE_ROWS)) as u32
                } else {
                    (round as u32 * 997 + (next() % 64) as u32) % STRIPE_ROWS
                };
                let expected = match model.get(&slot) {
                    None => Ok(()),
                    Some(&ts) if ts & UNCOMMITTED != 0 => {
                        Err(if ts == txn | UNCOMMITTED { Refusal::Gone } else { Refusal::Conflict })
                    }
                    Some(&ts) => {
                        Err(if ts <= snapshot { Refusal::Gone } else { Refusal::Conflict })
                    }
                };
                assert_eq!(vector.delete(slot, snapshot, txn), expected, "slot {slot}");
                if expected.is_ok() {
                    model.insert(slot, txn | UNCOMMITTED);
                    mine.push(slot);
                }
            }
            if next() % 5 == 0 {
                assert_eq!(vector.abort(txn), mine.len());
                for slot in mine {
                    model.remove(&slot);
                }
            } else {
                clock += 1;
                assert_eq!(vector.commit(txn, clock), mine.len());
                for slot in mine {
                    model.insert(slot, clock);
                }
            }
            if next() % 7 == 0 {
                floor = clock.saturating_sub(next() % 3).max(floor);
                vector.fold(floor);
            }
            // A fold forgets when the deletes it moved happened, so no reader is older than it.
            for at in [clock - 1, clock].into_iter().filter(|&at| at >= floor) {
                let expected: Vec<u32> = model
                    .iter()
                    .filter(|&(_, &ts)| ts & UNCOMMITTED == 0 && ts <= at)
                    .map(|(&slot, _)| slot)
                    .collect();
                if round % 50 == 0 {
                    assert_eq!(every_part(&vector, at, 1), expected, "round {round} at {at}");
                }
                for (&slot, _) in model.iter().take(20) {
                    assert_eq!(
                        vector.is_deleted(slot, at, 1),
                        expected.binary_search(&slot).is_ok()
                    );
                }
            }
        }
        vector.fold(clock);
        assert_eq!(vector.recent(), 0);
        assert_eq!(vector.base().slots(), model.keys().copied().collect::<Vec<_>>());
    }
}

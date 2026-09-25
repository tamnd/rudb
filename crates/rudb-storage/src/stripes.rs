//! Rids and a table's stripe directory, `engine-v4/03-the-shape.md` sections 3.2 and 3.3.
//!
//! A table is a sequence of stripes in scan order, each frozen or hot. A scan takes the sequence
//! as it is and keeps it for its lifetime, and a writer that adds a stripe publishes a new
//! sequence, so a scan never sees a stripe appear under it. The sequence is swapped under a lock
//! that is held for the length of an `Arc` clone, which a later change can make a plain atomic
//! swap without changing anything a caller sees.
//!
//! Stripe ids come from a per-table counter and are never reused, so the ids of the sequence are
//! increasing and a rid finds its stripe by binary search.

use std::ops::Range;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use crate::deletes::{DeleteVector, PART_ROWS, PART_WORDS, Refusal};
use crate::hot::{HotStripe, Lease, Width};
use crate::undo::{UndoBuffer, UndoSpace};

/// A row's place: its stripe id in the high 32 bits and its slot in the low 32.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rid(u64);

impl Rid {
    /// The rid of `slot` in stripe `stripe`.
    #[must_use]
    pub const fn new(stripe: u32, slot: u32) -> Self {
        Self(((stripe as u64) << 32) | slot as u64)
    }

    /// The rid a `u64` from [`Self::get`] names.
    #[must_use]
    pub const fn from_u64(raw: u64) -> Self {
        Self(raw)
    }

    /// The rid as a `u64`, the form undo records and log records hold.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Its stripe id.
    #[must_use]
    pub const fn stripe(self) -> u32 {
        (self.0 >> 32) as u32
    }

    /// Its slot in the stripe.
    #[must_use]
    pub const fn slot(self) -> u32 {
        self.0 as u32
    }

    /// Its part in the stripe.
    #[must_use]
    pub const fn part(self) -> u32 {
        self.slot() / PART_ROWS
    }
}

/// A stripe in the database file: encoded, never written in place, with a delete vector over it.
#[derive(Debug)]
pub struct FrozenStripe {
    id: u32,
    rows: u32,
    checkpoint: u64,
    deletes: Mutex<DeleteVector>,
}

impl FrozenStripe {
    /// Stripe `id` of `rows` rows, checkpointed at `checkpoint` with `deletes` over it.
    #[must_use]
    pub fn new(id: u32, rows: u32, checkpoint: u64, deletes: DeleteVector) -> Self {
        Self { id, rows, checkpoint, deletes: Mutex::new(deletes) }
    }

    /// Its id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Rows written into it, deleted or not.
    #[must_use]
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// The timestamp its pages and the base of its delete vector are as of.
    #[must_use]
    pub fn checkpoint(&self) -> u64 {
        self.checkpoint
    }

    /// Runs `f` with its delete vector.
    pub fn with_deletes<T>(&self, f: impl FnOnce(&mut DeleteVector) -> T) -> T {
        f(&mut self.deletes.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// A stripe of a table.
#[derive(Debug, Clone)]
pub enum Stripe {
    /// Encoded in the file.
    Frozen(Arc<FrozenStripe>),
    /// In memory and taking writes.
    Hot(Arc<HotStripe>),
}

impl Stripe {
    /// Its id.
    #[must_use]
    pub fn id(&self) -> u32 {
        match self {
            Self::Frozen(stripe) => stripe.id(),
            Self::Hot(stripe) => stripe.id(),
        }
    }

    /// Slots a scan reads: every row of a frozen stripe, every leased slot of a hot one.
    #[must_use]
    pub fn slots(&self) -> u32 {
        match self {
            Self::Frozen(stripe) => stripe.rows(),
            Self::Hot(stripe) => stripe.reserved(),
        }
    }

    /// Sets bit `i` of `mask` for every slot `i` of `part` that a reader at `snapshot` running as
    /// `txn` sees, clears the others, and says how many it set.
    ///
    /// # Panics
    ///
    /// If `part` is past the end of a stripe.
    pub fn visible(
        &self,
        part: u32,
        snapshot: u64,
        txn: u64,
        mask: &mut [u64; PART_WORDS],
    ) -> usize {
        match self {
            Self::Hot(stripe) => stripe.mask(part, snapshot, txn, mask),
            Self::Frozen(stripe) => {
                let start = part * PART_ROWS;
                let rows = stripe.rows().saturating_sub(start).min(PART_ROWS) as usize;
                mask.fill(0);
                if stripe.with_deletes(|deletes| deletes.mask(part, snapshot, txn, mask)) {
                    for word in mask.iter_mut() {
                        *word = !*word;
                    }
                } else {
                    mask.fill(u64::MAX);
                }
                clip(mask, rows);
                mask.iter().map(|word| word.count_ones() as usize).sum()
            }
        }
    }
}

/// Clears every bit of `mask` from `rows` on.
fn clip(mask: &mut [u64; PART_WORDS], rows: usize) {
    for (i, word) in mask.iter_mut().enumerate() {
        let start = i * 64;
        if start >= rows {
            *word = 0;
        } else if rows - start < 64 {
            *word &= (1 << (rows - start)) - 1;
        }
    }
}

/// The stripes of one table in scan order.
#[derive(Debug)]
pub struct StripeDirectory {
    stripes: RwLock<Arc<[Stripe]>>,
    /// The next stripe id.
    next: AtomicU32,
    widths: Arc<[Width]>,
    undos: Arc<UndoSpace>,
}

impl StripeDirectory {
    /// An empty table with a column of each width in `widths`, whose updates keep their undo
    /// records in `undos`.
    #[must_use]
    pub fn new(widths: &[Width], undos: Arc<UndoSpace>) -> Self {
        Self {
            stripes: RwLock::new(Arc::from(Vec::new())),
            next: AtomicU32::new(0),
            widths: widths.into(),
            undos,
        }
    }

    /// The stripes as they are now, for a scan to keep.
    #[must_use]
    pub fn stripes(&self) -> Arc<[Stripe]> {
        Arc::clone(&self.stripes.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The stripe with id `id`.
    #[must_use]
    pub fn find(&self, id: u32) -> Option<Stripe> {
        let stripes = self.stripes();
        let at = stripes.binary_search_by_key(&id, Stripe::id).ok()?;
        Some(stripes[at].clone())
    }

    /// Appends a frozen stripe of `rows` rows checkpointed at `checkpoint`, as a bulk load's
    /// Attach does, and returns its id.
    pub fn attach(&self, rows: u32, checkpoint: u64, deletes: DeleteVector) -> u32 {
        self.push(|id| Stripe::Frozen(Arc::new(FrozenStripe::new(id, rows, checkpoint, deletes))))
    }

    /// The hot stripe at the end that takes inserts, made when the last stripe is frozen or full.
    pub fn open_stripe(&self) -> Arc<HotStripe> {
        loop {
            let stripes = self.stripes();
            if let Some(Stripe::Hot(last)) = stripes.last()
                && !last.is_full()
            {
                return Arc::clone(last);
            }
            let mut guard = self.stripes.write().unwrap_or_else(PoisonError::into_inner);
            if !Arc::ptr_eq(&guard, &stripes) {
                continue;
            }
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            let hot = Arc::new(HotStripe::with_undo(id, &self.widths, Arc::clone(&self.undos)));
            let mut next = guard.to_vec();
            next.push(Stripe::Hot(Arc::clone(&hot)));
            *guard = next.into();
            return hot;
        }
    }

    /// Deletes the row at `rid` for transaction `txn` reading at `snapshot`: in place in a hot
    /// stripe, in the delete vector of a frozen one.
    ///
    /// # Errors
    ///
    /// [`Refusal::Gone`] when `txn` does not see the row, which includes a rid of no stripe, and
    /// [`Refusal::Conflict`] when another transaction holds it or deleted it after `snapshot`.
    pub fn delete(
        &self,
        rid: Rid,
        snapshot: u64,
        txn: u64,
        buffer: &mut UndoBuffer,
    ) -> Result<(), Refusal> {
        match self.find(rid.stripe()) {
            Some(Stripe::Hot(stripe)) => stripe.delete(rid.slot(), snapshot, txn, buffer),
            Some(Stripe::Frozen(stripe)) if rid.slot() < stripe.rows() => {
                stripe.with_deletes(|deletes| deletes.delete(rid.slot(), snapshot, txn))
            }
            _ => Err(Refusal::Gone),
        }
    }

    /// Stamps what `txn` did to the rows at `rids` with its commit timestamp `ts`.
    pub fn commit(&self, rids: &[Rid], txn: u64, ts: u64) {
        self.finish(rids, |stripe, slots| match stripe {
            Stripe::Hot(stripe) => {
                for slot in slots {
                    stripe.commit_write(slot, txn, ts);
                }
            }
            Stripe::Frozen(stripe) => {
                stripe.with_deletes(|deletes| deletes.commit(txn, ts));
            }
        });
    }

    /// Takes back what `txn` did to the rows at `rids`.
    pub fn abort(&self, rids: &[Rid], txn: u64) {
        self.finish(rids, |stripe, slots| match stripe {
            Stripe::Hot(stripe) => {
                for slot in slots {
                    stripe.abort_write(slot, txn);
                }
            }
            Stripe::Frozen(stripe) => {
                stripe.with_deletes(|deletes| deletes.abort(txn));
            }
        });
    }

    /// Rows a reader at `snapshot` running as `txn` sees.
    #[must_use]
    pub fn count(&self, snapshot: u64, txn: u64) -> u64 {
        let mut mask = [0_u64; PART_WORDS];
        let mut count = 0;
        for stripe in &*self.stripes() {
            for part in 0..stripe.slots().div_ceil(PART_ROWS) {
                count += stripe.visible(part, snapshot, txn, &mut mask) as u64;
            }
        }
        count
    }

    /// Runs `f` once per stripe of `rids` with the slots of that stripe, in the order given.
    fn finish(&self, rids: &[Rid], mut f: impl FnMut(&Stripe, &mut dyn Iterator<Item = u32>)) {
        let mut rest = rids;
        while let Some(first) = rest.first() {
            let run = rest.iter().take_while(|rid| rid.stripe() == first.stripe()).count();
            if let Some(stripe) = self.find(first.stripe()) {
                f(&stripe, &mut rest[..run].iter().map(|rid| rid.slot()));
            }
            rest = &rest[run..];
        }
    }

    fn push(&self, make: impl FnOnce(u32) -> Stripe) -> u32 {
        let mut guard = self.stripes.write().unwrap_or_else(PoisonError::into_inner);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut next = guard.to_vec();
        next.push(make(id));
        *guard = next.into();
        id
    }
}

/// A worker's inserts into a table: slots leased from the open hot stripe, moving to the next one
/// when a stripe fills.
#[derive(Debug)]
pub struct Inserter {
    directory: Arc<StripeDirectory>,
    lease: Option<Lease>,
}

impl Inserter {
    /// A worker's inserter into the table of `directory`.
    #[must_use]
    pub fn new(directory: Arc<StripeDirectory>) -> Self {
        Self { directory, lease: None }
    }

    /// Up to `want` consecutive slots in one stripe, with the lease that holds them for writing
    /// values and text. Never empty when `want` is not 0.
    pub fn take(&mut self, want: u32) -> (&mut Lease, Range<u32>) {
        let mut lease = match self.lease.take() {
            Some(lease) => lease,
            None => Lease::new(self.directory.open_stripe()),
        };
        loop {
            let slots = lease.take(want);
            if !slots.is_empty() || want == 0 {
                return (self.lease.insert(lease), slots);
            }
            lease = Lease::new(self.directory.open_stripe());
        }
    }

    /// The rids of `slots` in the stripe of the current lease.
    #[must_use]
    pub fn rids(&self, slots: Range<u32>) -> Vec<Rid> {
        let stripe = self.lease.as_ref().map_or(0, |lease| lease.stripe().id());
        slots.map(|slot| Rid::new(stripe, slot)).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::{Inserter, Rid, Stripe, StripeDirectory};
    use crate::deletes::{Base, DeleteVector, Refusal, STRIPE_ROWS};
    use crate::hot::Width;
    use crate::undo::{UndoBuffer, UndoSpace};

    fn directory() -> Arc<StripeDirectory> {
        Arc::new(StripeDirectory::new(&[Width::Eight], Arc::new(UndoSpace::new())))
    }

    #[test]
    fn a_rid_is_a_stripe_and_a_slot() {
        let rid = Rid::new(7, 524_287);
        assert_eq!((rid.stripe(), rid.slot(), rid.part()), (7, 524_287, 63));
        assert_eq!(Rid::from_u64(rid.get()), rid);
        assert!(Rid::new(1, 0) > Rid::new(0, 524_287));
    }

    #[test]
    fn frozen_and_hot_stripes_count_and_delete_under_snapshots() {
        let table = directory();
        let frozen = table.attach(10_000, 1, DeleteVector::from_base(Base::of(&[0, 9_999])));
        let mut inserter = Inserter::new(Arc::clone(&table));
        let (lease, slots) = inserter.take(100);
        for slot in slots.clone() {
            lease.stripe().write(slot, 0, Some(u128::from(slot)));
        }
        lease.stripe().insert(slots.clone(), 5);
        lease.stripe().commit_insert(slots.clone(), 5, 2);
        let hot = inserter.rids(slots)[0].stripe();
        assert_eq!(table.stripes().len(), 2);
        assert_ne!(hot, frozen);
        assert_eq!(table.count(1, 0), 9_998);
        assert_eq!(table.count(2, 0), 10_098);

        let mut buffer = UndoBuffer::default();
        let rids = [Rid::new(frozen, 5), Rid::new(frozen, 6), Rid::new(hot, 3)];
        for &rid in &rids {
            table.delete(rid, 2, 8, &mut buffer).expect("delete");
        }
        assert_eq!(table.count(2, 8), 10_095, "its own deletes");
        assert_eq!(table.count(9, 0), 10_098, "not committed");
        assert_eq!(table.delete(Rid::new(frozen, 5), 2, 9, &mut buffer), Err(Refusal::Conflict));
        assert_eq!(table.delete(Rid::new(frozen, 0), 2, 9, &mut buffer), Err(Refusal::Gone));
        assert_eq!(table.delete(Rid::new(frozen, 20_000), 2, 9, &mut buffer), Err(Refusal::Gone));
        assert_eq!(table.delete(Rid::new(99, 0), 2, 9, &mut buffer), Err(Refusal::Gone));
        table.commit(&rids, 8, 3);
        assert_eq!(table.count(2, 0), 10_098);
        assert_eq!(table.count(3, 0), 10_095);

        let again = [Rid::new(frozen, 7), Rid::new(hot, 4)];
        for &rid in &again {
            table.delete(rid, 3, 10, &mut buffer).expect("delete");
        }
        table.abort(&again, 10);
        assert_eq!(table.count(3, 10), 10_095);
    }

    #[test]
    fn a_full_stripe_is_followed_by_a_new_one() {
        let table = directory();
        let mut inserter = Inserter::new(Arc::clone(&table));
        let mut taken = 0;
        let mut stripes = Vec::new();
        while taken < STRIPE_ROWS + 10 {
            let (lease, slots) = inserter.take(4_096);
            taken += slots.len() as u32;
            stripes.push(lease.stripe().id());
        }
        stripes.dedup();
        assert_eq!(stripes, [0, 1]);
        assert!(matches!(table.stripes()[0], Stripe::Hot(ref stripe) if stripe.is_full()));
    }

    #[test]
    fn workers_share_the_open_stripe_and_make_one_new_stripe_at_a_time() {
        let table = directory();
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let table = Arc::clone(&table);
                thread::spawn(move || {
                    let mut inserter = Inserter::new(table);
                    let mut rids = Vec::new();
                    for _ in 0..100_000 {
                        let (_, slots) = inserter.take(1);
                        rids.extend(inserter.rids(slots));
                    }
                    rids
                })
            })
            .collect();
        let mut rids: Vec<Rid> =
            workers.into_iter().flat_map(|worker| worker.join().expect("finishes")).collect();
        let before = rids.len();
        rids.sort_unstable();
        rids.dedup();
        assert_eq!(rids.len(), before, "no rid handed out twice");
        let ids: Vec<u32> = table.stripes().iter().map(Stripe::id).collect();
        assert_eq!(ids, (0..ids.len() as u32).collect::<Vec<_>>());
        assert_eq!(ids.len(), 2, "800,000 rows and at most 8 short leases fit in two stripes");
    }
}

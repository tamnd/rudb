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
//!
//! The freezer, `10-freezing-and-checkpoint.md` sections 10.2 and 10.3, turns a hot stripe into a
//! frozen one in place in the sequence, under the same id. [`to_freeze`] picks the stripes,
//! [`StripeDirectory::begin_freeze`] and [`StripeDirectory::settle`] stop writes in place and wait
//! for the stripe's rows to be final, the caller encodes them, and [`StripeDirectory::publish`]
//! swaps the frozen stripe in. A statement still reading the table at a snapshot from before
//! would meet a stripe with no `created` stamps to tell it which rows were not there yet, so the
//! rows such statements see differently go into a [`Graveyard`] beside the frozen stripe, which
//! [`StripeDirectory::bury`] drops once they have all ended. Freezing writes nothing to the log:
//! it changes how the rows are held, not what they are.
//!
//! A thaw, `07-the-head.md` section 7.9, puts a hot stripe back under the frozen one's id, so
//! every rid stays where it is. [`StripeDirectory::begin_thaw`] marks the frozen stripe, and from
//! then on a delete or a move of one of its rows waits for the thaw instead of adding to its
//! delete vector. [`StripeDirectory::thaw`] has the caller decode the rows into a new hot stripe,
//! takes the delete vector over and swaps the hot stripe in, all under the delete vector's lock,
//! so a transaction that deleted a row before the thaw commits or aborts it on whichever side it
//! finds. [`crate::temper`] says which stripes to thaw.

use std::ops::Range;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};

use crate::deletes::{DeleteVector, PART_ROWS, PART_WORDS, Refusal};
use crate::graveyard::Graveyard;
use crate::horizon::Horizons;
use crate::hot::{HotStripe, Lease, Width};
use crate::park::Wait;
use crate::undo::{Heads, UndoBuffer, UndoRef, UndoSpace};

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

/// A frozen stripe no thaw has begun on.
const STILL: u8 = 0;

/// A frozen stripe being thawed, whose rows are waited for.
const THAWING: u8 = 1;

/// A frozen stripe a hot one has replaced.
const THAWED: u8 = 2;

/// A stripe in the database file: encoded, never written in place, with a delete vector over it.
#[derive(Debug)]
pub struct FrozenStripe {
    id: u32,
    rows: u32,
    checkpoint: u64,
    /// Shared with the hot stripe it was frozen from, which took deletes into it during the
    /// freeze.
    deletes: Arc<Mutex<DeleteVector>>,
    /// [`STILL`], [`THAWING`] or [`THAWED`], changed under the lock of `deletes`.
    thaw: AtomicU8,
    /// Wakes the writers that wait for a thaw.
    thawed: Condvar,
    /// Rows moved out of it into the head, which the thaw rule reads as a rate.
    migrations: AtomicU64,
    /// The rows the statements running when it was swapped in see differently from its
    /// checkpoint, until they end.
    graveyard: Mutex<Option<Arc<Graveyard>>>,
}

impl FrozenStripe {
    /// Stripe `id` of `rows` rows, checkpointed at `checkpoint` with `deletes` over it.
    #[must_use]
    pub fn new(id: u32, rows: u32, checkpoint: u64, deletes: DeleteVector) -> Self {
        Self::sharing(id, rows, checkpoint, Arc::new(Mutex::new(deletes)))
    }

    fn sharing(id: u32, rows: u32, checkpoint: u64, deletes: Arc<Mutex<DeleteVector>>) -> Self {
        Self {
            id,
            rows,
            checkpoint,
            deletes,
            thaw: AtomicU8::new(STILL),
            thawed: Condvar::new(),
            migrations: AtomicU64::new(0),
            graveyard: Mutex::new(None),
        }
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
        f(&mut self.lock())
    }

    /// Rows moved out of it into the head so far.
    #[must_use]
    pub fn migrations(&self) -> u64 {
        self.migrations.load(Ordering::Relaxed)
    }

    /// Whether a thaw of it began.
    #[must_use]
    pub fn is_thawing(&self) -> bool {
        self.thaw.load(Ordering::SeqCst) != STILL
    }

    /// The rows the statements that were running when it was swapped in see differently from its
    /// checkpoint, while one of them may still be running. A reader below the checkpoint reads a
    /// slot the graveyard holds from it, see [`Graveyard::seen`], and takes it once for its scan:
    /// it is not dropped while the reader runs.
    #[must_use]
    pub fn graveyard(&self) -> Option<Arc<Graveyard>> {
        self.graveyard.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Drops its graveyard once `horizon`, the oldest snapshot reading its table, is at or past
    /// its checkpoint, and says whether it did.
    fn bury(&self, horizon: u64) -> bool {
        let mut graveyard = self.graveyard.lock().unwrap_or_else(PoisonError::into_inner);
        let done = graveyard.as_ref().is_some_and(|graveyard| graveyard.at() <= horizon);
        if done {
            *graveyard = None;
        }
        done
    }

    /// Counts `n` more rows moved out, for a test of the thaw rule.
    #[cfg(test)]
    pub(crate) fn add_migrations(&self, n: u64) {
        self.migrations.fetch_add(n, Ordering::Relaxed);
    }

    fn lock(&self) -> MutexGuard<'_, DeleteVector> {
        self.deletes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f`, a delete, with its delete vector. `None` when a thaw began, after waiting for the
    /// hot stripe to take its place, where the caller makes the delete instead.
    fn delete_with<T>(&self, f: impl FnOnce(&mut DeleteVector) -> T) -> Option<T> {
        let mut deletes = self.lock();
        if self.thaw.load(Ordering::SeqCst) == STILL {
            return Some(f(&mut deletes));
        }
        while self.thaw.load(Ordering::SeqCst) != THAWED {
            deletes = self.thawed.wait(deletes).unwrap_or_else(PoisonError::into_inner);
        }
        None
    }

    /// Runs `f`, a commit or an abort, with its delete vector, and says whether it did. Not once
    /// the hot stripe of a thaw took its place, which took the deletes over with it.
    fn finish_with(&self, f: impl FnOnce(&mut DeleteVector)) -> bool {
        let mut deletes = self.lock();
        let still = self.thaw.load(Ordering::SeqCst) != THAWED;
        if still {
            f(&mut deletes);
        }
        still
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
                if snapshot < stripe.checkpoint()
                    && let Some(graveyard) = stripe.graveyard()
                {
                    graveyard.patch(part, snapshot, mask);
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
    /// The table's id, which its stripes' undo records name.
    table: u32,
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
            table: 0,
        }
    }

    /// The same table with id `table`, which its undo records are held to the horizon of.
    #[must_use]
    pub fn in_table(mut self, table: u32) -> Self {
        self.table = table;
        self
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
            let hot = Arc::new(
                HotStripe::with_undo(id, &self.widths, Arc::clone(&self.undos))
                    .in_table(self.table),
            );
            let mut next = guard.to_vec();
            next.push(Stripe::Hot(Arc::clone(&hot)));
            *guard = next.into();
            return hot;
        }
    }

    /// Deletes the row at `rid` for transaction `txn` reading at `snapshot`: in place in a hot
    /// stripe, in the delete vector of a frozen one. A row of a frozen stripe being thawed waits
    /// for the thaw and is deleted in place in the hot stripe it makes.
    ///
    /// # Errors
    ///
    /// [`Refusal::Gone`] when `txn` does not see the row, which includes a rid of no stripe, and
    /// [`Refusal::Conflict`] when another transaction holds it or deleted it after `snapshot`.
    /// `wait` says whether a hot row another transaction holds is waited for; a frozen row's
    /// entry is a conflict at once.
    pub fn delete(
        &self,
        rid: Rid,
        snapshot: u64,
        txn: u64,
        wait: Wait,
        buffer: &mut UndoBuffer,
    ) -> Result<(), Refusal> {
        loop {
            match self.find(rid.stripe()) {
                Some(Stripe::Hot(stripe)) => {
                    return stripe.delete(rid.slot(), snapshot, txn, wait, buffer);
                }
                Some(Stripe::Frozen(stripe)) if rid.slot() < stripe.rows() => {
                    let deleted =
                        stripe.delete_with(|deletes| deletes.delete(rid.slot(), snapshot, txn));
                    if let Some(deleted) = deleted {
                        return deleted;
                    }
                }
                _ => return Err(Refusal::Gone),
            }
        }
    }

    /// Updates the frozen row at `rid` for transaction `txn` reading at `snapshot` by moving it
    /// into the head, `engine-v4/07-the-head.md` section 7.8, and returns the rid of the new
    /// version.
    ///
    /// A frozen row is never written in place. Its delete vector entry is taken first, and that
    /// entry is the row's lock, the same one a delete of it takes, so an update and a delete of
    /// one frozen row conflict just as they do on a hot row. Then a slot is leased from
    /// `inserter` and `fill` writes the whole new version into it, because only the caller can
    /// decode the old row and apply the statement's changes to it. The slot is published as an
    /// insert by `txn`, so until [`Self::commit`] is given both rids, other transactions see the
    /// old row and `txn` sees the new one. A hot row is updated in place with
    /// [`HotStripe::update`] instead.
    ///
    /// A row of a hot stripe being frozen moves the same way, with its entry in the delete vector
    /// the frozen stripe will keep, since [`HotStripe::update`] refuses it with
    /// [`Refusal::Freezing`]. A row of a frozen stripe being thawed waits for the thaw and is
    /// refused with [`Refusal::Thawed`] after it, and the caller updates it in place.
    ///
    /// # Errors
    ///
    /// [`Refusal::Gone`] when `txn` does not see the row, which includes a rid of no frozen
    /// stripe and of a hot stripe that is not being frozen, and [`Refusal::Conflict`] when
    /// another transaction holds it or wrote it after `snapshot`. `fill` is not called then, and
    /// no slot is taken.
    pub fn migrate(
        &self,
        rid: Rid,
        snapshot: u64,
        txn: u64,
        inserter: &mut Inserter,
        mut fill: impl FnMut(&mut Lease, u32),
    ) -> Result<Rid, Refusal> {
        loop {
            match self.find(rid.stripe()) {
                Some(Stripe::Frozen(stripe)) if rid.slot() < stripe.rows() => {
                    let deleted =
                        stripe.delete_with(|deletes| deletes.delete(rid.slot(), snapshot, txn));
                    if let Some(deleted) = deleted {
                        deleted?;
                        stripe.migrations.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }
                Some(Stripe::Hot(stripe))
                    if stripe.is_freezing() && rid.slot() < stripe.reserved() =>
                {
                    stripe.delete_pending(rid.slot(), snapshot, txn)?;
                    break;
                }
                Some(Stripe::Hot(stripe))
                    if stripe.is_thawed() && rid.slot() < stripe.reserved() =>
                {
                    return Err(Refusal::Thawed);
                }
                _ => return Err(Refusal::Gone),
            }
        }
        loop {
            let (lease, slots) = inserter.take(1);
            fill(lease, slots.start);
            if lease.stripe().insert(slots.clone(), txn) {
                return Ok(Rid::new(lease.stripe().id(), slots.start));
            }
        }
    }

    /// Bytes its hot stripes hold.
    #[must_use]
    pub fn hot_bytes(&self) -> usize {
        self.stripes()
            .iter()
            .map(|stripe| match stripe {
                Stripe::Hot(stripe) => stripe.bytes(),
                Stripe::Frozen(_) => 0,
            })
            .sum()
    }

    /// Begins freezing hot stripe `id`, [`HotStripe::begin_freeze`], and returns it. `None` when
    /// it is not a hot stripe or its freeze already began.
    pub fn begin_freeze(&self, id: u32) -> Option<Arc<HotStripe>> {
        match self.find(id) {
            Some(Stripe::Hot(stripe)) if stripe.begin_freeze() => Some(stripe),
            _ => None,
        }
    }

    /// The freeze of `stripe` once its rows are final as of `clock`, [`HotStripe::settled`], with
    /// the slots that were holes or deleted set as the base of the delete vector the frozen stripe
    /// keeps. `None` while a write of it is in flight; the freezer asks again.
    pub fn settle(&self, stripe: &Arc<HotStripe>, clock: &AtomicU64) -> Option<Freeze> {
        let (at, rows, gone) = stripe.settled(clock)?;
        stripe.pending().lock().unwrap_or_else(PoisonError::into_inner).set_base(&gone);
        Some(Freeze { stripe: Arc::clone(stripe), at, rows })
    }

    /// Swaps the frozen stripe in for the hot one `freeze` settled and returns it. A statement
    /// that loaded the stripes before keeps reading the hot stripe, which lives as long as it
    /// does. A statement reading the table at a snapshot before the freeze's F, which may load the
    /// stripes again in its next scan, reads the rows it sees differently from F out of the
    /// frozen stripe's [`Graveyard`], dug here from the hot stripe's stamps and undo chains.
    ///
    /// The snapshots are read after F was, so a statement missing from them has a snapshot at or
    /// past F and needs nothing of the graveyard.
    ///
    /// # Panics
    ///
    /// If the hot stripe `freeze` settled is no longer in the table, which it is until this swaps
    /// it out, and a stripe is frozen once.
    pub fn publish(&self, freeze: Freeze, horizons: &Horizons) -> Arc<FrozenStripe> {
        let frozen = Arc::new(FrozenStripe::sharing(
            freeze.stripe.id(),
            freeze.rows,
            freeze.at,
            Arc::clone(freeze.stripe.pending()),
        ));
        let snapshots = horizons.snapshots(self.table);
        if let Some(graveyard) = Graveyard::dig(&freeze.stripe, freeze.rows, freeze.at, &snapshots)
        {
            *frozen.graveyard.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(Arc::new(graveyard));
        }
        let mut guard = self.stripes.write().unwrap_or_else(PoisonError::into_inner);
        let mut next = guard.to_vec();
        let at = next
            .iter()
            .position(
                |stripe| matches!(stripe, Stripe::Hot(hot) if Arc::ptr_eq(hot, &freeze.stripe)),
            )
            .expect("a stripe is frozen once");
        next[at] = Stripe::Frozen(Arc::clone(&frozen));
        *guard = next.into();
        freeze.stripe.frozen();
        frozen
    }

    /// Drops the graveyards of the frozen stripes no statement reading the table needs any more,
    /// the ones whose checkpoint its horizon reached, and says how many it dropped.
    pub fn bury(&self, horizons: &Horizons, clock: &AtomicU64) -> usize {
        let horizon = horizons.oldest(clock, self.table);
        self.stripes()
            .iter()
            .filter(|stripe| matches!(stripe, Stripe::Frozen(frozen) if frozen.bury(horizon)))
            .count()
    }

    /// Bytes the graveyards of its frozen stripes hold.
    #[must_use]
    pub fn graveyard_bytes(&self) -> usize {
        self.stripes()
            .iter()
            .filter_map(|stripe| match stripe {
                Stripe::Frozen(frozen) => frozen.graveyard().map(|graveyard| graveyard.bytes()),
                Stripe::Hot(_) => None,
            })
            .sum()
    }

    /// Begins thawing frozen stripe `id`, `07-the-head.md` section 7.9 step 1, and returns it:
    /// from here on a delete or a move of one of its rows waits for [`Self::thaw`]. `None` when
    /// it is not a frozen stripe, its thaw already began, or it still has a graveyard, whose rows
    /// a hot stripe made from its pages would not have.
    pub fn begin_thaw(&self, id: u32) -> Option<Arc<FrozenStripe>> {
        let Some(Stripe::Frozen(frozen)) = self.find(id) else { return None };
        let deletes = frozen.lock();
        if frozen.thaw.load(Ordering::SeqCst) != STILL || frozen.graveyard().is_some() {
            return None;
        }
        frozen.thaw.store(THAWING, Ordering::SeqCst);
        drop(deletes);
        Some(frozen)
    }

    /// Thaws `frozen`, whose thaw [`Self::begin_thaw`] began, and returns the hot stripe now in
    /// its place, steps 2 to 4: `fill` writes every slot's values into a hot stripe made with
    /// [`HotStripe::thawing`], decoded from the frozen stripe's pages as of its checkpoint, and
    /// then, under the lock of the delete vector, the hot stripe takes the deletes over, is
    /// swapped in, and the writers waiting are woken. A scan that loaded the stripes before
    /// finishes on the frozen stripe, which nothing changes once it is thawing but the commits
    /// and aborts of deletes already in it, and those are in it as of the scan's snapshot.
    ///
    /// # Panics
    ///
    /// If no thaw of `frozen` began, or it is not in the table.
    pub fn thaw(
        &self,
        frozen: &Arc<FrozenStripe>,
        fill: impl FnOnce(&HotStripe),
    ) -> Arc<HotStripe> {
        assert_eq!(frozen.thaw.load(Ordering::SeqCst), THAWING, "a thaw began");
        let hot = Arc::new(HotStripe::thawing(
            frozen.id(),
            self.table,
            &self.widths,
            Arc::clone(&self.undos),
            frozen.rows(),
        ));
        fill(&hot);
        let deletes = frozen.lock();
        hot.take_deletes(&deletes, frozen.checkpoint());
        let mut guard = self.stripes.write().unwrap_or_else(PoisonError::into_inner);
        let mut next = guard.to_vec();
        let at = next
            .iter()
            .position(|stripe| matches!(stripe, Stripe::Frozen(old) if Arc::ptr_eq(old, frozen)))
            .expect("a thawing stripe is in its table");
        next[at] = Stripe::Hot(Arc::clone(&hot));
        *guard = next.into();
        drop(guard);
        frozen.thaw.store(THAWED, Ordering::SeqCst);
        frozen.thawed.notify_all();
        drop(deletes);
        hot
    }

    /// The table's id, which its undo records and the thaw rule name it by.
    #[must_use]
    pub fn table(&self) -> u32 {
        self.table
    }

    /// The column widths of its stripes.
    #[must_use]
    pub fn widths(&self) -> &[Width] {
        &self.widths
    }

    /// Stamps what `txn` did to the rows at `rids` with its commit timestamp `ts`: its updates
    /// and deletes, and the rows it inserted, which is how the new version of a migrated row is
    /// committed beside the delete of the old one.
    pub fn commit(&self, rids: &[Rid], txn: u64, ts: u64) {
        self.finish(rids, |stripe, slots| match stripe {
            Stripe::Hot(stripe) => {
                for slot in slots {
                    stripe.commit_write(slot, txn, ts);
                    stripe.commit_insert(slot..slot + 1, txn, ts);
                }
                true
            }
            Stripe::Frozen(stripe) => stripe.finish_with(|deletes| {
                deletes.commit(txn, ts);
            }),
        });
    }

    /// Takes back what `txn` did to the rows at `rids`, the rows it inserted included.
    pub fn abort(&self, rids: &[Rid], txn: u64) {
        self.finish(rids, |stripe, slots| match stripe {
            Stripe::Hot(stripe) => {
                for slot in slots {
                    stripe.abort_write(slot, txn);
                    stripe.abort_insert(slot..slot + 1, txn);
                }
                true
            }
            Stripe::Frozen(stripe) => stripe.finish_with(|deletes| {
                deletes.abort(txn);
            }),
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

    /// Runs `f` once per stripe of `rids` with the slots of that stripe, in the order given, and
    /// again on the stripe in its place when `f` says a thaw replaced it.
    fn finish(
        &self,
        rids: &[Rid],
        mut f: impl FnMut(&Stripe, &mut dyn Iterator<Item = u32>) -> bool,
    ) {
        let mut rest = rids;
        while let Some(first) = rest.first() {
            let run = rest.iter().take_while(|rid| rid.stripe() == first.stripe()).count();
            while let Some(stripe) = self.find(first.stripe()) {
                if f(&stripe, &mut rest[..run].iter().map(|rid| rid.slot())) {
                    break;
                }
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

impl Heads for StripeDirectory {
    fn unlink(&self, table: u32, rid: u64, record: UndoRef, older: bool) {
        let rid = Rid::from_u64(rid);
        if table == self.table
            && let Some(Stripe::Hot(stripe)) = self.find(rid.stripe())
        {
            stripe.unlink(rid.slot(), record, older);
        }
    }

    fn prune(&self, table: u32, rid: u64, spare: &mut dyn FnMut(UndoRef) -> bool) -> Vec<UndoRef> {
        let rid = Rid::from_u64(rid);
        match self.find(rid.stripe()) {
            Some(Stripe::Hot(stripe)) if table == self.table => stripe.prune(rid.slot(), spare),
            _ => Vec::new(),
        }
    }

    fn replace(&self, table: u32, rid: u64, record: UndoRef, with: UndoRef) -> bool {
        let rid = Rid::from_u64(rid);
        table == self.table
            && matches!(self.find(rid.stripe()), Some(Stripe::Hot(stripe)) if stripe.replace(rid.slot(), record, with))
    }

    fn head(&self, table: u32, rid: u64) -> UndoRef {
        let rid = Rid::from_u64(rid);
        match self.find(rid.stripe()) {
            Some(Stripe::Hot(stripe)) if table == self.table => stripe.head(rid.slot()),
            _ => 0,
        }
    }
}

/// A hot stripe whose rows are final, on its way to frozen: [`StripeDirectory::settle`] made it
/// and [`StripeDirectory::publish`] takes it.
#[derive(Debug)]
pub struct Freeze {
    stripe: Arc<HotStripe>,
    at: u64,
    rows: u32,
}

impl Freeze {
    /// The hot stripe, which the encoder reads the rows of as of [`Self::at`].
    #[must_use]
    pub fn stripe(&self) -> &Arc<HotStripe> {
        &self.stripe
    }

    /// F, the timestamp its rows are final as of and the frozen stripe's checkpoint.
    #[must_use]
    pub fn at(&self) -> u64 {
        self.at
    }

    /// The slots the frozen stripe has, holes and deleted rows among them.
    #[must_use]
    pub fn rows(&self) -> u32 {
        self.rows
    }
}

/// The hot stripes to freeze across `tables`, as `(table, stripe id)` with `table` an index into
/// `tables`, `10-freezing-and-checkpoint.md` section 10.2: every sealed one no commit touched
/// for `quiet_ms` before `now_ms`, and then, while the hot set is over `limit` bytes, the coldest
/// of the rest by their last commit. When nothing sealed is left and the hot set is still over,
/// the coldest open stripe is sealed early and taken too, which leaves it short.
#[must_use]
pub fn to_freeze(
    tables: &[Arc<StripeDirectory>],
    now_ms: u64,
    quiet_ms: u64,
    limit: usize,
) -> Vec<(usize, u32)> {
    let mut hot: Vec<(u64, usize, Arc<HotStripe>)> = Vec::new();
    let mut total = 0;
    for (table, directory) in tables.iter().enumerate() {
        for stripe in &*directory.stripes() {
            if let Stripe::Hot(stripe) = stripe {
                total += stripe.bytes();
                if !stripe.is_freezing() {
                    hot.push((stripe.touched(), table, Arc::clone(stripe)));
                }
            }
        }
    }
    hot.sort_by_key(|&(touched, table, ref stripe)| (touched, table, stripe.id()));
    let mut picked = Vec::new();
    let mut take = |table: usize, stripe: &HotStripe, total: &mut usize| {
        *total = total.saturating_sub(stripe.bytes());
        picked.push((table, stripe.id()));
    };
    // A thawed stripe is sealed too, but goes back by the thaw rule's own test, [`crate::temper`].
    hot.retain(|(touched, table, stripe)| {
        let quiet =
            stripe.is_full() && !stripe.is_thawed() && now_ms.saturating_sub(*touched) >= quiet_ms;
        if quiet {
            take(*table, stripe, &mut total);
        }
        !quiet
    });
    let (sealed, open): (Vec<_>, Vec<_>) = hot.iter().partition(|(_, _, stripe)| stripe.is_full());
    for (_, table, stripe) in sealed {
        if total <= limit {
            break;
        }
        take(*table, stripe, &mut total);
    }
    for (_, table, stripe) in open {
        if total <= limit {
            break;
        }
        if stripe.seal() {
            take(*table, stripe, &mut total);
        }
    }
    picked
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

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::{Inserter, Rid, Stripe, StripeDirectory, to_freeze};
    use crate::arena::Space;
    use crate::deletes::{Base, DeleteVector, PART_WORDS, Refusal, STRIPE_ROWS};
    use crate::graveyard::Cell;
    use crate::horizon::{Horizons, Reads};
    use crate::hot::{HotStripe, Lease, Width};
    use crate::park::Wait;
    use crate::undo::{UndoBuffer, UndoSpace};

    fn directory() -> Arc<StripeDirectory> {
        Arc::new(StripeDirectory::new(&[Width::Eight], Arc::new(UndoSpace::new())))
    }

    /// `n` rows inserted by `txn` and committed at `ts`, each holding its slot.
    fn rows(inserter: &mut Inserter, n: u32, txn: u64, ts: u64) -> Vec<Rid> {
        let mut rids = Vec::new();
        while rids.len() < n as usize {
            let (lease, slots) = inserter.take(n - rids.len() as u32);
            for slot in slots.clone() {
                lease.stripe().write(slot, 0, Some(u128::from(slot)));
            }
            assert!(lease.stripe().insert(slots.clone(), txn));
            lease.stripe().commit_insert(slots.clone(), txn, ts);
            rids.extend(inserter.rids(slots));
        }
        rids
    }

    #[test]
    fn a_sealed_stripe_freezes_under_its_id_and_answers_the_same() {
        let table = directory();
        let clock = AtomicU64::new(10);
        let horizons = Horizons::new(1);
        let mut inserter = Inserter::new(Arc::clone(&table));
        let rids = rows(&mut inserter, 3_000, 1, 2);
        // An aborted insert leaves holes, and rows deleted before the freeze go in the base.
        let (lease, slots) = inserter.take(10);
        assert!(lease.stripe().insert(slots.clone(), 3));
        lease.stripe().abort_insert(slots, 3);
        let mut buffer = UndoBuffer::default();
        for &rid in &rids[..100] {
            table.delete(rid, 10, 4, Wait::NEVER, &mut buffer).expect("delete");
        }
        table.commit(&rids[..100], 4, 11);
        clock.store(11, Ordering::SeqCst);
        assert_eq!((table.count(10, 0), table.count(11, 0)), (3_000, 2_900));

        let id = rids[0].stripe();
        let hot = table.begin_freeze(id).expect("a hot stripe");
        assert!(table.begin_freeze(id).is_none(), "a freeze begins once");
        assert!(hot.is_full(), "and seals the stripe");
        let freeze = table.settle(&hot, &clock).expect("nothing in flight");
        assert_eq!((freeze.at(), freeze.rows()), (11, hot.reserved()));
        let frozen = table.publish(freeze, &horizons);
        assert_eq!((frozen.id(), frozen.checkpoint(), frozen.rows()), (id, 11, hot.reserved()));
        assert!(matches!(table.find(id), Some(Stripe::Frozen(_))));
        assert_eq!(table.count(11, 0), 2_900);
        let base = frozen.with_deletes(|deletes| deletes.base().len());
        assert_eq!(base, frozen.rows() as usize - 2_900, "holes and deleted rows");

        let next = rows(&mut inserter, 10, 5, 12);
        assert_ne!(next[0].stripe(), id, "inserts go to a new stripe");
        assert_eq!(table.count(12, 0), 2_910);
    }

    #[test]
    fn a_row_written_during_a_freeze_moves_and_its_delete_is_the_frozen_stripes() {
        let table = directory();
        let clock = AtomicU64::new(2);
        let horizons = Horizons::new(1);
        let mut inserter = Inserter::new(Arc::clone(&table));
        let rids = rows(&mut inserter, 100, 1, 2);
        let id = rids[0].stripe();
        let hot = table.begin_freeze(id).expect("a hot stripe");
        let mut buffer = UndoBuffer::default();
        let fill = |value: u128| {
            move |lease: &mut Lease, slot: u32| lease.stripe().write(slot, 0, Some(value))
        };

        let update = hot.update(rids[0].slot(), &[(0, Some(7))], 2, 8, Wait::NEVER, &mut buffer);
        assert_eq!(update, Err(Refusal::Freezing));
        let moved = table.migrate(rids[0], 2, 8, &mut inserter, fill(7)).expect("moves");
        assert_ne!(moved.stripe(), id);
        table.delete(rids[1], 2, 8, Wait::NEVER, &mut buffer).expect("into the delete vector");
        assert_eq!(table.delete(rids[1], 2, 9, Wait::NEVER, &mut buffer), Err(Refusal::Conflict));
        let again = table.migrate(rids[0], 2, 9, &mut inserter, fill(8)).map(|_| ());
        assert_eq!(again, Err(Refusal::Conflict));
        assert_eq!(table.delete(rids[1], 2, 8, Wait::NEVER, &mut buffer), Err(Refusal::Gone));
        table.delete(rids[2], 2, 9, Wait::NEVER, &mut buffer).expect("to be taken back");
        assert_eq!((table.count(2, 8), table.count(2, 9), table.count(2, 0)), (99, 99, 100));
        assert!(!hot.visible(rids[1].slot(), 2, 8) && hot.visible(rids[1].slot(), 2, 0));

        let freeze = table.settle(&hot, &clock).expect("the delete vector holds no row");
        let frozen = table.publish(freeze, &horizons);
        table.commit(&[rids[0], moved, rids[1]], 8, 3);
        table.abort(&[rids[2]], 9);
        clock.store(3, Ordering::SeqCst);
        assert_eq!(table.count(2, 0), 100, "a snapshot at F reads the frozen stripe as of F");
        assert_eq!(table.count(3, 0), 99);
        assert_eq!(frozen.with_deletes(|deletes| deletes.recent()), 2);
        table.delete(rids[2], 3, 10, Wait::NEVER, &mut buffer).expect("free again");
        assert_eq!(table.delete(rids[0], 3, 10, Wait::NEVER, &mut buffer), Err(Refusal::Gone));
    }

    #[test]
    fn a_freeze_waits_for_writes_in_flight_and_not_for_older_snapshots() {
        let table = directory();
        let clock = AtomicU64::new(2);
        let horizons = Horizons::new(1);
        let mut inserter = Inserter::new(Arc::clone(&table));
        let rids = rows(&mut inserter, 100, 1, 2);
        let id = rids[0].stripe();
        let old = horizons.begin(0, &clock, Reads::Tables(vec![0]));
        let hot = match table.find(id) {
            Some(Stripe::Hot(hot)) => hot,
            _ => panic!("a hot stripe"),
        };
        let mut buffer = UndoBuffer::default();
        hot.update(rids[0].slot(), &[(0, Some(7))], 2, 8, Wait::NEVER, &mut buffer).expect("held");
        let (lease, late) = inserter.take(5);
        let leased = Arc::clone(lease.stripe());
        assert_eq!(leased.id(), id);

        assert!(table.begin_freeze(id).is_some());
        assert!(table.settle(&hot, &clock).is_none(), "a row is held");
        hot.commit_write(rids[0].slot(), 8, 3);
        assert!(table.settle(&hot, &clock).is_none(), "the clock is behind a commit");
        clock.store(3, Ordering::SeqCst);
        assert!(!leased.insert(late.clone(), 9), "an insert leased before the freeze is refused");
        assert!(!hot.visible(late.start, 3, 9), "and leaves holes");

        let freeze = table.settle(&hot, &clock).expect("settled");
        assert_eq!(freeze.at(), 3);
        let seen = table.stripes();
        let frozen = table.publish(freeze, &horizons);
        assert!(matches!(seen[0], Stripe::Hot(_)), "a scan keeps the stripes it loaded");
        assert!(matches!(table.find(id), Some(Stripe::Frozen(_))));
        assert_eq!(seen[0].visible(0, 2, 0, &mut [0; 128]), 100, "and reads them as before");
        assert_eq!(table.count(3, 0), 100);

        // The statement from before reads the row written since out of the graveyard.
        let graveyard = frozen.graveyard().expect("a snapshot from before");
        assert_eq!((graveyard.snapshots(), graveyard.len()), (&[old][..], 1));
        let before = [Some(Cell::Fixed(0))];
        assert_eq!(graveyard.seen(rids[0].slot(), old), Some(Some(&before[..])));
        assert_eq!(graveyard.seen(rids[0].slot(), 3), None, "F reads the frozen stripe");
        assert_eq!(table.bury(&horizons, &clock), 0, "while the statement runs");
        assert!(table.begin_thaw(id).is_none(), "nor is it thawed");
        horizons.end(0);
        assert_eq!(table.bury(&horizons, &clock), 1);
        assert!(frozen.graveyard().is_none());
        assert!(table.begin_thaw(id).is_some());
    }

    #[test]
    fn a_snapshot_from_before_a_freeze_reads_its_rows_out_of_the_graveyard() {
        let table = Arc::new(StripeDirectory::new(
            &[Width::Eight, Width::Text],
            Arc::new(UndoSpace::new()),
        ));
        let clock = AtomicU64::new(2);
        let horizons = Horizons::new(2);
        let name = |slot: u32| format!("row {slot}, long enough to go in the arena");
        let mut inserter = Inserter::new(Arc::clone(&table));
        let mut insert = |n: u32, txn: u64, ts: u64| {
            let (lease, slots) = inserter.take(n);
            assert_eq!(slots.len(), n as usize);
            for slot in slots.clone() {
                lease.stripe().write(slot, 0, Some(u128::from(slot)));
                lease.write_text(slot, 1, Some(name(slot).as_bytes())).expect("text");
            }
            assert!(lease.stripe().insert(slots.clone(), txn));
            lease.stripe().commit_insert(slots.clone(), txn, ts);
            inserter.rids(slots)
        };
        let rids = insert(10, 1, 2);
        let id = rids[0].stripe();
        let Some(Stripe::Hot(hot)) = table.find(id) else { panic!("a hot stripe") };
        let old = horizons.begin(0, &clock, Reads::Tables(vec![0]));

        // After the old snapshot: a row written, a row deleted, two rows inserted.
        let mut buffer = UndoBuffer::default();
        let mut space = Space::default();
        let view = hot.stage_text(b"written after the old snapshot was taken", &mut space);
        let changes = [(0, Some(100)), (1, Some(view.expect("staged")))];
        hot.update(rids[0].slot(), &changes, 2, 8, Wait::NEVER, &mut buffer).expect("update");
        hot.commit_write(rids[0].slot(), 8, 3);
        table.delete(rids[1], 3, 9, Wait::NEVER, &mut buffer).expect("delete");
        table.commit(&rids[1..2], 9, 4);
        let late = insert(2, 10, 5);
        assert!(late.iter().all(|rid| rid.stripe() == id));
        clock.store(5, Ordering::SeqCst);
        let mid = horizons.begin(1, &clock, Reads::Tables(vec![0]));

        // After the middle one: another row written, and one of the new rows deleted.
        hot.update(rids[2].slot(), &[(0, Some(200))], 5, 11, Wait::NEVER, &mut buffer)
            .expect("update");
        hot.commit_write(rids[2].slot(), 11, 6);
        table.delete(late[0], 6, 12, Wait::NEVER, &mut buffer).expect("delete");
        table.commit(&late[..1], 12, 7);
        clock.store(7, Ordering::SeqCst);

        let hot = table.begin_freeze(id).expect("a hot stripe");
        let freeze = table.settle(&hot, &clock).expect("settled");
        assert_eq!(freeze.at(), 7);
        let frozen = table.publish(freeze, &horizons);
        assert!(matches!(table.find(id), Some(Stripe::Frozen(_))));
        assert_eq!((table.count(old, 0), table.count(mid, 0), table.count(7, 0)), (10, 11, 10));

        let graveyard = frozen.graveyard().expect("two snapshots from before");
        assert_eq!((graveyard.snapshots(), graveyard.len()), (&[old, mid][..], 5));
        assert!(graveyard.bytes() > 2 * name(0).len());
        assert_eq!(table.graveyard_bytes(), graveyard.bytes());
        let row = |slot: u32, value: u128| {
            vec![Some(Cell::Fixed(value)), Some(Cell::Text(name(slot).into_bytes().into()))]
        };
        let seen = |rid: Rid, snapshot| {
            graveyard.seen(rid.slot(), snapshot).map(|seen| seen.map(<[_]>::to_vec))
        };
        assert_eq!((seen(rids[0], old), seen(rids[0], mid)), (Some(Some(row(0, 0))), None));
        assert_eq!((seen(rids[1], old), seen(rids[1], mid)), (Some(Some(row(1, 1))), Some(None)));
        assert_eq!(seen(rids[2], old), Some(Some(row(2, 2))));
        assert_eq!(seen(rids[2], mid), Some(Some(row(2, 2))));
        let (gone, kept) = (late[0], late[1]);
        assert_eq!((seen(gone, old), seen(gone, mid)), (Some(None), Some(Some(row(10, 10)))));
        assert_eq!((seen(kept, old), seen(kept, mid)), (Some(None), None));
        assert_eq!((seen(rids[3], old), seen(rids[0], 7)), (None, None), "the frozen stripe's");

        assert_eq!(table.bury(&horizons, &clock), 0);
        horizons.end(0);
        assert_eq!(table.bury(&horizons, &clock), 0, "the middle one still reads");
        assert_eq!(table.count(mid, 0), 11);
        horizons.end(1);
        assert_eq!((table.bury(&horizons, &clock), table.graveyard_bytes()), (1, 0));
        assert_eq!(table.count(7, 0), 10);
    }

    #[test]
    fn the_freezer_takes_quiet_stripes_then_the_coldest_over_the_limit() {
        let tables = [directory(), directory()];
        // Each table has a sealed stripe and an open one after it.
        let mut stripes = Vec::new();
        for table in &tables {
            let mut inserter = Inserter::new(Arc::clone(table));
            for sealed in [true, false] {
                let rid = rows(&mut inserter, 100, 1, 2)[0];
                let Some(Stripe::Hot(hot)) = table.find(rid.stripe()) else { panic!("hot") };
                if sealed {
                    assert!(hot.seal());
                    assert!(!hot.seal(), "once");
                }
                stripes.push(hot);
            }
        }
        let [a0, a1, b0, b1] = [0, 1, 2, 3].map(|at| Arc::clone(&stripes[at]));
        assert_ne!(a0.id(), a1.id());
        for (stripe, ms) in [(&a0, 100), (&a1, 300), (&b0, 50), (&b1, 200)] {
            stripe.touch(ms);
        }
        let total: usize = tables.iter().map(|table| table.hot_bytes()).sum();
        let each = total / 4;

        assert_eq!(to_freeze(&tables, 1_000, 10_000, usize::MAX), []);
        assert_eq!(to_freeze(&tables, 1_000, 800, usize::MAX), [(1, b0.id()), (0, a0.id())]);
        assert_eq!(to_freeze(&tables, 1_000, 10_000, total - 1), [(1, b0.id())]);
        assert!(!a1.is_full() && !b1.is_full());
        let all = to_freeze(&tables, 1_000, 10_000, each / 2);
        assert_eq!(all, [(1, b0.id()), (0, a0.id()), (1, b1.id()), (0, a1.id())]);
        assert!(a1.is_full() && b1.is_full(), "the open stripes were sealed early");
    }

    #[test]
    fn deletes_and_moves_during_a_freeze_all_land() {
        const ROWS: u32 = 20_000;
        const WORKERS: u64 = 4;
        let table = directory();
        let clock = Arc::new(AtomicU64::new(2));
        let horizons = Horizons::new(1);
        let rids = Arc::new(rows(&mut Inserter::new(Arc::clone(&table)), ROWS, 1, 2));
        let id = rids[0].stripe();
        let workers: Vec<_> = (0..WORKERS)
            .map(|worker| {
                let (table, clock, rids) =
                    (Arc::clone(&table), Arc::clone(&clock), Arc::clone(&rids));
                thread::spawn(move || {
                    let mut inserter = Inserter::new(Arc::clone(&table));
                    let mut buffer = UndoBuffer::default();
                    for (i, &rid) in rids.iter().enumerate().skip(worker as usize) {
                        if i as u64 % WORKERS != worker {
                            continue;
                        }
                        let txn = 100 + i as u64;
                        let mut written = vec![rid];
                        if i % 2 == 0 {
                            table.delete(rid, 2, txn, Wait::NEVER, &mut buffer).expect("delete");
                        } else {
                            let update = match table.find(rid.stripe()) {
                                Some(Stripe::Hot(hot)) => hot.update(
                                    rid.slot(),
                                    &[(0, Some(7))],
                                    2,
                                    txn,
                                    Wait::NEVER,
                                    &mut buffer,
                                ),
                                _ => Err(Refusal::Freezing),
                            };
                            match update {
                                Ok(()) => {}
                                Err(Refusal::Freezing) => written.push(
                                    table
                                        .migrate(
                                            rid,
                                            2,
                                            txn,
                                            &mut inserter,
                                            |lease: &mut Lease, slot| {
                                                lease.stripe().write(slot, 0, Some(7));
                                            },
                                        )
                                        .expect("moves"),
                                ),
                                Err(refusal) => panic!("{refusal:?}"),
                            }
                        }
                        let ts = clock.fetch_add(1, Ordering::SeqCst) + 1;
                        table.commit(&written, txn, ts);
                    }
                })
            })
            .collect();

        thread::sleep(Duration::from_millis(5));
        let hot = table.begin_freeze(id).expect("a hot stripe");
        let freeze = loop {
            if let Some(freeze) = table.settle(&hot, &clock) {
                break freeze;
            }
            thread::sleep(Duration::from_millis(1));
        };
        assert!(table.publish(freeze, &horizons).graveyard().is_none(), "nobody reads");
        for worker in workers {
            worker.join().expect("writes");
        }
        let now = clock.load(Ordering::SeqCst);
        assert_eq!(table.count(now, 0), u64::from(ROWS / 2), "every delete landed once");
        let moved: u64 = table
            .stripes()
            .iter()
            .filter(|stripe| stripe.id() != id)
            .map(|stripe| {
                (0..stripe.slots().div_ceil(8_192))
                    .map(|part| stripe.visible(part, now, 0, &mut [0; 128]) as u64)
                    .sum::<u64>()
            })
            .sum();
        let frozen = match table.find(id) {
            Some(Stripe::Frozen(frozen)) => frozen,
            _ => panic!("frozen"),
        };
        let kept = frozen.rows() as u64
            - frozen.with_deletes(|deletes| deletes.base().len() + deletes.recent()) as u64;
        assert_eq!(
            kept + moved,
            u64::from(ROWS / 2),
            "a row updated in place stays, a moved one moved"
        );
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
            table.delete(rid, 2, 8, Wait::NEVER, &mut buffer).expect("delete");
        }
        assert_eq!(table.count(2, 8), 10_095, "its own deletes");
        assert_eq!(table.count(9, 0), 10_098, "not committed");
        assert_eq!(
            table.delete(Rid::new(frozen, 5), 2, 9, Wait::NEVER, &mut buffer),
            Err(Refusal::Conflict)
        );
        assert_eq!(
            table.delete(Rid::new(frozen, 0), 2, 9, Wait::NEVER, &mut buffer),
            Err(Refusal::Gone)
        );
        assert_eq!(
            table.delete(Rid::new(frozen, 20_000), 2, 9, Wait::NEVER, &mut buffer),
            Err(Refusal::Gone)
        );
        assert_eq!(
            table.delete(Rid::new(99, 0), 2, 9, Wait::NEVER, &mut buffer),
            Err(Refusal::Gone)
        );
        table.commit(&rids, 8, 3);
        assert_eq!(table.count(2, 0), 10_098);
        assert_eq!(table.count(3, 0), 10_095);

        let again = [Rid::new(frozen, 7), Rid::new(hot, 4)];
        for &rid in &again {
            table.delete(rid, 3, 10, Wait::NEVER, &mut buffer).expect("delete");
        }
        table.abort(&again, 10);
        assert_eq!(table.count(3, 10), 10_095);
    }

    #[test]
    fn an_updated_frozen_row_moves_into_the_head_under_its_delete_vector_entry() {
        let table = directory();
        let frozen = table.attach(100, 1, DeleteVector::default());
        let mut inserter = Inserter::new(Arc::clone(&table));
        let mut buffer = UndoBuffer::default();
        let fill = |value: u128| {
            move |lease: &mut Lease, slot: u32| lease.stripe().write(slot, 0, Some(value))
        };
        let hot = |rid: Rid| match table.find(rid.stripe()) {
            Some(Stripe::Hot(stripe)) => stripe,
            _ => panic!("the new version is in a hot stripe"),
        };

        let old = Rid::new(frozen, 5);
        let new = table.migrate(old, 2, 8, &mut inserter, fill(500)).expect("migrate");
        assert_ne!(new.stripe(), frozen);
        assert_eq!(hot(new).read(new.slot(), 0), Some(500));
        assert!(hot(new).visible(new.slot(), 2, 8), "its own new version");
        assert!(!hot(new).visible(new.slot(), 9, 0), "not committed");
        assert_eq!((table.count(2, 8), table.count(9, 0)), (100, 100));

        let refused = |result| assert_eq!(result, Err(Refusal::Conflict));
        refused(table.delete(old, 2, 9, Wait::NEVER, &mut buffer));
        refused(table.migrate(old, 2, 9, &mut inserter, fill(600)).map(|_| ()));
        table.delete(Rid::new(frozen, 6), 2, 9, Wait::NEVER, &mut buffer).expect("delete");
        refused(table.migrate(Rid::new(frozen, 6), 2, 8, &mut inserter, fill(700)).map(|_| ()));
        table.abort(&[Rid::new(frozen, 6)], 9);

        table.commit(&[old, new], 8, 3);
        assert!(hot(new).visible(new.slot(), 3, 0));
        assert!(!hot(new).visible(new.slot(), 2, 0), "an older snapshot keeps the old row");
        assert_eq!((table.count(2, 0), table.count(3, 0)), (100, 100));
        refused(table.migrate(old, 2, 10, &mut inserter, fill(800)).map(|_| ()));
        let gone = table.migrate(old, 3, 10, &mut inserter, fill(800));
        assert_eq!(gone, Err(Refusal::Gone), "its new version has another rid");
        let missing = table.migrate(new, 3, 10, &mut inserter, fill(800));
        assert_eq!(missing, Err(Refusal::Gone), "a hot row is updated in place");

        let old = Rid::new(frozen, 7);
        let new = table.migrate(old, 3, 11, &mut inserter, fill(900)).expect("migrate");
        table.abort(&[old, new], 11);
        assert!(!hot(new).visible(new.slot(), 3, 11), "the new version is a hole");
        assert_eq!(table.count(3, 11), 100, "the old row is back");
        table.migrate(old, 3, 12, &mut inserter, fill(901)).expect("free again");
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

    /// Stripe `id` of `table`'s rows with each slot's own number as its value, as a decode of
    /// the frozen pages would write them.
    fn decode(hot: &HotStripe) {
        for slot in 0..hot.reserved() {
            hot.write(slot, 0, Some(u128::from(slot)));
        }
    }

    #[test]
    fn a_thawed_stripe_keeps_its_rids_values_and_deletes() {
        let table = directory();
        let base: Vec<u32> = (0..10).collect();
        let id = table.attach(100, 3, DeleteVector::from_base(Base::of(&base)));
        let rid = |slot| Rid::new(id, slot);
        let mut buffer = UndoBuffer::default();
        table.delete(rid(20), 4, 7, Wait::NEVER, &mut buffer).expect("delete");
        table.commit(&[rid(20)], 7, 5);
        table.delete(rid(30), 5, 8, Wait::NEVER, &mut buffer).expect("delete");
        table.delete(rid(31), 5, 9, Wait::NEVER, &mut buffer).expect("delete");
        let before = table.stripes();

        let frozen = table.begin_thaw(id).expect("a frozen stripe");
        assert!(table.begin_thaw(id).is_none(), "a thaw begins once");
        let hot = table.thaw(&frozen, decode);
        assert!(matches!(table.find(id), Some(Stripe::Hot(found)) if Arc::ptr_eq(&found, &hot)));
        assert!(hot.is_thawed() && hot.is_full(), "sealed, so inserts go elsewhere");
        assert_eq!((hot.id(), hot.reserved(), hot.read(50, 0)), (id, 100, Some(50)));
        assert_eq!((table.count(4, 0), table.count(5, 0), table.count(5, 8)), (90, 89, 88));

        // The deletes in flight commit and abort on the hot stripe.
        table.commit(&[rid(30)], 8, 6);
        table.abort(&[rid(31)], 9);
        assert_eq!((table.count(5, 0), table.count(6, 0)), (89, 88));
        let mut mask = [0; PART_WORDS];
        assert_eq!(before[0].visible(0, 5, 0, &mut mask), 89, "a scan from before reads on");

        // Rows are written in place now, with the conflicts a hot row has.
        assert_eq!(hot.update(40, &[(0, Some(7))], 6, 10, Wait::NEVER, &mut buffer), Ok(()));
        hot.commit_write(40, 10, 7);
        assert_eq!(hot.read(40, 0), Some(7));
        assert_eq!(table.delete(rid(0), 7, 11, Wait::NEVER, &mut buffer), Err(Refusal::Gone));
        let update = hot.update(20, &[(0, None)], 4, 11, Wait::NEVER, &mut buffer);
        assert_eq!(update, Err(Refusal::Conflict), "deleted at 5, after the snapshot");
        let mut inserter = Inserter::new(Arc::clone(&table));
        let moved = table.migrate(rid(41), 7, 11, &mut inserter, |_, _| panic!("no move"));
        assert_eq!(moved, Err(Refusal::Thawed));
        let next = rows(&mut inserter, 1, 12, 8);
        assert_ne!(next[0].stripe(), id);
    }

    #[test]
    fn a_delete_waits_for_a_thaw_and_commits_land_on_either_side() {
        let table = directory();
        let id = table.attach(1_000, 3, DeleteVector::new());
        let rid = move |slot| Rid::new(id, slot);
        let mut buffer = UndoBuffer::default();
        table.delete(rid(1), 4, 5, Wait::NEVER, &mut buffer).expect("delete");
        table.delete(rid(2), 4, 6, Wait::NEVER, &mut buffer).expect("delete");
        let frozen = table.begin_thaw(id).expect("a frozen stripe");
        // Before the swap a commit lands in the frozen stripe's vector, which the thaw takes.
        table.commit(&[rid(1)], 5, 5);

        let waiter = {
            let table = Arc::clone(&table);
            thread::spawn(move || {
                let mut buffer = UndoBuffer::default();
                let deleted = table.delete(rid(3), 5, 7, Wait::NEVER, &mut buffer);
                (deleted, matches!(table.find(id), Some(Stripe::Hot(_))))
            })
        };
        thread::sleep(Duration::from_millis(50));
        assert!(!waiter.is_finished(), "the delete waits for the thaw");
        let hot = table.thaw(&frozen, decode);
        assert_eq!(waiter.join().expect("deletes"), (Ok(()), true));

        // After it, commits land in place.
        table.commit(&[rid(2)], 6, 6);
        table.commit(&[rid(3)], 7, 7);
        assert_eq!((table.count(4, 0), table.count(5, 0), table.count(7, 0)), (1_000, 999, 997));
        assert!(!hot.visible(3, 7, 0) && hot.visible(3, 6, 0));
    }

    #[test]
    fn deletes_and_moves_during_a_thaw_all_land() {
        const ROWS: u32 = 20_000;
        const WORKERS: u32 = 4;
        let table = directory();
        let id = table.attach(ROWS, 1, DeleteVector::new());
        let clock = Arc::new(AtomicU64::new(1));
        let workers: Vec<_> = (0..WORKERS)
            .map(|worker| {
                let (table, clock) = (Arc::clone(&table), Arc::clone(&clock));
                thread::spawn(move || {
                    let mut inserter = Inserter::new(Arc::clone(&table));
                    let mut buffer = UndoBuffer::default();
                    let (mut moved, mut updated) = (0, 0);
                    for slot in (worker..ROWS).step_by(WORKERS as usize) {
                        let txn = u64::from(slot) + 10;
                        let snapshot = clock.load(Ordering::SeqCst);
                        let rid = Rid::new(id, slot);
                        let mut written = vec![rid];
                        if slot % 2 == 0 {
                            table
                                .delete(rid, snapshot, txn, Wait::NEVER, &mut buffer)
                                .expect("own");
                        } else {
                            let fill = |lease: &mut Lease, at: u32| {
                                lease.stripe().write(at, 0, Some(u128::from(slot)));
                            };
                            match table.migrate(rid, snapshot, txn, &mut inserter, fill) {
                                Ok(new) => {
                                    written.push(new);
                                    moved += 1;
                                }
                                Err(Refusal::Thawed) => {
                                    let Some(Stripe::Hot(hot)) = table.find(id) else {
                                        panic!("thawed")
                                    };
                                    let change = [(0, Some(0))];
                                    let wait = Wait::NEVER;
                                    hot.update(slot, &change, snapshot, txn, wait, &mut buffer)
                                        .expect("own row");
                                    updated += 1;
                                }
                                Err(refusal) => panic!("{refusal:?}"),
                            }
                        }
                        let ts = clock.fetch_add(1, Ordering::SeqCst) + 1;
                        table.commit(&written, txn, ts);
                    }
                    (moved, updated)
                })
            })
            .collect();
        thread::sleep(Duration::from_millis(5));
        let frozen = table.begin_thaw(id).expect("a frozen stripe");
        table.thaw(&frozen, decode);
        let (moved, updated) = workers
            .into_iter()
            .map(|worker| worker.join().expect("writes"))
            .fold((0, 0), |(a, b), (c, d)| (a + c, b + d));
        assert_eq!(moved + updated, ROWS / 2);
        let now = clock.load(Ordering::SeqCst);
        // Half the rows deleted, and every moved row has its new version elsewhere.
        assert_eq!(table.count(now, 0), u64::from(ROWS / 2));
        let Some(Stripe::Hot(hot)) = table.find(id) else { panic!("thawed") };
        let in_place = (0..ROWS).filter(|&slot| hot.visible(slot, now, 0)).count();
        assert_eq!(in_place, usize::try_from(updated).expect("fits"));
    }
}

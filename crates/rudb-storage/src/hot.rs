//! The hot stripe, `engine-v4/03-the-shape.md` section 3.4 and `07-the-head.md` sections 7.2 to
//! 7.4 and 7.7.
//!
//! New rows go into a hot stripe: plain arrays, one per column per part of 8,192 rows, allocated
//! when a part gets its first row. Every row has a `created` stamp, the inserting transaction's id
//! with the top bit set until it commits and the commit timestamp after, and a slot that was never
//! written reads 0, which no snapshot sees. `deleted` is allocated for a part on its first delete
//! and is `u64::MAX` for a live row. A reader at snapshot `S` running as transaction `T` sees a
//! row when `1 <= created <= S or created == T`, and not `deleted <= S or deleted == T`.
//!
//! Workers take slots by lease, a run of consecutive slots taken with one `fetch_add`, and fill
//! them without touching anything shared. The lease grows from 64 slots to 1,024 as a worker uses
//! them up, so a worker that inserts rarely leaves at most a short hole when the stripe seals.
//!
//! Values are kept in relaxed atomics of their own width. A value is written once before the
//! release store of `created` publishes it, but the in-place update that comes with the lock word
//! writes over values that a scan may be reading at the same moment, and the undo chain is what
//! corrects such a read. With plain memory that race would be undefined behaviour; with relaxed
//! atomics it is a plain load or store on every machine rudb runs on, so the cells pay nothing for
//! being sound. Text columns and their arena come separately.

use std::ops::Range;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::deletes::{PART_ROWS, PART_WORDS, Refusal, STRIPE_ROWS, UNCOMMITTED};

/// Parts in a stripe.
pub const PARTS: usize = (STRIPE_ROWS / PART_ROWS) as usize;

/// The first lease a worker takes in a stripe.
pub const FIRST_LEASE: u32 = 64;

/// The largest lease a worker grows to.
pub const LARGEST_LEASE: u32 = 1_024;

/// Rows in a part, as a length.
const ROWS: usize = PART_ROWS as usize;

/// What `deleted` holds for a live row.
const LIVE: u64 = u64::MAX;

/// How many bytes a fixed-width column's values take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    /// Booleans and 8-bit integers.
    One,
    /// 16-bit integers.
    Two,
    /// 32-bit integers and floats, and dates.
    Four,
    /// 64-bit integers and floats, timestamps and short decimals.
    Eight,
    /// 128-bit integers, wide decimals, and intervals.
    Sixteen,
}

impl Width {
    /// The width in bytes.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::One => 1,
            Self::Two => 2,
            Self::Four => 4,
            Self::Eight => 8,
            Self::Sixteen => 16,
        }
    }
}

/// 64 arrays of 8,192 `T`, each allocated the first time it is asked for.
#[derive(Debug)]
struct Parts<T> {
    parts: Box<[OnceLock<Box<[T]>>]>,
    /// How many `T` a part holds.
    len: usize,
}

impl<T> Parts<T> {
    fn new(len: usize) -> Self {
        Self { parts: (0..PARTS).map(|_| OnceLock::new()).collect(), len }
    }

    fn get(&self, part: usize) -> Option<&[T]> {
        self.parts[part].get().map(|cells| &cells[..])
    }

    fn get_or(&self, part: usize, fill: impl Fn() -> T) -> &[T] {
        self.parts[part].get_or_init(|| (0..self.len).map(|_| fill()).collect())
    }

    fn allocated(&self) -> usize {
        self.parts.iter().filter(|part| part.get().is_some()).count()
    }
}

#[derive(Debug)]
enum Cells {
    One(Parts<AtomicU8>),
    Two(Parts<AtomicU16>),
    Four(Parts<AtomicU32>),
    Eight(Parts<AtomicU64>),
    Sixteen(Parts<AtomicU64>),
}

impl Cells {
    fn new(width: Width) -> Self {
        match width {
            Width::One => Self::One(Parts::new(ROWS)),
            Width::Two => Self::Two(Parts::new(ROWS)),
            Width::Four => Self::Four(Parts::new(ROWS)),
            Width::Eight => Self::Eight(Parts::new(ROWS)),
            Width::Sixteen => Self::Sixteen(Parts::new(2 * ROWS)),
        }
    }

    fn allocate(&self, part: usize) {
        match self {
            Self::One(parts) => _ = parts.get_or(part, || AtomicU8::new(0)),
            Self::Two(parts) => _ = parts.get_or(part, || AtomicU16::new(0)),
            Self::Four(parts) => _ = parts.get_or(part, || AtomicU32::new(0)),
            Self::Eight(parts) | Self::Sixteen(parts) => {
                _ = parts.get_or(part, || AtomicU64::new(0))
            }
        }
    }

    /// Stores the low bytes of `value` at `slot`, whose part is allocated.
    fn store(&self, slot: u32, value: u128) {
        let (part, at) = place(slot);
        match self {
            Self::One(parts) => cells(parts.get(part))[at].store(value as u8, Ordering::Relaxed),
            Self::Two(parts) => cells(parts.get(part))[at].store(value as u16, Ordering::Relaxed),
            Self::Four(parts) => cells(parts.get(part))[at].store(value as u32, Ordering::Relaxed),
            Self::Eight(parts) => cells(parts.get(part))[at].store(value as u64, Ordering::Relaxed),
            Self::Sixteen(parts) => {
                let cells = cells(parts.get(part));
                cells[2 * at].store(value as u64, Ordering::Relaxed);
                cells[2 * at + 1].store((value >> 64) as u64, Ordering::Relaxed);
            }
        }
    }

    /// The value at `slot`, zero extended, or 0 if its part was never allocated.
    fn load(&self, slot: u32) -> u128 {
        let (part, at) = place(slot);
        match self {
            Self::One(parts) => parts.get(part).map_or(0, |c| c[at].load(Ordering::Relaxed).into()),
            Self::Two(parts) => parts.get(part).map_or(0, |c| c[at].load(Ordering::Relaxed).into()),
            Self::Four(parts) => {
                parts.get(part).map_or(0, |c| c[at].load(Ordering::Relaxed).into())
            }
            Self::Eight(parts) => {
                parts.get(part).map_or(0, |c| c[at].load(Ordering::Relaxed).into())
            }
            Self::Sixteen(parts) => parts.get(part).map_or(0, |c| {
                u128::from(c[2 * at].load(Ordering::Relaxed))
                    | (u128::from(c[2 * at + 1].load(Ordering::Relaxed)) << 64)
            }),
        }
    }

    fn part_bytes(&self) -> usize {
        match self {
            Self::One(_) => PART_ROWS as usize,
            Self::Two(_) => PART_ROWS as usize * 2,
            Self::Four(_) => PART_ROWS as usize * 4,
            Self::Eight(_) => PART_ROWS as usize * 8,
            Self::Sixteen(_) => PART_ROWS as usize * 16,
        }
    }

    fn allocated(&self) -> usize {
        match self {
            Self::One(parts) => parts.allocated(),
            Self::Two(parts) => parts.allocated(),
            Self::Four(parts) => parts.allocated(),
            Self::Eight(parts) | Self::Sixteen(parts) => parts.allocated(),
        }
    }
}

/// The cells of a part the caller knows is allocated, because the slot was leased.
fn cells<T>(part: Option<&[T]>) -> &[T] {
    part.expect("a leased slot's part is allocated when the lease is taken")
}

/// A slot's part and its place in the part.
fn place(slot: u32) -> (usize, usize) {
    ((slot / PART_ROWS) as usize, (slot % PART_ROWS) as usize)
}

#[derive(Debug)]
struct Column {
    cells: Cells,
    /// One bit a slot, set when the value is not null.
    valid: Parts<AtomicU64>,
}

/// A stripe that takes inserts into leased slots and deletes in place.
#[derive(Debug)]
pub struct HotStripe {
    id: u32,
    /// Slots handed out so far. It can pass the end of the stripe, since leases are taken with
    /// a plain `fetch_add`, and anything at or past the end was never handed out.
    reserved: AtomicU32,
    columns: Box<[Column]>,
    created: Parts<AtomicU64>,
    deleted: Parts<AtomicU64>,
}

impl HotStripe {
    /// An empty stripe `id` with a column of each width in `widths`.
    #[must_use]
    pub fn new(id: u32, widths: &[Width]) -> Self {
        let columns = widths
            .iter()
            .map(|&width| Column { cells: Cells::new(width), valid: Parts::new(PART_WORDS) })
            .collect();
        Self {
            id,
            reserved: AtomicU32::new(0),
            columns,
            created: Parts::new(ROWS),
            deleted: Parts::new(ROWS),
        }
    }

    /// Its id, which the table assigned and never reuses.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// How many columns it has.
    #[must_use]
    pub fn width(&self) -> usize {
        self.columns.len()
    }

    /// How many slots were handed out, which is where a scan stops.
    #[must_use]
    pub fn reserved(&self) -> u32 {
        self.reserved.load(Ordering::Acquire).min(STRIPE_ROWS)
    }

    /// Whether every slot is handed out, which seals the stripe against inserts.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.reserved.load(Ordering::Relaxed) >= STRIPE_ROWS
    }

    /// Hands out up to `want` consecutive slots, with every part they touch allocated, or `None`
    /// once the stripe is full.
    pub fn lease(&self, want: u32) -> Option<Range<u32>> {
        let first = self.reserved.fetch_add(want, Ordering::AcqRel);
        if first >= STRIPE_ROWS {
            return None;
        }
        let slots = first..first.saturating_add(want).min(STRIPE_ROWS);
        for part in place(slots.start).0..=place(slots.end - 1).0 {
            for column in &*self.columns {
                column.cells.allocate(part);
                column.valid.get_or(part, || AtomicU64::new(0));
            }
            self.created.get_or(part, || AtomicU64::new(0));
        }
        Some(slots)
    }

    /// Writes `value` into `column` at `slot`, a slot of the caller's lease that is not stamped
    /// yet. `None` is null.
    ///
    /// # Panics
    ///
    /// If the slot was never leased or the column does not exist.
    pub fn write(&self, slot: u32, column: usize, value: Option<u128>) {
        let column = &self.columns[column];
        let (part, at) = place(slot);
        let word = &cells(column.valid.get(part))[at / 64];
        match value {
            Some(value) => {
                column.cells.store(slot, value);
                word.fetch_or(1 << (at % 64), Ordering::Relaxed);
            }
            None => {
                column.cells.store(slot, 0);
                word.fetch_and(!(1 << (at % 64)), Ordering::Relaxed);
            }
        }
    }

    /// The value of `column` at `slot` as it is now, whoever may see it, zero extended, or `None`
    /// for null. Visibility is [`Self::visible`]'s question.
    ///
    /// # Panics
    ///
    /// If the column does not exist.
    #[must_use]
    pub fn read(&self, slot: u32, column: usize) -> Option<u128> {
        let column = &self.columns[column];
        let (part, at) = place(slot);
        let valid =
            column.valid.get(part)?[at / 64].load(Ordering::Relaxed) & (1 << (at % 64)) != 0;
        valid.then(|| column.cells.load(slot))
    }

    /// Publishes the rows at `slots`, written by `txn`, as its uncommitted inserts. The values are
    /// written before this, and the release here is what makes a reader that sees the stamp see
    /// them too.
    ///
    /// # Panics
    ///
    /// If a slot was never leased.
    pub fn insert(&self, slots: Range<u32>, txn: u64) {
        for slot in slots {
            let (part, at) = place(slot);
            cells(self.created.get(part))[at].store(txn | UNCOMMITTED, Ordering::Release);
        }
    }

    /// Stamps the inserts `txn` made at `slots` with its commit timestamp `ts`.
    ///
    /// # Panics
    ///
    /// If a slot was never leased.
    pub fn commit_insert(&self, slots: Range<u32>, txn: u64, ts: u64) {
        self.restamp(&self.created, slots, txn | UNCOMMITTED, ts);
    }

    /// Takes back the inserts `txn` made at `slots`. They become holes, which no snapshot sees.
    ///
    /// # Panics
    ///
    /// If a slot was never leased.
    pub fn abort_insert(&self, slots: Range<u32>, txn: u64) {
        self.restamp(&self.created, slots, txn | UNCOMMITTED, 0);
    }

    /// Deletes the row at `slot` for transaction `txn` reading at `snapshot`. It stays visible to
    /// everyone else until [`Self::commit_delete`].
    ///
    /// # Errors
    ///
    /// [`Refusal::Gone`] when `txn` does not see the row, because it was never inserted as far as
    /// `txn` can tell or was already deleted, and [`Refusal::Conflict`] when someone else deleted it
    /// first: an uncommitted delete, or one committed after `snapshot`.
    ///
    /// # Panics
    ///
    /// If the slot is past the end of the stripe.
    pub fn delete(&self, slot: u32, snapshot: u64, txn: u64) -> Result<(), Refusal> {
        let me = txn | UNCOMMITTED;
        let (part, at) = place(slot);
        let created = self.created.get(part).map_or(0, |part| part[at].load(Ordering::Acquire));
        if created.wrapping_sub(1) >= snapshot && created != me {
            return Err(Refusal::Gone);
        }
        let cell = &self.deleted.get_or(part, || AtomicU64::new(LIVE))[at];
        match cell.compare_exchange(LIVE, me, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Ok(()),
            Err(held) if held == me || (held & UNCOMMITTED == 0 && held <= snapshot) => {
                Err(Refusal::Gone)
            }
            Err(_) => Err(Refusal::Conflict),
        }
    }

    /// Stamps the delete `txn` made at `slot` with its commit timestamp `ts`.
    pub fn commit_delete(&self, slot: u32, txn: u64, ts: u64) {
        self.restamp(&self.deleted, slot..slot + 1, txn | UNCOMMITTED, ts);
    }

    /// Takes back the delete `txn` made at `slot`.
    pub fn abort_delete(&self, slot: u32, txn: u64) {
        self.restamp(&self.deleted, slot..slot + 1, txn | UNCOMMITTED, LIVE);
    }

    fn restamp(&self, stamps: &Parts<AtomicU64>, slots: Range<u32>, from: u64, to: u64) {
        for slot in slots {
            let (part, at) = place(slot);
            if let Some(part) = stamps.get(part) {
                let _ = part[at].compare_exchange(from, to, Ordering::AcqRel, Ordering::Relaxed);
            }
        }
    }

    /// Whether the row at `slot` is visible to a reader at `snapshot` running as `txn`.
    #[must_use]
    pub fn visible(&self, slot: u32, snapshot: u64, txn: u64) -> bool {
        let (part, at) = place(slot);
        let created = self.created.get(part).map_or(0, |part| part[at].load(Ordering::Acquire));
        let deleted = self.deleted.get(part).map_or(LIVE, |part| part[at].load(Ordering::Acquire));
        seen(created, deleted, snapshot, txn | UNCOMMITTED)
    }

    /// Sets bit `i` of `mask` for every slot `i` of part `part` that is visible to a reader at
    /// `snapshot` running as `txn`, and clears the others, and says how many are visible.
    ///
    /// # Panics
    ///
    /// If `part` is past the end of the stripe.
    pub fn mask(&self, part: u32, snapshot: u64, txn: u64, mask: &mut [u64; PART_WORDS]) -> usize {
        mask.fill(0);
        let part = part as usize;
        let Some(created) = self.created.get(part) else { return 0 };
        let deleted = self.deleted.get(part);
        let me = txn | UNCOMMITTED;
        let mut count = 0;
        for (word, out) in mask.iter_mut().enumerate() {
            let mut bits = 0_u64;
            for bit in 0..64 {
                let at = word * 64 + bit;
                let gone = deleted.map_or(LIVE, |part| part[at].load(Ordering::Acquire));
                if seen(created[at].load(Ordering::Acquire), gone, snapshot, me) {
                    bits |= 1 << bit;
                }
            }
            *out = bits;
            count += bits.count_ones() as usize;
        }
        count
    }

    /// Bytes the stripe holds in allocated parts.
    #[must_use]
    pub fn bytes(&self) -> usize {
        let columns: usize = self
            .columns
            .iter()
            .map(|column| {
                column.cells.allocated() * column.cells.part_bytes()
                    + column.valid.allocated() * PART_WORDS * 8
            })
            .sum();
        columns + (self.created.allocated() + self.deleted.allocated()) * PART_ROWS as usize * 8
    }
}

/// The visibility rule of `07-the-head.md` section 7.4, with `me` the reader's id with the top bit
/// set. The subtraction wraps, so a hole's 0 fails the first test, and an id with the top bit set
/// that is not the reader's compares past every snapshot on both sides.
fn seen(created: u64, deleted: u64, snapshot: u64, me: u64) -> bool {
    (created.wrapping_sub(1) < snapshot || created == me) && !(deleted <= snapshot || deleted == me)
}

/// One worker's claim on consecutive slots of a stripe, grown as the worker uses it up.
#[derive(Debug)]
pub struct Lease {
    stripe: Arc<HotStripe>,
    next: u32,
    end: u32,
    size: u32,
}

impl Lease {
    /// A worker's lease in `stripe`, which takes no slots until it is asked for some.
    #[must_use]
    pub fn new(stripe: Arc<HotStripe>) -> Self {
        Self { stripe, next: 0, end: 0, size: FIRST_LEASE }
    }

    /// The stripe it leases from.
    #[must_use]
    pub fn stripe(&self) -> &Arc<HotStripe> {
        &self.stripe
    }

    /// Up to `want` consecutive slots, from what the worker holds or from a new lease when that is
    /// used up. Empty when the stripe is full, and the worker moves to the table's next open
    /// stripe.
    pub fn take(&mut self, want: u32) -> Range<u32> {
        if self.next == self.end {
            let Some(slots) = self.stripe.lease(self.size.max(want.min(LARGEST_LEASE))) else {
                return 0..0;
            };
            self.next = slots.start;
            self.end = slots.end;
            self.size = (self.size * 2).min(LARGEST_LEASE);
        }
        let first = self.next;
        self.next = self.end.min(first.saturating_add(want));
        first..self.next
    }

    /// Slots leased and not filled, which become holes if the lease is dropped.
    #[must_use]
    pub fn unfilled(&self) -> u32 {
        self.end - self.next
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::{FIRST_LEASE, HotStripe, LARGEST_LEASE, Lease, PART_ROWS, PART_WORDS, Width};
    use crate::deletes::{Refusal, STRIPE_ROWS};

    fn stripe() -> HotStripe {
        HotStripe::new(7, &[Width::One, Width::Two, Width::Four, Width::Eight, Width::Sixteen])
    }

    fn visible(stripe: &HotStripe, snapshot: u64, txn: u64) -> usize {
        let mut mask = [0_u64; PART_WORDS];
        (0..STRIPE_ROWS / PART_ROWS).map(|part| stripe.mask(part, snapshot, txn, &mut mask)).sum()
    }

    #[test]
    fn every_width_keeps_its_values_and_nulls() {
        let stripe = stripe();
        let slots = stripe.lease(3).expect("slots");
        let wide = 0x0123_4567_89AB_CDEF_FEDC_BA98_7654_3210_u128;
        for column in 0..5 {
            stripe.write(slots.start, column, Some(wide));
            stripe.write(slots.start + 1, column, None);
            stripe.write(slots.start + 2, column, Some(1));
        }
        let widths = [1, 2, 4, 8, 16];
        for (column, bytes) in widths.into_iter().enumerate() {
            let mask = if bytes == 16 { u128::MAX } else { (1_u128 << (bytes * 8)) - 1 };
            assert_eq!(stripe.read(slots.start, column), Some(wide & mask));
            assert_eq!(stripe.read(slots.start + 1, column), None);
            assert_eq!(stripe.read(slots.start + 2, column), Some(1));
        }
        assert_eq!(stripe.read(100_000, 0), None, "a part nobody leased");
    }

    #[test]
    fn an_insert_is_seen_by_its_transaction_then_by_snapshots_at_its_commit() {
        let stripe = stripe();
        let slots = stripe.lease(10).expect("slots");
        assert!(!stripe.visible(slots.start, 100, 1), "leased and not written is a hole");
        stripe.insert(slots.clone(), 1);
        assert!(stripe.visible(slots.start, 0, 1), "its own transaction");
        assert!(!stripe.visible(slots.start, 100, 2), "someone else before the commit");
        assert_eq!(visible(&stripe, 0, 1), 10);
        stripe.commit_insert(slots.clone(), 1, 5);
        assert!(!stripe.visible(slots.start, 4, 2));
        assert!(stripe.visible(slots.start, 5, 2));
        assert_eq!(visible(&stripe, 5, 2), 10);

        let more = stripe.lease(4).expect("slots");
        stripe.insert(more.clone(), 3);
        stripe.abort_insert(more.clone(), 3);
        assert!(!stripe.visible(more.start, u64::MAX >> 1, 3), "an aborted insert is a hole");
    }

    #[test]
    fn a_delete_follows_first_writer_wins() {
        let stripe = stripe();
        let slots = stripe.lease(2).expect("slots");
        stripe.insert(slots.clone(), 1);
        assert_eq!(stripe.delete(slots.start, 0, 2), Err(Refusal::Gone), "not visible to it");
        stripe.commit_insert(slots.clone(), 1, 5);
        stripe.delete(slots.start, 5, 2).expect("delete");
        assert!(!stripe.visible(slots.start, 5, 2), "its own delete");
        assert!(stripe.visible(slots.start, 9, 3), "someone else's uncommitted delete");
        assert_eq!(stripe.delete(slots.start, 5, 2), Err(Refusal::Gone));
        assert_eq!(stripe.delete(slots.start, 5, 3), Err(Refusal::Conflict));
        stripe.commit_delete(slots.start, 2, 8);
        assert!(stripe.visible(slots.start, 7, 3), "a snapshot before the delete");
        assert!(!stripe.visible(slots.start, 8, 3));
        assert_eq!(stripe.delete(slots.start, 7, 3), Err(Refusal::Conflict), "after its snapshot");
        assert_eq!(stripe.delete(slots.start, 8, 3), Err(Refusal::Gone));

        stripe.delete(slots.start + 1, 5, 4).expect("delete");
        stripe.abort_delete(slots.start + 1, 4);
        stripe.delete(slots.start + 1, 5, 5).expect("the row is live again");
    }

    #[test]
    fn a_lease_grows_to_its_largest_and_the_stripe_ends() {
        let stripe = Arc::new(stripe());
        let mut lease = Lease::new(Arc::clone(&stripe));
        let mut sizes = Vec::new();
        let mut reserved = 0;
        while sizes.len() < 6 {
            assert_eq!(lease.take(1).len(), 1);
            if stripe.reserved() != reserved {
                sizes.push(stripe.reserved() - reserved);
                reserved = stripe.reserved();
            }
        }
        assert_eq!(sizes, [64, 128, 256, 512, 1024, 1024]);
        assert_eq!(lease.unfilled(), 1023);
        assert_eq!(lease.take(10).len(), 10);
        assert_eq!(lease.unfilled(), 1013);
        assert_eq!(lease.take(u32::MAX).len(), 1013, "no more than it holds");
        assert_eq!(lease.take(u32::MAX).len(), 1024, "a large ask takes the largest lease");
        assert_eq!(stripe.reserved(), 64 + 128 + 256 + 512 + 3 * 1024);
        let rest = STRIPE_ROWS - stripe.reserved() - 5;
        stripe.lease(rest).expect("all but five");
        let mut late = Lease::new(Arc::clone(&stripe));
        assert_eq!(late.take(1).len(), 1, "a short lease at the end");
        assert_eq!(late.unfilled(), 4);
        assert!(stripe.is_full());
        assert_eq!(Lease::new(Arc::clone(&stripe)).take(1), 0..0);
        assert_eq!(stripe.reserved(), STRIPE_ROWS);
        assert_eq!(FIRST_LEASE, 64);
        assert_eq!(LARGEST_LEASE, 1024);
    }

    #[test]
    fn parts_are_allocated_only_where_rows_are() {
        let stripe = HotStripe::new(0, &[Width::Eight]);
        assert_eq!(stripe.bytes(), 0);
        stripe.lease(10).expect("slots");
        let one = stripe.bytes();
        assert_eq!(one, 8_192 * 8 + 128 * 8 + 8_192 * 8);
        stripe.lease(10_000).expect("slots");
        assert_eq!(stripe.bytes(), 2 * one, "a second part and no more");
    }

    #[test]
    fn workers_fill_their_own_slots_at_the_same_time() {
        let stripe = Arc::new(HotStripe::new(1, &[Width::Eight, Width::Four]));
        let workers: Vec<_> = (0..8_u64)
            .map(|worker| {
                let stripe = Arc::clone(&stripe);
                thread::spawn(move || {
                    let mut lease = Lease::new(Arc::clone(&stripe));
                    let mut mine = Vec::new();
                    for row in 0..20_000_u64 {
                        let slots = lease.take(1);
                        stripe.write(slots.start, 0, Some(u128::from(worker)));
                        stripe.write(slots.start, 1, Some(u128::from(row)));
                        stripe.insert(slots.clone(), 10 + worker);
                        stripe.commit_insert(slots.clone(), 10 + worker, 1);
                        mine.push(slots.start);
                    }
                    mine
                })
            })
            .collect();
        let mut seen = vec![false; STRIPE_ROWS as usize];
        for (worker, handle) in workers.into_iter().enumerate() {
            for (row, slot) in handle.join().expect("the worker finishes").into_iter().enumerate() {
                assert!(!seen[slot as usize], "slot {slot} handed out twice");
                seen[slot as usize] = true;
                assert_eq!(stripe.read(slot, 0), Some(worker as u128));
                assert_eq!(stripe.read(slot, 1), Some(row as u128));
            }
        }
        assert_eq!(visible(&stripe, 1, 99), 160_000);
        assert!(stripe.reserved() >= 160_000 && stripe.reserved() <= 160_000 + 8 * 1024);
    }
}

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
//! being sound.
//!
//! A text column holds 16-byte views, the layout of Umbra, DuckDB and Arrow's `StringView`: the
//! length and the first 4 bytes, then either the next 8 bytes inline, for strings up to 12 bytes,
//! or the chunk and offset of the whole string in the stripe's [`Arena`]. An update writes a new
//! view in place and the column never shifts.

use std::ops::Range;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Arc, OnceLock};

use rudb_common::Error;

use crate::arena::{Arena, Place, Space};
use crate::deletes::{PART_ROWS, PART_WORDS, Refusal, STRIPE_ROWS, UNCOMMITTED};
use crate::undo::{Image, UndoBuffer, UndoKind, UndoRef, UndoSpace};

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

/// The lock word's bit for a row a writer holds. The holder's id has the top bit set already.
const HELD: u64 = 1 << 63;

/// The lock word's bit for a row someone is parked on, `08-concurrency.md` section 8.3.
const WAITERS: u64 = 1 << 62;

/// The lock word's bit for a row an uncommitted delta names.
const DELTA: u64 = 1 << 61;

/// The stamp of an undo record whose writer aborted. It is nobody's id, not even that of a reader
/// with none, and like an id it is past every snapshot: every reader applies the record, which
/// holds the values the abort put back, and a writer checking for a newer committed change skips
/// it.
const ABORTED: u64 = u64::MAX;

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
    /// Text and blobs, as 16-byte views.
    Text,
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
            Self::Sixteen | Self::Text => 16,
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
            Width::Sixteen | Width::Text => Self::Sixteen(Parts::new(2 * ROWS)),
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
    text: bool,
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
    /// 0 when free, else the holder's id and the waiter and delta bits.
    lock: Parts<AtomicU64>,
    /// The newest undo record of the row, or 0.
    undo: Parts<AtomicU32>,
    arena: Arena,
    undos: Arc<UndoSpace>,
}

impl HotStripe {
    /// An empty stripe `id` with a column of each width in `widths` and an undo space of its own.
    #[must_use]
    pub fn new(id: u32, widths: &[Width]) -> Self {
        Self::with_undo(id, widths, Arc::new(UndoSpace::new()))
    }

    /// An empty stripe `id` with a column of each width in `widths`, whose updates keep their undo
    /// records in `undos`, the database's.
    #[must_use]
    pub fn with_undo(id: u32, widths: &[Width], undos: Arc<UndoSpace>) -> Self {
        let columns = widths
            .iter()
            .map(|&width| Column {
                text: width == Width::Text,
                cells: Cells::new(width),
                valid: Parts::new(PART_WORDS),
            })
            .collect();
        Self {
            id,
            reserved: AtomicU32::new(0),
            columns,
            created: Parts::new(ROWS),
            deleted: Parts::new(ROWS),
            lock: Parts::new(ROWS),
            undo: Parts::new(ROWS),
            arena: Arena::new(),
            undos,
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

    /// Writes the text `value` into the text column `column` at `slot`, a slot of the caller's
    /// lease that is not stamped yet, copying a string longer than 12 bytes into the arena from
    /// the worker's `space`. `None` is null.
    ///
    /// # Errors
    ///
    /// When the arena is full, after 64 GiB of strings in the stripe, or the string is longer than
    /// 4 GiB.
    ///
    /// # Panics
    ///
    /// If the slot was never leased or the column does not exist.
    pub fn write_text(
        &self,
        slot: u32,
        column: usize,
        value: Option<&[u8]>,
        space: &mut Space,
    ) -> rudb_common::Result<()> {
        debug_assert!(self.columns[column].text, "column {column} holds fixed-width values");
        let Some(value) = value else {
            self.write(slot, column, None);
            return Ok(());
        };
        let view = self.stage_text(value, space)?;
        self.write(slot, column, Some(view));
        Ok(())
    }

    /// Appends the text of `column` at `slot` to `out`, as it is now, and says whether it is not
    /// null. Visibility is [`Self::visible`]'s question.
    ///
    /// # Panics
    ///
    /// If the column does not exist.
    pub fn read_text(&self, slot: u32, column: usize, out: &mut Vec<u8>) -> bool {
        debug_assert!(self.columns[column].text, "column {column} holds fixed-width values");
        self.text(self.read(slot, column), out)
    }

    /// Appends the text a view names to `out`, and says whether it is not null. The view is a
    /// text column's value from [`Self::read`] or [`Self::read_as_of`].
    pub fn text(&self, view: Option<u128>, out: &mut Vec<u8>) -> bool {
        let Some(view) = view else { return false };
        let head = view as u64;
        let tail = (view >> 64) as u64;
        let len = head as u32;
        if len as usize > INLINE {
            let place = Place { chunk: (tail >> 32) as u32, offset: tail as u32 };
            self.arena.read(place, len, out);
        } else {
            let mut inline = [0_u8; 12];
            inline[..4].copy_from_slice(&(head >> 32).to_le_bytes()[..4]);
            inline[4..].copy_from_slice(&tail.to_le_bytes());
            out.extend_from_slice(&inline[..len as usize]);
        }
        true
    }

    /// The 16-byte view of the text `value`, with the bytes put in the arena from `space` when it
    /// is longer than 12 bytes, for [`Self::update`].
    ///
    /// # Errors
    ///
    /// When the arena is full or the string is longer than 4 GiB.
    pub fn stage_text(&self, value: &[u8], space: &mut Space) -> rudb_common::Result<u128> {
        let place = if value.len() > INLINE {
            let place = self.arena.put(space, value).ok_or_else(|| {
                Error::out_of_memory(format!(
                    "a string of {} bytes does not fit in the strings arena of stripe {}",
                    value.len(),
                    self.id
                ))
            })?;
            Some(place)
        } else {
            None
        };
        Ok(view(value, place))
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

    /// Updates the row at `slot` in place for transaction `txn` reading at `snapshot`, setting
    /// each column in `changes` to its value, `None` for null and a view from
    /// [`Self::stage_text`] for text. The values it overwrites go into an undo record in `buffer`,
    /// published before the new values are written, so a reader that copies a new value finds
    /// the record and puts the old one back.
    ///
    /// # Errors
    ///
    /// [`Refusal::Gone`] when `txn` does not see the row, and [`Refusal::Conflict`] when another
    /// transaction holds it or changed it after `snapshot`. Both leave the row as it was.
    ///
    /// # Panics
    ///
    /// If the slot was never leased, a column does not exist, or the undo space is used up.
    pub fn update(
        &self,
        slot: u32,
        changes: &[(usize, Option<u128>)],
        snapshot: u64,
        txn: u64,
        buffer: &mut UndoBuffer,
    ) -> Result<(), Refusal> {
        let me = txn | UNCOMMITTED;
        self.claim(slot, snapshot, me)?;
        let images: Vec<Image> = changes
            .iter()
            .map(|&(column, _)| Image { column: column as u16, value: self.read(slot, column) })
            .collect();
        self.push_undo(slot, UndoKind::Update, me, &images, buffer);
        for &(column, value) in changes {
            self.write(slot, column, value);
        }
        Ok(())
    }

    /// Deletes the row at `slot` for transaction `txn` reading at `snapshot`. It stays visible to
    /// everyone else until [`Self::commit_write`], and its values stay in place for them.
    ///
    /// # Errors
    ///
    /// [`Refusal::Gone`] when `txn` does not see the row, because it was never inserted as far as
    /// `txn` can tell or was already deleted, and [`Refusal::Conflict`] when another transaction
    /// holds it or changed it after `snapshot`.
    ///
    /// # Panics
    ///
    /// If the slot is past the end of the stripe or the undo space is used up.
    pub fn delete(
        &self,
        slot: u32,
        snapshot: u64,
        txn: u64,
        buffer: &mut UndoBuffer,
    ) -> Result<(), Refusal> {
        let me = txn | UNCOMMITTED;
        self.claim(slot, snapshot, me)?;
        let (part, at) = place(slot);
        let deleted = &self.deleted.get_or(part, || AtomicU64::new(LIVE))[at];
        self.push_undo(slot, UndoKind::Delete, me, &[], buffer);
        deleted.store(me, Ordering::Release);
        Ok(())
    }

    /// Stamps what `txn` did to the row at `slot` with its commit timestamp `ts` and lets the row
    /// go. A row `txn` does not hold is left alone.
    pub fn commit_write(&self, slot: u32, txn: u64, ts: u64) {
        let me = txn | UNCOMMITTED;
        if !self.holds(slot, me) {
            return;
        }
        let (part, at) = place(slot);
        self.walk_own(slot, me, |record| record.stamp(ts));
        if let Some(deleted) = self.deleted.get(part) {
            let _ = deleted[at].compare_exchange(me, ts, Ordering::AcqRel, Ordering::Relaxed);
        }
        self.release(slot);
    }

    /// Takes back what `txn` did to the row at `slot`: the values its updates overwrote go back
    /// in place, newest first, a delete is undone, and the row goes free. The undo records stay in
    /// the chain stamped `ABORTED`, because a reader may have copied a value the abort took
    /// back, and the record is what puts the old value over it.
    ///
    /// # Panics
    ///
    /// If a column an undo record names does not exist, which it always does.
    pub fn abort_write(&self, slot: u32, txn: u64) {
        let me = txn | UNCOMMITTED;
        if !self.holds(slot, me) {
            return;
        }
        let (part, at) = place(slot);
        self.walk_own(slot, me, |record| {
            for image in record.images() {
                self.write(slot, usize::from(image.column), image.value);
            }
            record.stamp(ABORTED);
        });
        if let Some(deleted) = self.deleted.get(part) {
            let _ = deleted[at].compare_exchange(me, LIVE, Ordering::AcqRel, Ordering::Relaxed);
        }
        self.release(slot);
    }

    /// The values of `columns` at `slot` as a reader at `snapshot` running as `txn` sees them,
    /// written to `out` in the same order: the values in place, with every undo record the reader
    /// does not see applied over them, newest first. Whether the row is visible at all is
    /// [`Self::visible`]'s question.
    ///
    /// # Panics
    ///
    /// If a column does not exist or `out` is shorter than `columns`.
    pub fn read_as_of(
        &self,
        slot: u32,
        columns: &[usize],
        snapshot: u64,
        txn: u64,
        out: &mut [Option<u128>],
    ) {
        for (value, &column) in out.iter_mut().zip(columns) {
            *value = self.read(slot, column);
        }
        // Pairs with the release fence in `push_undo`: a value copied above that an update wrote
        // after publishing its record means the record is seen below.
        fence(Ordering::Acquire);
        let (part, at) = place(slot);
        let Some(undo) = self.undo.get(part) else { return };
        let me = txn | UNCOMMITTED;
        let mut next = undo[at].load(Ordering::Acquire);
        while let Some(record) = self.undos.record(next) {
            let ts = record.ts();
            if ts == me || (ts & UNCOMMITTED == 0 && ts <= snapshot) {
                break;
            }
            for image in record.images() {
                if let Some(i) = columns.iter().position(|&c| c == usize::from(image.column)) {
                    out[i] = image.value;
                }
            }
            next = record.prev();
        }
    }

    /// Takes the row lock of `slot` for `me` and checks that nothing committed after `snapshot`
    /// changed the row, `08-concurrency.md` section 8.3. Nobody waits yet: a row another
    /// transaction holds is a conflict at once, which is the pin's behaviour with
    /// `lock_timeout = 0`.
    fn claim(&self, slot: u32, snapshot: u64, me: u64) -> Result<(), Refusal> {
        let (part, at) = place(slot);
        let created = self.created.get(part).map_or(0, |part| part[at].load(Ordering::Acquire));
        if created.wrapping_sub(1) >= snapshot && created != me {
            return Err(Refusal::Gone);
        }
        let lock = &self.lock.get_or(part, || AtomicU64::new(0))[at];
        let mut word = lock.load(Ordering::Acquire);
        loop {
            if word & HELD != 0 && word & !(WAITERS | DELTA) != me {
                return Err(Refusal::Conflict);
            }
            if word & HELD != 0 {
                // Its own row: nothing else can have changed it, but it may have deleted it.
                let deleted = self.deleted.get(part).map(|part| part[at].load(Ordering::Acquire));
                return if deleted == Some(me) { Err(Refusal::Gone) } else { Ok(()) };
            }
            let held = me | (word & (WAITERS | DELTA));
            match lock.compare_exchange_weak(word, held, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => word = now,
            }
        }
        let deleted = self.deleted.get(part).map_or(LIVE, |part| part[at].load(Ordering::Acquire));
        let refusal = if deleted != LIVE {
            Some(if deleted <= snapshot { Refusal::Gone } else { Refusal::Conflict })
        } else if self.newest_change(slot) > snapshot {
            Some(Refusal::Conflict)
        } else {
            None
        };
        match refusal {
            Some(refusal) => {
                self.release(slot);
                Err(refusal)
            }
            None => Ok(()),
        }
    }

    /// The commit timestamp of the newest committed change to the row at `slot`, skipping aborted
    /// records, or 0.
    fn newest_change(&self, slot: u32) -> u64 {
        let (part, at) = place(slot);
        let Some(undo) = self.undo.get(part) else { return 0 };
        let mut next = undo[at].load(Ordering::Acquire);
        while let Some(record) = self.undos.record(next) {
            let ts = record.ts();
            if ts != ABORTED {
                return ts;
            }
            next = record.prev();
        }
        0
    }

    /// Writes an undo record for `me` at `slot` and publishes it as the row's newest, then fences
    /// so the values written after it cannot be seen without it.
    fn push_undo(
        &self,
        slot: u32,
        kind: UndoKind,
        me: u64,
        images: &[Image],
        buffer: &mut UndoBuffer,
    ) {
        let (part, at) = place(slot);
        let head = &self.undo.get_or(part, || AtomicU32::new(0))[at];
        let rid = (u64::from(self.id) << 32) | u64::from(slot);
        let prev = head.load(Ordering::Acquire);
        let record = buffer
            .write(&self.undos, kind, me, rid, prev, images)
            .expect("the undo space holds 64 GiB of records before garbage collection");
        head.store(record, Ordering::Release);
        fence(Ordering::Release);
    }

    /// Runs `f` over the undo records of `slot` that `me` wrote, newest first.
    fn walk_own(&self, slot: u32, me: u64, mut f: impl FnMut(crate::undo::Record<'_>)) {
        let (part, at) = place(slot);
        let Some(undo) = self.undo.get(part) else { return };
        let mut next: UndoRef = undo[at].load(Ordering::Acquire);
        while let Some(record) = self.undos.record(next) {
            if record.ts() != me {
                break;
            }
            f(record);
            next = record.prev();
        }
    }

    fn holds(&self, slot: u32, me: u64) -> bool {
        let (part, at) = place(slot);
        self.lock
            .get(part)
            .is_some_and(|lock| lock[at].load(Ordering::Acquire) & !(WAITERS | DELTA) == me)
    }

    fn release(&self, slot: u32) {
        let (part, at) = place(slot);
        if let Some(lock) = self.lock.get(part) {
            lock[at].fetch_and(WAITERS | DELTA, Ordering::Release);
        }
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
        let stamps = (self.created.allocated() + self.deleted.allocated() + self.lock.allocated())
            * ROWS
            * 8
            + self.undo.allocated() * ROWS * 4;
        columns + stamps + usize::try_from(self.arena.bytes()).unwrap_or(usize::MAX)
    }
}

/// The longest string a view holds inline.
const INLINE: usize = 12;

/// The 16-byte view of `value`: its length and first 4 bytes, then the next 8 bytes, or `place`
/// when it is longer than [`INLINE`].
fn view(value: &[u8], place: Option<Place>) -> u128 {
    let mut prefix = [0_u8; 4];
    let first = value.len().min(4);
    prefix[..first].copy_from_slice(&value[..first]);
    let head = value.len() as u64 | (u64::from(u32::from_le_bytes(prefix)) << 32);
    let tail = match place {
        Some(place) => (u64::from(place.chunk) << 32) | u64::from(place.offset),
        None => {
            let mut rest = [0_u8; 8];
            let more = value.get(4..).unwrap_or_default();
            rest[..more.len()].copy_from_slice(more);
            u64::from_le_bytes(rest)
        }
    };
    u128::from(head) | (u128::from(tail) << 64)
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
    /// The worker's carve of the stripe's arena.
    space: Space,
}

impl Lease {
    /// A worker's lease in `stripe`, which takes no slots until it is asked for some.
    #[must_use]
    pub fn new(stripe: Arc<HotStripe>) -> Self {
        Self { stripe, next: 0, end: 0, size: FIRST_LEASE, space: Space::default() }
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

    /// [`HotStripe::write_text`] from the worker's carve of the arena.
    ///
    /// # Errors
    ///
    /// When the arena is full.
    ///
    /// # Panics
    ///
    /// If the slot was never leased or the column does not exist.
    pub fn write_text(
        &mut self,
        slot: u32,
        column: usize,
        value: Option<&[u8]>,
    ) -> rudb_common::Result<()> {
        self.stripe.write_text(slot, column, value, &mut self.space)
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

    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::{FIRST_LEASE, HotStripe, LARGEST_LEASE, Lease, PART_ROWS, PART_WORDS, Width};
    use crate::arena::Space;
    use crate::deletes::{Refusal, STRIPE_ROWS};
    use crate::undo::UndoBuffer;

    /// A committed row of `values` at a fresh slot, inserted by transaction 1 at timestamp 1.
    fn row(stripe: &HotStripe, values: &[Option<u128>]) -> u32 {
        let slots = stripe.lease(1).expect("room");
        for (column, &value) in values.iter().enumerate() {
            stripe.write(slots.start, column, value);
        }
        stripe.insert(slots.clone(), 1);
        stripe.commit_insert(slots.clone(), 1, 1);
        slots.start
    }

    fn as_of(stripe: &HotStripe, slot: u32, snapshot: u64, txn: u64) -> Vec<Option<u128>> {
        let columns: Vec<usize> = (0..stripe.width()).collect();
        let mut out = vec![None; columns.len()];
        stripe.read_as_of(slot, &columns, snapshot, txn, &mut out);
        out
    }

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
        let mut buffer = UndoBuffer::default();
        let slots = stripe.lease(2).expect("slots");
        stripe.insert(slots.clone(), 1);
        let first = slots.start;
        assert_eq!(stripe.delete(first, 0, 2, &mut buffer), Err(Refusal::Gone), "not visible");
        stripe.commit_insert(slots.clone(), 1, 5);
        stripe.delete(first, 5, 2, &mut buffer).expect("delete");
        assert!(!stripe.visible(first, 5, 2), "its own delete");
        assert!(stripe.visible(first, 9, 3), "someone else's uncommitted delete");
        assert_eq!(stripe.delete(first, 5, 2, &mut buffer), Err(Refusal::Gone));
        assert_eq!(stripe.delete(first, 5, 3, &mut buffer), Err(Refusal::Conflict));
        stripe.commit_write(first, 2, 8);
        assert!(stripe.visible(first, 7, 3), "a snapshot before the delete");
        assert!(!stripe.visible(first, 8, 3));
        assert_eq!(stripe.delete(first, 7, 3, &mut buffer), Err(Refusal::Conflict), "too late");
        assert_eq!(stripe.delete(first, 8, 3, &mut buffer), Err(Refusal::Gone));

        stripe.delete(first + 1, 5, 4, &mut buffer).expect("delete");
        stripe.abort_write(first + 1, 4);
        assert!(stripe.visible(first + 1, 5, 6));
        stripe.delete(first + 1, 5, 5, &mut buffer).expect("the row is live again");
    }

    #[test]
    fn an_update_is_seen_from_its_commit_and_the_old_values_before() {
        let stripe = stripe();
        let mut buffer = UndoBuffer::default();
        let slot = row(&stripe, &[Some(1), Some(2), Some(3), Some(4), Some(5)]);
        let before = as_of(&stripe, slot, 1, 9);
        stripe.update(slot, &[(1, Some(20)), (3, None)], 1, 2, &mut buffer).expect("update");
        let after = vec![Some(1), Some(20), Some(3), None, Some(5)];
        assert_eq!(as_of(&stripe, slot, 1, 2), after, "its own writes");
        assert_eq!(as_of(&stripe, slot, 100, 9), before, "not committed yet");
        stripe.update(slot, &[(1, Some(21)), (0, Some(10))], 1, 2, &mut buffer).expect("again");
        assert_eq!(as_of(&stripe, slot, 100, 9), before, "both records applied");
        stripe.commit_write(slot, 2, 6);
        assert_eq!(as_of(&stripe, slot, 5, 9), before);
        assert_eq!(as_of(&stripe, slot, 6, 9), vec![Some(10), Some(21), Some(3), None, Some(5)]);

        let mut out = [None; 1];
        stripe.read_as_of(slot, &[3], 5, 9, &mut out);
        assert_eq!(out, [Some(4)], "a column subset");
    }

    #[test]
    fn writers_to_one_row_conflict_until_the_first_is_done() {
        let stripe = stripe();
        let mut buffer = UndoBuffer::default();
        let slot = row(&stripe, &[Some(1), Some(2), Some(3), Some(4), Some(5)]);
        stripe.update(slot, &[(0, Some(7))], 1, 2, &mut buffer).expect("the first writer");
        assert_eq!(stripe.update(slot, &[(0, Some(8))], 1, 3, &mut buffer), Err(Refusal::Conflict));
        assert_eq!(stripe.delete(slot, 1, 3, &mut buffer), Err(Refusal::Conflict));
        stripe.commit_write(slot, 2, 4);
        assert_eq!(
            stripe.update(slot, &[(0, Some(8))], 3, 3, &mut buffer),
            Err(Refusal::Conflict),
            "a change committed after its snapshot"
        );
        stripe.update(slot, &[(0, Some(8))], 4, 5, &mut buffer).expect("a later snapshot");
        stripe.commit_write(slot, 5, 9);
        assert_eq!(as_of(&stripe, slot, 4, 0)[0], Some(7));
        assert_eq!(as_of(&stripe, slot, 9, 0)[0], Some(8));
        assert_eq!(as_of(&stripe, slot, 3, 0)[0], Some(1));
        stripe.delete(slot, 9, 6, &mut buffer).expect("delete");
        assert_eq!(stripe.update(slot, &[(0, Some(1))], 9, 6, &mut buffer), Err(Refusal::Gone));
    }

    #[test]
    fn an_abort_puts_the_old_values_back_and_frees_the_row() {
        let stripe = stripe();
        let mut buffer = UndoBuffer::default();
        let slot = row(&stripe, &[Some(1), Some(2), Some(3), Some(4), Some(5)]);
        let before = as_of(&stripe, slot, 1, 0);
        stripe.update(slot, &[(0, Some(100)), (4, None)], 1, 2, &mut buffer).expect("update");
        stripe.update(slot, &[(0, Some(200)), (2, Some(300))], 1, 2, &mut buffer).expect("more");
        stripe.delete(slot, 1, 2, &mut buffer).expect("and delete");
        stripe.abort_write(slot, 2);
        let row: Vec<_> = (0..5).map(|column| stripe.read(slot, column)).collect();
        assert_eq!(row, before, "in place");
        assert_eq!(as_of(&stripe, slot, 1, 0), before, "through the chain");
        assert!(stripe.visible(slot, 1, 0));
        stripe.update(slot, &[(0, Some(9))], 1, 3, &mut buffer).expect("an old snapshot writes");
        stripe.commit_write(slot, 3, 2);
        assert_eq!(as_of(&stripe, slot, 1, 0), before);
        assert_eq!(as_of(&stripe, slot, 2, 0)[0], Some(9));
        stripe.commit_write(slot, 3, 5);
        assert_eq!(as_of(&stripe, slot, 2, 0)[0], Some(9), "a row it no longer holds");
    }

    #[test]
    fn a_text_update_reads_back_old_and_new() {
        let stripe = HotStripe::new(0, &[Width::Text]);
        let mut buffer = UndoBuffer::default();
        let mut space = Space::default();
        let slots = stripe.lease(1).expect("room");
        stripe
            .write_text(slots.start, 0, Some(b"the original long string"), &mut space)
            .expect("room");
        stripe.insert(slots.clone(), 1);
        stripe.commit_insert(slots.clone(), 1, 1);
        let new = stripe.stage_text(b"its replacement, longer still", &mut space).expect("room");
        stripe.update(slots.start, &[(0, Some(new))], 1, 2, &mut buffer).expect("update");
        stripe.commit_write(slots.start, 2, 2);
        let mut view = [None];
        let mut text = Vec::new();
        stripe.read_as_of(slots.start, &[0], 1, 0, &mut view);
        assert!(stripe.text(view[0], &mut text));
        assert_eq!(text, b"the original long string");
        text.clear();
        stripe.read_as_of(slots.start, &[0], 2, 0, &mut view);
        stripe.text(view[0], &mut text);
        assert_eq!(text, b"its replacement, longer still");
    }

    #[test]
    fn readers_never_see_half_an_update() {
        let stripe = Arc::new(HotStripe::new(0, &[Width::Eight, Width::Eight, Width::Text]));
        let slot = row(&stripe, &[Some(1), Some(1), None]);
        let committed = Arc::new(AtomicU64::new(1));
        let done = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let (stripe, committed, done) =
                    (Arc::clone(&stripe), Arc::clone(&committed), Arc::clone(&done));
                thread::spawn(move || {
                    let mut reads = 0_u64;
                    let mut text = Vec::new();
                    while !done.load(Ordering::Acquire) || reads < 10_000 {
                        let snapshot = committed.load(Ordering::Acquire);
                        let mut out = [None; 3];
                        stripe.read_as_of(slot, &[0, 1, 2], snapshot, 0, &mut out);
                        assert_eq!(out[0], Some(u128::from(snapshot)), "at {snapshot}");
                        assert_eq!(out[1], out[0]);
                        text.clear();
                        if stripe.text(out[2], &mut text) {
                            assert_eq!(text, format!("value number {snapshot:020}").as_bytes());
                        } else {
                            assert_eq!(snapshot, 1);
                        }
                        reads += 1;
                    }
                    reads
                })
            })
            .collect();
        let mut buffer = UndoBuffer::default();
        let mut space = Space::default();
        for ts in 2..20_000_u64 {
            let text = format!("value number {ts:020}");
            let view = stripe.stage_text(text.as_bytes(), &mut space).expect("room");
            let changes = [(0, Some(u128::from(ts))), (1, Some(u128::from(ts))), (2, Some(view))];
            stripe.update(slot, &changes, ts - 1, ts, &mut buffer).expect("the only writer");
            if ts % 7 == 0 {
                stripe.abort_write(slot, ts);
                let changes =
                    [(0, Some(u128::from(ts))), (1, Some(u128::from(ts))), (2, Some(view))];
                stripe.update(slot, &changes, ts - 1, ts, &mut buffer).expect("again");
            }
            stripe.commit_write(slot, ts, ts);
            committed.store(ts, Ordering::Release);
        }
        done.store(true, Ordering::Release);
        for reader in readers {
            assert!(reader.join().expect("the reader finishes") > 0);
        }
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
    fn text_comes_back_short_long_empty_and_null() {
        let stripe = Arc::new(HotStripe::new(3, &[Width::Text, Width::Eight]));
        let mut lease = Lease::new(Arc::clone(&stripe));
        let values: Vec<Option<Vec<u8>>> = vec![
            Some(Vec::new()),
            Some(b"abc".to_vec()),
            Some(b"four".to_vec()),
            Some(b"twelve bytes".to_vec()),
            Some(b"thirteen byte".to_vec()),
            None,
            Some(vec![0xFF; 100_000]),
            Some("unicode \u{00e9}t\u{00e9} caf\u{00e9}".as_bytes().to_vec()),
        ];
        let slots = lease.take(values.len() as u32);
        for (slot, value) in slots.clone().zip(&values) {
            lease.write_text(slot, 0, value.as_deref()).expect("room");
        }
        for (slot, value) in slots.zip(&values) {
            let mut out = b"kept".to_vec();
            assert_eq!(stripe.read_text(slot, 0, &mut out), value.is_some());
            assert_eq!(&out[..4], b"kept", "appended, not replaced");
            assert_eq!(out[4..], value.clone().unwrap_or_default()[..]);
        }
    }

    #[test]
    fn a_text_update_in_place_leaves_the_old_bytes_readable() {
        let stripe = Arc::new(HotStripe::new(3, &[Width::Text]));
        let mut lease = Lease::new(Arc::clone(&stripe));
        let slot = lease.take(1).start;
        lease.write_text(slot, 0, Some(b"the first long value")).expect("room");
        let old = stripe.read(slot, 0).expect("a view");
        lease.write_text(slot, 0, Some(b"a second, longer value than before")).expect("room");
        let mut out = Vec::new();
        stripe.read_text(slot, 0, &mut out);
        assert_eq!(out, b"a second, longer value than before");
        stripe.write(slot, 0, Some(old));
        out.clear();
        stripe.read_text(slot, 0, &mut out);
        assert_eq!(out, b"the first long value", "an undo image is still good");
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

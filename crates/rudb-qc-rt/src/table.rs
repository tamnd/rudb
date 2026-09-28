//! The aggregation hash table and the distinct sets, per `spec/compiler/11-aggregation-sort-window.md`.
//!
//! A group is a row in a page that never moves. `ht_insert` hands compiled code the row's address
//! and the code updates the accumulators in place, so the table only has to find rows, not know
//! what is in them. The row is the group id, then the key as the generator laid it out, then the
//! accumulators:
//!
//! ```text
//! [gid: u64][key: key_size bytes, padded to 8][accumulators: acc_size bytes]
//! ```
//!
//! A key field is its value at its physical width followed by one byte that is 1 when the value
//! is null. Fixed width values compare as bytes, which is why the generator stores a zero value
//! under a null. Strings compare by content, and a long one is copied into the runtime heap when
//! its group is made, so the row never points into a morsel.

#![allow(unsafe_code)]

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, PoisonError};

use crate::text::{self, Heap};

/// One key column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyField {
    /// Where the value is in the key.
    pub offset: u32,
    /// The width of the value. A string is 16.
    pub width: u32,
    /// Whether the value is a `str16`.
    pub text: bool,
}

impl KeyField {
    /// Where the null byte is.
    #[must_use]
    pub fn null(self) -> u32 {
        self.offset + self.width
    }
}

/// The shape of a table's rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    /// The key columns.
    pub keys: Vec<KeyField>,
    /// The bytes of key, null bytes included.
    pub key_size: u32,
    /// What a new group's accumulators start as.
    pub init: Vec<u8>,
}

impl Layout {
    /// Where the accumulators start in a row.
    #[must_use]
    pub fn acc_offset(key_size: u32) -> u32 {
        8 + key_size.next_multiple_of(8)
    }

    /// The size of a row.
    #[must_use]
    pub fn row_size(&self) -> usize {
        (Layout::acc_offset(self.key_size) as usize + self.init.len()).next_multiple_of(16)
    }
}

const ROWS_PER_PAGE: usize = 1024;

/// An odd number with its bits spread out, the golden ratio in fixed point.
const SPREAD: u64 = 0x9e37_79b9_7f4a_7c15;

/// One piece of the work of [`GroupTable::join_with`].
pub type Job<'a> = Box<dyn FnOnce() + Send + 'a>;

/// A grouping hash table.
#[derive(Debug)]
pub struct GroupTable {
    layout: Layout,
    row_size: usize,
    pages: Vec<Box<[u8]>>,
    /// How many rows of the last page are taken.
    fill: usize,
    /// The address of every row, by group id. A row is read through this and not worked out from
    /// its page, because a table [`join`](GroupTable::join) makes holds pages that are not full.
    rows: Vec<usize>,
    hashes: Vec<u64>,
    /// Open addressing over group ids plus one in the low half, with the low half of the group's
    /// hash above it, zero for empty, and empty until the first insert in a table made by
    /// [`join`](GroupTable::join). The hash in the slot is what lets a probe pass a slot of another
    /// key without reading that group's row.
    slots: Vec<u64>,
    /// How many groups the slots may hold before they are emptied, zero for no limit. The rows
    /// stay where they are, so a key seen again after that gets a second group, which a merge by
    /// parts folds together. A table made small this way keeps its slots and its newest rows in
    /// the cache while a worker probes it.
    cap: usize,
    /// The first group the slots know about.
    since: usize,
    /// The groups this table may make, when a limit with no order above the aggregate reads only
    /// that many of them.
    limited: Option<Limited>,
    /// Whether a new group in [`absorb_some`](GroupTable::absorb_some) keeps the row it came from.
    adopting: bool,
}

/// The groups the tables of a limited aggregate's workers agree on: the first keys any of them
/// saw, up to the limit. A worker makes a group only for a key in the set, and the rows of every
/// other key go to a row that is never read, so each group the workers make sees every row of its
/// key and the merge adds up whole groups. A set with fewer keys than the limit takes the key of
/// any worker that asks.
#[derive(Debug)]
pub struct Agreed {
    held: Mutex<(GroupTable, Heap)>,
    limit: usize,
}

impl Agreed {
    /// A set of at most `limit` keys for tables of this shape.
    #[must_use]
    pub fn new(layout: Layout, limit: usize) -> Agreed {
        Agreed { held: Mutex::new((GroupTable::new(layout), Heap::new())), limit }
    }
}

/// A table's side of an [`Agreed`] set.
#[derive(Debug)]
struct Limited {
    agreed: Arc<Agreed>,
    /// Whether the table has made a group for every key of the full set, after which a key it does
    /// not have is one it never will.
    installed: bool,
    /// The row the rows of the keys left out update, as long as a row and never read.
    dump: Vec<u128>,
}

impl GroupTable {
    /// A table for rows of this shape. A table with no key columns is a scalar aggregate and has
    /// its one group from the start, which is what makes `SELECT count(*)` over nothing one row.
    #[must_use]
    pub fn new(layout: Layout) -> GroupTable {
        let row_size = layout.row_size();
        let mut table = GroupTable {
            layout,
            row_size,
            pages: Vec::new(),
            fill: ROWS_PER_PAGE,
            rows: Vec::new(),
            hashes: Vec::new(),
            slots: vec![0; 64],
            cap: 0,
            since: 0,
            limited: None,
            adopting: false,
        };
        if table.layout.keys.is_empty() {
            table.add(&[], 0, None);
        }
        table
    }

    /// A table like [`new`](GroupTable::new) makes, with room for `groups` groups before it grows.
    #[must_use]
    pub fn with_capacity(layout: Layout, groups: usize) -> GroupTable {
        let mut table = GroupTable::new(layout);
        table.slots = vec![0; (groups * 2).next_power_of_two().max(64)];
        table.rows.reserve(groups);
        table.hashes.reserve(groups);
        table
    }

    /// Lets the table forget the groups it has once it holds `cap` of them, for a worker whose
    /// groups are merged by parts after the scan.
    pub fn cap(&mut self, cap: usize) {
        self.cap = cap;
    }

    /// Makes a group only for the keys of `agreed`, which the other tables of the same aggregate
    /// share. A table with no key columns has one group whatever the limit, and is left as it is.
    pub fn limit(&mut self, agreed: Arc<Agreed>) {
        if self.layout.keys.is_empty() {
            return;
        }
        let dump = self.layout.init.len() + Layout::acc_offset(self.layout.key_size) as usize;
        let mut dump = vec![0u128; dump.div_ceil(16)];
        let acc = Layout::acc_offset(self.layout.key_size) as usize;
        // SAFETY: the vector is as long as a row, and any byte pattern is a `u128`.
        let row = unsafe {
            std::slice::from_raw_parts_mut(dump.as_mut_ptr().cast::<u8>(), dump.len() * 16)
        };
        row[acc..acc + self.layout.init.len()].copy_from_slice(&self.layout.init);
        self.limited = Some(Limited { agreed, installed: false, dump });
    }

    /// Frees the slots of a table that is done taking keys, on the thread that made it. An insert
    /// after this builds them again.
    pub fn seal(&mut self) {
        self.slots = Vec::new();
    }

    /// Whether a key may have more than one group, because the slots were emptied.
    #[must_use]
    pub fn forgot(&self) -> bool {
        self.since != 0
    }

    /// The shape of the rows.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// How many groups there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether there are no groups.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The row of group `gid`.
    ///
    /// # Panics
    ///
    /// If there is no such group.
    #[must_use]
    pub fn row(&self, gid: usize) -> &[u8] {
        // SAFETY: every address in `rows` is a row of `row_size` bytes in a page this table owns.
        unsafe { crate::mem::slice(self.rows[gid], self.row_size) }
    }

    /// The hash of the key of group `gid`.
    #[must_use]
    pub fn hash(&self, gid: usize) -> u64 {
        self.hashes[gid]
    }

    /// The address of the row of group `gid`, for compiled code.
    #[must_use]
    pub fn address(&self, gid: usize) -> usize {
        self.rows[gid]
    }

    /// The group of the key at `key`, made if it is new, and the address of its row.
    ///
    /// # Safety
    ///
    /// `key` must be the address of `key_size` readable bytes laid out as the table's [`Layout`]
    /// says, and every non-inline string in it must point at live bytes.
    pub unsafe fn insert(&mut self, key: usize, hash: u64, heap: &mut Heap) -> usize {
        // SAFETY: the caller's contract.
        let key = unsafe { crate::mem::slice(key, self.layout.key_size as usize) };
        if self.limited.is_some() {
            return self.insert_limited(key, hash, heap);
        }
        let gid = self.find_or_add(key, hash, Some(heap));
        self.rows[gid]
    }

    /// [`insert`](GroupTable::insert) for a table with an [`Agreed`] set. A key the table has is
    /// found without the lock, and so is every key once the table has the whole set.
    fn insert_limited(&mut self, key: &[u8], hash: u64, heap: &mut Heap) -> usize {
        if let Some(gid) = self.find(key, hash) {
            return self.rows[gid];
        }
        let Some(limited) = &self.limited else { return 0 };
        if !limited.installed {
            let agreed = Arc::clone(&limited.agreed);
            let mut held = agreed.held.lock().unwrap_or_else(PoisonError::into_inner);
            let (set, kept) = &mut *held;
            let known = set.find(key, hash).is_some();
            if known || set.len() < agreed.limit {
                if !known {
                    set.find_or_add(key, hash, Some(kept));
                }
                drop(held);
                let gid = self.find_or_add(key, hash, Some(heap));
                return self.rows[gid];
            }
            // The set is full and this key is not in it. The table makes the groups of the set it
            // does not have yet, so it never has to ask again.
            let size = self.layout.key_size as usize;
            for gid in 0..set.len() {
                self.find_or_add(&set.row(gid)[8..8 + size], set.hashes[gid], Some(heap));
            }
            drop(held);
            if let Some(limited) = &mut self.limited {
                limited.installed = true;
            }
        }
        self.limited.as_ref().map_or(0, |l| l.dump.as_ptr().expose_provenance())
    }

    /// Folds every group of `other` into this table. A group this table does not have yet is made,
    /// and then `combine` gets its row and the row from `other`, so a new group is combined into
    /// its starting accumulators the same way an old one is. `map` gets the group id each group of
    /// `other` landed on, in order, for the state that is kept by group id outside the rows.
    ///
    /// The two tables must have the same layout, which a worker's copy of a table does. A long
    /// string in a key is not copied, so the caller keeps whatever holds `other`'s alive as long as
    /// this table, which a runtime does with the workers it folds.
    pub fn absorb(
        &mut self,
        other: &GroupTable,
        map: &mut Vec<usize>,
        mut combine: impl FnMut(&mut [u8], &[u8]),
    ) {
        map.clear();
        // A table with no keys made its one group without a slot, so it is never found by key.
        if self.layout.keys.is_empty() {
            if !other.is_empty() && !self.is_empty() {
                combine(self.row_mut(0), other.row(0));
                map.push(0);
            }
            return;
        }
        let size = self.layout.key_size as usize;
        for gid in 0..other.len() {
            let src = other.row(gid);
            let at = self.find_or_add(&src[8..8 + size], other.hashes[gid], None);
            map.push(at);
            combine(self.row_mut(at), src);
        }
    }

    /// The groups of this table split into `1 << bits` parts by their hashes, so that two tables
    /// split the same way put a key in the same part. A group is its row's address and its hash,
    /// read here in the order the table holds them, so that the part that folds it reads nothing
    /// of this table but the row.
    ///
    /// A key's hash is a CRC-32C, which leaves the top half of the word zero, so the part is the
    /// top bits of the hash times an odd number, which every bit of the hash moves.
    #[must_use]
    pub fn split(&self, bits: u32) -> Vec<Vec<(usize, u64)>> {
        let room = (self.len() >> bits) + (self.len() >> (bits + 3)) + 8;
        let mut parts = vec![Vec::with_capacity(room); 1 << bits];
        let shift = 64 - bits;
        for (&row, &hash) in self.rows.iter().zip(&self.hashes) {
            let part = if bits == 0 { 0 } else { (hash.wrapping_mul(SPREAD) >> shift) as usize };
            parts[part].push((row, hash));
        }
        parts
    }

    /// Folds the groups `picks` of `other`, as [`split`](GroupTable::split) gave them, into this
    /// table, as [`absorb`](GroupTable::absorb) does with all of them. With `made`, each group's id
    /// in `other` and the group it became here are pushed to it. With `whole`, a key this table
    /// does not have yet takes the row of `other` as it is, which is right when combining a row
    /// into a new group's row gives that row back.
    pub fn absorb_some(
        &mut self,
        other: &GroupTable,
        picks: &[(usize, u64)],
        mut made: Option<&mut Vec<(u32, u32)>>,
        whole: bool,
        mut combine: impl FnMut(&mut [u8], &[u8]),
    ) {
        /// How many groups ahead a row is asked for before it is read.
        const AHEAD: usize = 8;
        let size = self.layout.key_size as usize;
        for (i, &(row, hash)) in picks.iter().enumerate() {
            if let Some(&(next, _)) = picks.get(i + AHEAD) {
                prefetch(next);
            }
            // SAFETY: `split` took the address from `other`'s rows, and `other` is borrowed, so
            // its pages are still there, or kept by whoever took them.
            let src = unsafe { crate::mem::slice(row, other.row_size) };
            let key = &src[8..8 + size];
            let at = match self.probe(key, hash) {
                Ok(gid) => {
                    combine(self.row_mut(gid), src);
                    gid
                }
                Err(slot) => {
                    let gid = if whole && self.adopting {
                        self.rows.push(row);
                        self.hashes.push(hash);
                        self.rows.len() - 1
                    } else if whole {
                        self.copy(src, hash)
                    } else {
                        let gid = self.add(key, hash, None);
                        combine(self.row_mut(gid), src);
                        gid
                    };
                    self.place(slot, gid, hash);
                    gid
                }
            };
            if let Some(made) = made.as_deref_mut() {
                let gid = u64::from_le_bytes(src[..8].try_into().unwrap_or_default());
                made.push((gid as u32, at as u32));
            }
        }
    }

    /// Takes the pages this table's rows are in, for a merge whose parts keep the rows where they
    /// are. The rows can still be read and written through their addresses as long as the caller
    /// keeps the pages, and a row the table makes after this goes in a page of its own.
    pub fn take_pages(&mut self) -> Vec<Box<[u8]>> {
        self.fill = ROWS_PER_PAGE;
        std::mem::take(&mut self.pages)
    }

    /// Keeps `pages`, which [`take_pages`](GroupTable::take_pages) took from the tables whose rows
    /// this one holds, for as long as this table.
    pub fn keep(&mut self, mut pages: Vec<Box<[u8]>>) {
        self.pages.append(&mut pages);
        self.fill = ROWS_PER_PAGE;
    }

    /// Makes [`absorb_some`](GroupTable::absorb_some) with `whole` take a row it does not have yet
    /// where it is, with no copy, which is right when the pages of the tables it absorbs from were
    /// taken and will be kept by the table this one ends up in.
    pub fn adopt_rows(&mut self) {
        self.adopting = true;
    }

    /// One table of the groups of `parts`, which have the layout `layout` and no key in common,
    /// in the order they come. The rows do not move, so the group id at the front of one is the
    /// one it had in its part, which nothing reads once the workers are done.
    #[must_use]
    pub fn join(layout: Layout, parts: Vec<GroupTable>) -> GroupTable {
        GroupTable::join_with(layout, parts, |jobs| jobs.into_iter().for_each(|job| job())).0
    }

    /// [`join`](GroupTable::join), with the copying of each part's group ids and the freeing of
    /// what is left of it handed to `run` as one job a part, for it to run on as many threads as
    /// it has. A merge of millions of groups spent a fifth of its time doing that on one.
    pub fn join_with<R>(
        layout: Layout,
        parts: Vec<GroupTable>,
        run: impl FnOnce(Vec<Job<'_>>) -> R,
    ) -> (GroupTable, R) {
        let n = parts.iter().map(GroupTable::len).sum();
        let mut rows = vec![0; n];
        let mut hashes = vec![0; n];
        let mut pages = Vec::new();
        let ran = {
            let (mut r, mut h) = (rows.as_mut_slice(), hashes.as_mut_slice());
            let mut jobs: Vec<Job<'_>> = Vec::with_capacity(parts.len());
            for mut part in parts {
                pages.append(&mut part.pages);
                let (rows, rest) = std::mem::take(&mut r).split_at_mut(part.len());
                r = rest;
                let (hashes, rest) = std::mem::take(&mut h).split_at_mut(part.len());
                h = rest;
                jobs.push(Box::new(move || {
                    rows.copy_from_slice(&part.rows);
                    hashes.copy_from_slice(&part.hashes);
                    drop(part);
                }));
            }
            run(jobs)
        };
        let table = GroupTable {
            row_size: layout.row_size(),
            layout,
            pages,
            fill: ROWS_PER_PAGE,
            rows,
            hashes,
            slots: Vec::new(),
            cap: 0,
            since: 0,
            limited: None,
            adopting: false,
        };
        (table, ran)
    }

    fn row_mut(&mut self, gid: usize) -> &mut [u8] {
        let at = std::ptr::with_exposed_provenance_mut::<u8>(self.rows[gid]);
        // SAFETY: as in `row`, and `&mut self` means nothing else is reading it.
        unsafe { std::slice::from_raw_parts_mut(at, self.row_size) }
    }

    /// The group id of `key`, if the table has it.
    fn find(&self, key: &[u8], hash: u64) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut at = (hash as usize) & mask;
        let tag = hash << 32;
        loop {
            let slot = self.slots[at];
            if slot == 0 {
                return None;
            }
            let gid = (slot as u32 - 1) as usize;
            if slot & !0xffff_ffff == tag && self.same(gid, key) {
                return Some(gid);
            }
            at = (at + 1) & mask;
        }
    }

    /// The group id of `key`, made if it is new. A long string in a new key is copied into `heap`
    /// when there is one.
    fn find_or_add(&mut self, key: &[u8], hash: u64, heap: Option<&mut Heap>) -> usize {
        match self.probe(key, hash) {
            Ok(gid) => gid,
            Err(at) => {
                let gid = self.add(key, hash, heap);
                self.place(at, gid, hash);
                gid
            }
        }
    }

    /// The group id of `key`, or the empty slot a new group of it goes in.
    fn probe(&mut self, key: &[u8], hash: u64) -> Result<usize, usize> {
        if self.slots.is_empty() {
            self.slots = vec![0; 64];
            self.grow();
        }
        let mask = self.slots.len() - 1;
        let mut at = (hash as usize) & mask;
        let tag = hash << 32;
        loop {
            let slot = self.slots[at];
            if slot == 0 {
                return Err(at);
            }
            let gid = (slot as u32 - 1) as usize;
            if slot & !0xffff_ffff == tag && self.same(gid, key) {
                return Ok(gid);
            }
            at = (at + 1) & mask;
        }
    }

    /// Points the empty slot `at` that [`probe`](GroupTable::probe) gave at the new group `gid`,
    /// and forgets the groups or grows the slots when there are enough of them.
    fn place(&mut self, at: usize, gid: usize, hash: u64) {
        self.slots[at] = (hash << 32) | (gid as u64 + 1);
        if self.cap != 0 && self.rows.len() - self.since >= self.cap {
            self.slots.fill(0);
            self.since = self.rows.len();
        } else if (self.rows.len() - self.since) * 2 > self.slots.len() {
            self.grow();
        }
    }

    fn same(&self, gid: usize, key: &[u8]) -> bool {
        let row = &self.row(gid)[8..8 + key.len()];
        for f in &self.layout.keys {
            let n = f.null() as usize;
            if row[n] != key[n] {
                return false;
            }
            if key[n] != 0 {
                continue;
            }
            let (o, w) = (f.offset as usize, f.width as usize);
            if f.text {
                let a = read_u128(&row[o..o + 16]);
                let b = read_u128(&key[o..o + 16]);
                // SAFETY: a row's strings are inline or in the heap, and the caller of `insert`
                // vouches for the key's.
                if a != b && unsafe { text::bytes(&a) != text::bytes(&b) } {
                    return false;
                }
            } else if row[o..o + w] != key[o..o + w] {
                return false;
            }
        }
        true
    }

    /// The address of a row no group has yet, at the end of the last page.
    fn room(&mut self) -> usize {
        if self.fill == ROWS_PER_PAGE || self.pages.is_empty() {
            self.pages.push(vec![0u8; ROWS_PER_PAGE * self.row_size].into_boxed_slice());
            self.fill = 0;
        }
        let at = self.fill * self.row_size;
        self.fill += 1;
        self.pages.last_mut().map_or(0, |page| page[at..].as_mut_ptr().expose_provenance())
    }

    /// A new group whose row is `src` but for the group id at its front.
    fn copy(&mut self, src: &[u8], hash: u64) -> usize {
        let gid = self.rows.len();
        let address = self.room();
        // SAFETY: `room` gave a row of `row_size` bytes in a page this table owns, and no group
        // points at it yet.
        let row = unsafe {
            std::slice::from_raw_parts_mut(
                std::ptr::with_exposed_provenance_mut::<u8>(address),
                self.row_size,
            )
        };
        row.copy_from_slice(&src[..self.row_size]);
        row[..8].copy_from_slice(&(gid as u64).to_le_bytes());
        self.rows.push(address);
        self.hashes.push(hash);
        gid
    }

    fn add(&mut self, key: &[u8], hash: u64, mut heap: Option<&mut Heap>) -> usize {
        let gid = self.rows.len();
        let address = self.room();
        let acc = Layout::acc_offset(self.layout.key_size) as usize;
        // SAFETY: as in `copy`.
        let row = unsafe {
            std::slice::from_raw_parts_mut(
                std::ptr::with_exposed_provenance_mut::<u8>(address),
                self.row_size,
            )
        };
        let layout = &self.layout;
        row[..8].copy_from_slice(&(gid as u64).to_le_bytes());
        row[8..8 + key.len()].copy_from_slice(key);
        for f in &layout.keys {
            let o = 8 + f.offset as usize;
            if let Some(heap) = heap.as_deref_mut()
                && f.text
                && row[8 + f.null() as usize] == 0
            {
                let s = read_u128(&row[o..o + 16]);
                if text::len(s) > text::INLINE {
                    // SAFETY: as in `same`.
                    let kept = heap.keep(unsafe { text::bytes(&s) });
                    row[o..o + 16].copy_from_slice(&kept.to_le_bytes());
                }
            }
        }
        row[acc..acc + layout.init.len()].copy_from_slice(&layout.init);
        self.rows.push(address);
        self.hashes.push(hash);
        gid
    }

    fn grow(&mut self) {
        let mut size = self.slots.len() * 2;
        while (self.rows.len() - self.since) * 2 > size {
            size *= 2;
        }
        let mut slots = vec![0u64; size];
        let mask = slots.len() - 1;
        for (gid, &hash) in self.hashes.iter().enumerate().skip(self.since) {
            let mut at = (hash as usize) & mask;
            while slots[at] != 0 {
                at = (at + 1) & mask;
            }
            slots[at] = (hash << 32) | (gid as u64 + 1);
        }
        self.slots = slots;
    }
}

/// Asks for the cache line at `address` ahead of reading it.
#[inline]
fn prefetch(address: usize) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a prefetch is a hint that does not fault, whatever the address.
    unsafe {
        std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(address as *const i8);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = address;
}

/// Reads a `u128` from sixteen bytes.
#[must_use]
pub fn read_u128(b: &[u8]) -> u128 {
    let mut a = [0u8; 16];
    a.copy_from_slice(&b[..16]);
    u128::from_le_bytes(a)
}

/// The distinct values of one `COUNT(DISTINCT x)`, per group.
///
/// Every group shares one open addressing table of (group, value) pairs, so a new group costs
/// nothing and a lookup is one probe sequence over a flat array. A string is copied into an arena
/// the set owns, and an entry keeps where it is.
#[derive(Debug, Default)]
pub struct Distinct {
    ints: Pairs,
    texts: Pairs,
    arena: Vec<u8>,
    counts: Vec<u64>,
}

/// One table of pairs. A slot is empty at zero, and otherwise holds the top half of the hash and
/// one more than the entry's index.
#[derive(Debug, Default)]
struct Pairs {
    slots: Vec<u64>,
    entries: Vec<Pair>,
}

/// A group and a value. A string's value is where its bytes are in the arena, the offset in the
/// low half and the length in the high half.
#[derive(Clone, Copy, Debug)]
struct Pair {
    gid: u64,
    value: u128,
    hash: u64,
}

const MIX: u64 = 0x9e37_79b9_7f4a_7c15;

fn mix(h: u64) -> u64 {
    let h = (h ^ (h >> 32)).wrapping_mul(MIX);
    h ^ (h >> 29)
}

fn int_hash(gid: u64, v: u128) -> u64 {
    mix(gid.wrapping_mul(MIX) ^ (v as u64) ^ ((v >> 64) as u64).rotate_left(23))
}

fn text_hash(gid: u64, bytes: &[u8]) -> u64 {
    let mut h = gid.wrapping_mul(MIX) ^ (bytes.len() as u64);
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        let mut w = [0u8; 8];
        w.copy_from_slice(c);
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(MIX).rotate_left(29);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let mut w = [0u8; 8];
        w[..rest.len()].copy_from_slice(rest);
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(MIX).rotate_left(29);
    }
    mix(h)
}

impl Pairs {
    /// Finds the pair whose hash is `hash` and that `same` accepts, or says where to put it.
    fn find(&self, hash: u64, same: impl Fn(&Pair) -> bool) -> Result<(), usize> {
        let mask = self.slots.len() - 1;
        let tag = hash & !0xffff_ffff;
        let mut at = hash as usize & mask;
        loop {
            let slot = self.slots[at];
            if slot == 0 {
                return Err(at);
            }
            if slot & !0xffff_ffff == tag && same(&self.entries[(slot & 0xffff_ffff) as usize - 1])
            {
                return Ok(());
            }
            at = (at + 1) & mask;
        }
    }

    /// Makes room for one more pair.
    fn reserve(&mut self) {
        if (self.entries.len() + 1) * 8 <= self.slots.len() * 7 {
            return;
        }
        let size = (self.slots.len() * 2).max(64);
        self.slots = vec![0; size];
        let mask = size - 1;
        for (i, e) in self.entries.iter().enumerate() {
            let mut at = e.hash as usize & mask;
            while self.slots[at] != 0 {
                at = (at + 1) & mask;
            }
            self.slots[at] = (e.hash & !0xffff_ffff) | (i as u64 + 1);
        }
    }

    fn put(&mut self, at: usize, pair: Pair) {
        self.slots[at] = (pair.hash & !0xffff_ffff) | (self.entries.len() as u64 + 1);
        self.entries.push(pair);
    }
}

impl Distinct {
    /// An empty set for every group.
    #[must_use]
    pub fn new() -> Distinct {
        Distinct::default()
    }

    fn counted(&mut self, gid: u64) {
        let gid = gid as usize;
        if self.counts.len() <= gid {
            self.counts.resize(gid + 1, 0);
        }
        self.counts[gid] += 1;
    }

    /// Adds a number to the set of group `gid`.
    pub fn add_int(&mut self, gid: usize, v: u128) {
        let gid = gid as u64;
        self.ints.reserve();
        let hash = int_hash(gid, v);
        if let Err(at) = self.ints.find(hash, |p| p.gid == gid && p.value == v) {
            self.ints.put(at, Pair { gid, value: v, hash });
            self.counted(gid);
        }
    }

    /// Adds a string to the set of group `gid`.
    pub fn add_text(&mut self, gid: usize, v: &[u8]) {
        let gid = gid as u64;
        let hash = text_hash(gid, v);
        self.add_text_hashed(gid, v, hash);
    }

    fn add_text_hashed(&mut self, gid: u64, v: &[u8], hash: u64) {
        self.texts.reserve();
        let arena = &self.arena;
        let found = self.texts.find(hash, |p| p.gid == gid && Self::bytes(arena, p.value) == v);
        if let Err(at) = found {
            let value = (self.arena.len() as u128) | ((v.len() as u128) << 64);
            self.arena.extend_from_slice(v);
            self.texts.put(at, Pair { gid, value, hash });
            self.counted(gid);
        }
    }

    fn bytes(arena: &[u8], value: u128) -> &[u8] {
        let (at, len) = (value as u64 as usize, (value >> 64) as u32 as usize);
        &arena[at..at + len]
    }

    /// Folds the sets of `other` into these, the sets of its group `g` into those of `map[g]`.
    pub fn absorb(&mut self, other: Distinct, map: &[usize]) {
        let to = |gid: u64| map.get(gid as usize).map(|&g| g as u64);
        for p in &other.ints.entries {
            if let Some(gid) = to(p.gid) {
                self.add_int(gid as usize, p.value);
            }
        }
        for p in &other.texts.entries {
            if let Some(gid) = to(p.gid) {
                let v = Self::bytes(&other.arena, p.value);
                // A group that keeps its id keeps its hash.
                let hash = if gid == p.gid { p.hash } else { text_hash(gid, v) };
                self.add_text_hashed(gid, v, hash);
            }
        }
    }

    /// The pairs of these sets split into `1 << bits` parts by the hash of the value and the group
    /// `map` renames its group to, so that two sets split the same way put an equal pair in the
    /// same part. A number is named by where it is in the sets and a string by that plus the
    /// count of numbers, and a pair of a group `map` drops is left out.
    #[must_use]
    pub fn split(&self, map: &[usize], bits: u32) -> Vec<Vec<u32>> {
        let mut parts = vec![Vec::new(); 1 << bits];
        let shift = 64 - bits;
        let part = |hash: u64| if bits == 0 { 0 } else { (hash >> shift) as usize };
        for (i, p) in self.ints.entries.iter().enumerate() {
            if let Some(&gid) = map.get(p.gid as usize) {
                parts[part(int_hash(gid as u64, p.value))].push(i as u32);
            }
        }
        let ints = self.ints.entries.len();
        for (i, p) in self.texts.entries.iter().enumerate() {
            if let Some(&gid) = map.get(p.gid as usize) {
                let hash = text_hash(gid as u64, Self::bytes(&self.arena, p.value));
                parts[part(hash)].push((ints + i) as u32);
            }
        }
        parts
    }

    /// Counts the distinct pairs of one part: `picks` of each set, with its groups renamed by
    /// `map`, as [`split`](Distinct::split) named and renamed them. Each new pair adds one to the
    /// count of its group in `counts`, which the parts share.
    pub fn gather(sources: &[(&Distinct, &[usize], &[u32])], counts: &[AtomicU64]) {
        let most = sources.iter().map(|(_, _, picks)| picks.len()).sum::<usize>();
        let mut pairs = Pairs {
            slots: vec![0; (most * 8 / 7 + 1).next_power_of_two().max(64)],
            entries: Vec::with_capacity(most),
        };
        // Pairs of one group tend to come together, so their count is added once for the run.
        let mut run = (u64::MAX, 0u64);
        let mut add = |gid: u64| {
            if run.0 != gid {
                if run.1 != 0 {
                    counts[run.0 as usize].fetch_add(run.1, Relaxed);
                }
                run = (gid, 0);
            }
            run.1 += 1;
        };
        for (at, &(set, map, picks)) in sources.iter().enumerate() {
            let ints = set.ints.entries.len();
            for &pick in picks {
                let pick = pick as usize;
                if pick < ints {
                    let p = set.ints.entries[pick];
                    let gid = map[p.gid as usize] as u64;
                    let hash = int_hash(gid, p.value);
                    if let Err(slot) = pairs.find(hash, |q| q.gid == gid && q.value == p.value) {
                        pairs.put(slot, Pair { gid, value: p.value, hash });
                        add(gid);
                    }
                } else {
                    let p = set.texts.entries[pick - ints];
                    let gid = map[p.gid as usize] as u64;
                    let v = Self::bytes(&set.arena, p.value);
                    let hash = text_hash(gid, v);
                    // The value of a pair here says which set its bytes are in, above the length.
                    let value = p.value | ((at as u128) << 96);
                    let same = |q: &Pair| {
                        q.gid == gid && {
                            let from = sources[(q.value >> 96) as usize].0;
                            Self::bytes(&from.arena, q.value & !(u128::from(u32::MAX) << 96)) == v
                        }
                    };
                    if let Err(slot) = pairs.find(hash, same) {
                        pairs.put(slot, Pair { gid, value, hash });
                        add(gid);
                    }
                }
            }
        }
        if run.1 != 0 {
            counts[run.0 as usize].fetch_add(run.1, Relaxed);
        }
    }

    /// Sets that know only how many distinct values each group saw, which is all a merged set
    /// is read for.
    #[must_use]
    pub fn counted_only(counts: Vec<u64>) -> Distinct {
        Distinct { counts, ..Distinct::default() }
    }

    /// How many distinct values group `gid` saw.
    #[must_use]
    pub fn count(&self, gid: usize) -> usize {
        self.counts.get(gid).map_or(0, |&n| n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(v: u64, s: &[u8], null: bool) -> Vec<u8> {
        let mut k = vec![0u8; 26];
        k[..8].copy_from_slice(&v.to_le_bytes());
        k[9..25].copy_from_slice(&text::make(s).to_le_bytes());
        k[25] = u8::from(null);
        k
    }

    #[test]
    fn a_capped_table_forgets_and_a_merge_by_parts_folds_it_back() {
        let layout = Layout {
            keys: vec![KeyField { offset: 0, width: 8, text: false }],
            key_size: 9,
            init: vec![0; 8],
        };
        let mut t = GroupTable::new(layout.clone());
        t.cap(100);
        let mut heap = Heap::new();
        for round in 0..3u64 {
            for i in 0..150u64 {
                let k = (i + round * 50).to_le_bytes().into_iter().chain([0]).collect::<Vec<_>>();
                // SAFETY: the key is alive and has no strings.
                unsafe { t.insert(k.as_ptr().expose_provenance(), i + round * 50, &mut heap) };
            }
        }
        assert!(t.forgot());
        assert!(t.len() > 250);
        let mut parts = Vec::new();
        for pick in t.split(2) {
            let mut part = GroupTable::with_capacity(layout.clone(), pick.len());
            part.absorb_some(&t, &pick, None, false, |_, _| {});
            parts.push(part);
        }
        let merged = GroupTable::join(layout, parts);
        assert_eq!(merged.len(), 250);
        assert!(!merged.forgot());
    }

    #[test]
    fn limited_tables_make_groups_for_the_keys_they_agree_on_and_no_others() {
        let layout = Layout {
            keys: vec![KeyField { offset: 0, width: 8, text: false }],
            key_size: 9,
            init: vec![0; 8],
        };
        let agreed = Arc::new(Agreed::new(layout.clone(), 3));
        let (mut a, mut b) = (GroupTable::new(layout.clone()), GroupTable::new(layout));
        a.limit(Arc::clone(&agreed));
        b.limit(Arc::clone(&agreed));
        let mut heap = Heap::new();
        let mut put = |t: &mut GroupTable, v: u64| {
            let k = v.to_le_bytes().into_iter().chain([0]).collect::<Vec<_>>();
            // SAFETY: the key is alive and has no strings.
            unsafe { t.insert(k.as_ptr().expose_provenance(), v * 7, &mut heap) }
        };
        put(&mut a, 1);
        put(&mut b, 2);
        put(&mut a, 3);
        let dump = put(&mut b, 4);
        assert_eq!(put(&mut a, 5), put(&mut a, 6));
        assert_eq!(put(&mut b, 9), dump);
        put(&mut b, 1);
        let keys = |t: &GroupTable| {
            let mut keys: Vec<u8> = (0..t.len()).map(|g| t.row(g)[8]).collect();
            keys.sort_unstable();
            keys
        };
        assert_eq!(keys(&a), [1, 2, 3]);
        assert_eq!(keys(&b), [1, 2, 3]);
    }

    #[test]
    fn equal_keys_find_one_row_and_strings_compare_by_content() {
        let layout = Layout {
            keys: vec![
                KeyField { offset: 0, width: 8, text: false },
                KeyField { offset: 9, width: 16, text: true },
            ],
            key_size: 26,
            init: vec![0; 8],
        };
        let mut t = GroupTable::new(layout);
        let mut heap = Heap::new();
        let long_a = b"a string longer than twelve".to_vec();
        let long_b = long_a.clone();
        let ka = key(1, &long_a, false);
        let kb = key(1, &long_b, false);
        let kc = key(2, &long_a, false);
        let kn = key(1, b"", true);
        // SAFETY: the keys are alive and their strings point at live vectors.
        let (a, b, c, n) = unsafe {
            (
                t.insert(ka.as_ptr().expose_provenance(), 7, &mut heap),
                t.insert(kb.as_ptr().expose_provenance(), 7, &mut heap),
                t.insert(kc.as_ptr().expose_provenance(), 9, &mut heap),
                t.insert(kn.as_ptr().expose_provenance(), 7, &mut heap),
            )
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, n);
        assert_eq!(t.len(), 3);
        for i in 0..5000u64 {
            let k = key(i + 10, b"x", false);
            // SAFETY: as above.
            unsafe { t.insert(k.as_ptr().expose_provenance(), i.wrapping_mul(0x9e37), &mut heap) };
        }
        assert_eq!(t.len(), 5003);
        // SAFETY: as above.
        let again = unsafe { t.insert(ka.as_ptr().expose_provenance(), 7, &mut heap) };
        assert_eq!(again, a);
    }

    #[test]
    fn a_scalar_table_has_its_group_before_any_row() {
        let t = GroupTable::new(Layout { init: vec![0; 16], ..Layout::default() });
        assert_eq!(t.len(), 1);
    }
}

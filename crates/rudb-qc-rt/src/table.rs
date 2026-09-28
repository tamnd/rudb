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

/// How many bits of a key's hash pick the lane a table that is merged by parts puts its row in.
pub const LANE_BITS: u32 = 8;

/// A table with lanes whose slots had a group for fewer than one key in this many of those it was
/// asked for before they were emptied makes a row for each of the next [`BLIND_TABLES`] times
/// [`cap`](GroupTable::cap) keys without looking for it. The merge folds the rows of a key that
/// has more than one, as it does for the keys the slots forgot, and the table looks again after,
/// in case the keys changed.
const MISSES: usize = 5;
const BLIND_TABLES: usize = 8;

/// The most rows a page of a lane holds. The first page of a lane holds [`FIRST_LANE_ROWS`] and
/// each one after it twice as many as the one before, so a table with few groups does not zero a
/// full page for every lane.
const LANE_ROWS: usize = 256;
const FIRST_LANE_ROWS: usize = 16;

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
    /// The rows by the top bits of their hashes, for a table that is merged by parts, which then
    /// folds each lane of every worker as one part and reads its rows in the order they were made.
    /// Empty when the rows go in `pages`. A row in a lane starts with its hash and not its group id.
    lanes: Vec<Lane>,
    /// How many groups a table with lanes made before its slots were last emptied, which it no
    /// longer has an address for.
    gone: usize,
    /// How many keys a table with lanes was asked for since its slots were last emptied.
    asked: usize,
    /// How many more keys a table with lanes makes a row for without looking for them, because
    /// most of the keys it was asked for before were new.
    blind: usize,
}

/// The rows of one lane.
#[derive(Debug, Default)]
struct Lane {
    pages: Vec<Box<[u8]>>,
    /// How many rows of the last page are taken.
    fill: usize,
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
            lanes: Vec::new(),
            gone: 0,
            asked: 0,
            blind: 0,
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

    /// Puts the rows of the groups made from now on in lanes by their hashes, for a worker whose
    /// groups are merged by parts after the scan, and forgets the rows it has no slot for once
    /// [`cap`](GroupTable::cap) empties them. The merge then folds a lane of every table with
    /// [`absorb_lane`](GroupTable::absorb_lane), and has no use for a group id in a row, so a table
    /// with a distinct set keeps its rows in pages.
    pub fn lanes(&mut self) {
        self.lanes = (0..1 << LANE_BITS).map(|_| Lane::default()).collect();
    }

    /// Whether the rows are in lanes.
    #[must_use]
    pub fn laned(&self) -> bool {
        !self.lanes.is_empty()
    }

    /// How many rows lane `lane` holds.
    #[must_use]
    pub fn lane_len(&self, lane: usize) -> usize {
        self.lanes.get(lane).map_or(0, |l| {
            let rows = l.pages.iter().map(|p| p.len() / self.row_size).sum::<usize>();
            let last = l.pages.last().map_or(0, |p| p.len() / self.row_size);
            rows - last + l.fill
        })
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
        self.since != 0 || self.gone != 0
    }

    /// The shape of the rows.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// How many groups there are, counting those a table with lanes forgot the rows of.
    #[must_use]
    pub fn len(&self) -> usize {
        self.gone + self.rows.len()
    }

    /// Whether there are no groups.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
        if self.blind != 0 {
            self.blind -= 1;
            self.gone += 1;
            let address = self.room(hash);
            self.write_row(address, key, hash, Some(heap));
            return address;
        }
        self.asked += 1;
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
    /// does not have yet keeps the row of `other` where it is, which is right when combining a row
    /// into a new group's row gives that row back, and which needs whoever
    /// [took](GroupTable::take_pages) `other`'s pages to keep them as long as this table.
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
                    let gid = if whole {
                        self.rows.push(row);
                        self.hashes.push(hash);
                        self.rows.len() - 1
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

    /// Folds the rows of lane `lane` of `other` into this table, as
    /// [`absorb_some`](GroupTable::absorb_some) does with `whole`, which a lane is only made for.
    /// The rows are read in the order `other` made them, and the ones of a key this table does not
    /// have yet stay where they are, so whoever [took](GroupTable::take_pages) `other`'s pages
    /// keeps them as long as this table. `seen` gets the group id and the row of every group a row
    /// of the lane made or was combined into, right after, while the row is in the cache.
    pub fn absorb_lane(
        &mut self,
        other: &GroupTable,
        lane: usize,
        mut combine: impl FnMut(&mut [u8], &[u8]),
        mut seen: impl FnMut(usize, &[u8]),
    ) {
        /// How many rows ahead a slot is asked for, and a row twice as far.
        const AHEAD: usize = 8;
        let Some(from) = other.lanes.get(lane) else { return };
        let size = self.layout.key_size as usize;
        let last = from.pages.len().saturating_sub(1);
        if self.slots.is_empty() {
            self.slots = vec![0; 64];
        }
        for (i, page) in from.pages.iter().enumerate() {
            let rows = if i == last { from.fill } else { page.len() / other.row_size };
            let base = page.as_ptr().addr();
            let at = |r: usize| base + r * other.row_size;
            for r in 0..rows {
                // The rows are read in order and their slots are not, so the slot of a row a few
                // ahead is asked for with the hash at its front, which was asked for before that.
                if r + 2 * AHEAD < rows {
                    prefetch(at(r + 2 * AHEAD));
                }
                if r + AHEAD < rows {
                    // SAFETY: as for `src` below.
                    let front = unsafe { crate::mem::slice(at(r + AHEAD), 8) };
                    let hash = u64::from_le_bytes(front.try_into().unwrap_or_default());
                    let slot = (hash as usize) & (self.slots.len() - 1);
                    prefetch(self.slots.as_ptr().wrapping_add(slot).addr());
                }
                let row = at(r);
                // SAFETY: the first `rows` rows of the page are rows `other` made, and `other` is
                // borrowed, so the page is still there.
                let src = unsafe { crate::mem::slice(row, other.row_size) };
                let hash = u64::from_le_bytes(src[..8].try_into().unwrap_or_default());
                match self.probe(&src[8..8 + size], hash) {
                    Ok(gid) => {
                        combine(self.row_mut(gid), src);
                        seen(gid, self.row(gid));
                    }
                    Err(slot) => {
                        self.rows.push(row);
                        self.hashes.push(hash);
                        let gid = self.rows.len() - 1;
                        self.place(slot, gid, hash);
                        seen(gid, src);
                    }
                }
            }
        }
    }

    /// Takes the pages this table's rows are in, for a merge whose parts keep the rows where they
    /// are. The rows can still be read and written through their addresses as long as the caller
    /// keeps the pages, and a row the table makes after this goes in a page of its own.
    pub fn take_pages(&mut self) -> Vec<Box<[u8]>> {
        self.fill = ROWS_PER_PAGE;
        let mut pages = std::mem::take(&mut self.pages);
        for lane in &mut self.lanes {
            pages.append(&mut lane.pages);
            lane.fill = 0;
        }
        pages
    }

    /// Keeps `pages`, which [`take_pages`](GroupTable::take_pages) took from the tables whose rows
    /// this one holds, for as long as this table.
    pub fn keep(&mut self, mut pages: Vec<Box<[u8]>>) {
        self.pages.append(&mut pages);
        self.fill = ROWS_PER_PAGE;
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
            lanes: Vec::new(),
            gone: 0,
            asked: 0,
            blind: 0,
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
            if !self.lanes.is_empty() && self.asked.saturating_sub(self.cap) * MISSES < self.asked {
                self.blind = self.cap * BLIND_TABLES;
            }
            self.asked = 0;
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

    /// The address of a row no group has yet, at the end of the last page, or of the last page of
    /// the lane of `hash`.
    fn room(&mut self, hash: u64) -> usize {
        if !self.lanes.is_empty() {
            let at = (hash.wrapping_mul(SPREAD) >> (64 - LANE_BITS)) as usize;
            let lane = &mut self.lanes[at];
            let room = lane.pages.last().map_or(0, |p| p.len() / self.row_size);
            if lane.fill == room {
                let rows = (room * 2).clamp(FIRST_LANE_ROWS, LANE_ROWS);
                lane.pages.push(vec![0u8; rows * self.row_size].into_boxed_slice());
                lane.fill = 0;
            }
            let at = lane.fill * self.row_size;
            lane.fill += 1;
            return lane
                .pages
                .last_mut()
                .map_or(0, |page| page[at..].as_mut_ptr().expose_provenance());
        }
        if self.fill == ROWS_PER_PAGE || self.pages.is_empty() {
            self.pages.push(vec![0u8; ROWS_PER_PAGE * self.row_size].into_boxed_slice());
            self.fill = 0;
        }
        let at = self.fill * self.row_size;
        self.fill += 1;
        self.pages.last_mut().map_or(0, |page| page[at..].as_mut_ptr().expose_provenance())
    }

    fn add(&mut self, key: &[u8], hash: u64, heap: Option<&mut Heap>) -> usize {
        if !self.lanes.is_empty() && self.since != 0 {
            // The rows the slots forgot are only read again by the merge, from their lanes.
            self.gone += self.rows.len();
            self.rows.clear();
            self.hashes.clear();
            self.since = 0;
        }
        let gid = self.rows.len();
        let address = self.room(hash);
        let front = if self.lanes.is_empty() { gid as u64 } else { hash };
        self.write_row(address, key, front, heap);
        self.rows.push(address);
        self.hashes.push(hash);
        gid
    }

    /// Writes a new group's row at `address`, which [`room`](GroupTable::room) gave, starting with
    /// `front`.
    fn write_row(&self, address: usize, key: &[u8], front: u64, mut heap: Option<&mut Heap>) {
        let acc = Layout::acc_offset(self.layout.key_size) as usize;
        // SAFETY: `room` gave a row of `row_size` bytes in a page this table owns, and no group
        // points at it yet.
        let row = unsafe {
            std::slice::from_raw_parts_mut(
                std::ptr::with_exposed_provenance_mut::<u8>(address),
                self.row_size,
            )
        };
        let layout = &self.layout;
        row[..8].copy_from_slice(&front.to_le_bytes());
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
/// nothing and a lookup is one probe sequence over a flat array. A number that fits in 64 bits is
/// kept in the slot itself, so a lookup touches one line. A wider one, or a string, is kept in an
/// entry the slot points at. A string is copied into an arena the set owns, and an entry keeps
/// where it is.
#[derive(Debug, Default)]
pub struct Distinct {
    narrow: Narrow,
    spill: Option<Spill>,
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

/// Pairs of a group and a number that fits in 64 bits. A slot holds the number and one more than
/// the group, and is empty when that is zero. The table is at most half full, because a number
/// has no tag to tell a slot apart by before it is compared.
#[derive(Debug, Default)]
struct Narrow {
    slots: Vec<[u64; 2]>,
    len: usize,
}

impl Narrow {
    /// A table with room for `n` pairs.
    fn with_room(n: usize) -> Narrow {
        Narrow { slots: vec![[0; 2]; (n * 2).next_power_of_two().max(64)], len: 0 }
    }

    /// Adds the pair of group `gid` and number `v`, whose hash is `hash`, and says whether it
    /// is new.
    fn add(&mut self, gid: u64, v: u64, hash: u64) -> bool {
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mask = self.slots.len() - 1;
        let pair = [v, gid + 1];
        let mut at = hash as usize & mask;
        loop {
            let slot = self.slots[at];
            if slot[1] == 0 {
                self.slots[at] = pair;
                self.len += 1;
                return true;
            }
            if slot == pair {
                return false;
            }
            at = (at + 1) & mask;
        }
    }

    fn grow(&mut self) {
        let size = (self.slots.len() * 2).max(64);
        let old = std::mem::replace(&mut self.slots, vec![[0; 2]; size]);
        let mask = size - 1;
        for pair in old.into_iter().filter(|p| p[1] != 0) {
            let mut at = narrow_hash(pair[1] - 1, pair[0]) as usize & mask;
            while self.slots[at][1] != 0 {
                at = (at + 1) & mask;
            }
            self.slots[at] = pair;
        }
    }

    /// Every pair, as the group and the number.
    fn pairs(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.slots.iter().filter(|p| p[1] != 0).map(|p| (p[1] - 1, p[0]))
    }
}

/// A set to gather one part of: the set, how its groups are renamed, and its pairs and picks of
/// the part from [`Distinct::split`].
pub type SetPart<'a> = (&'a Distinct, &'a [usize], &'a [[u64; 2]], &'a [u32]);

/// One set's pairs split into parts by [`Distinct::split`].
#[derive(Debug)]
pub struct Split {
    /// The numbers that fit in 64 bits of each part, as the number and the renamed group.
    pub pairs: Vec<Vec<[u64; 2]>>,
    /// Where the other values of each part are in the set.
    pub picks: Vec<Vec<u32>>,
}

/// Pairs of a group and a number that fits in 64 bits, kept as they come instead of looked up,
/// for a worker's sets that only the merge counts. Looking a pair up in a table as big as all of
/// a worker's pairs misses the cache on almost every row, where adding it to the end does not,
/// and the merge splits the pairs into parts small enough to find the repeats in the cache.
///
/// A pair the same as the last one in its slot of `recent` is left out, which catches most of
/// the repeats of a set with few values. When the pairs get to `squeeze` they are split the same
/// way and the repeats taken out, so a set of many repeats that `recent` misses does not grow
/// without end.
#[derive(Debug)]
struct Spill {
    pairs: Vec<[u64; 2]>,
    recent: Vec<[u64; 2]>,
    squeeze: usize,
}

/// The slots of [`Spill::recent`].
const RECENT: usize = 1 << 10;

/// The fewest pairs a spill takes the repeats out of.
const SQUEEZE: usize = 1 << 20;

impl Spill {
    fn new() -> Spill {
        Spill { pairs: Vec::new(), recent: vec![[0; 2]; RECENT], squeeze: SQUEEZE }
    }

    fn add(&mut self, gid: u64, v: u64, hash: u64) {
        let pair = [v, gid + 1];
        let last = &mut self.recent[hash as usize & (RECENT - 1)];
        if *last == pair {
            return;
        }
        *last = pair;
        self.pairs.push([v, gid]);
        if self.pairs.len() >= self.squeeze {
            self.squeeze();
        }
    }

    fn squeeze(&mut self) {
        const BITS: u32 = 8;
        let mut parts = vec![Vec::new(); 1 << BITS];
        for &[v, gid] in &self.pairs {
            parts[(narrow_hash(gid, v) >> (64 - BITS)) as usize].push([v, gid]);
        }
        self.pairs.clear();
        for part in parts {
            let mut seen = Narrow::with_room(part.len());
            self.pairs
                .extend(part.into_iter().filter(|&[v, gid]| seen.add(gid, v, narrow_hash(gid, v))));
        }
        self.squeeze = (self.pairs.len() * 2).max(SQUEEZE);
    }
}

/// One part's new pairs per group, on their way to the counts every part adds to.
///
/// With few groups every part adds to the same few counts, and the threads would take their lines
/// from each other on almost every pair, so a part counts its own and adds them at the end. With
/// many, the pairs of one group tend to come together, so a run of them is added at once.
enum Tally {
    Own { counts: Vec<u64>, seen: Vec<u32> },
    Run(u64, u64),
}

/// The most groups a part counts on its own.
const OWN_GROUPS: usize = 1 << 16;

impl Tally {
    fn new(groups: usize) -> Tally {
        if groups <= OWN_GROUPS {
            Tally::Own { counts: vec![0; groups], seen: Vec::new() }
        } else {
            Tally::Run(u64::MAX, 0)
        }
    }

    fn add(&mut self, gid: u64, to: &[AtomicU64]) {
        match self {
            Tally::Own { counts, seen } => {
                let n = &mut counts[gid as usize];
                if *n == 0 {
                    seen.push(gid as u32);
                }
                *n += 1;
            }
            Tally::Run(run, n) => {
                if *run != gid {
                    if *n != 0 {
                        to[*run as usize].fetch_add(*n, Relaxed);
                    }
                    (*run, *n) = (gid, 0);
                }
                *n += 1;
            }
        }
    }

    fn add_to(self, to: &[AtomicU64]) {
        match self {
            Tally::Own { counts, seen } => {
                for gid in seen {
                    to[gid as usize].fetch_add(counts[gid as usize], Relaxed);
                }
            }
            Tally::Run(run, n) => {
                if n != 0 {
                    to[run as usize].fetch_add(n, Relaxed);
                }
            }
        }
    }
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

fn narrow_hash(gid: u64, v: u64) -> u64 {
    mix(gid.wrapping_mul(MIX) ^ v)
}

/// The number `v` as 64 bits, if it is a sign extended `i64`, which is how compiled code passes
/// every signed number up to that wide.
fn narrow(v: u128) -> Option<u64> {
    let x = v as u64;
    (x as i64 as i128 as u128 == v).then_some(x)
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

    /// Keeps the numbers that fit in 64 bits as they come from here on, for a set whose count
    /// only [`gather`](Distinct::gather) reads. [`count`](Distinct::count) does not see them.
    pub fn spill(&mut self) {
        self.spill.get_or_insert_with(Spill::new);
    }

    /// How many pairs the sets hold, some of them the same when they spill.
    #[must_use]
    pub fn held(&self) -> usize {
        let spilled = self.spill.as_ref().map_or(0, |s| s.pairs.len());
        self.narrow.len + spilled + self.ints.entries.len() + self.texts.entries.len()
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
        if let Some(x) = narrow(v) {
            let hash = narrow_hash(gid, x);
            if let Some(spill) = &mut self.spill {
                spill.add(gid, x, hash);
            } else if self.narrow.add(gid, x, hash) {
                self.counted(gid);
            }
            return;
        }
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
        let spilled = other.spill.iter().flat_map(|s| s.pairs.iter().map(|&[v, gid]| (gid, v)));
        for (gid, v) in other.narrow.pairs().chain(spilled) {
            if let Some(gid) = to(gid) {
                self.add_int(gid as usize, v as i64 as i128 as u128);
            }
        }
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
    /// same part, and a pair of a group `map` drops is left out. A number that fits in 64 bits is
    /// copied into its part with its new group, as the number and the group. Any other value is
    /// named by where it is in the sets, a number by its place and a string by that plus the
    /// count of numbers.
    #[must_use]
    pub fn split(&self, map: &[usize], bits: u32) -> Split {
        let mut split =
            Split { pairs: vec![Vec::new(); 1 << bits], picks: vec![Vec::new(); 1 << bits] };
        let shift = 64 - bits;
        let part = |hash: u64| if bits == 0 { 0 } else { (hash >> shift) as usize };
        let spilled = self.spill.iter().flat_map(|s| s.pairs.iter().map(|&[v, gid]| (gid, v)));
        for (gid, v) in self.narrow.pairs().chain(spilled) {
            if let Some(&gid) = map.get(gid as usize) {
                let gid = gid as u64;
                split.pairs[part(narrow_hash(gid, v))].push([v, gid]);
            }
        }
        for (i, p) in self.ints.entries.iter().enumerate() {
            if let Some(&gid) = map.get(p.gid as usize) {
                split.picks[part(int_hash(gid as u64, p.value))].push(i as u32);
            }
        }
        let ints = self.ints.entries.len();
        for (i, p) in self.texts.entries.iter().enumerate() {
            if let Some(&gid) = map.get(p.gid as usize) {
                let hash = text_hash(gid as u64, Self::bytes(&self.arena, p.value));
                split.picks[part(hash)].push((ints + i) as u32);
            }
        }
        split
    }

    /// Counts the distinct pairs of one part: the pairs and `picks` of each set, with its groups
    /// renamed by `map`, as [`split`](Distinct::split) named and renamed them. Each new pair adds
    /// one to the count of its group in `counts`, which the parts share.
    pub fn gather(sources: &[SetPart<'_>], counts: &[AtomicU64]) {
        let most = sources.iter().map(|(_, _, _, picks)| picks.len()).sum::<usize>();
        let mut pairs = Pairs {
            slots: vec![0; (most * 8 / 7 + 1).next_power_of_two().max(64)],
            entries: Vec::with_capacity(most),
        };
        let narrow = sources.iter().map(|(_, _, pairs, _)| pairs.len()).sum::<usize>();
        let mut seen = Narrow::with_room(narrow);
        let mut tally = Tally::new(counts.len());
        let mut add = |gid: u64| tally.add(gid, counts);
        for &(_, _, given, _) in sources {
            // The slot of the pair a few ahead is asked for early, since the pairs come in no
            // order of their slots.
            const AHEAD: usize = 8;
            let mask = seen.slots.len() - 1;
            for (i, &[v, gid]) in given.iter().enumerate() {
                if let Some(&[w, g]) = given.get(i + AHEAD) {
                    let at = narrow_hash(g, w) as usize & mask;
                    prefetch(std::ptr::from_ref(&seen.slots[at]).addr());
                }
                if seen.add(gid, v, narrow_hash(gid, v)) {
                    add(gid);
                }
            }
        }
        for (at, &(set, map, _, picks)) in sources.iter().enumerate() {
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
        tally.add_to(counts);
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
    fn a_merge_that_keeps_the_rows_where_they_are_folds_every_count() {
        let layout = Layout {
            keys: vec![KeyField { offset: 0, width: 8, text: false }],
            key_size: 9,
            init: vec![0; 8],
        };
        let acc = Layout::acc_offset(9) as usize;
        let mut t = GroupTable::new(layout.clone());
        t.cap(100);
        let mut heap = Heap::new();
        for round in 0..3u64 {
            for i in 0..150u64 {
                let k = (i + round * 50).to_le_bytes().into_iter().chain([0]).collect::<Vec<_>>();
                // SAFETY: the key is alive and has no strings, and the row is the table's.
                unsafe {
                    let row = t.insert(k.as_ptr().expose_provenance(), i + round * 50, &mut heap);
                    let count = std::ptr::with_exposed_provenance_mut::<u64>(row + acc);
                    count.write_unaligned(count.read_unaligned() + 1);
                }
            }
        }
        let pages = t.take_pages();
        let add = |d: &mut [u8], s: &[u8]| {
            let n = |b: &[u8]| u64::from_le_bytes(b[acc..acc + 8].try_into().unwrap_or_default());
            let total = n(d) + n(s);
            d[acc..acc + 8].copy_from_slice(&total.to_le_bytes());
        };
        let mut parts = Vec::new();
        for pick in t.split(2) {
            let mut part = GroupTable::with_capacity(layout.clone(), pick.len());
            part.absorb_some(&t, &pick, None, true, add);
            parts.push(part);
        }
        drop(t);
        let mut merged = GroupTable::join(layout, parts);
        merged.keep(pages);
        let counts: Vec<u64> = (0..merged.len())
            .map(|gid| u64::from_le_bytes(merged.row(gid)[acc..acc + 8].try_into().unwrap()))
            .collect();
        assert_eq!(counts.len(), 250);
        assert_eq!(counts.iter().sum::<u64>(), 450);
    }

    #[test]
    fn tables_with_lanes_forget_their_rows_and_a_merge_by_lanes_folds_every_count() {
        let layout = Layout {
            keys: vec![KeyField { offset: 0, width: 8, text: false }],
            key_size: 9,
            init: vec![0; 8],
        };
        let acc = Layout::acc_offset(9) as usize;
        let mut heap = Heap::new();
        let mut tables = Vec::new();
        for first in [0u64, 100] {
            let mut t = GroupTable::new(layout.clone());
            t.cap(100);
            t.lanes();
            for round in 0..3u64 {
                for i in 0..150u64 {
                    let v = first + i + round * 50;
                    let k = v.to_le_bytes().into_iter().chain([0]).collect::<Vec<_>>();
                    // SAFETY: the key is alive and has no strings, and the row is the table's.
                    unsafe {
                        let row = t.insert(k.as_ptr().expose_provenance(), v * 7, &mut heap);
                        let count = std::ptr::with_exposed_provenance_mut::<u64>(row + acc);
                        count.write_unaligned(count.read_unaligned() + 1);
                    }
                }
            }
            assert!(t.forgot());
            let made: usize = (0..1 << LANE_BITS).map(|lane| t.lane_len(lane)).sum();
            assert_eq!(made, t.len());
            tables.push(t);
        }
        let add = |d: &mut [u8], s: &[u8]| {
            let n = |b: &[u8]| u64::from_le_bytes(b[acc..acc + 8].try_into().unwrap_or_default());
            let total = n(d) + n(s);
            d[acc..acc + 8].copy_from_slice(&total.to_le_bytes());
        };
        let mut counts = Vec::new();
        for lane in 0..1 << LANE_BITS {
            let mut part = GroupTable::new(layout.clone());
            for t in &tables {
                part.absorb_lane(t, lane, add, |_, _| {});
            }
            for gid in 0..part.len() {
                let row = part.row(gid);
                counts.push(u64::from_le_bytes(row[acc..acc + 8].try_into().unwrap()));
            }
        }
        assert_eq!(counts.len(), 350);
        assert_eq!(counts.iter().sum::<u64>(), 900);
    }

    #[test]
    fn a_table_with_lanes_that_finds_no_key_stops_looking_and_the_merge_folds_every_count() {
        let layout = Layout {
            keys: vec![KeyField { offset: 0, width: 8, text: false }],
            key_size: 9,
            init: vec![0; 8],
        };
        let acc = Layout::acc_offset(9) as usize;
        let mut heap = Heap::new();
        let mut t = GroupTable::new(layout.clone());
        t.cap(100);
        t.lanes();
        // Every key is new at first, then the first hundred come back three times.
        let keys = (0..500u64).chain((0..300).map(|i| i % 100));
        for v in keys {
            let k = v.to_le_bytes().into_iter().chain([0]).collect::<Vec<_>>();
            // SAFETY: the key is alive and has no strings, and the row is the table's.
            unsafe {
                let row = t.insert(k.as_ptr().expose_provenance(), v * 7, &mut heap);
                let count = std::ptr::with_exposed_provenance_mut::<u64>(row + acc);
                count.write_unaligned(count.read_unaligned() + 1);
            }
        }
        // Past the first hundred keys no row was looked for, so each key that came back got more.
        assert_eq!(t.len(), 800);
        let add = |d: &mut [u8], s: &[u8]| {
            let n = |b: &[u8]| u64::from_le_bytes(b[acc..acc + 8].try_into().unwrap_or_default());
            let total = n(d) + n(s);
            d[acc..acc + 8].copy_from_slice(&total.to_le_bytes());
        };
        let mut counts = Vec::new();
        for lane in 0..1 << LANE_BITS {
            let mut part = GroupTable::new(layout.clone());
            part.absorb_lane(&t, lane, add, |_, _| {});
            for gid in 0..part.len() {
                let row = part.row(gid);
                let key = u64::from_le_bytes(row[8..16].try_into().unwrap());
                counts.push((key, u64::from_le_bytes(row[acc..acc + 8].try_into().unwrap())));
            }
        }
        counts.sort_unstable();
        let want: Vec<(u64, u64)> = (0..500).map(|v| (v, if v < 100 { 4 } else { 1 })).collect();
        assert_eq!(counts, want);
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

    #[test]
    fn spilled_sets_count_what_a_set_of_every_pair_counts() {
        // Numbers that repeat far apart, so that `recent` misses them, some that do not fit in 64
        // bits, and more pairs than a spill keeps before it squeezes them.
        let value = |i: u64| {
            if i % 1000 == 7 {
                u128::MAX - u128::from(i % 5)
            } else {
                ((i * 7919 % 50_000) as i64 - 25_000) as i128 as u128
            }
        };
        // The second worker has its groups in the other order.
        let maps = [vec![0, 1, 2], vec![2, 1, 0]];
        let mut want = std::collections::HashSet::new();
        let mut sets = [Distinct::new(), Distinct::new()];
        for (w, set) in sets.iter_mut().enumerate() {
            set.spill();
            for i in 0..SQUEEZE as u64 + 5000 {
                let (gid, v) = ((i % 3) as usize, value(i + w as u64 * 3));
                set.add_int(gid, v);
                want.insert((maps[w][gid], v));
            }
        }
        assert!(sets[0].held() < SQUEEZE, "the squeeze took the repeats out");
        let bits = 4;
        let splits: Vec<Split> = sets.iter().zip(&maps).map(|(d, m)| d.split(m, bits)).collect();
        let counts: Vec<AtomicU64> = (0..3).map(|_| AtomicU64::new(0)).collect();
        for part in 0..1 << bits {
            let sources: Vec<SetPart<'_>> = sets
                .iter()
                .zip(&maps)
                .zip(&splits)
                .map(|((d, m), s)| {
                    (d, m.as_slice(), s.pairs[part].as_slice(), s.picks[part].as_slice())
                })
                .collect();
            Distinct::gather(&sources, &counts);
        }
        let got: Vec<u64> = counts.iter().map(|c| c.load(Relaxed)).collect();
        let want: Vec<u64> =
            (0..3).map(|g| want.iter().filter(|&&(gid, _)| gid == g).count() as u64).collect();
        assert_eq!(got, want);
    }
}

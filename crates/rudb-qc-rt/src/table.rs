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

use std::collections::HashSet;

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
    /// Open addressing over group ids plus one, zero for empty, and empty until the first insert
    /// in a table made by [`join`](GroupTable::join).
    slots: Vec<u32>,
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
        };
        if table.layout.keys.is_empty() {
            table.add(&[], 0, None);
        }
        table
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
        let gid = self.find_or_add(key, hash, Some(heap));
        self.rows[gid]
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

    /// The groups of this table split by the top `bits` bits of their hashes, so that two tables
    /// split the same way put a key in the same part.
    #[must_use]
    pub fn split(&self, bits: u32) -> Vec<Vec<u32>> {
        let mut parts = vec![Vec::new(); 1 << bits];
        let shift = 64 - bits;
        for (gid, &hash) in self.hashes.iter().enumerate() {
            let part = if bits == 0 { 0 } else { (hash >> shift) as usize };
            parts[part].push(gid as u32);
        }
        parts
    }

    /// Folds the groups `gids` of `other` into this table, as [`absorb`](GroupTable::absorb) does
    /// with all of them, and with nothing kept by group id outside the rows.
    pub fn absorb_some(
        &mut self,
        other: &GroupTable,
        gids: &[u32],
        mut combine: impl FnMut(&mut [u8], &[u8]),
    ) {
        let size = self.layout.key_size as usize;
        for &gid in gids {
            let src = other.row(gid as usize);
            let at = self.find_or_add(&src[8..8 + size], other.hashes[gid as usize], None);
            combine(self.row_mut(at), src);
        }
    }

    /// One table of the groups of `parts`, which have the layout `layout` and no key in common,
    /// in the order they come. The rows do not move, so the group id at the front of one is the
    /// one it had in its part, which only a distinct set reads, and a table with distinct sets is
    /// folded with [`absorb`](GroupTable::absorb) instead.
    #[must_use]
    pub fn join(layout: Layout, parts: Vec<GroupTable>) -> GroupTable {
        let n = parts.iter().map(GroupTable::len).sum();
        let mut table = GroupTable {
            row_size: layout.row_size(),
            layout,
            pages: Vec::new(),
            fill: ROWS_PER_PAGE,
            rows: Vec::with_capacity(n),
            hashes: Vec::with_capacity(n),
            slots: Vec::new(),
        };
        for part in parts {
            table.pages.extend(part.pages);
            table.rows.extend(part.rows);
            table.hashes.extend(part.hashes);
        }
        table
    }

    fn row_mut(&mut self, gid: usize) -> &mut [u8] {
        let at = std::ptr::with_exposed_provenance_mut::<u8>(self.rows[gid]);
        // SAFETY: as in `row`, and `&mut self` means nothing else is reading it.
        unsafe { std::slice::from_raw_parts_mut(at, self.row_size) }
    }

    /// The group id of `key`, made if it is new. A long string in a new key is copied into `heap`
    /// when there is one.
    fn find_or_add(&mut self, key: &[u8], hash: u64, heap: Option<&mut Heap>) -> usize {
        if self.slots.is_empty() {
            self.slots = vec![0; 64];
            self.grow();
        }
        let mask = self.slots.len() - 1;
        let mut at = (hash as usize) & mask;
        loop {
            let slot = self.slots[at];
            if slot == 0 {
                break;
            }
            let gid = (slot - 1) as usize;
            if self.hashes[gid] == hash && self.same(gid, key) {
                return gid;
            }
            at = (at + 1) & mask;
        }
        let gid = self.add(key, hash, heap);
        self.slots[at] = gid as u32 + 1;
        if self.rows.len() * 2 > self.slots.len() {
            self.grow();
        }
        gid
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

    fn add(&mut self, key: &[u8], hash: u64, mut heap: Option<&mut Heap>) -> usize {
        let gid = self.rows.len();
        if self.fill == ROWS_PER_PAGE {
            self.pages.push(vec![0u8; ROWS_PER_PAGE * self.row_size].into_boxed_slice());
            self.fill = 0;
        }
        let size = self.row_size;
        let acc = Layout::acc_offset(self.layout.key_size) as usize;
        let keys = self.layout.keys.clone();
        let init = self.layout.init.clone();
        let Some(page) = self.pages.last_mut() else { return 0 };
        let at = self.fill * size;
        self.fill += 1;
        let row = &mut page[at..at + size];
        row[..8].copy_from_slice(&(gid as u64).to_le_bytes());
        row[8..8 + key.len()].copy_from_slice(key);
        for f in &keys {
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
        row[acc..acc + init.len()].copy_from_slice(&init);
        self.rows.push(row.as_mut_ptr().expose_provenance());
        self.hashes.push(hash);
        gid
    }

    fn grow(&mut self) {
        let mut size = self.slots.len() * 2;
        while self.rows.len() * 2 > size {
            size *= 2;
        }
        let mut slots = vec![0u32; size];
        let mask = slots.len() - 1;
        for (gid, hash) in self.hashes.iter().enumerate() {
            let mut at = (*hash as usize) & mask;
            while slots[at] != 0 {
                at = (at + 1) & mask;
            }
            slots[at] = gid as u32 + 1;
        }
        self.slots = slots;
    }
}

/// Reads a `u128` from sixteen bytes.
#[must_use]
pub fn read_u128(b: &[u8]) -> u128 {
    let mut a = [0u8; 16];
    a.copy_from_slice(&b[..16]);
    u128::from_le_bytes(a)
}

/// The distinct values of one `COUNT(DISTINCT x)`, per group.
#[derive(Debug, Default)]
pub struct Distinct {
    ints: Vec<HashSet<u128>>,
    texts: Vec<HashSet<Box<[u8]>>>,
}

impl Distinct {
    /// An empty set for every group.
    #[must_use]
    pub fn new() -> Distinct {
        Distinct::default()
    }

    /// Adds a number to the set of group `gid`.
    pub fn add_int(&mut self, gid: usize, v: u128) {
        if self.ints.len() <= gid {
            self.ints.resize_with(gid + 1, HashSet::new);
        }
        self.ints[gid].insert(v);
    }

    /// Adds a string to the set of group `gid`.
    pub fn add_text(&mut self, gid: usize, v: &[u8]) {
        if self.texts.len() <= gid {
            self.texts.resize_with(gid + 1, HashSet::new);
        }
        if !self.texts[gid].contains(v) {
            self.texts[gid].insert(v.into());
        }
    }

    /// Folds the sets of `other` into these, the sets of its group `g` into those of `map[g]`.
    pub fn absorb(&mut self, other: Distinct, map: &[usize]) {
        fn fold<T: Eq + std::hash::Hash>(
            to: &mut Vec<HashSet<T>>,
            from: Vec<HashSet<T>>,
            map: &[usize],
        ) {
            for (gid, set) in from.into_iter().enumerate() {
                let Some(&at) = map.get(gid) else { continue };
                if to.len() <= at {
                    to.resize_with(at + 1, HashSet::new);
                }
                if to[at].len() < set.len() {
                    let smaller = std::mem::replace(&mut to[at], set);
                    to[at].extend(smaller);
                } else {
                    to[at].extend(set);
                }
            }
        }
        fold(&mut self.ints, other.ints, map);
        fold(&mut self.texts, other.texts, map);
    }

    /// How many distinct values group `gid` saw.
    #[must_use]
    pub fn count(&self, gid: usize) -> usize {
        self.ints.get(gid).map_or(0, HashSet::len) + self.texts.get(gid).map_or(0, HashSet::len)
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

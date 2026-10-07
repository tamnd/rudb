//! L0: the newest entries of an index, in a B+tree in memory under optimistic lock coupling
//! (section 11.4).
//!
//! Every node has a version word. A reader reads the version, reads the node and reads the version
//! again, and starts over from the root if it moved, so a lookup writes nothing to shared memory. A
//! writer locks the leaf by setting the low bit of its version, and on a split locks the parent
//! too. An inner node that is full is split on the way down, so a split below never finds its
//! parent full. This is the scheme of Leis, Haubenschild and Neumann (IEEE Data Engineering
//! Bulletin 2019).
//!
//! Everything in a node is an atomic word, so a reader that races a writer reads stale or torn
//! numbers, never memory it may not, and the version check throws the read away. Nodes are never
//! freed while the tree lives: a node id a reader read from a stale parent still names a node. The
//! tree is dropped whole when it is flushed to a run, so the space comes back then. Removing an
//! entry does not merge leaves.
//!
//! Keys are of one width a tree, the normalized keys of an index whose columns are all fixed
//! width, which is every TPC-C key. A key is held as big-endian words with the rid as one more
//! word after it, so entries with equal keys sort by rid and an entry is found and removed by key
//! and rid together. Comparing those words in order is comparing the bytes of the key.

use std::cmp::Ordering;
use std::ops::Bound;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering as Memory, fence};

use rudb_common::{Error, Result};

use crate::run::{Hit, Run, RunWriter};

/// The bytes a node is sized to.
const NODE: usize = 4096;

/// No node: the right sibling of the last leaf.
const NONE: u32 = u32::MAX;

/// The slots of the first arena segment, as a power of two. Segment `s` has twice the slots of
/// segment `s - 1`.
const FIRST: u32 = 6;

/// Segments of the arena, enough for more nodes than a `u32` names.
const SEGMENTS: usize = 32;

/// One node. A leaf holds entries, each a key and its ts, and the id of its right sibling. An
/// inner node holds `count` separators and `count + 1` children: child `i` holds the entries below
/// separator `i`, and the last child the entries at or above the last separator.
#[derive(Debug)]
struct Node {
    /// Even when the node is free, odd while a writer holds it. It moves by two at each write.
    version: AtomicU64,
    leaf: bool,
    count: AtomicUsize,
    /// The keys, `words` words each.
    keys: Box<[AtomicU64]>,
    /// The ts of each entry of a leaf, or the ids of the children of an inner node.
    values: Box<[AtomicU64]>,
    /// The right sibling of a leaf.
    next: AtomicU32,
}

impl Node {
    fn new(leaf: bool, capacity: usize, words: usize) -> Self {
        let values = if leaf { capacity } else { capacity + 1 };
        Self {
            version: AtomicU64::new(0),
            leaf,
            count: AtomicUsize::new(0),
            keys: (0..capacity * words).map(|_| AtomicU64::new(0)).collect(),
            values: (0..values).map(|_| AtomicU64::new(0)).collect(),
            next: AtomicU32::new(NONE),
        }
    }

    /// The version, when no writer holds the node.
    fn read(&self) -> Option<u64> {
        let version = self.version.load(Memory::Acquire);
        (version & 1 == 0).then_some(version)
    }

    /// Whether the node is still at `version`, so what was read since is what it held.
    fn check(&self, version: u64) -> bool {
        fence(Memory::Acquire);
        self.version.load(Memory::Relaxed) == version
    }

    /// Locks the node if it is still at `version`.
    fn upgrade(&self, version: u64) -> bool {
        let locked = self
            .version
            .compare_exchange(version, version + 1, Memory::Acquire, Memory::Relaxed)
            .is_ok();
        if locked {
            fence(Memory::Release);
        }
        locked
    }

    fn unlock(&self) {
        self.version.fetch_add(1, Memory::Release);
    }

    /// The count, at most `capacity` whatever a racing writer left there.
    fn count(&self, capacity: usize) -> usize {
        self.count.load(Memory::Relaxed).min(capacity)
    }

    fn word(&self, at: usize) -> u64 {
        self.keys.get(at).map_or(0, |word| word.load(Memory::Relaxed))
    }

    fn value(&self, at: usize) -> u64 {
        self.values.get(at).map_or(0, |value| value.load(Memory::Relaxed))
    }

    fn set_value(&self, at: usize, value: u64) {
        self.values[at].store(value, Memory::Relaxed);
    }

    /// The child at `at` of an inner node, or [`NONE`] when a racing writer left nonsense there.
    fn child(&self, at: usize) -> u32 {
        u32::try_from(self.value(at)).unwrap_or(NONE)
    }
}

/// The nodes of a tree, in segments that are never moved or freed while the tree lives, so an id
/// read from anywhere names the same node for as long as the tree does.
#[derive(Debug)]
struct Arena {
    next: AtomicU32,
    segments: [OnceLock<Box<[OnceLock<Node>]>>; SEGMENTS],
}

impl Arena {
    fn new() -> Self {
        Self { next: AtomicU32::new(0), segments: std::array::from_fn(|_| OnceLock::new()) }
    }

    /// The segment of `id` and its place there.
    fn locate(id: u32) -> (usize, usize) {
        let n = u64::from(id) + (1 << FIRST);
        let segment = 63 - n.leading_zeros() - FIRST;
        (segment as usize, (n - (1 << (segment + FIRST))) as usize)
    }

    fn get(&self, id: u32) -> Option<&Node> {
        let (segment, at) = Self::locate(id);
        self.segments.get(segment)?.get()?.get(at)?.get()
    }

    /// Puts `node` in the arena and answers its id.
    fn add(&self, node: Node) -> Result<u32> {
        let id = self.next.fetch_add(1, Memory::Relaxed);
        if id == NONE {
            return Err(Error::internal("an index L0 ran out of node ids"));
        }
        let (segment, at) = Self::locate(id);
        let slots = self.segments[segment].get_or_init(|| {
            (0..1_usize << (segment + FIRST as usize)).map(|_| OnceLock::new()).collect()
        });
        if slots[at].set(node).is_err() {
            return Err(Error::internal("an index L0 gave one node id twice"));
        }
        Ok(id)
    }

    fn len(&self) -> u32 {
        self.next.load(Memory::Relaxed)
    }
}

/// A node on the way down, with the version it was read at.
type Held<'a> = (&'a Node, u64);

/// The L0 of one index.
#[derive(Debug)]
pub struct L0 {
    /// The bytes of a key.
    width: usize,
    /// The words of an entry's key, the rid's included.
    words: usize,
    /// The entries a leaf holds, and the separators an inner node holds.
    capacity: usize,
    root: AtomicU32,
    nodes: Arena,
    len: AtomicU64,
    restarts: AtomicU64,
}

impl L0 {
    /// An empty tree for keys of `width` bytes.
    ///
    /// # Errors
    ///
    /// Never for a width under a few hundred bytes. The first leaf has to fit.
    pub fn new(width: usize) -> Result<Self> {
        let words = width.div_ceil(8) + 1;
        // A leaf entry is its key and its ts, and an inner node spends as much on a separator and
        // a child.
        let capacity = (NODE / (8 * words + 8)).max(4);
        let nodes = Arena::new();
        let root = nodes.add(Node::new(true, capacity, words))?;
        Ok(Self {
            width,
            words,
            capacity,
            root: AtomicU32::new(root),
            nodes,
            len: AtomicU64::new(0),
            restarts: AtomicU64::new(0),
        })
    }

    /// The bytes of a key.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// The entries the tree holds.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len.load(Memory::Relaxed)
    }

    /// Whether the tree holds no entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes the nodes take, which is what is held against `index_l0_bytes`.
    #[must_use]
    pub fn bytes(&self) -> usize {
        let node = 8 * (self.capacity * self.words + self.capacity + 1) + 32;
        self.nodes.len() as usize * node
    }

    /// How many times an operation started over because a writer moved a node it read.
    #[must_use]
    pub fn restarts(&self) -> u64 {
        self.restarts.load(Memory::Relaxed)
    }

    /// The words of `key` and `rid` as an entry's key, or `None` when the key is not this tree's
    /// width.
    fn probe(&self, key: &[u8], rid: u64) -> Option<Vec<u64>> {
        if key.len() != self.width {
            return None;
        }
        let mut probe = vec![0; self.words];
        for (word, chunk) in probe.iter_mut().zip(key.chunks(8)) {
            let mut bytes = [0; 8];
            bytes[..chunk.len()].copy_from_slice(chunk);
            *word = u64::from_be_bytes(bytes);
        }
        probe[self.words - 1] = rid;
        Some(probe)
    }

    /// The bytes of the key in an entry's words.
    fn key_of(&self, words: &[u64], out: &mut Vec<u8>) {
        out.clear();
        for word in &words[..self.words - 1] {
            out.extend_from_slice(&word.to_be_bytes());
        }
        out.truncate(self.width);
    }

    fn compare(&self, node: &Node, slot: usize, probe: &[u64]) -> Ordering {
        let base = slot * self.words;
        for (at, &word) in probe.iter().enumerate() {
            match node.word(base + at).cmp(&word) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        Ordering::Equal
    }

    /// The first slot of the first `count` whose key is at least `probe`.
    fn lower_bound(&self, node: &Node, count: usize, probe: &[u64]) -> usize {
        let (mut low, mut high) = (0, count);
        while low < high {
            let middle = low + (high - low) / 2;
            if self.compare(node, middle, probe) == Ordering::Less {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    /// The child of an inner node that holds `probe`: the count of separators at or below it.
    fn child_at(&self, node: &Node, count: usize, probe: &[u64]) -> usize {
        let (mut low, mut high) = (0, count);
        while low < high {
            let middle = low + (high - low) / 2;
            if self.compare(node, middle, probe) == Ordering::Greater {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        low
    }

    fn copy_key(&self, from: &Node, from_slot: usize, to: &Node, to_slot: usize) {
        for at in 0..self.words {
            let word = from.word(from_slot * self.words + at);
            to.keys[to_slot * self.words + at].store(word, Memory::Relaxed);
        }
    }

    fn set_key(&self, node: &Node, slot: usize, key: &[u64]) {
        for (at, &word) in key.iter().enumerate() {
            node.keys[slot * self.words + at].store(word, Memory::Relaxed);
        }
    }

    fn read_key(&self, node: &Node, slot: usize) -> Vec<u64> {
        (0..self.words).map(|at| node.word(slot * self.words + at)).collect()
    }

    fn restart(&self) {
        self.restarts.fetch_add(1, Memory::Relaxed);
        std::hint::spin_loop();
    }

    /// The root, read at a version and checked to still be the root.
    fn top(&self) -> Option<(u32, &Node, u64)> {
        let id = self.root.load(Memory::Acquire);
        let node = self.nodes.get(id)?;
        let version = node.read()?;
        (self.root.load(Memory::Acquire) == id).then_some((id, node, version))
    }

    /// Adds the entry that `key` had a row at `hit.rid`, or sets its ts if the tree has it.
    ///
    /// # Errors
    ///
    /// When `key` is not the tree's width, or the tree runs out of node ids.
    pub fn insert(&self, key: &[u8], hit: Hit) -> Result<()> {
        let Some(probe) = self.probe(key, hit.rid) else {
            return Err(Error::internal(format!(
                "a key of {} bytes for an index L0 of {}-byte keys",
                key.len(),
                self.width
            )));
        };
        'restart: loop {
            let Some((mut id, mut node, mut version)) = self.top() else {
                self.restart();
                continue;
            };
            let mut parent: Option<Held<'_>> = None;
            while !node.leaf {
                let count = node.count(self.capacity);
                if count == self.capacity {
                    if !self.lock_pair(parent, (node, version)) {
                        self.restart();
                        continue 'restart;
                    }
                    let split = self.split_inner(id, node, parent.map(|(parent, _)| parent));
                    unlock_pair(parent, node);
                    split?;
                    continue 'restart;
                }
                if let Some((parent, seen)) = parent
                    && !parent.check(seen)
                {
                    self.restart();
                    continue 'restart;
                }
                let child = node.child(self.child_at(node, count, &probe));
                if !node.check(version) {
                    self.restart();
                    continue 'restart;
                }
                let Some((below, seen)) =
                    self.nodes.get(child).and_then(|below| Some((below, below.read()?)))
                else {
                    self.restart();
                    continue 'restart;
                };
                parent = Some((node, version));
                (id, node, version) = (child, below, seen);
            }
            if node.count(self.capacity) == self.capacity {
                if !self.lock_pair(parent, (node, version)) {
                    self.restart();
                    continue;
                }
                // The entry may be there already, and then a split is not needed, but a full leaf
                // splits either way: it will take another entry soon enough.
                let split = self.split_leaf(id, node, parent.map(|(parent, _)| parent));
                unlock_pair(parent, node);
                split?;
                continue;
            }
            if !node.upgrade(version) {
                self.restart();
                continue;
            }
            if let Some((parent, seen)) = parent
                && !parent.check(seen)
            {
                node.unlock();
                self.restart();
                continue;
            }
            self.leaf_insert(node, &probe, hit.ts);
            node.unlock();
            return Ok(());
        }
    }

    /// Locks `node`, and `parent` first when there is one, if neither moved. A root with no parent
    /// has to be the root still.
    fn lock_pair(&self, parent: Option<Held<'_>>, (node, version): Held<'_>) -> bool {
        if let Some((parent, seen)) = parent
            && !parent.upgrade(seen)
        {
            return false;
        }
        if !node.upgrade(version) {
            if let Some((parent, _)) = parent {
                parent.unlock();
            }
            return false;
        }
        true
    }

    fn leaf_insert(&self, node: &Node, probe: &[u64], ts: u64) {
        let count = node.count(self.capacity);
        let at = self.lower_bound(node, count, probe);
        if at < count && self.compare(node, at, probe) == Ordering::Equal {
            node.set_value(at, ts);
            return;
        }
        for slot in (at..count).rev() {
            self.copy_key(node, slot, node, slot + 1);
            node.set_value(slot + 1, node.value(slot));
        }
        self.set_key(node, at, probe);
        node.set_value(at, ts);
        node.count.store(count + 1, Memory::Relaxed);
        self.len.fetch_add(1, Memory::Relaxed);
    }

    /// Splits the full leaf `node`, locked, under `parent`, locked, or under a new root.
    fn split_leaf(&self, id: u32, node: &Node, parent: Option<&Node>) -> Result<()> {
        let count = node.count(self.capacity);
        let middle = count / 2;
        let right = Node::new(true, self.capacity, self.words);
        for slot in middle..count {
            self.copy_key(node, slot, &right, slot - middle);
            right.set_value(slot - middle, node.value(slot));
        }
        right.count.store(count - middle, Memory::Relaxed);
        right.next.store(node.next.load(Memory::Relaxed), Memory::Relaxed);
        let separator = self.read_key(&right, 0);
        let right = self.nodes.add(right)?;
        node.next.store(right, Memory::Relaxed);
        node.count.store(middle, Memory::Relaxed);
        self.publish(id, parent, &separator, right)
    }

    /// Splits the full inner node `node`, locked, under `parent`, locked, or under a new root.
    fn split_inner(&self, id: u32, node: &Node, parent: Option<&Node>) -> Result<()> {
        let count = node.count(self.capacity);
        let middle = count / 2;
        let separator = self.read_key(node, middle);
        let right = Node::new(false, self.capacity, self.words);
        for slot in middle + 1..count {
            self.copy_key(node, slot, &right, slot - middle - 1);
        }
        for slot in middle + 1..=count {
            right.set_value(slot - middle - 1, node.value(slot));
        }
        right.count.store(count - middle - 1, Memory::Relaxed);
        let right = self.nodes.add(right)?;
        node.count.store(middle, Memory::Relaxed);
        self.publish(id, parent, &separator, right)
    }

    /// Hangs `right`, split off `left`, beside it under `parent`, or under a new root over both.
    fn publish(
        &self,
        left: u32,
        parent: Option<&Node>,
        separator: &[u64],
        right: u32,
    ) -> Result<()> {
        let Some(parent) = parent else {
            let root = Node::new(false, self.capacity, self.words);
            self.set_key(&root, 0, separator);
            root.set_value(0, u64::from(left));
            root.set_value(1, u64::from(right));
            root.count.store(1, Memory::Relaxed);
            let root = self.nodes.add(root)?;
            self.root.store(root, Memory::Release);
            return Ok(());
        };
        let count = parent.count(self.capacity);
        let at = self.child_at(parent, count, separator);
        for slot in (at..count).rev() {
            self.copy_key(parent, slot, parent, slot + 1);
            parent.set_value(slot + 2, parent.value(slot + 1));
        }
        self.set_key(parent, at, separator);
        parent.set_value(at + 1, u64::from(right));
        parent.count.store(count + 1, Memory::Relaxed);
        Ok(())
    }

    /// Takes out the entry that `key` had a row at `rid`, which is what the undo collector does
    /// once a delete is below every snapshot (section 11.6). Answers whether the tree had it.
    pub fn remove(&self, key: &[u8], rid: u64) -> bool {
        let Some(probe) = self.probe(key, rid) else { return false };
        loop {
            let Some(((node, version), parent)) = self.leaf_for(&probe) else {
                self.restart();
                continue;
            };
            if !node.upgrade(version) {
                self.restart();
                continue;
            }
            if let Some((parent, seen)) = parent
                && !parent.check(seen)
            {
                node.unlock();
                self.restart();
                continue;
            }
            let count = node.count(self.capacity);
            let at = self.lower_bound(node, count, &probe);
            let found = at < count && self.compare(node, at, &probe) == Ordering::Equal;
            if found {
                for slot in at + 1..count {
                    self.copy_key(node, slot, node, slot - 1);
                    node.set_value(slot - 1, node.value(slot));
                }
                node.count.store(count - 1, Memory::Relaxed);
                self.len.fetch_sub(1, Memory::Relaxed);
            }
            node.unlock();
            return found;
        }
    }

    /// The leaf that holds `probe` and its parent, each read at a version, or `None` when a writer
    /// moved something on the way.
    fn leaf_for(&self, probe: &[u64]) -> Option<(Held<'_>, Option<Held<'_>>)> {
        let (_, mut node, mut version) = self.top()?;
        let mut parent: Option<Held<'_>> = None;
        while !node.leaf {
            let count = node.count(self.capacity);
            let child = node.child(self.child_at(node, count, probe));
            if !node.check(version) {
                return None;
            }
            let below = self.nodes.get(child)?;
            let seen = below.read()?;
            parent = Some((node, version));
            (node, version) = (below, seen);
        }
        Some(((node, version), parent))
    }

    /// Calls `each` with the words and the ts of every entry from `from` on, in order, until it
    /// answers false. With `after`, an entry equal to `from` is passed over.
    fn scan(&self, from: &[u64], after: bool, mut each: impl FnMut(&[u64], u64) -> bool) {
        let mut from = from.to_vec();
        let mut after = after;
        let mut batch: Vec<u64> = Vec::new();
        'restart: loop {
            let Some(((mut node, mut version), _)) = self.leaf_for(&from) else {
                self.restart();
                continue;
            };
            let mut first = true;
            loop {
                let count = node.count(self.capacity);
                let start = if first { self.lower_bound(node, count, &from) } else { 0 };
                batch.clear();
                for slot in start..count {
                    batch.extend((0..self.words).map(|at| node.word(slot * self.words + at)));
                    batch.push(node.value(slot));
                }
                let next = node.next.load(Memory::Relaxed);
                if !node.check(version) {
                    self.restart();
                    continue 'restart;
                }
                for entry in batch.chunks_exact(self.words + 1) {
                    let (words, ts) = entry.split_at(self.words);
                    if after && words <= from.as_slice() {
                        continue;
                    }
                    if !each(words, ts[0]) {
                        return;
                    }
                    from.copy_from_slice(words);
                    after = true;
                }
                if next == NONE {
                    return;
                }
                let Some((right, seen)) =
                    self.nodes.get(next).and_then(|right| Some((right, right.read()?)))
                else {
                    self.restart();
                    continue 'restart;
                };
                (node, version, first) = (right, seen, false);
            }
        }
    }

    /// The first entry for `key`, lowest rid first, that `accept` takes, or `None`.
    pub fn find(&self, key: &[u8], mut accept: impl FnMut(Hit) -> bool) -> Option<Hit> {
        let probe = self.probe(key, 0)?;
        let keyed = self.words - 1;
        let mut found = None;
        self.scan(&probe, false, |words, ts| {
            if words[..keyed] != probe[..keyed] {
                return false;
            }
            let hit = Hit { rid: words[keyed], ts };
            if accept(hit) {
                found = Some(hit);
                return false;
            }
            true
        });
        found
    }

    /// Calls `each` with every entry whose key is between `lo` and `hi`, in key order and then by
    /// rid, until it answers false. A bound of another width than the tree's yields nothing.
    pub fn range(
        &self,
        lo: Bound<&[u8]>,
        hi: Bound<&[u8]>,
        mut each: impl FnMut(&[u8], Hit) -> bool,
    ) {
        let (from, after) = match lo {
            Bound::Included(key) => (self.probe(key, 0), false),
            Bound::Excluded(key) => (self.probe(key, u64::MAX), true),
            Bound::Unbounded => (Some(vec![0; self.words]), false),
        };
        let Some(from) = from else { return };
        let keyed = self.words - 1;
        let (end, inclusive) = match hi {
            Bound::Included(key) => (self.probe(key, 0), true),
            Bound::Excluded(key) => (self.probe(key, 0), false),
            Bound::Unbounded => (None, true),
        };
        if !matches!(hi, Bound::Unbounded) && end.is_none() {
            return;
        }
        let mut key = Vec::with_capacity(self.width);
        self.scan(&from, after, |words, ts| {
            if let Some(end) = &end {
                match words[..keyed].cmp(&end[..keyed]) {
                    Ordering::Greater => return false,
                    Ordering::Equal if !inclusive => return false,
                    _ => {}
                }
            }
            self.key_of(words, &mut key);
            each(&key, Hit { rid: words[keyed], ts })
        });
    }

    /// The tree as a run, in key order and then by rid, which is what a flush writes once the tree
    /// is frozen and takes no more entries.
    #[must_use]
    pub fn to_run(&self) -> Run {
        let mut writer = RunWriter::new();
        self.range(Bound::Unbounded, Bound::Unbounded, |key, hit| {
            // The tree hands the keys over in order, which is all `push` asks.
            writer.push(key, hit).is_ok()
        });
        writer.finish()
    }
}

fn unlock_pair(parent: Option<Held<'_>>, node: &Node) {
    node.unlock();
    if let Some((parent, _)) = parent {
        parent.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn key(n: u64) -> Vec<u8> {
        let mut key = vec![1];
        key.extend_from_slice(&(n ^ (1 << 63)).to_be_bytes());
        key
    }

    fn hits(tree: &L0, from: u64, to: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        tree.range(Bound::Included(&key(from)), Bound::Excluded(&key(to)), |key, hit| {
            let bits: [u8; 8] = key[1..].try_into().expect("nine bytes");
            out.push((u64::from_be_bytes(bits) ^ (1 << 63), hit.rid));
            true
        });
        out
    }

    #[test]
    fn entries_come_back_in_order_through_many_splits() {
        let tree = L0::new(9).expect("a tree");
        // An order that is neither rising nor falling, so splits happen all over the tree.
        let keys: Vec<u64> = (0..20_000_u64).map(|n| (n * 7919) % 20_000).collect();
        for &n in &keys {
            tree.insert(&key(n), Hit { rid: n * 10, ts: 1 }).expect("inserts");
        }
        assert_eq!(tree.len(), 20_000);
        assert!(tree.nodes.len() > 100, "the tree has inner nodes: {}", tree.nodes.len());
        for n in [0, 1, 4_999, 19_999] {
            assert_eq!(tree.find(&key(n), |_| true), Some(Hit { rid: n * 10, ts: 1 }));
        }
        assert_eq!(tree.find(&key(20_000), |_| true), None);
        let all = hits(&tree, 0, 20_000);
        assert_eq!(all, (0..20_000).map(|n| (n, n * 10)).collect::<Vec<_>>());
        assert_eq!(hits(&tree, 100, 103), vec![(100, 1000), (101, 1010), (102, 1020)]);
    }

    #[test]
    fn a_key_holds_an_entry_for_each_rid_and_one_comes_out_by_key_and_rid() {
        let tree = L0::new(9).expect("a tree");
        for rid in [30, 10, 20] {
            tree.insert(&key(5), Hit { rid, ts: rid + 1 }).expect("inserts");
        }
        tree.insert(&key(5), Hit { rid: 20, ts: 99 }).expect("sets the ts");
        assert_eq!(tree.len(), 3);
        let mut seen = Vec::new();
        tree.find(&key(5), |hit| {
            seen.push(hit);
            false
        });
        let expected = [(10, 11), (20, 99), (30, 31)].map(|(rid, ts)| Hit { rid, ts });
        assert_eq!(seen, expected);
        assert_eq!(tree.find(&key(5), |hit| hit.rid > 10), Some(Hit { rid: 20, ts: 99 }));

        assert!(tree.remove(&key(5), 20));
        assert!(!tree.remove(&key(5), 20));
        assert!(!tree.remove(&key(6), 10));
        assert_eq!(tree.len(), 2);
        assert_eq!(hits(&tree, 0, 10), vec![(5, 10), (5, 30)]);
        assert!(tree.insert(&[1, 2], Hit { rid: 0, ts: 0 }).is_err());
        assert_eq!(tree.find(&[1, 2], |_| true), None);
    }

    #[test]
    fn bounds_exclude_and_include_whole_keys() {
        let tree = L0::new(9).expect("a tree");
        for n in 0..10 {
            for rid in 0..3 {
                tree.insert(&key(n), Hit { rid, ts: 0 }).expect("inserts");
            }
        }
        let count = |lo: Bound<&[u8]>, hi: Bound<&[u8]>| {
            let mut count = 0;
            tree.range(lo, hi, |_, _| {
                count += 1;
                true
            });
            count
        };
        let (two, five) = (key(2), key(5));
        assert_eq!(count(Bound::Included(&two), Bound::Included(&five)), 12);
        assert_eq!(count(Bound::Excluded(&two), Bound::Included(&five)), 9);
        assert_eq!(count(Bound::Excluded(&two), Bound::Excluded(&five)), 6);
        assert_eq!(count(Bound::Unbounded, Bound::Excluded(&two)), 6);
        assert_eq!(count(Bound::Included(&five), Bound::Unbounded), 15);
        assert_eq!(count(Bound::Unbounded, Bound::Unbounded), 30);
    }

    #[test]
    fn a_flush_writes_the_tree_as_a_run() {
        let tree = L0::new(27).expect("a tree");
        let composite = |w: u64, d: u64, o: u64| [key(w), key(d), key(o)].concat();
        for o in (1..=3000).rev() {
            tree.insert(&composite(1, o % 10, o), Hit { rid: o, ts: 7 }).expect("inserts");
        }
        let run = tree.to_run();
        assert_eq!(run.len(), 3000);
        assert_eq!(run.find(&composite(1, 3, 2993), |_| true), Some(Hit { rid: 2993, ts: 7 }));
        assert!(run.may_hold(&composite(1, 0, 3000)));
    }

    /// Writers and readers on one tree at once: every entry a writer put in is found by every
    /// reader that looks after it was, the scans see keys in order, and the tree ends with
    /// everything put in and nothing taken out.
    #[test]
    fn readers_and_writers_share_a_tree() {
        let tree = Arc::new(L0::new(9).expect("a tree"));
        let writers = 4;
        let each = 5_000_u64;
        let threads: Vec<_> = (0..writers)
            .map(|writer| {
                let tree = Arc::clone(&tree);
                std::thread::spawn(move || {
                    for i in 0..each {
                        let n = i * writers + writer;
                        tree.insert(&key(n), Hit { rid: n, ts: 1 }).expect("inserts");
                        // A removed entry that came and went.
                        tree.insert(&key(n), Hit { rid: n + 1_000_000, ts: 1 }).expect("inserts");
                        assert!(tree.remove(&key(n), n + 1_000_000));
                        if i % 64 == 0 {
                            assert_eq!(tree.find(&key(n), |_| true), Some(Hit { rid: n, ts: 1 }));
                        }
                    }
                })
            })
            .collect();
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let tree = Arc::clone(&tree);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let mut last = None;
                        tree.range(Bound::Unbounded, Bound::Unbounded, |key, hit| {
                            let entry = (key.to_vec(), hit.rid);
                            assert!(last.as_ref().is_none_or(|last| *last < entry), "in order");
                            last = Some(entry);
                            true
                        });
                    }
                })
            })
            .collect();
        for thread in threads.into_iter().chain(readers) {
            thread.join().expect("the thread finished");
        }
        let total = writers * each;
        assert_eq!(tree.len(), total);
        let mut model = BTreeMap::new();
        tree.range(Bound::Unbounded, Bound::Unbounded, |key, hit| {
            model.insert(key.to_vec(), hit.rid);
            true
        });
        assert_eq!(model.len() as u64, total);
        assert!(model.values().all(|&rid| rid < 1_000_000));
    }
}

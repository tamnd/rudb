//! Running a [`Node::Consistent`](rudb_plan::Node::Consistent): two sweeps of semijoins over a join
//! tree and a MIN or MAX over what is left, with no join anywhere.
//!
//! The plan side, in `rudb_opt::consistent`, explains why this answers the same question the join
//! would have. This file is how, and the how is shaped by one observation: the first sweep, from the
//! leaves of the tree up to the root, can happen while the relations are being scanned rather than
//! after. The relations are scanned children first, one pipeline each, and a pipeline waits for the
//! pipelines of its children. By the time a relation's rows arrive, the set of join keys each of its
//! children kept is finished, so a row whose key is in none of them is dropped on the spot and never
//! stored. What a relation hands up to its parent is the set of its own keys in the class it shares
//! with that parent, taken over the rows it kept.
//!
//! A root has everything in its tree under it, so the rows a root keeps are exactly its rows that
//! take part in the join, and its extremes are read off them as they arrive. The same goes for the
//! keys it hands back down. Every other relation an extreme is read from, and every relation on the
//! path from a root down to one, keeps its surviving rows until the second sweep reaches it, and only
//! the columns that sweep reads: the key it shares with its parent, the keys it shares with the
//! children the sweep goes on to, and the columns its extremes are read from. A relation with no
//! extreme under it is never read again once it has handed its keys up.
//!
//! The second sweep runs in [`Answer`], the source that produces the one row, over the held rows
//! and nothing else. It goes from the roots down, parents before children, which is the stored order
//! backwards, keeping each held row whose key its parent kept and folding the extremes in.
//!
//! # The sets
//!
//! A set of keys is a bitmap over the values from zero to a limit, which is where the identifiers of
//! a real schema live, with a hash set beside it for anything outside. The bitmap grows to the
//! largest key it has seen rather than being sized up front, because nothing says in advance how
//! large a key will be and a set of a few hundred small keys should not cost sixteen megabytes. Each
//! thread builds its own and they are merged when the relation is finished, which is a word by word
//! OR over the bitmap.
//!
//! # Nulls
//!
//! A null key matches nothing, so a row with a null in any of its keys is dropped as it is read.
//! Every key of a relation is joined to something, which is how the plan side chose them, so there
//! is no key a null could hide in unnoticed. A null in a column an extreme is read from is the
//! aggregate's own business, and the accumulator skips it the way it does in an ordinary MIN. A join
//! with no rows produces a null for every extreme, which is what an ungrouped aggregate over an empty
//! input produces.
//!
//! # Stopping early
//!
//! A relation that keeps no rows means the join is empty and the answer is all nulls, whatever the
//! other relations hold. The first relation to finish empty says so, and every sink after that
//! answers the first chunk it is given with [`Progress::Done`], which stops its scan.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Value};
use rudb_kernels::aggregate::Accumulator;
use rudb_pipeline::{Lease, Morsel, Progress, Sink, Source};
use rudb_plan::Reducer;
use rudb_vector::{Chunk, Selection, Vector};

use crate::schema::Schema;
use crate::source::Handout;

/// The keys below this go in the bitmap, and everything else in the hash set.
///
/// Sixteen megabytes of bitmap at the most, per set per thread, which is the price of the largest
/// identifier this covers. The identifiers of the Join Order Benchmark stop well under it, the
/// largest being the forty million rows of `cast_info`, and those are never a join key.
const DENSE: usize = 1 << 27;

/// A set of join keys.
#[derive(Debug, Default)]
struct Keys {
    /// One bit per key below [`DENSE`], as long as the largest key seen needs.
    words: Vec<u64>,
    /// The keys that are negative or at least [`DENSE`].
    spread: HashSet<i64>,
}

impl Keys {
    /// Puts a key in.
    fn insert(&mut self, key: i64) {
        match usize::try_from(key) {
            Ok(at) if at < DENSE => {
                let word = at >> 6;
                if word >= self.words.len() {
                    // Doubling, so that a relation read in ascending key order grows the bitmap a
                    // logarithmic number of times rather than once per word.
                    let wanted = (word + 1).next_power_of_two().min(DENSE >> 6);
                    self.words.resize(wanted, 0);
                }
                self.words[word] |= 1 << (at & 63);
            }
            _ => {
                self.spread.insert(key);
            }
        }
    }

    /// Whether a key is in.
    fn contains(&self, key: i64) -> bool {
        match usize::try_from(key) {
            Ok(at) if at < DENSE => {
                self.words.get(at >> 6).is_some_and(|word| word >> (at & 63) & 1 == 1)
            }
            _ => self.spread.contains(&key),
        }
    }

    /// Puts every key of another set in.
    fn merge(&mut self, other: Self) {
        if other.words.len() > self.words.len() {
            let mut other = other;
            std::mem::swap(self, &mut other);
            self.merge(other);
            return;
        }
        for (mine, theirs) in self.words.iter_mut().zip(&other.words) {
            *mine |= theirs;
        }
        self.spread.extend(other.spread);
    }
}

/// What one relation does with its rows, worked out once from the tree.
#[derive(Debug)]
struct Role {
    /// Every join column, as a position in what the relation's input produces.
    keys: Vec<usize>,
    /// For each child, which child and which of `keys` it is joined on.
    children: Vec<(usize, usize)>,
    /// Which of `keys` the relation shares with its parent, or nothing for a root.
    parent: Option<usize>,
    /// The children the second sweep goes on to, each with which of `keys` it is joined on.
    down: Vec<(usize, usize)>,
    /// The extremes read from this relation, each with the column it is read from.
    extremes: Vec<(usize, usize)>,
    /// Whether the rows are kept for the second sweep, and if so which columns, as positions in
    /// the relation's input.
    held: Option<Vec<usize>>,
}

/// What every relation of one node shares: the sets, the held rows and the running extremes.
#[derive(Debug)]
pub(crate) struct Reduction {
    roles: Vec<Role>,
    /// The type of each extreme, which is also the type of each output column.
    types: Vec<LogicalType>,
    /// Per relation, the keys it kept in the class it shares with its parent, while it is running.
    gathering: Vec<Mutex<Keys>>,
    /// The same, once the relation has finished, for its parent's sink to read without a lock.
    up: Vec<OnceLock<Keys>>,
    /// Per relation, the keys of its parent's class that a root allowed it, while the root runs.
    allowing: Vec<Mutex<Keys>>,
    /// The same, once the root has finished.
    allowed: Vec<OnceLock<Keys>>,
    /// Per relation, the rows it kept for the second sweep.
    held: Vec<Mutex<Vec<Chunk>>>,
    /// Per relation, how many rows it kept.
    kept: Vec<Mutex<u64>>,
    /// One accumulator per extreme that has seen nothing, which every instance of a root's sink
    /// starts from.
    fresh: Vec<Accumulator>,
    /// The extremes the roots have seen so far, in output order.
    extremes: Mutex<Vec<Accumulator>>,
    /// Whether some relation kept no rows, which makes the join empty.
    empty: AtomicBool,
    /// What the held rows are charged, for as long as they are held.
    charged: Mutex<Vec<Reservation>>,
    memory: Memory,
}

impl Reduction {
    /// The shared state for one node's tree, with each extreme of the given type.
    ///
    /// # Errors
    ///
    /// If an extreme's type is one no accumulator can hold.
    pub(crate) fn new(tree: &Reducer, types: Vec<LogicalType>, memory: &Memory) -> Result<Self> {
        let count = tree.leaves.len();
        let mut roles = Vec::with_capacity(count);
        for (at, leaf) in tree.leaves.iter().enumerate() {
            let position = u32::try_from(at).map_err(|_| Error::internal("too many relations"))?;
            let keys: Vec<usize> = leaf.keys.iter().map(|key| key.column as usize).collect();
            let slot = |class: u32| {
                leaf.keys.iter().position(|key| key.class == class).ok_or_else(|| {
                    Error::internal(format!("relation {at} has no column in class {class}"))
                })
            };
            let mut children = Vec::new();
            let mut down = Vec::new();
            for (child, class) in tree.children(position) {
                children.push((child as usize, slot(class)?));
                if tree.held(child) {
                    down.push((child as usize, slot(class)?));
                }
            }
            let parent = leaf.parent.map(|edge| slot(edge.class)).transpose()?;
            let extremes: Vec<(usize, usize)> = tree
                .extremes
                .iter()
                .enumerate()
                .filter(|(_, extreme)| extreme.leaf == position)
                .map(|(output, extreme)| (output, extreme.column as usize))
                .collect();
            let held = tree.held(position).then(|| {
                let mut columns: Vec<usize> = parent.iter().map(|&key| keys[key]).collect();
                columns.extend(down.iter().map(|&(_, key)| keys[key]));
                columns.extend(extremes.iter().map(|&(_, column)| column));
                columns
            });
            roles.push(Role { keys, children, parent, down, extremes, held });
        }
        let mut extremes = Vec::with_capacity(types.len());
        for (extreme, ty) in tree.extremes.iter().zip(&types) {
            extremes.push(Accumulator::new(if extreme.max { "max" } else { "min" }, ty)?);
        }
        Ok(Self {
            roles,
            types,
            gathering: (0..count).map(|_| Mutex::new(Keys::default())).collect(),
            up: (0..count).map(|_| OnceLock::new()).collect(),
            allowing: (0..count).map(|_| Mutex::new(Keys::default())).collect(),
            allowed: (0..count).map(|_| OnceLock::new()).collect(),
            held: (0..count).map(|_| Mutex::new(Vec::new())).collect(),
            kept: (0..count).map(|_| Mutex::new(0)).collect(),
            fresh: extremes.clone(),
            extremes: Mutex::new(extremes),
            empty: AtomicBool::new(false),
            charged: Mutex::new(Vec::new()),
            memory: memory.clone(),
        })
    }

    /// What the roots have folded in so far, which is where the second sweep starts from.
    fn accumulators(&self) -> Result<Vec<Accumulator>> {
        let held = self.extremes.lock().map_err(poisoned)?;
        Ok(held.clone())
    }
}

/// The sink at the end of one relation's scan, which runs the first sweep over it.
#[derive(Debug)]
pub(crate) struct Collect {
    shared: Arc<Reduction>,
    at: usize,
}

impl Collect {
    /// The sink for relation `at` of the tree.
    pub(crate) fn new(shared: Arc<Reduction>, at: usize) -> Self {
        Self { shared, at }
    }
}

/// What one instance of a relation's sink builds.
#[derive(Debug)]
pub(crate) struct Collecting {
    /// The keys handed up to the parent.
    up: Keys,
    /// Per child the second sweep goes on to, the keys a root allows it, in the order of
    /// [`Role::down`].
    down: Vec<Keys>,
    /// The extremes, for a root, and nothing otherwise.
    extremes: Option<Vec<Accumulator>>,
    /// The rows held for the second sweep.
    held: Vec<Chunk>,
    charged: Reservation,
    kept: u64,
    /// One column of keys at a time, reused from chunk to chunk.
    values: Vec<Vec<i64>>,
    nulls: Vec<bool>,
}

impl Sink for Collect {
    type Local = Collecting;

    fn local(&self) -> Collecting {
        let role = &self.shared.roles[self.at];
        let root = role.parent.is_none();
        Collecting {
            up: Keys::default(),
            down: role.down.iter().map(|_| Keys::default()).collect(),
            // Only a root reads extremes as it goes.
            extremes: (root && !role.extremes.is_empty()).then(|| self.shared.fresh.clone()),
            held: Vec::new(),
            charged: self.shared.memory.reservation(),
            kept: 0,
            values: role.keys.iter().map(|_| Vec::new()).collect(),
            nulls: Vec::new(),
        }
    }

    fn sink(&self, chunk: &Chunk, local: &mut Collecting) -> Result<Progress> {
        if self.shared.empty.load(Ordering::Relaxed) {
            return Ok(Progress::Done);
        }
        let rows = chunk.len();
        if rows == 0 {
            return Ok(Progress::More);
        }
        let role = &self.shared.roles[self.at];
        let mut kept: Vec<u32> = match chunk.kept() {
            Some(selection) => selection.indices().to_vec(),
            None => {
                (0..u32::try_from(rows).map_err(|_| Error::internal("a huge chunk"))?).collect()
            }
        };
        for (key, &column) in role.keys.iter().enumerate() {
            let vector = chunk.column(column)?;
            let nulls = read(vector, rows, &mut local.values[key], &mut local.nulls)?;
            if nulls {
                let flags = &local.nulls;
                kept.retain(|&row| !flags[row as usize]);
            }
        }
        for &(child, key) in &role.children {
            let Some(allowed) = self.shared.up[child].get() else {
                return Err(Error::internal("a relation ran before one of its children finished"));
            };
            let values = &local.values[key];
            kept.retain(|&row| allowed.contains(values[row as usize]));
        }
        if kept.is_empty() {
            return Ok(Progress::More);
        }
        local.kept += kept.len() as u64;
        if let Some(parent) = role.parent {
            let values = &local.values[parent];
            for &row in &kept {
                local.up.insert(values[row as usize]);
            }
        }
        if role.parent.is_none() {
            for (slot, &(_, key)) in role.down.iter().enumerate() {
                let values = &local.values[key];
                for &row in &kept {
                    local.down[slot].insert(values[row as usize]);
                }
            }
        }
        let selection = Selection::from_indices(kept);
        if let Some(extremes) = local.extremes.as_mut() {
            fold(extremes, &role.extremes, chunk, &selection)?;
        }
        if let Some(columns) = &role.held {
            let taken: Vec<Vector> =
                columns.iter().map(|&column| chunk.columns()[column].clone()).collect();
            let taken = Chunk::with_rows(taken, rows)?.compact(&selection)?;
            local.charged.grow(u64::try_from(taken.footprint()).unwrap_or(u64::MAX))?;
            local.held.push(taken);
        }
        Ok(Progress::More)
    }

    fn combine(&self, local: Collecting) -> Result<()> {
        let shared = &self.shared;
        let role = &shared.roles[self.at];
        if role.parent.is_some() {
            shared.gathering[self.at].lock().map_err(poisoned)?.merge(local.up);
        }
        for (&(child, _), keys) in role.down.iter().zip(local.down) {
            shared.allowing[child].lock().map_err(poisoned)?.merge(keys);
        }
        if let Some(extremes) = local.extremes {
            let mut held = shared.extremes.lock().map_err(poisoned)?;
            for &(output, _) in &role.extremes {
                held[output].combine(&extremes[output])?;
            }
        }
        shared.held[self.at].lock().map_err(poisoned)?.extend(local.held);
        *shared.kept[self.at].lock().map_err(poisoned)? += local.kept;
        shared.charged.lock().map_err(poisoned)?.push(local.charged);
        Ok(())
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        let shared = &self.shared;
        let role = &shared.roles[self.at];
        if *shared.kept[self.at].lock().map_err(poisoned)? == 0 {
            shared.empty.store(true, Ordering::Relaxed);
        }
        let up = std::mem::take(&mut *shared.gathering[self.at].lock().map_err(poisoned)?);
        let _ = shared.up[self.at].set(up);
        for &(child, _) in &role.down {
            let allowed = std::mem::take(&mut *shared.allowing[child].lock().map_err(poisoned)?);
            let _ = shared.allowed[child].set(allowed);
        }
        Ok(())
    }
}

/// Reads one column of join keys as `i64`, and says whether any of them is null.
///
/// `nulls` holds a flag per row when the answer is yes and is left alone otherwise, so a column with
/// no nulls, which is nearly every key column there is, costs a block read and nothing else.
fn read(vector: &Vector, rows: usize, out: &mut Vec<i64>, nulls: &mut Vec<bool>) -> Result<bool> {
    if vector.signed_block(out) && out.len() >= rows {
        if vector.none_null() {
            return Ok(false);
        }
        nulls.clear();
        nulls.extend((0..rows).map(|row| vector.is_null_at(row)));
        return Ok(true);
    }
    out.clear();
    nulls.clear();
    // row at a time: the fallback for a form `signed_block` does not hand over as a block, which is
    // the run and the compressed forms, read one key at a time the way every other operator reads
    // them when the block read says no.
    for row in 0..rows {
        if vector.is_null_at(row) {
            out.push(0);
            nulls.push(true);
            continue;
        }
        let key = match vector.signed_at(row) {
            Some(key) => key,
            None => match vector.try_value_at(row)? {
                Value::TinyInt(key) => i128::from(key),
                Value::SmallInt(key) => i128::from(key),
                Value::Integer(key) => i128::from(key),
                Value::BigInt(key) => i128::from(key),
                other => {
                    return Err(Error::internal(format!(
                        "a join key that is not an integer: {other}"
                    )));
                }
            },
        };
        out.push(i64::try_from(key).map_err(|_| Error::internal("a join key past i64"))?);
        nulls.push(false);
    }
    Ok(true)
}

/// Folds the rows of `chunk` that `selection` keeps into the extremes read from its columns.
fn fold(
    extremes: &mut [Accumulator],
    read: &[(usize, usize)],
    chunk: &Chunk,
    selection: &Selection,
) -> Result<()> {
    for &(output, column) in read {
        let selected =
            Vector::dictionary(selection.indices().to_vec(), chunk.column(column)?.clone())?;
        extremes[output].update_run(std::slice::from_ref(&selected), selection.len())?;
    }
    Ok(())
}

/// The source of the one row, which runs the second sweep before it produces it.
#[derive(Debug)]
pub(crate) struct Answer {
    shared: Arc<Reduction>,
    schema: Schema,
    one: Handout,
}

impl Answer {
    /// The source for a node whose relations are `shared`, producing `schema`.
    pub(crate) fn new(shared: Arc<Reduction>, schema: Schema) -> Self {
        Self { shared, schema, one: Handout::new(1) }
    }

    /// What this produces, which is one column per extreme.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The extremes, or nothing when the join turned out to be empty.
    fn answer(&self) -> Result<Option<Vec<Accumulator>>> {
        let shared = &self.shared;
        if shared.empty.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let mut extremes = shared.accumulators()?;
        let count = shared.roles.len();
        let mut allowed: Vec<Option<Keys>> = (0..count).map(|_| None).collect();
        for at in (0..count).rev() {
            let role = &shared.roles[at];
            let Some(columns) = &role.held else { continue };
            // A child of a root was allowed its keys by the root's sink, and every other held
            // relation by its parent a step earlier in this loop.
            let owned = allowed[at].take();
            let permitted: &Keys = match &owned {
                Some(keys) => keys,
                None => shared.allowed[at]
                    .get()
                    .ok_or_else(|| Error::internal("a held relation with no allowed keys"))?,
            };
            let mut down: Vec<Keys> = role.down.iter().map(|_| Keys::default()).collect();
            let chunks = std::mem::take(&mut *shared.held[at].lock().map_err(poisoned)?);
            let mut survived = 0usize;
            let mut values = Vec::new();
            let mut nulls = Vec::new();
            let mut others: Vec<Vec<i64>> = role.down.iter().map(|_| Vec::new()).collect();
            for chunk in &chunks {
                let rows = chunk.len();
                // The parent key is the first held column, which `Reduction::new` put there.
                read(chunk.column(0)?, rows, &mut values, &mut nulls)?;
                let kept: Vec<u32> = (0..rows)
                    .filter(|&row| permitted.contains(values[row]))
                    .map(|row| u32::try_from(row).unwrap_or(u32::MAX))
                    .collect();
                if kept.is_empty() {
                    continue;
                }
                survived += kept.len();
                for (slot, other) in others.iter_mut().enumerate() {
                    read(chunk.column(1 + slot)?, rows, other, &mut nulls)?;
                    for &row in &kept {
                        down[slot].insert(other[row as usize]);
                    }
                }
                let selection = Selection::from_indices(kept);
                let first = 1 + role.down.len();
                let read: Vec<(usize, usize)> = role
                    .extremes
                    .iter()
                    .enumerate()
                    .map(|(slot, &(output, _))| (output, first + slot))
                    .collect();
                fold(&mut extremes, &read, chunk, &selection)?;
            }
            debug_assert_eq!(columns.len(), 1 + role.down.len() + role.extremes.len());
            if survived == 0 {
                return Ok(None);
            }
            for (&(child, _), keys) in role.down.iter().zip(down) {
                allowed[child] = Some(keys);
            }
        }
        Ok(Some(extremes))
    }
}

impl Source for Answer {
    fn morsel(&self) -> Option<Morsel> {
        self.one.take()
    }

    fn morsels(&self, _threads: usize, _weight: usize) -> Option<usize> {
        Some(self.one.total())
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let answered = self.answer()?;
        let mut columns = Vec::with_capacity(self.shared.types.len());
        for (at, ty) in self.shared.types.iter().enumerate() {
            let value = match &answered {
                Some(extremes) => extremes[at].finish()?,
                None => Value::Null,
            };
            columns.push(Vector::from_values(ty.clone(), &[value])?);
        }
        *out = Chunk::with_rows(columns, 1)?;
        // The held rows have been read and dropped, so what they were charged goes with them.
        self.shared.charged.lock().map_err(poisoned)?.clear();
        morsel.advance(1);
        Ok(Progress::Done)
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the state of a consistent reduction")
}

#[cfg(test)]
mod tests {
    use super::{DENSE, Keys};

    #[test]
    fn a_key_is_in_the_set_it_was_put_in_wherever_it_lands() {
        let mut keys = Keys::default();
        let far = i64::try_from(DENSE).expect("fits") + 5;
        for key in [0, 63, 64, 1_000_000, -3, far] {
            keys.insert(key);
        }
        for key in [0, 63, 64, 1_000_000, -3, far] {
            assert!(keys.contains(key), "{key}");
        }
        for key in [1, 62, 65, 999_999, -2, far - 1, i64::MAX] {
            assert!(!keys.contains(key), "{key}");
        }
    }

    #[test]
    fn merging_keeps_both_sides_whichever_is_longer() {
        let mut short = Keys::default();
        short.insert(3);
        short.insert(-1);
        let mut long = Keys::default();
        long.insert(100_000);
        short.merge(long);
        for key in [3, -1, 100_000] {
            assert!(short.contains(key), "{key}");
        }
        assert!(!short.contains(4));
    }
}

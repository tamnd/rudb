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
//! # Trailing a root
//!
//! A relation with no children under a root can be scanned after the root rather than before it,
//! which the plan does where its own filter is dear and the root leaves few of its keys. It is read
//! against the keys the root kept, like any relation after another in its class. The root cannot
//! test its rows against a set that is not built yet, so it holds them, and the second sweep starts
//! by keeping the ones whose key is in the set of every relation that trails it. Only then are the
//! root's extremes folded in and the keys it hands down taken.
//!
//! # Dropping rows in the scan
//!
//! Testing a row's keys in the sink is late: by then the scan has read and decoded every column the
//! relation hands over, strings included, for rows that are nearly all about to go. On the Join
//! Order Benchmark that was most of the time of the worst queries, `movie_info` read and flattened
//! all fifteen million of its `info` strings to keep a few thousand. So each relation's finished
//! sets are also handed to scans through the same [`Sideways`] a hash join hands its build side's
//! keys through, as a bitmap over the key values with the range and, when they are few, the list of
//! keys beside it. The scan then rules out parts whose keys it holds none of, tests the key column
//! of each part it reads against the bitmap before the pushed filter runs, and reads the other
//! columns at the rows that pass. See `crate::source::Scan::read_deferring`. The sink still tests
//! every key, because the scan is allowed to stop testing a bitmap that keeps most rows and a scan
//! the builder could not reach tests nothing.
//!
//! A set goes to more scans than the parent's. The columns of one class are equal in every row of
//! the join, so the keys a relation kept in a class hold every value that class takes in the join,
//! and any other relation with a column in that class can drop a row whose value is not among them
//! without changing the answer, wherever it is in the tree. The relations are scanned in the order
//! of the leaves, cheapest first, and a scan waits for every relation that hands it a set, so a set
//! goes to the next relation in its class to be scanned and to no other. That relation keeps a
//! subset of it and hands that on in turn, so each class is a chain from its cheapest relation to
//! its dearest, and the dearest tests one set per class rather than one per relation before it.
//! That is what lets `title` narrowed to eight movies in JOB 24b cut down the scan of `movie_info`
//! in a branch of its own.
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
//! # Strings read late
//!
//! A string extreme of a relation under a root can come as each row's place in its table rather
//! than the string, see `rudb_plan::Extreme::fetch`. The second sweep then gathers the places of the
//! rows it keeps and reads the strings at those alone, in the table's order, and folds them in.
//!
//! # Stopping early
//!
//! A relation that keeps no rows means the join is empty and the answer is all nulls, whatever the
//! other relations hold. The first relation to finish empty says so, and every sink after that
//! answers the first chunk it is given with [`Progress::Done`], which stops its scan.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rudb_catalog::Table;
use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Value};
use rudb_kernels::aggregate::Accumulator;
use rudb_metrics::{Counters, KeySets};
use rudb_pipeline::{Lease, Morsel, Progress, Sink, Source};
use rudb_plan::Reducer;
use rudb_vector::{Chunk, Selection, Vector};

use crate::schema::Schema;
use crate::sideways::{Found, Sideways};
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
    /// Puts a key in, which the sinks do a chunk at a time through [`Self::insert_rows`].
    #[cfg(test)]
    fn insert(&mut self, key: i64) {
        match usize::try_from(key) {
            Ok(at) if at < DENSE => {
                self.reach(at);
                self.words[at >> 6] |= 1 << (at & 63);
            }
            _ => {
                self.spread.insert(key);
            }
        }
    }

    /// Grows the bitmap to hold `at`, which is under [`DENSE`].
    fn reach(&mut self, at: usize) {
        let word = at >> 6;
        if word >= self.words.len() {
            // Doubling, so that a relation read in ascending key order grows the bitmap a
            // logarithmic number of times rather than once per word.
            let wanted = (word + 1).next_power_of_two().min(DENSE >> 6);
            self.words.resize(wanted, 0);
        }
    }

    /// Puts in the key of each row `rows` names.
    ///
    /// [`Self::insert`] a row at a time was a call a row that looked at the length of the bitmap
    /// every time, and on JOB 13d the sink was a third of the query, half of it in those calls.
    /// Here the bitmap is grown once for the largest key and the loop is a load and an or a row.
    fn insert_rows(&mut self, values: &[i64], rows: &[u32]) {
        let top = rows
            .iter()
            .filter_map(|&row| usize::try_from(values[row as usize]).ok())
            .filter(|&at| at < DENSE)
            .max();
        if let Some(top) = top {
            self.reach(top);
        }
        let Self { words, spread } = self;
        for &row in rows {
            let key = values[row as usize];
            match usize::try_from(key) {
                Ok(at) if at < DENSE => words[at >> 6] |= 1 << (at & 63),
                _ => {
                    spread.insert(key);
                }
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

    /// Keeps the rows of `rows` whose key in `values` is in.
    ///
    /// With `retain` this was a branch a row on whether the key is in, and a child that keeps half
    /// its parent's keys makes that branch a coin toss. On JOB 13d the bit test was a fifth of the
    /// sink. Here every row is written and the count moves on by whether it stays, so there is no
    /// branch to guess. When every key is in the bitmap a key off either end of it, negative ones
    /// included once taken as unsigned, is past the last word, so one bounds test covers both.
    fn filter(&self, values: &[i64], rows: &mut Vec<u32>) {
        if !self.spread.is_empty() {
            rows.retain(|&row| self.contains(values[row as usize]));
            return;
        }
        let words = &self.words[..];
        let mut stay = 0;
        for at in 0..rows.len() {
            let row = rows[at];
            // Two's complement makes a negative key a huge offset, past every word.
            #[allow(clippy::cast_sign_loss)]
            let key = values[row as usize] as u64;
            let held = usize::try_from(key >> 6)
                .ok()
                .and_then(|word| words.get(word))
                .map_or(0, |word| (word >> (key & 63)) & 1);
            rows[stay] = row;
            stay += held as usize;
        }
        rows.truncate(stay);
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
    /// For each child scanned before it, which child and which of `keys` it is joined on.
    children: Vec<(usize, usize)>,
    /// The same for each child that trails it, which only a root has.
    trailing: Vec<(usize, usize)>,
    /// Which of `keys` the relation shares with its parent, or nothing for a root.
    parent: Option<usize>,
    /// The children the second sweep goes on to, each with which of `keys` it is joined on.
    down: Vec<(usize, usize)>,
    /// The extremes read from this relation, each with the column it is read from.
    extremes: Vec<(usize, usize)>,
    /// Whether the rows are kept for the second sweep, and if so which columns, as positions in
    /// the relation's input: the parent's key, the keys of the children that trail it, the keys of
    /// `down` and the columns of `extremes`, in that order.
    held: Option<Vec<usize>>,
    /// The keys whose kept sets go to other relations' scans, each with the relations and the
    /// column of each that is in the same class. See the module documentation.
    published: Vec<(usize, Vec<(usize, u32)>)>,
}

/// What every relation of one node shares: the sets, the held rows and the running extremes.
#[derive(Debug)]
pub(crate) struct Reduction {
    roles: Vec<Role>,
    /// The order the second sweep takes the relations in, parents before children.
    sweep: Vec<usize>,
    /// The type of each extreme, which is also the type of each output column.
    types: Vec<LogicalType>,
    /// Per relation, the keys it kept in the class it shares with its parent, while it is running.
    gathering: Vec<Mutex<Keys>>,
    /// Per relation, the keys it kept for other scans, in the order of [`Role::published`], while it
    /// is running. The one in the parent's class is left empty and `gathering` stands in for it.
    publishing: Vec<Mutex<Vec<Keys>>>,
    /// The same, once the relation has finished, for its parent's sink to read without a lock.
    up: Vec<OnceLock<Keys>>,
    /// Per relation, the keys of its parent's class that a root allowed it, while the root runs.
    allowing: Vec<Mutex<Keys>>,
    /// The same, once the root has finished.
    allowed: Vec<OnceLock<Keys>>,
    /// Per relation, the rows it kept for the second sweep.
    held: Vec<Mutex<Vec<Held>>>,
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
    /// How many relations have finished their first sweep.
    finished: AtomicUsize,
    /// The answer of the second sweep, when the last relation to finish ran it.
    answered: Mutex<Option<Option<Vec<Accumulator>>>>,
    /// How many key sets the two sweeps finished, and how many keys of them were outside the
    /// bitmap, for `EXPLAIN ANALYZE`. See [`KeySets`].
    sets: AtomicU64,
    hashed: AtomicU64,
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
            let mut trailing = Vec::new();
            let mut down = Vec::new();
            for (child, class) in tree.children(position) {
                if tree.trailing(child) {
                    trailing.push((child as usize, slot(class)?));
                } else {
                    children.push((child as usize, slot(class)?));
                }
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
                columns.extend(trailing.iter().map(|&(_, key)| keys[key]));
                columns.extend(down.iter().map(|&(_, key)| keys[key]));
                columns.extend(extremes.iter().map(|&(_, column)| column));
                columns
            });
            let published = leaf
                .keys
                .iter()
                .enumerate()
                .filter_map(|(slot, key)| {
                    // Only the next relation in the class to be scanned. The scans run in the
                    // order of the leaves and each waits for the ones that hand it keys, so that
                    // relation is read against these keys and keeps a subset of them, which it
                    // hands on in turn. The relations further on would test their rows against
                    // this set and then against a smaller one, and the ones already scanned
                    // would be handed keys they never read. A relation that trails this one is
                    // under it and is read after it all the same.
                    let readers: Vec<(usize, u32)> = (at + 1..count)
                        .filter(|&other| {
                            !below(tree, other, at)
                                || tree.leaves[other]
                                    .parent
                                    .is_some_and(|edge| edge.leaf as usize == at)
                        })
                        .find_map(|other| {
                            let column = tree.key(u32::try_from(other).ok()?, key.class)?;
                            Some((other, column))
                        })
                        .into_iter()
                        .collect();
                    (!readers.is_empty()).then_some((slot, readers))
                })
                .collect();
            roles.push(Role { keys, children, trailing, parent, down, extremes, held, published });
        }
        // Backwards through the stored order, which puts parents before children, with each
        // relation that trails a root taken right after it.
        let mut sweep = Vec::with_capacity(count);
        for at in (0..count).rev() {
            if tree.trailing(u32::try_from(at).unwrap_or(u32::MAX)) {
                continue;
            }
            sweep.push(at);
            sweep.extend(roles[at].trailing.iter().map(|&(child, _)| child));
        }
        let mut extremes = Vec::with_capacity(types.len());
        for (extreme, ty) in tree.extremes.iter().zip(&types) {
            extremes.push(Accumulator::new(if extreme.max { "max" } else { "min" }, ty)?);
        }
        let publishing = roles
            .iter()
            .map(|role| Mutex::new(role.published.iter().map(|_| Keys::default()).collect()))
            .collect();
        Ok(Self {
            roles,
            sweep,
            types,
            gathering: (0..count).map(|_| Mutex::new(Keys::default())).collect(),
            publishing,
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
            finished: AtomicUsize::new(0),
            answered: Mutex::new(None),
            sets: AtomicU64::new(0),
            hashed: AtomicU64::new(0),
        })
    }

    /// The relations whose scans the keys of relation `at` go to, each with the column of that
    /// relation they are about, in the order [`Collect::new`] wants its edges in.
    pub(crate) fn readers(&self, at: usize) -> impl Iterator<Item = (usize, u32)> + '_ {
        self.roles[at].published.iter().flat_map(|(_, readers)| readers.iter().copied())
    }

    /// What the roots have folded in so far, which is where the second sweep starts from.
    /// Counts a finished key set, and the keys of it the bitmap did not take.
    fn tally(&self, keys: &Keys) {
        self.sets.fetch_add(1, Ordering::Relaxed);
        let hashed = u64::try_from(keys.spread.len()).unwrap_or(u64::MAX);
        self.hashed.fetch_add(hashed, Ordering::Relaxed);
    }

    fn accumulators(&self) -> Result<Vec<Accumulator>> {
        let held = self.extremes.lock().map_err(poisoned)?;
        Ok(held.clone())
    }
}

/// The sink at the end of one relation's scan, which runs the first sweep over it.
#[derive(Debug)]
pub(crate) struct Collect<'a> {
    shared: Arc<Reduction>,
    at: usize,
    /// Where the keys this relation kept go for other scans, one edge per reader in the order of
    /// [`Reduction::readers`].
    feeds: Vec<Arc<Sideways<'a>>>,
    /// Per extreme, the table and column it is read from when it is read late, for the second
    /// sweep when this relation is the last to finish.
    fetches: Vec<Option<(&'a Table, usize)>>,
}

impl<'a> Collect<'a> {
    /// The sink for relation `at` of the tree, handing what it keeps to `feeds` as well.
    pub(crate) fn new(
        shared: Arc<Reduction>,
        at: usize,
        feeds: Vec<Arc<Sideways<'a>>>,
        fetches: Vec<Option<(&'a Table, usize)>>,
    ) -> Self {
        Self { shared, at, feeds, fetches }
    }

    /// Whether this relation is the last of its tree to finish, which is known once every other
    /// relation's pipeline has run, because they all run before this one's lease is taken.
    fn last(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire) + 1 == self.shared.roles.len()
    }

    /// The rows of `chunk` that `selection` keeps, as the second sweep reads them. The keys are
    /// the ones `sink` read into `local.values` for this chunk.
    fn hold(&self, chunk: &Chunk, selection: &Selection, local: &mut Collecting) -> Result<Held> {
        let role = &self.shared.roles[self.at];
        let kept = selection.indices();
        let gather =
            |values: &[i64]| -> Vec<i64> { kept.iter().map(|&row| values[row as usize]).collect() };
        let keys = role
            .parent
            .iter()
            .copied()
            .chain(role.trailing.iter().map(|&(_, key)| key))
            .chain(role.down.iter().map(|&(_, key)| key));
        let mut numbers: Vec<Vec<i64>> = keys.map(|key| gather(&local.values[key])).collect();
        let mut values = Vec::new();
        for &(output, column) in &role.extremes {
            let vector = chunk.column(column)?;
            if self.fetches[output].is_some() {
                read(vector, chunk.len(), &mut local.places, &mut local.nulls)?;
                numbers.push(gather(&local.places));
            } else {
                values.push(vector.clone());
            }
        }
        let values = if values.is_empty() {
            None
        } else {
            Some(Chunk::with_rows(values, chunk.len())?.compact(selection)?)
        };
        Ok(Held { numbers, values, rows: kept.len() })
    }
}

/// What one instance of a relation's sink builds.
#[derive(Debug)]
pub(crate) struct Collecting {
    /// The keys handed up to the parent.
    up: Keys,
    /// The keys handed to other scans, in the order of [`Role::published`], with the one in the
    /// parent's class left empty because `up` is the same set.
    published: Vec<Keys>,
    /// Per child the second sweep goes on to, the keys a root allows it, in the order of
    /// [`Role::down`].
    down: Vec<Keys>,
    /// The extremes, for a root, and nothing otherwise.
    extremes: Option<Vec<Accumulator>>,
    /// The rows held for the second sweep.
    held: Vec<Held>,
    charged: Reservation,
    kept: u64,
    /// One column of keys at a time, reused from chunk to chunk.
    values: Vec<Vec<i64>>,
    nulls: Vec<bool>,
    /// The row places of an extreme read late, reused the same way.
    places: Vec<i64>,
}

/// The rows of one chunk that a relation kept for the second sweep.
///
/// The keys are held as the integers the sink already read them as, gathered down to the kept rows,
/// and so are the row places of the extremes read late. Holding them as the vectors they came in
/// was a gather of each one through whatever form it was in, a run or a dictionary or the ids of a
/// link join, and then a second read of the same keys in the second sweep. On JOB 8c that gather
/// was a quarter of the query.
#[derive(Debug)]
struct Held {
    /// The parent's key, the keys of the relations that trail it, the keys of `down` and the row
    /// places of the extremes read late, in that order, one value per kept row.
    numbers: Vec<Vec<i64>>,
    /// The columns of the extremes read here rather than late, for the kept rows.
    values: Option<Chunk>,
    rows: usize,
}

impl Sink for Collect<'_> {
    type Local = Collecting;

    fn local(&self) -> Collecting {
        let role = &self.shared.roles[self.at];
        // Only a root reads extremes and hands keys down as it goes, and not one that relations
        // trail, which does both in the second sweep.
        let root = role.parent.is_none() && role.trailing.is_empty();
        Collecting {
            up: Keys::default(),
            published: role.published.iter().map(|_| Keys::default()).collect(),
            down: if root {
                role.down.iter().map(|_| Keys::default()).collect()
            } else {
                Vec::new()
            },
            extremes: (root && !role.extremes.is_empty()).then(|| self.shared.fresh.clone()),
            held: Vec::new(),
            charged: self.shared.memory.reservation(),
            kept: 0,
            values: role.keys.iter().map(|_| Vec::new()).collect(),
            nulls: Vec::new(),
            places: Vec::new(),
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
            allowed.filter(&local.values[key], &mut kept);
        }
        if kept.is_empty() {
            return Ok(Progress::More);
        }
        local.kept += kept.len() as u64;
        if let Some(parent) = role.parent {
            local.up.insert_rows(&local.values[parent], &kept);
        }
        for (slot, &(key, _)) in role.published.iter().enumerate() {
            if Some(key) == role.parent {
                continue;
            }
            local.published[slot].insert_rows(&local.values[key], &kept);
        }
        if role.parent.is_none() && role.trailing.is_empty() {
            for (slot, &(_, key)) in role.down.iter().enumerate() {
                local.down[slot].insert_rows(&local.values[key], &kept);
            }
        }
        let selection = Selection::from_indices(kept);
        if let Some(extremes) = local.extremes.as_mut() {
            fold(extremes, &role.extremes, chunk, &selection)?;
        }
        if role.held.is_some() {
            let taken = self.hold(chunk, &selection, local)?;
            let footprint = taken.numbers.len() * taken.rows * size_of::<i64>()
                + taken.values.as_ref().map_or(0, Chunk::footprint);
            local.charged.grow(u64::try_from(footprint).unwrap_or(u64::MAX))?;
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
        let mut publishing = shared.publishing[self.at].lock().map_err(poisoned)?;
        for (held, keys) in publishing.iter_mut().zip(local.published) {
            held.merge(keys);
        }
        drop(publishing);
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

    fn finalize(&self, threads: &Lease<'_>) -> Result<()> {
        let shared = &self.shared;
        let role = &shared.roles[self.at];
        if *shared.kept[self.at].lock().map_err(poisoned)? == 0 {
            shared.empty.store(true, Ordering::Relaxed);
        }
        let up = std::mem::take(&mut *shared.gathering[self.at].lock().map_err(poisoned)?);
        let published = std::mem::take(&mut *shared.publishing[self.at].lock().map_err(poisoned)?);
        if role.parent.is_some() {
            shared.tally(&up);
        }
        for (&(key, _), keys) in role.published.iter().zip(&published) {
            if Some(key) != role.parent {
                shared.tally(keys);
            }
        }
        let mut feeds = self.feeds.iter();
        for ((key, readers), keys) in role.published.iter().zip(&published) {
            let keys = if Some(*key) == role.parent { &up } else { keys };
            for _ in readers {
                let Some(feed) = feeds.next() else { break };
                // A key outside the bitmap would be one the scan's bitmap turns away, so a set with
                // any is not handed over and the sink alone tests it.
                if feed.binding().is_some() && keys.spread.is_empty() {
                    feed.found(Found::kept(keys.words.clone(), feed.exact()));
                }
            }
        }
        let _ = shared.up[self.at].set(up);
        if role.trailing.is_empty() {
            for &(child, _) in &role.down {
                let allowed =
                    std::mem::take(&mut *shared.allowing[child].lock().map_err(poisoned)?);
                shared.tally(&allowed);
                let _ = shared.allowed[child].set(allowed);
            }
        }
        // The last relation to finish runs the second sweep on the threads its pipeline leased,
        // which `finalize_degree` asked to be all of them, rather than leave it to the source of the
        // one row, whose pipeline runs on one.
        if shared.finished.fetch_add(1, Ordering::AcqRel) + 1 == shared.roles.len() {
            let answer = sweep(shared, &self.fetches, Some(threads))?;
            *shared.answered.lock().map_err(poisoned)? = Some(answer);
        }
        Ok(())
    }

    fn finalize_degree(&self, ceiling: usize) -> usize {
        if self.last() { ceiling } else { 1 }
    }
}

/// Whether relation `leaf` is `ancestor` or somewhere under it.
fn below(tree: &Reducer, leaf: usize, ancestor: usize) -> bool {
    let mut at = Some(leaf);
    while let Some(here) = at {
        if here == ancestor {
            return true;
        }
        at = tree.leaves[here].parent.map(|edge| edge.leaf as usize);
    }
    false
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

/// The most row places read in one go when an extreme is read late, which is one chunk, the most
/// rows a chunk may hold.
const FETCHED: usize = rudb_vector::VECTOR_SIZE;

/// Reads the column `column` of `table` at the rows `places` names and folds it into `extreme`,
/// a batch at a time over the threads of `lease`. `fresh` is an empty accumulator of the same kind.
fn fetch(
    extreme: &mut Accumulator,
    fresh: &Accumulator,
    table: &Table,
    column: usize,
    ty: &LogicalType,
    mut places: Vec<i64>,
    lease: Option<&Lease<'_>>,
) -> Result<()> {
    places.sort_unstable();
    places.dedup();
    let places = places
        .into_iter()
        .map(|place| u64::try_from(place).map_err(|_| Error::internal("a negative row place")))
        .collect::<Result<Vec<u64>>>()?;
    let types = std::slice::from_ref(ty);
    let batches: Vec<&[u64]> = places.chunks(FETCHED).collect();
    let widest = lease.map_or(1, Lease::degree).min(batches.len());
    let built = spread(lease, widest, batches.len(), &|| fresh.clone(), &|folded, at| {
        let read = table.rows().rows_at(types, &[column], batches[at])?;
        folded.update_run(std::slice::from_ref(read.column(0)?), read.len())
    })?;
    for folded in &built {
        extreme.combine(folded)?;
    }
    Ok(())
}

/// The source of the one row, which runs the second sweep before it produces it.
#[derive(Debug)]
pub(crate) struct Answer<'a> {
    shared: Arc<Reduction>,
    schema: Schema,
    one: Handout,
    /// Per extreme, the table and column it is read from when it is read late.
    fetches: Vec<Option<(&'a Table, usize)>>,
    /// Where the shape of the key sets is reported, when the plan is watched.
    counters: Option<Arc<Counters>>,
}

impl<'a> Answer<'a> {
    /// The source for a node whose relations are `shared`, producing `schema`, with the extremes
    /// `fetches` names read late.
    pub(crate) fn new(
        shared: Arc<Reduction>,
        schema: Schema,
        fetches: Vec<Option<(&'a Table, usize)>>,
    ) -> Self {
        Self { shared, schema, one: Handout::new(1), fetches, counters: None }
    }

    /// Reports the shape of the key sets. See [`KeySets`].
    #[must_use]
    pub(crate) fn watched(mut self, counters: Arc<Counters>) -> Self {
        self.counters = Some(counters);
        self
    }

    /// What this produces, which is one column per extreme.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The extremes, or nothing when the join turned out to be empty.
    ///
    /// The last relation to finish has usually run the second sweep already, on its own lease, and
    /// left the answer here. See [`Collect::finalize`].
    fn answer(&self) -> Result<Option<Vec<Accumulator>>> {
        if let Some(answered) = self.shared.answered.lock().map_err(poisoned)?.take() {
            return Ok(answered);
        }
        sweep(&self.shared, &self.fetches, None)
    }
}

/// How many held rows the second sweep gives each thread at the least.
///
/// Splitting a relation's held rows costs a wake and a merge of the key sets per thread, which is
/// about what sweeping this many rows costs.
const SWEPT: usize = 32_768;

/// What one thread of the second sweep builds over the held rows of one relation.
#[derive(Debug)]
struct Sweeping {
    down: Vec<Keys>,
    extremes: Vec<Accumulator>,
    places: Vec<Vec<i64>>,
    survived: usize,
}

impl Sweeping {
    fn new(shared: &Reduction, role: &Role) -> Self {
        Self {
            down: role.down.iter().map(|_| Keys::default()).collect(),
            extremes: shared.fresh.clone(),
            places: role.extremes.iter().map(|_| Vec::new()).collect(),
            survived: 0,
        }
    }

    /// Keeps the rows of `held` whose keys are allowed, and folds them in.
    fn chunk(
        &mut self,
        role: &Role,
        held: &Held,
        permitted: Option<&Keys>,
        trailed: &[&Keys],
        fetches: &[Option<(&Table, usize)>],
    ) -> Result<()> {
        let rows = u32::try_from(held.rows).map_err(|_| Error::internal("a huge chunk"))?;
        let mut kept: Vec<u32> = (0..rows).collect();
        let first = usize::from(role.parent.is_some());
        // The parent key is the first held column and the keys of the relations that trail it
        // come next, which `Collect::hold` put there.
        if let Some(permitted) = permitted {
            permitted.filter(&held.numbers[0], &mut kept);
        }
        for (slot, keys) in trailed.iter().enumerate() {
            if kept.is_empty() {
                break;
            }
            keys.filter(&held.numbers[first + slot], &mut kept);
        }
        if kept.is_empty() {
            return Ok(());
        }
        self.survived += kept.len();
        let after = first + trailed.len();
        for (slot, keys) in self.down.iter_mut().enumerate() {
            keys.insert_rows(&held.numbers[after + slot], &kept);
        }
        let mut late = after + role.down.len();
        let mut direct = Vec::with_capacity(role.extremes.len());
        for (slot, &(output, _)) in role.extremes.iter().enumerate() {
            if fetches[output].is_some() {
                let places = &held.numbers[late];
                self.places[slot].extend(kept.iter().map(|&row| places[row as usize]));
                late += 1;
            } else {
                direct.push((output, direct.len()));
            }
        }
        match &held.values {
            Some(values) => {
                fold(&mut self.extremes, &direct, values, &Selection::from_indices(kept))
            }
            None => Ok(()),
        }
    }

    /// Takes in what another thread built over other chunks of the same relation.
    fn merge(&mut self, role: &Role, other: Self) -> Result<()> {
        for (keys, more) in self.down.iter_mut().zip(other.down) {
            keys.merge(more);
        }
        for &(output, _) in &role.extremes {
            self.extremes[output].combine(&other.extremes[output])?;
        }
        for (places, more) in self.places.iter_mut().zip(other.places) {
            places.extend(more);
        }
        self.survived += other.survived;
        Ok(())
    }
}

/// Runs `work` over the indices below `count` on up to `widest` threads of `lease`, each with a
/// state from `start`, and hands back the states.
///
/// The indices are taken one at a time off a counter, so a thread that drew short pieces takes more
/// of them. The first error any thread met is the one returned.
fn spread<T: Send>(
    lease: Option<&Lease<'_>>,
    widest: usize,
    count: usize,
    start: &(dyn Fn() -> T + Sync),
    work: &(dyn Fn(&mut T, usize) -> Result<()> + Sync),
) -> Result<Vec<T>> {
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<T>> = Mutex::new(Vec::new());
    let failed: Mutex<Option<Error>> = Mutex::new(None);
    let run = || {
        let mut state = start();
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            if at >= count {
                break;
            }
            if let Err(error) = work(&mut state, at) {
                if let Ok(mut failed) = failed.lock() {
                    failed.get_or_insert(error);
                }
                next.store(count, Ordering::Relaxed);
                break;
            }
        }
        if let Ok(mut done) = done.lock() {
            done.push(state);
        }
    };
    let panicked = match lease {
        Some(lease) if widest > 1 => lease.scatter_at_most(widest, &run, run).1,
        _ => {
            run();
            false
        }
    };
    if panicked {
        return Err(Error::internal("a thread panicked in the second sweep"));
    }
    if let Some(error) = failed.into_inner().map_err(poisoned)? {
        return Err(error);
    }
    done.into_inner().map_err(poisoned)
}

/// The second sweep, from the roots down, over the rows the relations held.
///
/// Each relation's held chunks are split over the threads of `lease` when there are enough rows to
/// pay for it. On JOB 8c this is four million held rows of `aka_name`, `cast_info`, `title` and
/// `movie_companies`, and on one thread it was 75 of the query's 130 milliseconds with the rest of
/// the machine parked.
fn sweep(
    shared: &Reduction,
    fetches: &[Option<(&Table, usize)>],
    lease: Option<&Lease<'_>>,
) -> Result<Option<Vec<Accumulator>>> {
    if shared.empty.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let mut extremes = shared.accumulators()?;
    let count = shared.roles.len();
    let mut allowed: Vec<Option<Keys>> = (0..count).map(|_| None).collect();
    let threads = lease.map_or(1, Lease::degree);
    for &at in &shared.sweep {
        let role = &shared.roles[at];
        let Some(columns) = &role.held else { continue };
        // A child of a root was allowed its keys by the root's sink, and every other held
        // relation by its parent a step earlier in this loop. A root that relations trail is
        // allowed what they kept instead.
        let owned = allowed[at].take();
        let permitted: Option<&Keys> = match &owned {
            Some(keys) => Some(keys),
            None if role.parent.is_none() => None,
            None => Some(
                shared.allowed[at]
                    .get()
                    .ok_or_else(|| Error::internal("a held relation with no allowed keys"))?,
            ),
        };
        let mut trailed = Vec::with_capacity(role.trailing.len());
        for &(child, _) in &role.trailing {
            trailed.push(shared.up[child].get().ok_or_else(|| {
                Error::internal("a root was swept before a relation that trails it finished")
            })?);
        }
        let first = usize::from(role.parent.is_some());
        let chunks = std::mem::take(&mut *shared.held[at].lock().map_err(poisoned)?);
        let rows: usize = chunks.iter().map(|held| held.rows).sum();
        let widest = threads.min(chunks.len()).min(rows / SWEPT + 1);
        let built = spread(
            lease,
            widest,
            chunks.len(),
            &|| Sweeping::new(shared, role),
            &|state, index| state.chunk(role, &chunks[index], permitted, &trailed, fetches),
        )?;
        drop(chunks);
        let mut built = built.into_iter();
        let mut swept = built.next().unwrap_or_else(|| Sweeping::new(shared, role));
        for other in built {
            swept.merge(role, other)?;
        }
        for &(output, _) in &role.extremes {
            extremes[output].combine(&swept.extremes[output])?;
        }
        for (&(output, _), places) in role.extremes.iter().zip(swept.places) {
            if let Some((table, column)) = fetches[output] {
                let fresh = &shared.fresh[output];
                let ty = &shared.types[output];
                fetch(&mut extremes[output], fresh, table, column, ty, places, lease)?;
            }
        }
        debug_assert_eq!(
            columns.len(),
            first + trailed.len() + role.down.len() + role.extremes.len()
        );
        if swept.survived == 0 {
            return Ok(None);
        }
        for (&(child, _), keys) in role.down.iter().zip(swept.down) {
            shared.tally(&keys);
            allowed[child] = Some(keys);
        }
    }
    Ok(Some(extremes))
}

impl Source for Answer<'_> {
    fn morsel(&self) -> Option<Morsel> {
        self.one.take()
    }

    fn morsels(&self, _threads: usize, _weight: usize) -> Option<usize> {
        Some(self.one.total())
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let answered = self.answer()?;
        if let Some(counters) = &self.counters {
            counters.classing(KeySets {
                sets: self.shared.sets.load(Ordering::Relaxed),
                hashed: self.shared.hashed.load(Ordering::Relaxed),
            });
        }
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

    #[test]
    fn rows_put_in_and_kept_a_chunk_at_a_time_match_a_key_at_a_time() {
        let far = i64::try_from(DENSE).expect("fits") + 9;
        let values: Vec<i64> = vec![5, -1, 64, 5, 200_000, far, 0, 63, i64::MIN, 7, 1 << 40, 128];
        let rows: Vec<u32> = vec![0, 2, 3, 4, 6, 7, 9, 11];
        for with_far in [false, true] {
            let mut batch = Keys::default();
            let mut single = Keys::default();
            let put: Vec<u32> = if with_far { vec![0, 1, 5, 6, 11] } else { vec![0, 6, 11] };
            batch.insert_rows(&values, &put);
            for &row in &put {
                single.insert(values[row as usize]);
            }
            for &value in &values {
                assert_eq!(batch.contains(value), single.contains(value), "{value}");
            }
            let mut kept = rows.clone();
            batch.filter(&values, &mut kept);
            let want: Vec<u32> =
                rows.iter().copied().filter(|&row| single.contains(values[row as usize])).collect();
            assert_eq!(kept, want, "with far keys {with_far}");
        }
        let mut empty = Keys::default();
        empty.insert_rows(&values, &[]);
        let mut kept = rows.clone();
        empty.filter(&values, &mut kept);
        assert!(kept.is_empty());
    }
}

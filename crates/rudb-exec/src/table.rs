//! The hash table a group by probes, held column at a time.
//!
//! `key.rs` answers what one row is, and this answers how to find that row again quickly. The two
//! live next to each other rather than one replacing the other because they are asked different
//! questions. A `DISTINCT` inside an aggregate is handed one argument at a time and has nowhere
//! better to put it than a [`Value`]. A group by is handed a thousand rows that are already sitting
//! in vectors, and building a `Vec<Value>` per row in order to ask about it is where most of
//! ClickBench went: #237 measured `GROUP BY WatchID, ClientIP` at ten times the pinned binary, on a
//! key that never needed to leave the vectors it arrived in.
//!
//! So this table never sees a row. It is given the key vectors and a row number, it hashes a column
//! at a time into one word per row before the row loop starts, and it compares the key it stored
//! against the vector in place. A new group costs a push onto one flat vector per key column, which
//! is an allocation amortized over the groups rather than one `Vec<Value>` from the allocator for
//! every group in the answer.
//!
//! # What the hash has to agree with
//!
//! Two values that [`same`] calls one value have to hash to one word, or the table holds two entries
//! for one group and the second is never found again. That is the whole invariant, and the thing
//! that makes it delicate here is that the same column arrives in more than one form. A `VARCHAR`
//! read out of a parquet file is a dictionary in one chunk and flat in the next, and if the fast
//! path over a flat column and the general path over a dictionary disagreed by so much as a
//! discriminant, a group by would answer with the same string twice.
//!
//! [`fold`] is therefore written so that every form of a column produces the same word for the same
//! value. What that costs is the type tag: the general path does not mix the discriminant the way
//! [`Key`](crate::key::Key) does, because a typed arm over a run of `i32` has no discriminant to
//! mix. It is not needed. Every value in a column has that column's type, so within one column the
//! tag is a constant, and two key columns of different types are told apart by their position in the
//! fold and by the comparison that follows the probe.

use rudb_common::{Error, Result, Value, interval_micros};
use rudb_vector::{Data, Packed, Vector};
use std::sync::Arc;

use crate::key::{canonical, mix, same, spread};
use crate::rows;

/// What the slot half of a bucket holds when the bucket holds nothing.
const EMPTY: u32 = u32::MAX;

/// The most groups one of these can hold.
///
/// A slot is a `u32` because it shares its bucket with the salt beside it, and the two together are
/// one aligned word that a probe reads in one load. The bound that leaves is four billion groups,
/// which at the width of a key is a hundred gigabytes of them, so a query that reaches it has run
/// out of memory in every sense that matters and the only question is which error says so.
const LIMIT: usize = EMPTY as usize;

/// How many buckets a table starts with.
///
/// Small, because most group bys in a query have few groups, and the doubling gets to a large table
/// in the twenty steps that a table with many of them needs.
const FIRST: usize = 64;

/// How many rows [`Table::probe_run`] walks at once.
///
/// Large enough that the misses it issues together fill the queue a core keeps outstanding, and small
/// enough that the four buffers it walks with, and the buckets it touched on the way, are still in
/// the first level of cache when the caller comes back for the rows that missed.
pub(crate) const BATCH: usize = 64;

/// Below this many buckets a table is small enough to probe a row at a time.
///
/// The batch below buys one thing, which is cache misses that overlap instead of queueing. A table
/// of eight thousand buckets is sixty four kilobytes and there are no misses to overlap, so what is
/// left is the cost of the batch, and a group by over a handful of groups is a query where that is
/// the whole of the time.
const HOT: usize = 8 * 1024;

/// The word a null contributes to the hash.
///
/// A constant rather than nothing at all, so that a null in a column of zeroes does not hash as a
/// zero. It can collide with a real value that happens to be this pattern, which costs one
/// comparison and no correctness, because the comparison is what decides.
const NOTHING: u64 = 0x9e37_79b9_7f4a_7c15;

/// A bucket holding nothing, which is [`EMPTY`] in its slot half and a salt that is never read.
const VACANT: u64 = EMPTY as u64;

/// The part of a hash that is kept in the bucket beside the slot.
///
/// The top thirty two bits, because the bottom ones are the bucket number and would say nothing: two
/// keys only ever meet in a bucket by having the same bottom bits already. The top ones are the bits
/// a linear probe has not looked at yet, so they are the ones that can tell two keys apart.
fn salt_of(hash: u64) -> u32 {
    (hash >> 32) as u32
}

/// One bucket, which is a salt over a slot in a single word.
///
/// The two together rather than in two vectors is the whole point of the layout. A probe used to
/// read the bucket and then read the stored hash of whatever slot was in it, and the second address
/// is only known once the first load has landed, so every probe step on a table larger than the
/// cache was two trips to memory one after the other. With the salt in the bucket the first load
/// settles all but about one step in four billion, and the second trip is to the key itself, which
/// is the comparison that has to happen anyway.
fn bucket_of(salt: u32, slot: usize) -> u64 {
    (u64::from(salt) << 32) | slot as u64
}

/// The slot half of a bucket, which is [`EMPTY`] if it holds nothing.
fn slot_of(bucket: u64) -> u32 {
    bucket as u32
}

/// The salt half of a bucket, which means nothing unless the slot half is not [`EMPTY`].
fn bucket_salt(bucket: u64) -> u32 {
    (bucket >> 32) as u32
}

/// A hash table from a row of key columns to the slot its group was given.
///
/// The slot is the number of groups seen before this one, so the answer comes out in the order the
/// groups were first seen, which is what the operator above this relies on and what makes a failing
/// test a diff rather than an investigation.
#[derive(Debug)]
pub(crate) struct Table {
    /// One salt and slot per bucket, [`VACANT`] where there is no group. A power of two long, so
    /// the bucket a hash belongs to is a mask rather than a division.
    buckets: Vec<u64>,
    /// The stored keys, column at a time. `columns[column][slot]` is one group's value in one key
    /// column, which is the layout that lets a group be pushed without asking the allocator for a
    /// row to put it in.
    columns: Vec<Column>,
    /// The hash of each group's key. Not read by a probe, which has the salt in the bucket instead.
    /// It is here for the two places that would otherwise hash a key that was hashed once already,
    /// which are growing the buckets and merging one of these tables into another.
    hashes: Vec<u64>,
    /// What the stored keys own away from themselves, which is the strings and blobs among them.
    owned: u64,
}

/// What a probe found, which is either a group or the bucket a new one goes in.
///
/// The bucket is handed back rather than looked for again by [`Table::insert`] because the probe
/// has already walked to it, and walking a second time is the cost of the first one over again on
/// exactly the rows where the table is about to grow.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Probe {
    /// The row's key is the group in this slot.
    Found(usize),
    /// The row's key is not in the table, and this is the bucket it would go in.
    Vacant(usize),
}

impl Table {
    /// An empty table over a key of `columns` columns.
    pub(crate) fn new(types: &[rudb_common::LogicalType]) -> Self {
        Self {
            buckets: vec![VACANT; FIRST],
            columns: types.iter().map(Column::new).collect(),
            hashes: Vec::new(),
            owned: 0,
        }
    }

    /// How many groups are in it.
    pub(crate) fn len(&self) -> usize {
        self.hashes.len()
    }

    /// The hash of the group in `slot`.
    ///
    /// Only wanted by a merge of two tables, which probes this table's groups against another one
    /// and would otherwise hash keys that were hashed once already. The two tables came from the
    /// same operator and so hashed the same way, which is what makes reusing the number sound.
    ///
    /// # Panics
    ///
    /// If `slot` is not a group in this table, which is a bug in the caller.
    pub(crate) fn hash_of(&self, slot: usize) -> u64 {
        self.hashes[slot]
    }

    /// What the stored keys own away from themselves.
    ///
    /// Charged separately from [`Self::footprint`] because it is charged for longer. The strings a
    /// key holds move out of this table and into the rows the answer is built from, and survive it,
    /// while everything `footprint` counts is given back when the table goes.
    pub(crate) fn owned(&self) -> u64 {
        self.owned
    }

    /// What this has taken from the allocator, capacity rather than length in all three parts.
    ///
    /// Capacity for the reason #227 gives. A `Vec` doubles and so sits between half empty and full,
    /// and charging the used half of a structure that paid for all of it is how a query passes a
    /// limit it was told it was inside of.
    pub(crate) fn footprint(&self) -> u64 {
        let buckets = self.buckets.capacity() * size_of::<u64>();
        let hashes = self.hashes.capacity() * size_of::<u64>();
        let keys: usize = self.columns.iter().map(Column::footprint).sum();
        u64::try_from(buckets + hashes + keys).unwrap_or(u64::MAX)
    }

    /// Looks for the key that `keys` holds at `row`.
    ///
    /// `hash` is that row's entry in what [`hash`] built for the chunk. Passing it in rather than
    /// computing it here is the point of the whole arrangement: the hash of a column of a thousand
    /// rows is one pass over a run of `i32` with the type dispatch done once, and doing it per row
    /// inside the probe would put the dispatch back.
    ///
    /// A step that gets past the salt goes straight to the key rather than to the stored hash. The
    /// salt is already thirty two bits of the hash the bucket number did not cover, so the stored
    /// hash would rule out about one step in four billion and cost a load from memory on every one
    /// of the others. The key comparison is exact, so what the salt lets through it settles.
    pub(crate) fn probe(&self, hash: u64, keys: &[Vector], row: usize) -> Probe {
        let mask = self.buckets.len() - 1;
        let salt = salt_of(hash);
        let mut at = (hash as usize) & mask;
        loop {
            let bucket = self.buckets[at];
            let slot = slot_of(bucket);
            if slot == EMPTY {
                return Probe::Vacant(at);
            }
            let slot = slot as usize;
            if bucket_salt(bucket) == salt && self.holds(slot, keys, row) {
                return Probe::Found(slot);
            }
            at = (at + 1) & mask;
        }
    }

    /// Looks for a run of rows at once, which is where the time in a group by goes.
    ///
    /// [`Self::probe`] is two loads deep on every row. It reads the bucket, then the key beside the
    /// slot the bucket held, and the second address is only known once the first load has landed. On
    /// a table larger than the cache both are misses, so a row costs two trips to memory end to end
    /// and the core has nothing to get on with while it waits. A probe of a single `INTEGER` key
    /// measured at seventy two nanoseconds a row when it was three deep, which is not work, it is
    /// waiting.
    ///
    /// Rows are independent of each other, so this walks [`BATCH`] of them together and does one kind
    /// of load at a time across all of them: every bucket, then every key. Inside a pass every
    /// address is known before the pass starts, so the misses are all outstanding at once and the
    /// batch waits about as long as one row used to.
    ///
    /// Rows whose walk reaches an empty bucket go on `pending` in row order instead of being
    /// inserted here. An insert moves the table under the rest of the batch, and two rows in one
    /// batch can be the first two rows of one group, so both have to go through one path that sees
    /// them in order. The caller finishes them with [`Self::probe`] and [`Self::insert`], and the
    /// second probe is cheap because the buckets it walks are the ones this just read.
    ///
    /// `slots` is left alone for a pending row, so the caller's own idea of what an unfilled slot
    /// means is what survives.
    pub(crate) fn probe_run(
        &self,
        hashes: &[u64],
        keys: &[Vector],
        from: usize,
        upto: usize,
        slots: &mut [usize],
        walk: &mut Walk,
    ) {
        self.probe_at(hashes, keys, Rows::Run(from, upto), &mut slots[from..upto], walk);
        // Back from a place in the batch to a row, because a run of rows is what this caller asked
        // about and a row is what it wants to hear about.
        for place in &mut walk.pending {
            *place += from;
        }
    }

    /// The same, for a list of rows rather than a run of them.
    ///
    /// `slots` is as long as `rows` and is written by place rather than by row, and the places
    /// [`Walk::pending`] hands back index `rows` the same way. A join builds its table in
    /// partitions and the rows of a partition are scattered through the side, so what one thread
    /// probes with is a list.
    pub(crate) fn probe_these(
        &self,
        hashes: &[u64],
        keys: &[Vector],
        rows: &[usize],
        slots: &mut [usize],
        walk: &mut Walk,
    ) {
        self.probe_at(hashes, keys, Rows::These(rows), slots, walk);
    }

    /// The batched probe itself, answering by place in the batch.
    fn probe_at(
        &self,
        hashes: &[u64],
        keys: &[Vector],
        rows: Rows<'_>,
        slots: &mut [usize],
        walk: &mut Walk,
    ) {
        let mask = self.buckets.len() - 1;
        walk.pending.clear();
        if self.buckets.len() <= HOT {
            for (out, found) in slots.iter_mut().enumerate().take(rows.len()) {
                let row = rows.at(out);
                match self.probe(hashes[row], keys, row) {
                    Probe::Found(slot) => *found = slot,
                    Probe::Vacant(_) => walk.pending.push(out),
                }
            }
            return;
        }
        walk.here.clear();
        walk.here.extend((0..rows.len()).map(|out| {
            let row = rows.at(out);
            Step { row, out, at: (hashes[row] as usize) & mask }
        }));
        while !walk.here.is_empty() {
            // The bucket of every row still walking, which is the only pass that goes to memory
            // ahead of the keys.
            walk.seen.clear();
            walk.seen.extend(walk.here.iter().map(|step| self.buckets[step.at]));
            // Which of those buckets could hold the row's group, off the salt that came back with
            // the slot. No load of its own: the buckets were just written and the hashes are read in
            // row order, so this pass is arithmetic over what is already in the first level cache.
            walk.same.clear();
            walk.same.extend(walk.here.iter().zip(&walk.seen).map(|(step, &bucket)| {
                slot_of(bucket) != EMPTY && bucket_salt(bucket) == salt_of(hashes[step.row])
            }));
            // The keys, one column at a time, so the type of the stored column and the form of the
            // vector it is compared against are matched on once for the batch rather than once for
            // every row and every column. Each column narrows what the pass above left marked.
            for (at, column) in keys.iter().enumerate() {
                self.columns[at].holds_run(&walk.here, &walk.seen, column, &mut walk.same);
            }
            // What is left, which is the only pass that branches per row and the only one that can
            // end a row's walk. A row that is neither a hit nor a vacancy moves along one bucket and
            // comes back around, so a run of collisions costs passes rather than a walk per row.
            walk.next.clear();
            for ((step, &bucket), &same) in walk.here.iter().zip(&walk.seen).zip(&walk.same) {
                let slot = slot_of(bucket);
                if slot == EMPTY {
                    walk.pending.push(step.out);
                } else if same {
                    slots[step.out] = slot as usize;
                } else {
                    walk.next.push(Step { row: step.row, out: step.out, at: (step.at + 1) & mask });
                }
            }
            std::mem::swap(&mut walk.here, &mut walk.next);
        }
        // In row order, because the slot a group is given is the order the answer comes out in, and
        // the passes above reach a vacancy in whatever order the walks happen to end. The places
        // sort into row order too, because a batch is given in row order either way it is given.
        walk.pending.sort_unstable();
    }

    /// Adds the key that `keys` holds at `row` in the bucket a probe of the same row left vacant.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::OutOfMemory`] at [`LIMIT`] groups.
    pub(crate) fn insert(
        &mut self,
        bucket: usize,
        hash: u64,
        keys: &[Vector],
        row: usize,
    ) -> Result<usize> {
        let slot = self.hashes.len();
        if slot >= LIMIT {
            return Err(Error::out_of_memory(format!(
                "a single group by cannot hold more than {LIMIT} groups"
            )));
        }
        for (at, column) in keys.iter().enumerate() {
            self.owned += self.columns[at].push_from(column, row)?;
        }
        self.hashes.push(hash);
        self.buckets[bucket] = bucket_of(salt_of(hash), slot);
        // Half full rather than the seven eighths a `HashMap` allows, because this probes linearly
        // and a linear probe at seven eighths walks a run of about eight buckets to find a miss.
        // The buckets are eight bytes each, so the room the other half costs is small next to the
        // keys beside it.
        if self.hashes.len() * 2 >= self.buckets.len() {
            self.regrow();
        }
        Ok(slot)
    }

    /// Whether the group in `slot` has the key that `keys` holds at `row`.
    ///
    /// The string case is the one worth writing out. [`Vector::value_at`] on a `VARCHAR` allocates a
    /// `String` per call, and this is called at least once per input row of a group by over one, so
    /// the comparison reads the bytes where they already are. Everything else owns nothing, so
    /// `value_at` on it is a copy of a few bytes and going through [`same`] keeps the one definition
    /// of what groups together.
    ///
    /// [`Vector::bytes_at`] hands back nothing for the forms that do not store their text per
    /// position, and a caller that reads that as a difference is a caller that never finds a group
    /// again. A nested loop join makes its left side constant vectors, so a group by on a string
    /// column from the left of a join is exactly that case, and it answered with one group per row.
    /// So nothing from `bytes_at` means fall through to `value_at`, which is right for every form.
    fn holds(&self, slot: usize, keys: &[Vector], row: usize) -> bool {
        for (at, column) in keys.iter().enumerate() {
            if !self.columns[at].holds(slot, column, row) {
                return false;
            }
        }
        true
    }

    /// Doubles the buckets and puts every group back in one.
    ///
    /// The keys do not move and are not looked at. A rehash reads the hash of each group, which is
    /// stored, so growing a table of seventeen million string keys touches no strings.
    fn regrow(&mut self) {
        let mut buckets = vec![VACANT; self.buckets.len() * 2];
        let mask = buckets.len() - 1;
        for (slot, &hash) in self.hashes.iter().enumerate() {
            let mut at = (hash as usize) & mask;
            while slot_of(buckets[at]) != EMPTY {
                at = (at + 1) & mask;
            }
            buckets[at] = bucket_of(salt_of(hash), slot);
        }
        self.buckets = buckets;
    }

    /// One key column of a range of groups, in slot order, as a vector.
    ///
    /// This is the whole reason the keys are stored a column at a time rather than a row at a time.
    /// A chunk of the answer wants a column, so the operator above cuts a range out of this, and no
    /// group is ever a row of its own on the way out.
    ///
    /// It builds the vector rather than handing back values for the operator to build one from,
    /// because the two widths that most grouping keys are stored in are the same two a flat vector
    /// holds. Going out through a `Vec<Value>` copied every group into a tagged value and then
    /// straight back out of one, which on a group by with a million groups was the single largest
    /// line in the profile of the operator that finishes them.
    ///
    /// # Errors
    ///
    /// If the type does not match what the column was built to store, which is a bug in the caller.
    ///
    /// # Panics
    ///
    /// If `at` is not a column of the key this table was built over, which is a bug in the caller.
    pub(crate) fn column(
        &self,
        at: usize,
        ty: &rudb_common::LogicalType,
        range: std::ops::Range<usize>,
    ) -> Result<Vector> {
        self.columns[at].vector(ty, range)
    }

    /// Selected groups of one key column, used when an aggregate can discard groups before emit.
    pub(crate) fn column_slots(
        &self,
        at: usize,
        ty: &rudb_common::LogicalType,
        slots: &[usize],
    ) -> Result<Vector> {
        let values = self.columns[at].values_at(slots);
        Vector::from_values(ty.clone(), &values)
    }
}

/// One row part way through a batched probe.
#[derive(Debug, Clone, Copy)]
struct Step {
    /// Its row in the chunk, which is where its hash and its key are.
    row: usize,
    /// Where its answer goes, which is its place in the batch rather than its row.
    ///
    /// The two are the same number for a run of rows and are not for a list of them. A join builds
    /// its table in partitions and a partition's rows are scattered through the side, so the batch
    /// it hands over is a list and the answers come back packed.
    out: usize,
    /// The bucket its walk is looking at now.
    at: usize,
}

/// Which rows of a chunk a probe is being asked about.
#[derive(Debug, Clone, Copy)]
enum Rows<'a> {
    /// Every row from the first up to the last, which is what a group by asks.
    Run(usize, usize),
    /// These and no others, in the order they are given, which is what one partition of a join's
    /// build asks.
    These(&'a [usize]),
}

impl Rows<'_> {
    /// How many rows are in the batch.
    fn len(&self) -> usize {
        match *self {
            Rows::Run(from, upto) => upto - from,
            Rows::These(rows) => rows.len(),
        }
    }

    /// The row in the `index`th place of the batch.
    fn at(&self, index: usize) -> usize {
        match *self {
            Rows::Run(from, _) => from + index,
            Rows::These(rows) => rows[index],
        }
    }
}

/// The buffers [`Table::probe_run`] walks a batch with.
///
/// Held by the caller and reused, so a chunk of a thousand rows asks the allocator for nothing. All
/// four are [`BATCH`] long at the most, which is a couple of kilobytes between them and small enough
/// that charging it against a memory budget would be noise.
#[derive(Debug, Default)]
pub(crate) struct Walk {
    /// The rows whose walk has not ended.
    here: Vec<Step>,
    /// The ones that are still walking after this pass, swapped into `here` at the end of it.
    next: Vec<Step>,
    /// What each row's bucket holds, salt and slot together.
    seen: Vec<u64>,
    /// Whether each row's key could still be the group its bucket holds.
    same: Vec<bool>,
    /// The rows of the last batch whose key was not in the table, in row order.
    pending: Vec<usize>,
}

impl Walk {
    /// The rows the last batch could not finish, in row order.
    ///
    /// Row order because the caller inserts them in this order and a group's slot is the order it was
    /// first seen in, which is the order the answer comes out in.
    pub(crate) fn pending(&self) -> &[usize] {
        &self.pending
    }
}

/// The most key columns a chunk is read through codes for.
///
/// Four, because the product of four dictionaries has passed [`COMBOS`] on every chunk this was
/// measured on, and because a fixed array of four is one fewer allocation per chunk than a `Vec`.
const KEYS: usize = 4;

/// The most code combinations the direct map covers.
///
/// The bound is about the map rather than about the key. The caller keeps one slot per combination
/// and clears it when the dictionaries change, so a map of a few thousand entries against a chunk of
/// a thousand rows is one where building it costs more than the probe it replaces. A map that lives
/// into the next chunk costs nothing at all, which is the ordinary case, but the first chunk of a
/// row group still has to pay for one.
const COMBOS: usize = 2048;

/// Where one key column's place in the combined index comes from.
///
/// Two forms rather than one, because a stored column of small integers is not a dictionary and does
/// not need to become one to be read this way. A bit packed run already holds each value as the
/// value minus a base, in as many bits as the widest value on the page needs, so its code is a place
/// in exactly the way a dictionary code is and its span is `1 << width` rather than the length of
/// anything. `l_linenumber` takes seven values and the native file stores it in three bits, and a
/// group by on it was hashing six million rows to find one of seven answers.
#[derive(Debug, Clone, Copy)]
enum Places<'a> {
    /// Codes into a dictionary, whose identity is the dictionary they point at.
    Codes {
        /// This chunk's code per row, already cut to the rows being asked about.
        codes: &'a [u32],
        /// The dictionary those codes point into, kept for its identity rather than its values.
        values: &'a Arc<Vector>,
    },
    /// A packed run, whose identity is the base and width that turn a code back into a value.
    Bits {
        /// The words and what they mean, which is all a code needs to be read.
        packed: Packed<'a>,
    },
}

/// What a direct map's places were built from, so a chunk under different ones rebuilds it.
///
/// A place only means a value against the thing it was read out of. Two pages of one column can
/// hold different dictionaries, or pack their values against different bases, and a map carried
/// from the first into the second would answer a row with some other row's group. This is what the
/// caller holds beside the map so that it can tell.
#[derive(Debug, Clone)]
pub(crate) enum Origin {
    /// A dictionary, kept by identity rather than by value.
    Dictionary(Arc<Vector>),
    /// A packed run's base and width, which are all a code needs in order to mean a value.
    Bits(i128, u32),
}

/// One key column of a chunk, read as places into a map of every combination.
#[derive(Debug, Clone, Copy)]
struct CodedColumn<'a> {
    /// Where this column's place per row comes from.
    places: Places<'a>,
    /// What this column's place is multiplied by, which is the spans of the columns before it.
    stride: usize,
    /// The place a null row takes, which is one past the last code.
    nothing: usize,
    /// Whether any row here can be null at all, asked once for the chunk.
    nullable: bool,
    /// The column itself, asked about a row only when there is a null in it to find.
    ///
    /// Through the vector rather than through the two validities beside it because a dictionary
    /// keeps its nulls in the vector it points at, and that vector can be a dictionary in its own
    /// turn. [`Vector::is_null_at`] is the one answer that follows the whole chain, and it is the
    /// answer [`Column::push_from`] and [`Column::holds`] decide by, so the place a null row takes
    /// here has to be decided by it too.
    column: &'a Vector,
}

impl CodedColumn<'_> {
    /// Adds what this column contributes to every row's place, in one pass over the column.
    ///
    /// A pass per column rather than a column loop per row, which is the same argument the hash
    /// beside it is written on. The form of the column and whether it has any null in it are both
    /// settled before the loop starts, so the loop itself is a load, a multiply and an add, and
    /// neither question is asked six million times. Doing it the other way round cost seven
    /// instructions a row on `GROUP BY l_returnflag, l_linestatus` the moment there were two forms
    /// to tell apart, which is more than the whole probe it saves.
    fn add_into(&self, into: &mut [usize]) {
        let stride = self.stride;
        if self.nullable {
            let nothing = self.nothing;
            let column = self.column;
            match self.places {
                Places::Codes { codes, .. } => {
                    for (row, place) in into.iter_mut().enumerate() {
                        let code =
                            if column.is_null_at(row) { nothing } else { codes[row] as usize };
                        *place += code * stride;
                    }
                }
                Places::Bits { packed } => {
                    for (row, place) in into.iter_mut().enumerate() {
                        let code = if column.is_null_at(row) {
                            nothing
                        } else {
                            packed.code(row) as usize
                        };
                        *place += code * stride;
                    }
                }
            }
            return;
        }
        match self.places {
            Places::Codes { codes, .. } => {
                for (row, place) in into.iter_mut().enumerate() {
                    *place += codes[row] as usize * stride;
                }
            }
            Places::Bits { packed } => {
                for (row, place) in into.iter_mut().enumerate() {
                    *place += packed.code(row) as usize * stride;
                }
            }
        }
    }
}

/// A chunk whose whole key is a few small codes, so a row's group is an index into a table.
///
/// A Parquet reader hands a low cardinality column over as a dictionary, and a group by over two of
/// those spends its time hashing the bytes each code points at and then comparing those same bytes
/// against the key the table stored, once per row. Neither question depends on the row. The
/// dictionary behind `l_returnflag` holds three values and the one behind `l_linestatus` holds two,
/// so between them there are six answers and TPC-H q1 asks for one of them six million times.
///
/// So this turns a row into the index of its combination of codes, and the caller keeps one slot per
/// combination beside it. A row whose combination has been seen costs a load of each code, a
/// multiply add and a load from a table of six, and nothing is hashed or compared at all. The rows
/// that pay the full price are the first of each combination, which is six rows in a scan of six
/// million, and they pay it through the same probe and insert every other row used to.
///
/// The codes are only meaningful against the dictionary they came from, which is why the caller
/// holds those dictionaries beside the map and throws the map away when a chunk arrives under
/// different ones. A Parquet dictionary covers a column chunk, so that happens once a row group
/// rather than once a chunk.
pub(crate) struct Coded<'a> {
    columns: [Option<CodedColumn<'a>>; KEYS],
    combos: usize,
}

impl<'a> Coded<'a> {
    /// How many combinations of codes the key can take, which is how long the map has to be.
    pub(crate) fn combos(&self) -> usize {
        self.combos
    }

    /// Fills `places` with the index in the map of each row's key, one pass per key column.
    pub(crate) fn places(&self, rows: usize, places: &mut Vec<usize>) {
        places.clear();
        places.resize(rows, 0);
        for column in self.columns.iter().flatten() {
            column.add_into(places);
        }
    }

    /// Whether these are the same things `held` was filled from, so the map still means what it
    /// meant.
    pub(crate) fn same_as(&self, held: &[Origin]) -> bool {
        let mut at = 0;
        for column in self.columns.iter().flatten() {
            let same = match (held.get(at), column.places) {
                (Some(Origin::Dictionary(dictionary)), Places::Codes { values, .. }) => {
                    Arc::ptr_eq(dictionary, values)
                }
                (Some(Origin::Bits(base, width)), Places::Bits { packed }) => {
                    *base == packed.base() && *width == packed.width()
                }
                _ => false,
            };
            if !same {
                return false;
            }
            at += 1;
        }
        at == held.len()
    }

    /// Records what the map about to be built reads its places out of.
    pub(crate) fn hold(&self, into: &mut Vec<Origin>) {
        into.clear();
        for column in self.columns.iter().flatten() {
            into.push(match column.places {
                Places::Codes { values, .. } => Origin::Dictionary(Arc::clone(values)),
                Places::Bits { packed } => Origin::Bits(packed.base(), packed.width()),
            });
        }
    }
}

/// Reads a chunk's key columns as places, when every one of them takes few enough of them.
///
/// `None` the moment any part of that is not true, which is a key column in a form that has no
/// places to read, one with a span large enough that the map would cost more than the probe it
/// replaces, or a code outside the dictionary it points into. The last of those is not a shape
/// anything builds, and the pass that rules it out is a run of `u32` against a constant, which is
/// cheaper than being wrong about it once: a code out of range would index the map as some other
/// combination and answer a group that is not the row's own.
pub(crate) fn coded<'a>(keys: &'a [Vector], rows: usize) -> Option<Coded<'a>> {
    if keys.is_empty() || keys.len() > KEYS {
        return None;
    }
    let mut columns = [None; KEYS];
    let mut combos: usize = 1;
    for (at, key) in keys.iter().enumerate() {
        let (places, span, nullable) = places_of(key, rows)?;
        if combos.checked_mul(span)? > COMBOS {
            return None;
        }
        columns[at] = Some(CodedColumn {
            places,
            stride: combos,
            // The last place of the span, which is the one no value of the column can take.
            nothing: span - 1,
            nullable,
            column: key,
        });
        combos *= span;
    }
    Some(Coded { columns, combos })
}

/// One key column read as places, with how many it can take and whether a row of it can be null.
///
/// The span counts the place a null takes as well as the places the values take, so it is one more
/// than the column has distinct values it could hold.
fn places_of(key: &Vector, rows: usize) -> Option<(Places<'_>, usize, bool)> {
    if let Some((codes, values)) = key.shared_dictionary_parts() {
        let codes = codes.get(..rows)?;
        if codes.iter().any(|&code| code as usize >= values.len()) {
            return None;
        }
        // A pass over the dictionary and not over the chunk, which is at most the two thousand
        // entries `COMBOS` allows and is usually three. What it buys is the row loop skipping the
        // null question entirely on the columns that have no null in them.
        let nullable =
            key.validity().has_nulls(rows) || (0..values.len()).any(|at| values.is_null_at(at));
        return Some((Places::Codes { codes, values }, values.len().checked_add(1)?, nullable));
    }
    let packed = key.packed_parts()?;
    if key.len() < rows {
        return None;
    }
    // The width is the whole of the span, and it is on the page rather than in the data, so a column
    // of small integers is known to be a small key before a single row of it has been looked at.
    // Refused here rather than by the multiply above so that the shift cannot be what overflows.
    let span = 1_usize.checked_shl(packed.width()).filter(|&span| span <= COMBOS)?;
    // A packed run keeps its nulls in the vector's own validity rather than in what it points at,
    // so unlike a dictionary there is nothing else to ask.
    Some((Places::Bits { packed }, span.checked_add(1)?, key.validity().has_nulls(rows)))
}

/// One key column in its common physical width.
///
/// ClickBench's keys are `TINYINT`, `SMALLINT`, `INTEGER`, `BIGINT` and `VARCHAR`, and half its
/// schema is `SMALLINT`. Keeping those in a general tagged value made every number 32 bytes wide
/// and, worse than the width, made the comparison that runs once per input row per probe step build
/// a tagged value on each side of it. The validity is separate because a nullable integer
/// represented as `Option<i64>` is 16 bytes, while `Vec<bool>` uses one bit.
#[derive(Debug)]
struct Column {
    valid: Vec<bool>,
    data: StoredData,
}

#[derive(Debug)]
enum StoredData {
    TinyInt(Vec<i8>),
    SmallInt(Vec<i16>),
    Integer(Vec<i32>),
    BigInt(Vec<i64>),
    /// A type whose value is one signed integer that is not one of the four widths above.
    ///
    /// A `DATE` is a day count, a `TIME` and a `TIMESTAMP` are microsecond counts, and a `DECIMAL`
    /// and a `HUGEINT` are integers that can want all 128 bits. Every one of them is stored as a
    /// signed integer in the column it came from and compares the way that integer does, which is
    /// the same grouping [`rudb_vector::Vector::signed_at`] already makes and the same one
    /// `rudb_kernels::aggregate` makes for a grouped `min`. Without this arm they went to
    /// [`StoredData::Other`], and the comparison that runs once per input row per probe step built
    /// a tagged value on each side of it. Over a packed column that is worse than it sounds: a
    /// packed row has no value to hand over, so building one allocates. See
    /// [`../../../spec/perf/16-a-key-that-is-not-an-integer.md`].
    Wide {
        ty: rudb_common::LogicalType,
        values: Vec<i128>,
    },
    Varchar(StringColumn),
    StableText { dictionary: Arc<Vector>, codes: Vec<u32> },
    Other(Vec<Stored>),
}

/// Whether a key of this type is one signed integer, so that [`StoredData::Wide`] can hold it.
///
/// Asked of the type rather than of the value because the run is chosen before the first row
/// arrives. The unsigned integers are left out even though four of the five fit, because
/// [`rudb_vector::Vector::signed_at`] does not read them and a run whose reader always answers
/// `None` is a slower way of reaching the same fallback. A `UHUGEINT` does not fit at all.
fn one_integer(ty: &rudb_common::LogicalType) -> bool {
    matches!(
        ty,
        rudb_common::LogicalType::HugeInt
            | rudb_common::LogicalType::Decimal { .. }
            | rudb_common::LogicalType::Date
            | rudb_common::LogicalType::Time
            | rudb_common::LogicalType::Timestamp
    )
}

/// The integer a value of one of [`one_integer`]'s types is, and `None` for anything else.
///
/// A decimal keeps its width and scale in the type rather than beside every value, so the unscaled
/// integer is the whole of what has to be stored. Two decimals of different scale never reach one
/// key column, because the column's type is the type the plan gave it.
fn one_integer_of(value: &Value) -> Option<i128> {
    match *value {
        Value::HugeInt(held) | Value::Decimal { unscaled: held, .. } => Some(held),
        Value::Date(days) => Some(i128::from(days)),
        Value::Time(micros) | Value::Timestamp(micros) => Some(i128::from(micros)),
        _ => None,
    }
}

/// The value that integer is, read back under the type it was stored as.
///
/// The turn round of [`one_integer_of`], for the group key on its way out into the answer. A value
/// that does not fit the narrower types saturates rather than wrapping, which cannot happen for a
/// key that came in through [`one_integer_of`] and is the harmless answer if it ever did.
fn one_integer_as(ty: &rudb_common::LogicalType, held: i128) -> Value {
    match ty {
        rudb_common::LogicalType::Date => {
            Value::Date(i32::try_from(held).unwrap_or(i32::MAX))
        }
        rudb_common::LogicalType::Time => Value::Time(i64::try_from(held).unwrap_or(i64::MAX)),
        rudb_common::LogicalType::Timestamp => {
            Value::Timestamp(i64::try_from(held).unwrap_or(i64::MAX))
        }
        rudb_common::LogicalType::Decimal { width, scale } => {
            Value::Decimal { unscaled: held, width: *width, scale: *scale }
        }
        _ => Value::HugeInt(held),
    }
}

impl Column {
    fn new(ty: &rudb_common::LogicalType) -> Self {
        let data = match ty {
            rudb_common::LogicalType::TinyInt => StoredData::TinyInt(Vec::new()),
            rudb_common::LogicalType::SmallInt => StoredData::SmallInt(Vec::new()),
            rudb_common::LogicalType::Integer => StoredData::Integer(Vec::new()),
            rudb_common::LogicalType::BigInt => StoredData::BigInt(Vec::new()),
            rudb_common::LogicalType::Varchar => StoredData::Varchar(StringColumn::default()),
            ty if one_integer(ty) => StoredData::Wide { ty: ty.clone(), values: Vec::new() },
            _ => StoredData::Other(Vec::new()),
        };
        Self { valid: Vec::new(), data }
    }

    fn push(&mut self, value: Value) -> Result<()> {
        // A borrowed dictionary run only holds while the rows keep arriving under the dictionary it
        // was read from. A value being handed over rather than a row being taken where it lies says
        // that has stopped, so the run ends here and the arms below carry on over bytes this column
        // owns. Without it a column that had started a run had no arm at all for the value and said
        // so as an internal error, which is what a null row read back from a file used to raise.
        self.end_the_run()?;
        let present = !matches!(value, Value::Null);
        match (&mut self.data, value) {
            (StoredData::TinyInt(values), Value::TinyInt(value)) => values.push(value),
            (StoredData::TinyInt(values), Value::Null) => values.push(0),
            (StoredData::SmallInt(values), Value::SmallInt(value)) => values.push(value),
            (StoredData::SmallInt(values), Value::Null) => values.push(0),
            (StoredData::Integer(values), Value::Integer(value)) => values.push(value),
            (StoredData::Integer(values), Value::Null) => values.push(0),
            (StoredData::BigInt(values), Value::BigInt(value)) => values.push(value),
            (StoredData::BigInt(values), Value::Null) => values.push(0),
            (StoredData::Varchar(values), Value::Varchar(value)) => {
                values.push(value.as_bytes())?
            }
            (StoredData::Varchar(values), Value::Null) => values.push(&[])?,
            (StoredData::Wide { values, .. }, Value::Null) => values.push(0),
            (StoredData::Wide { values, .. }, value) => match one_integer_of(&value) {
                Some(held) => values.push(held),
                None => {
                    return Err(Error::internal(format!(
                        "a group key column was given a value of the wrong type: {value:?}"
                    )));
                }
            },
            (StoredData::Other(values), value) => values.push(Stored::from(value)),
            (_, value) => {
                return Err(Error::internal(format!(
                    "a group key column was given a value of the wrong type: {value:?}"
                )));
            }
        }
        self.valid.push(present);
        Ok(())
    }

    /// Ends a borrowed dictionary run, copying what it holds into bytes this column owns.
    ///
    /// The codes mean something only next to the dictionary they were read from, so a row arriving
    /// under a different dictionary, or under none at all, cannot join the run and there is nowhere
    /// to put it. What happens instead is that the run ends: every slot already in it is read back
    /// through the dictionary it came from and pushed into an ordinary string column, and the row
    /// that ended it goes in after. A null slot copies nothing, which is the same stand in a null
    /// takes everywhere else a string column is built. Nothing but the space is lost by this, since
    /// a run is a way of holding the same bytes rather than a different set of them.
    fn end_the_run(&mut self) -> Result<()> {
        let StoredData::StableText { dictionary, codes } = &self.data else {
            return Ok(());
        };
        let mut owned = StringColumn::default();
        for (slot, &code) in codes.iter().enumerate() {
            let bytes = match self.valid.get(slot) {
                Some(true) => dictionary.bytes_at(code as usize).unwrap_or_default(),
                _ => Default::default(),
            };
            owned.push(bytes)?;
        }
        self.data = StoredData::Varchar(owned);
        Ok(())
    }

    /// Adds the key that `column` holds at `row`, taking it where it lies when the widths agree.
    ///
    /// What comes back is what the key owns away from this column, which the table adds to its own
    /// total. It is what the key owns and not what it is: the value itself is in one of the runs
    /// below, whose capacity `footprint` counts, and counting it here as well would charge every
    /// group twice for the part of it that is not a string.
    ///
    /// The stored widths read the row straight out of the vector, so an integer key costs a range
    /// check and a push and a `VARCHAR` key costs a copy of its bytes. Everything else builds a
    /// value, which is what all of this used to do.
    fn push_from(&mut self, column: &Vector, row: usize) -> Result<u64> {
        if matches!(&self.data, StoredData::Varchar(values) if values.ends.is_empty()) {
            if let Some((codes, dictionary)) = column.stable_dictionary_parts() {
                let code = *codes
                    .get(row)
                    .ok_or_else(|| Error::internal("a stable dictionary row is missing"))?;
                self.data = StoredData::StableText {
                    dictionary: Arc::clone(dictionary),
                    codes: vec![code],
                };
                self.valid.push(!column.is_null_at(row));
                return Ok(0);
            }
        }
        if column.is_null_at(row) {
            // A null does not end a run. Both sides keep their nulls in the validity beside the
            // codes rather than in a code of their own, so the row is stored the way every other
            // row of the run is and the validity says what it is. Nothing reads the code back,
            // because everything that reads a slot asks the validity first, but it still has to be
            // a code the dictionary has, since the vector handed back at the end is built from the
            // whole run at once and a code past the end of a dictionary is refused there.
            if let StoredData::StableText { dictionary, codes } = &mut self.data {
                if let Some((incoming, values)) = column.stable_dictionary_parts() {
                    if Arc::ptr_eq(dictionary, values) {
                        let code = *incoming
                            .get(row)
                            .ok_or_else(|| Error::internal("a stable dictionary row is missing"))?;
                        codes.push(code);
                        self.valid.push(false);
                        return Ok(0);
                    }
                }
            }
            return self.push(Value::Null).map(|()| 0);
        }
        /// One signed run taking the row where it lies, when the value fits the run's width.
        macro_rules! signed {
            ($values:expr, $width:ty) => {
                match column.signed_at(row).and_then(|value| <$width>::try_from(value).ok()) {
                    Some(value) => {
                        $values.push(value);
                        true
                    }
                    None => false,
                }
            };
        }
        let taken = match &mut self.data {
            StoredData::TinyInt(values) => signed!(values, i8),
            StoredData::SmallInt(values) => signed!(values, i16),
            StoredData::Integer(values) => signed!(values, i32),
            StoredData::BigInt(values) => signed!(values, i64),
            // No `try_from` here because the run is already the widest signed integer there is, so
            // the only thing that can turn this down is a form that does not read as one.
            StoredData::Wide { values, .. } => match column.signed_at(row) {
                Some(value) => {
                    values.push(value);
                    true
                }
                None => false,
            },
            StoredData::Varchar(values) => match column.bytes_at(row) {
                Some(bytes) => {
                    values.push(bytes)?;
                    true
                }
                None => false,
            },
            StoredData::StableText { dictionary, codes } => {
                match column.stable_dictionary_parts() {
                    Some((incoming, values)) if Arc::ptr_eq(dictionary, values) => {
                        codes.push(incoming[row]);
                        true
                    }
                    _ => false,
                }
            }
            StoredData::Other(_) => false,
        };
        if taken {
            self.valid.push(true);
            return Ok(0);
        }
        // A form that does not hand its rows over where they lie, which is the packed one and the
        // compressed one, or a type wider than the three runs above. The row becomes a value and
        // the general path takes it.
        // Whether the column keeps the payload is asked after the push rather than before it,
        // because a push is the one thing that can change the answer: a column that was reading a
        // dictionary run owns nothing until the run ends, and the push is what ends it. Asking
        // first would charge the string to the table and then charge it again to the column that
        // now holds it.
        let value = column.value_at(row);
        let owned = rows::owned(&value);
        self.push(value)?;
        Ok(if self.stores_payload() { 0 } else { owned })
    }

    fn footprint(&self) -> usize {
        let values = match &self.data {
            StoredData::TinyInt(values) => values.capacity(),
            StoredData::SmallInt(values) => values.capacity() * size_of::<i16>(),
            StoredData::Integer(values) => values.capacity() * size_of::<i32>(),
            StoredData::BigInt(values) => values.capacity() * size_of::<i64>(),
            StoredData::Wide { values, .. } => values.capacity() * size_of::<i128>(),
            StoredData::Varchar(values) => values.footprint(),
            StoredData::StableText { codes, .. } => codes.capacity() * size_of::<u32>(),
            StoredData::Other(values) => values.capacity() * size_of::<Stored>(),
        };
        values + self.valid.capacity().div_ceil(8)
    }

    fn holds(&self, slot: usize, column: &Vector, row: usize) -> bool {
        if !self.valid[slot] {
            return column.is_null_at(row);
        }
        match &self.data {
            // Read where it lies rather than through a value, because this is the one line in the
            // whole aggregate that runs once per input row per probe step. The fallback is not
            // decoration: a form that cannot hand its rows over as integers answers `None` here,
            // and treating that as a key that does not match would put every row of a packed
            // column in a group of its own.
            StoredData::TinyInt(values) => match column.signed_at(row) {
                Some(value) => value == i128::from(values[slot]),
                None => same(&Value::TinyInt(values[slot]), &column.value_at(row)),
            },
            StoredData::SmallInt(values) => match column.signed_at(row) {
                Some(value) => value == i128::from(values[slot]),
                None => same(&Value::SmallInt(values[slot]), &column.value_at(row)),
            },
            StoredData::Integer(values) => match column.signed_at(row) {
                Some(value) => value == i128::from(values[slot]),
                None => same(&Value::Integer(values[slot]), &column.value_at(row)),
            },
            StoredData::BigInt(values) => match column.signed_at(row) {
                Some(value) => value == i128::from(values[slot]),
                None => same(&Value::BigInt(values[slot]), &column.value_at(row)),
            },
            StoredData::Wide { ty, values } => match column.signed_at(row) {
                Some(value) => value == values[slot],
                None => same(&one_integer_as(ty, values[slot]), &column.value_at(row)),
            },
            StoredData::Varchar(values) => column.bytes_at(row).map_or_else(
                || same(&Value::Varchar(values.string(slot)), &column.value_at(row)),
                |value| value == values.get(slot),
            ),
            StoredData::StableText { dictionary, codes } => {
                match column.stable_dictionary_parts() {
                    Some((incoming, values)) if Arc::ptr_eq(dictionary, values) => {
                        incoming.get(row) == codes.get(slot)
                    }
                    _ => dictionary.bytes_at(codes[slot] as usize) == column.bytes_at(row),
                }
            }
            StoredData::Other(values) => same(&values[slot].value(), &column.value_at(row)),
        }
    }

    /// The same question asked of a whole batch of rows at once, one column at a time.
    ///
    /// [`Self::holds`] matches on the stored column's type, then asks the vector for one row in a way
    /// that matches on the vector's form, and then widens both sides to something they can be
    /// compared in. All three of those are per row and per key column, and none of them depend on the
    /// row. A batch has its rows in hand, so this does the two matches once for the batch and the arm
    /// underneath compares two runs of the same width where they lie. It is the same move [`fold`]
    /// makes for the hash, applied to the comparison that follows it.
    ///
    /// `same` comes in marked true for the rows whose salt matched, and each column narrows it
    /// rather than replacing it, so calling this for every key column in turn leaves exactly the rows
    /// whose whole key is the group they landed on. A row already ruled out by an earlier column is
    /// skipped, which is what makes a wide key cost less than its width on the rows that differ early.
    ///
    /// Every arm is [`Self::holds`] with the dispatch lifted out and nothing else. Anything the arms
    /// do not cover, which is every form that is not flat and every type without a run of its own,
    /// falls through to `holds` a row at a time, exactly as it did before.
    fn holds_run(&self, here: &[Step], seen: &[u64], column: &Vector, same: &mut [bool]) {
        let validity = column.validity();
        if let StoredData::StableText { dictionary, codes: stored } = &self.data {
            if let Some((values, incoming)) = column.stable_dictionary_parts() {
                if Arc::ptr_eq(dictionary, incoming) {
                    for ((step, &bucket), flag) in here.iter().zip(seen).zip(same.iter_mut()) {
                        if !*flag {
                            continue;
                        }
                        let slot = slot_of(bucket) as usize;
                        *flag = match values.get(step.row) {
                            _ if !self.valid[slot] => !validity.is_valid(step.row),
                            Some(code) => {
                                validity.is_valid(step.row) && Some(code) == stored.get(slot)
                            }
                            None => false,
                        };
                    }
                    return;
                }
            }
        }
        // A packed run, either the column's own or one a dictionary points into. Neither has `Data`
        // for the match below to index, so before this both of them went to [`Self::holds`] a row at
        // a time and a row there is a walk down through the form, an unpack, a widening to 128 bits
        // and a narrowing back, once per probe step rather than once per row. TPC-H q20 groups
        // lineitem by two keys and reads the second of them as a dictionary over a packed run.
        if let Some(packed) = column.packed_parts() {
            if self.packed_run(here, seen, same, &packed, |row| !validity.is_valid(row), |row| row)
            {
                return;
            }
        } else if let Some((at, values)) = column.positions() {
            // A dictionary keeps its nulls in the vector it points at, so a row is null when
            // either the column says so or the value its code names does.
            let inner = values.validity();
            let nulled = |row: usize| {
                !validity.is_valid(row)
                    || at.get(row).is_none_or(|&code| !inner.is_valid(code as usize))
            };
            let code = |row: usize| at.get(row).map_or(usize::MAX, |&code| code as usize);
            if let Some(packed) = values.packed_parts() {
                if self.packed_run(here, seen, same, &packed, nulled, code) {
                    return;
                }
            } else if let Some(data) = values.data() {
                // A dictionary over a plain run, which is what a filter leaves behind on a column
                // the scan handed over flat: the values stay where they were and the rows that got
                // through are a list of codes into them. That is the shape the driving side of
                // every join after a filter arrives in, and before this it was the one shape with
                // no pass of its own, so a probe step walked down through the form and built a
                // `Value` per row per column. TPC-H q13 probes 1.5 million filtered `o_custkey`
                // against a table of 150,000 customers this way.
                if self.flat_run(here, seen, same, column, data, nulled, code) {
                    return;
                }
            }
        }
        if let Some(data) = column.data() {
            if self.flat_run(
                here,
                seen,
                same,
                column,
                data,
                |row| !validity.is_valid(row),
                |row| row,
            ) {
                return;
            }
        }
        for ((step, &bucket), flag) in here.iter().zip(seen).zip(same.iter_mut()) {
            if *flag {
                *flag = self.holds(slot_of(bucket) as usize, column, step.row);
            }
        }
    }

    /// [`Self::holds_run`] for a batch whose values lie in a plain run, read through a mapping.
    ///
    /// The mapping is what lets one function serve both a flat column, where a row is its own
    /// index, and a dictionary over a flat run, where a row names a code. `nulled` is separate from
    /// it for the reason [`Self::packed_run`] gives: a dictionary answers the null from two
    /// validities and the value from the code, and the two questions do not go to the same place.
    ///
    /// `false` when the stored column and the run are not a pair this compares, which sends the
    /// batch on to whatever the caller has after this.
    #[expect(
        clippy::too_many_arguments,
        reason = "the batch as three parallel runs, the column and the run inside it, and the two \
                  mappings that say where a row's value and a row's nulls are"
    )]
    fn flat_run<N: Fn(usize) -> bool, C: Fn(usize) -> usize>(
        &self,
        here: &[Step],
        seen: &[u64],
        same: &mut [bool],
        column: &Vector,
        data: &Data,
        nulled: N,
        code: C,
    ) -> bool {
        /// One pass over a run of values of the same width as the run the table stored.
        macro_rules! run {
            ($stored:expr, $values:expr) => {{
                let stored = $stored;
                let values = $values.as_slice();
                for ((step, &bucket), flag) in here.iter().zip(seen).zip(same.iter_mut()) {
                    if !*flag {
                        continue;
                    }
                    let slot = slot_of(bucket) as usize;
                    *flag = match values.get(code(step.row)) {
                        _ if !self.valid[slot] => nulled(step.row),
                        Some(value) => !nulled(step.row) && *value == stored[slot],
                        // A row whose code is out of the run is one this cannot answer, and the
                        // row at a time path is where it went before any of this existed.
                        None => self.holds(slot, column, step.row),
                    };
                }
                return true;
            }};
        }
        /// The same pass where the run and the stored column are two different widths.
        ///
        /// One type reaches this at more than one width. A `DECIMAL(9, 2)` is four bytes a value
        /// and a `DECIMAL(30, 2)` is sixteen, and both are stored here as the `i128` the wider of
        /// them is, so the comparison widens the run's side. The widening is a move against a probe
        /// step that would otherwise build a value on each side.
        macro_rules! wide {
            ($stored:expr, $values:expr) => {{
                let stored = $stored;
                let values = $values.as_slice();
                for ((step, &bucket), flag) in here.iter().zip(seen).zip(same.iter_mut()) {
                    if !*flag {
                        continue;
                    }
                    let slot = slot_of(bucket) as usize;
                    *flag = match values.get(code(step.row)) {
                        _ if !self.valid[slot] => nulled(step.row),
                        Some(value) => !nulled(step.row) && i128::from(*value) == stored[slot],
                        None => self.holds(slot, column, step.row),
                    };
                }
                return true;
            }};
        }
        match (&self.data, data) {
            (StoredData::TinyInt(stored), Data::Int8(values)) => run!(stored, values),
            (StoredData::SmallInt(stored), Data::Int16(values)) => run!(stored, values),
            (StoredData::Integer(stored), Data::Int32(values)) => run!(stored, values),
            (StoredData::BigInt(stored), Data::Int64(values)) => run!(stored, values),
            // The layouts a `DATE`, a `TIME`, a `TIMESTAMP`, a `DECIMAL` or a `HUGEINT` is stored
            // in. A decimal takes whichever of them holds its width, so a `DECIMAL(4, 2)` is two
            // bytes a value and a `DECIMAL(30, 2)` is sixteen, which is why the narrower ones are
            // here and not just the widest. The one byte arm is not reached by any type today and
            // is here so that a narrower physical form than the run's cannot fall back silently.
            (StoredData::Wide { values: stored, .. }, Data::Int128(values)) => run!(stored, values),
            (StoredData::Wide { values: stored, .. }, Data::Int8(values)) => wide!(stored, values),
            (StoredData::Wide { values: stored, .. }, Data::Int16(values)) => wide!(stored, values),
            (StoredData::Wide { values: stored, .. }, Data::Int32(values)) => wide!(stored, values),
            (StoredData::Wide { values: stored, .. }, Data::Int64(values)) => wide!(stored, values),
            (StoredData::Varchar(stored), Data::Varlen(strings)) => {
                for ((step, &bucket), flag) in here.iter().zip(seen).zip(same.iter_mut()) {
                    if !*flag {
                        continue;
                    }
                    let slot = slot_of(bucket) as usize;
                    *flag = match strings.bytes(code(step.row)) {
                        _ if !self.valid[slot] => nulled(step.row),
                        Some(bytes) => !nulled(step.row) && bytes == stored.get(slot),
                        None => self.holds(slot, column, step.row),
                    };
                }
                true
            }
            _ => false,
        }
    }

    /// [`Self::holds_run`] for a batch whose column reads its values out of a packed run.
    ///
    /// `nulled` says whether a row is null and `code` says which code of the run it reads, and the
    /// two are separate because a dictionary answers the first from two validities and the second
    /// through its codes while a packed column answers both from the row.
    ///
    /// `false` when the stored column is not one of the four signed runs, which sends the batch on
    /// to whatever the caller has after this. The comparison is in 128 bits because that is the one
    /// width all four stored runs and any packed value fit in, and the widening is two moves against
    /// a probe step that would otherwise walk a form and unpack.
    fn packed_run(
        &self,
        here: &[Step],
        seen: &[u64],
        same: &mut [bool],
        packed: &Packed<'_>,
        nulled: impl Fn(usize) -> bool,
        code: impl Fn(usize) -> usize,
    ) -> bool {
        /// One pass over the batch against a stored run of one width.
        macro_rules! run {
            ($stored:expr) => {{
                let stored = $stored;
                let base = packed.base();
                for ((step, &bucket), flag) in here.iter().zip(seen).zip(same.iter_mut()) {
                    if !*flag {
                        continue;
                    }
                    let slot = slot_of(bucket) as usize;
                    let missing = nulled(step.row);
                    *flag = if !self.valid[slot] {
                        missing
                    } else if missing {
                        false
                    } else {
                        base + i128::from(packed.code(code(step.row))) == i128::from(stored[slot])
                    };
                }
                return true;
            }};
        }
        /// The same pass where the stored run is already the width the comparison is done in.
        macro_rules! held {
            ($stored:expr) => {{
                let stored = $stored;
                let base = packed.base();
                for ((step, &bucket), flag) in here.iter().zip(seen).zip(same.iter_mut()) {
                    if !*flag {
                        continue;
                    }
                    let slot = slot_of(bucket) as usize;
                    let missing = nulled(step.row);
                    *flag = if !self.valid[slot] {
                        missing
                    } else if missing {
                        false
                    } else {
                        base + i128::from(packed.code(code(step.row))) == stored[slot]
                    };
                }
                return true;
            }};
        }
        match &self.data {
            StoredData::TinyInt(stored) => run!(stored),
            StoredData::SmallInt(stored) => run!(stored),
            StoredData::Integer(stored) => run!(stored),
            StoredData::BigInt(stored) => run!(stored),
            // The arm this whole change is for. A `DATE` column read out of a native file behind a
            // filter arrives as a dictionary over a packed run, and without this the probe step
            // under it built a value on each side, which for a packed row means an allocation.
            StoredData::Wide { values, .. } => held!(values),
            _ => false,
        }
    }

    /// A range of groups of this column as a vector, built from the run rather than through values.
    ///
    /// The two fixed widths are stored as exactly what a flat vector holds, so the slice is copied
    /// and the validity is read off the bits beside it. A string key is bytes in one allocation
    /// already, so it is pushed into the vector's arena where it lies. Only the types that did not
    /// earn a run of their own go the long way.
    ///
    /// The string arm is worth the few lines. Going through [`Value`] for it meant a `to_vec` and a
    /// `String::from_utf8` and then a second copy out of the `String` into the vector, which is an
    /// allocation, two copies and a validation for every group in the answer. The validation is the
    /// part that is not merely slow but wrong to be doing at all: these bytes were validated on the
    /// way into the column the scan built, and nothing between there and here does anything to them
    /// but copy. callgrind on `SELECT URL, COUNT(*) FROM hits GROUP BY URL` put `from_utf8` at 4.68
    /// percent of the query, 515,958 calls, one per group, all of them answering a question that
    /// had already been answered.
    fn vector(
        &self,
        ty: &rudb_common::LogicalType,
        range: std::ops::Range<usize>,
    ) -> Result<Vector> {
        let (start, len) = (range.start, range.len());
        let data = match &self.data {
            StoredData::TinyInt(values) => Data::Int8(values[range.clone()].to_vec().into()),
            StoredData::SmallInt(values) => Data::Int16(values[range.clone()].to_vec().into()),
            StoredData::Integer(values) => Data::Int32(values[range.clone()].to_vec().into()),
            StoredData::BigInt(values) => Data::Int64(values[range.clone()].to_vec().into()),
            StoredData::Varchar(values) => {
                let mut out = rudb_vector::StringColumn::with_capacity(len);
                // row at a time: a group key is a range of the packed bytes and the lengths differ,
                // so there is no run of them to hand over in one piece. What this loop does per
                // group is one copy, which is what the arm is for.
                for slot in range.clone() {
                    if self.valid[slot] {
                        out.push_bytes(values.get(slot));
                    } else {
                        // The empty string, which the validity beside it says is not a string at
                        // all. Same stand in the null takes everywhere else a vector is built.
                        out.push("");
                    }
                }
                Data::Varlen(out)
            }
            StoredData::StableText { dictionary, codes } => {
                let vector = Vector::stable_dictionary(
                    codes[range.clone()].to_vec(),
                    Arc::clone(dictionary),
                )?;
                let valid = &self.valid;
                let validity = rudb_vector::Validity::from_iter(len, |index| valid[start + index]);
                return Ok(vector.with_validity(validity));
            }
            // Through values, like the arm below it and unlike the four runs above. A group key on
            // its way out is one row per group where everything else here is one row per input row,
            // so the width the answer is built in is not worth a second copy of the mapping from a
            // type to the layout it is stored in.
            StoredData::Wide { .. } | StoredData::Other(_) => {
                return Vector::from_values(ty.clone(), &self.values(range));
            }
        };
        let valid = &self.valid;
        let validity = rudb_vector::Validity::from_iter(len, |index| valid[start + index]);
        let vector = Vector::flat(ty.clone(), data)?.with_validity(validity);
        if matches!(self.data, StoredData::Varchar(_)) { vector.shared_text() } else { Ok(vector) }
    }

    fn values(&self, range: std::ops::Range<usize>) -> Vec<Value> {
        range
            .map(|slot| {
                if !self.valid[slot] {
                    return Value::Null;
                }
                match &self.data {
                    StoredData::TinyInt(values) => Value::TinyInt(values[slot]),
                    StoredData::SmallInt(values) => Value::SmallInt(values[slot]),
                    StoredData::Integer(values) => Value::Integer(values[slot]),
                    StoredData::BigInt(values) => Value::BigInt(values[slot]),
                    StoredData::Wide { ty, values } => one_integer_as(ty, values[slot]),
                    StoredData::Varchar(values) => Value::Varchar(values.string(slot)),
                    StoredData::StableText { dictionary, codes } => {
                        dictionary.value_at(codes[slot] as usize)
                    }
                    StoredData::Other(values) => values[slot].value(),
                }
            })
            .collect()
    }

    fn values_at(&self, slots: &[usize]) -> Vec<Value> {
        slots
            .iter()
            .map(|&slot| {
                if !self.valid[slot] {
                    return Value::Null;
                }
                match &self.data {
                    StoredData::TinyInt(values) => Value::TinyInt(values[slot]),
                    StoredData::SmallInt(values) => Value::SmallInt(values[slot]),
                    StoredData::Integer(values) => Value::Integer(values[slot]),
                    StoredData::BigInt(values) => Value::BigInt(values[slot]),
                    StoredData::Wide { ty, values } => one_integer_as(ty, values[slot]),
                    StoredData::Varchar(values) => Value::Varchar(values.string(slot)),
                    StoredData::StableText { dictionary, codes } => {
                        dictionary.value_at(codes[slot] as usize)
                    }
                    StoredData::Other(values) => values[slot].value(),
                }
            })
            .collect()
    }

    fn stores_payload(&self) -> bool {
        matches!(self.data, StoredData::Varchar(_))
    }
}

/// UTF-8 group keys packed into one allocation, with one end offset per group.
#[derive(Debug, Default)]
struct StringColumn {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl StringColumn {
    fn push(&mut self, value: &[u8]) -> Result<()> {
        let length = self.bytes.len().checked_add(value.len()).ok_or_else(|| {
            Error::out_of_memory("an aggregate partition's string keys are too large")
        })?;
        let end = u32::try_from(length).map_err(|_| {
            Error::out_of_memory("one aggregate partition holds more than 4 GiB of string keys")
        })?;
        self.bytes.extend_from_slice(value);
        self.ends.push(end);
        Ok(())
    }

    fn get(&self, slot: usize) -> &[u8] {
        let start = slot.checked_sub(1).map_or(0, |before| self.ends[before]) as usize;
        &self.bytes[start..self.ends[slot] as usize]
    }

    fn string(&self, slot: usize) -> String {
        String::from_utf8(self.get(slot).to_vec()).expect("a VARCHAR group key is valid UTF-8")
    }

    fn footprint(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * size_of::<u32>()
    }
}

/// A group key cell without the 64-byte width of the general recursive [`Value`] enum.
///
/// Lists and structs are uncommon grouping keys and stay behind one pointer. Primitive and string
/// keys, which dominate analytical grouping, remain inline at half the width.
#[derive(Debug, Clone)]
enum Stored {
    Null,
    Boolean(bool),
    TinyInt(i8),
    SmallInt(i16),
    Integer(i32),
    BigInt(i64),
    HugeInt(i128),
    UTinyInt(u8),
    USmallInt(u16),
    UInteger(u32),
    UBigInt(u64),
    UHugeInt(u128),
    Float(f32),
    Double(f64),
    Decimal { unscaled: i128, width: u8, scale: u8 },
    Varchar(String),
    Blob(Vec<u8>),
    Date(i32),
    Time(i64),
    TimeTz(i64),
    Timestamp(i64),
    TimestampTz(i64),
    Interval { months: i32, days: i32, micros: i64 },
    Other(Box<Value>),
}

impl From<Value> for Stored {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Boolean(v) => Self::Boolean(v),
            Value::TinyInt(v) => Self::TinyInt(v),
            Value::SmallInt(v) => Self::SmallInt(v),
            Value::Integer(v) => Self::Integer(v),
            Value::BigInt(v) => Self::BigInt(v),
            Value::HugeInt(v) => Self::HugeInt(v),
            Value::UTinyInt(v) => Self::UTinyInt(v),
            Value::USmallInt(v) => Self::USmallInt(v),
            Value::UInteger(v) => Self::UInteger(v),
            Value::UBigInt(v) => Self::UBigInt(v),
            Value::UHugeInt(v) => Self::UHugeInt(v),
            Value::Float(v) => Self::Float(v),
            Value::Double(v) => Self::Double(v),
            Value::Decimal { unscaled, width, scale } => Self::Decimal { unscaled, width, scale },
            Value::Varchar(v) => Self::Varchar(v),
            Value::Blob(v) => Self::Blob(v),
            Value::Date(v) => Self::Date(v),
            Value::Time(v) => Self::Time(v),
            Value::TimeTz(v) => Self::TimeTz(v),
            Value::Timestamp(v) => Self::Timestamp(v),
            Value::TimestampTz(v) => Self::TimestampTz(v),
            Value::Interval { months, days, micros } => Self::Interval { months, days, micros },
            other => Self::Other(Box::new(other)),
        }
    }
}

impl Stored {
    fn value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Boolean(v) => Value::Boolean(*v),
            Self::TinyInt(v) => Value::TinyInt(*v),
            Self::SmallInt(v) => Value::SmallInt(*v),
            Self::Integer(v) => Value::Integer(*v),
            Self::BigInt(v) => Value::BigInt(*v),
            Self::HugeInt(v) => Value::HugeInt(*v),
            Self::UTinyInt(v) => Value::UTinyInt(*v),
            Self::USmallInt(v) => Value::USmallInt(*v),
            Self::UInteger(v) => Value::UInteger(*v),
            Self::UBigInt(v) => Value::UBigInt(*v),
            Self::UHugeInt(v) => Value::UHugeInt(*v),
            Self::Float(v) => Value::Float(*v),
            Self::Double(v) => Value::Double(*v),
            Self::Decimal { unscaled, width, scale } => {
                Value::Decimal { unscaled: *unscaled, width: *width, scale: *scale }
            }
            Self::Varchar(v) => Value::Varchar(v.clone()),
            Self::Blob(v) => Value::Blob(v.clone()),
            Self::Date(v) => Value::Date(*v),
            Self::Time(v) => Value::Time(*v),
            Self::TimeTz(v) => Value::TimeTz(*v),
            Self::Timestamp(v) => Value::Timestamp(*v),
            Self::TimestampTz(v) => Value::TimestampTz(*v),
            Self::Interval { months, days, micros } => {
                Value::Interval { months: *months, days: *days, micros: *micros }
            }
            Self::Other(v) => (**v).clone(),
        }
    }
}

/// How many inputs the hashes being taken have to agree across.
///
/// A stable dictionary promises one code space, so two rows of it hold the same value exactly when
/// they hold the same code, and hashing the code instead of the value it points at is a pass over a
/// run of `u32` instead of a pass over the strings. The promise covers one column of one table and
/// nothing wider. A group by reads one input, so it can take that.
///
/// A join reads two. The same string arrives on the build side under one dictionary and on the
/// probe side under another, with a different code in each, and hashing the codes puts the two
/// halves of a pair in different buckets. The comparison would still say they are equal, because
/// [`Column::holds`] compares the bytes when the dictionaries are not the same one, but a probe
/// that never reaches the right bucket never asks it. The join would simply produce nothing, which
/// is the worst shape a bug like this comes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Across {
    /// One input, so a stable dictionary's codes stand in for the values they point at.
    OneInput,
    /// Two inputs, so every column is hashed by the value itself.
    TwoInputs,
}

/// Hashes a chunk of key columns into one word per row.
///
/// This is the column at a time half of #237. The type of a column is matched on once per column
/// per chunk rather than once per value, so the hash of a thousand rows of `INTEGER` is a pass over
/// a run of `i32` with a multiply and a rotate in it.
///
/// `hashes` is the caller's buffer, kept between chunks so that this does not go to the allocator
/// once per chunk either. `across` is how many inputs these hashes have to agree with, which is the
/// one thing the caller knows and this cannot work out for itself.
pub(crate) fn hash(keys: &[Vector], rows: usize, hashes: &mut Vec<u64>, across: Across) {
    hashes.clear();
    hashes.resize(rows, 0);
    if let ([column], Across::OneInput) = (keys, across) {
        if let Some((codes, _)) = column.stable_dictionary_parts() {
            let validity = column.validity();
            for (row, state) in hashes.iter_mut().enumerate() {
                let word = if validity.is_valid(row) { u64::from(codes[row]) } else { NOTHING };
                *state = spread(mix(0, word));
            }
            return;
        }
    }
    for column in keys {
        fold(column, rows, hashes, across);
    }
    for state in hashes.iter_mut() {
        *state = spread(*state);
    }
}

/// Marks every row of a chunk whose whole key is the key of the row before it.
///
/// A group by over a column the rows happen to arrive sorted on asks the table the same question
/// over and over. Three quarters of the rows of TPC-H's lineitem carry the order key of the row
/// before them, so a `GROUP BY l_orderkey` over six million rows walks the buckets six million
/// times to land on a slot it landed on for the row before four times out of five. Whether two
/// rows that sit next to each other hold the same key is answerable where the rows are, in one
/// sequential pass per key column, and that is arithmetic against a probe's trip to memory.
///
/// What comes back is one flag per row, false at the first row because nothing is before it, and
/// how many flags were set. False where the keys are in fact equal is allowed, since a caller
/// probes those rows the way it always did. True where they are not is a wrong answer, which is
/// why a column in a form with no run to read gives up for the whole chunk rather than for itself.
///
/// `least` is how many rows the caller needs marked for the answer to be worth having, and a chunk
/// that cannot reach it is dropped as soon as that is known rather than after the last key column.
/// A group by on unsorted rows is the case that has to stay cheap, and it pays one pass over one
/// column to find out that this is not for it.
pub(crate) fn repeats(keys: &[Vector], rows: usize, least: usize, same: &mut Vec<bool>) -> usize {
    same.clear();
    same.resize(rows, true);
    if rows == 0 {
        return 0;
    }
    same[0] = false;
    let mut marked = rows - 1;
    for column in keys {
        // The count between columns rather than only after the last, which is a pass over a run of
        // bytes that are in the first level cache against a pass over a key column that may not be.
        let read = repeats_in(column, rows, same);
        marked = same.iter().filter(|&&flag| flag).count();
        if !read || marked < least {
            same.clear();
            same.resize(rows, false);
            return 0;
        }
    }
    marked
}

/// Narrows `same` to the rows where this column holds what it held at the row before.
///
/// False where the column is in a form with no run of its own to walk, which is the same set of
/// forms [`fold`] falls through to a value at a time for, and there is no value at a time path
/// here because a caller that gets nothing is a caller that does what it did before.
///
/// Two nulls count as the same key, which is what [`Column::holds`] says a stored null matches, and
/// a float column answers false throughout rather than comparing, because which of `-0.0` and `0.0`
/// and which pair of nulls groups together is settled in one place and this is not it.
fn repeats_in(column: &Vector, rows: usize, same: &mut [bool]) -> bool {
    let validity = column.validity();
    let same = &mut same[..rows];
    /// One pass over a run of fixed width values, each row reading its own place in it.
    macro_rules! run {
        ($values:expr) => {{
            let values = $values.as_slice();
            if values.len() < rows {
                return false;
            }
            narrow(same, validity, |row| values[row] == values[row - 1]);
            return true;
        }};
    }
    // A code stands for the value it points at, so equal codes are an equal key whether or not the
    // run behind them holds the value twice. Unequal codes over an equal value is the other way
    // round and is allowed: the row is probed.
    if let Some(packed) = column.packed_parts() {
        narrow(same, validity, |row| packed.code(row) == packed.code(row - 1));
        return true;
    }
    if let Some(data) = column.data() {
        match data {
            Data::Bool(values) => run!(values),
            Data::Int8(values) => run!(values),
            Data::Int16(values) => run!(values),
            Data::Int32(values) => run!(values),
            Data::Int64(values) => run!(values),
            Data::Int128(values) => run!(values),
            Data::UInt8(values) => run!(values),
            Data::UInt16(values) => run!(values),
            Data::UInt32(values) => run!(values),
            Data::UInt64(values) => run!(values),
            Data::UInt128(values) => run!(values),
            Data::Varlen(strings) => {
                narrow(same, validity, |row| strings.bytes(row) == strings.bytes(row - 1));
                return true;
            }
            _ => return false,
        }
    }
    if let Some((at, _)) = column.positions() {
        if at.len() < rows {
            return false;
        }
        narrow(same, validity, |row| at[row] == at[row - 1]);
        return true;
    }
    false
}

/// Clears the flag of every row this column says is not what the row before it is.
///
/// `equal` compares the values of a row and the one before it and is asked nothing about nulls,
/// because the rule for those is the same for every type and is written here once: a row repeats
/// when it and the row before are both nothing, or both something and the same something. That is
/// what [`Column::holds`] says a stored null matches, so a run of nulls is one group the way the
/// table would have put them in one.
fn narrow(same: &mut [bool], validity: &rudb_vector::Validity, equal: impl Fn(usize) -> bool) {
    for (row, flag) in same.iter_mut().enumerate().skip(1) {
        *flag = *flag
            && validity.is_valid(row) == validity.is_valid(row - 1)
            && (!validity.is_valid(row) || equal(row));
    }
}

/// Folds one key column's values into the running hash of every row.
///
/// Every arm here has to produce what the general path at the bottom produces for the same value,
/// because the same column arrives flat in one chunk and as a dictionary in the next. The module
/// comment has the argument. The arms that are missing, which are the intervals and the nested
/// types, are missing on purpose: they fall through to the general path in every form, so there is
/// nothing for them to disagree with. An interval is hashed as the one length its three counts add
/// up to, which is what makes a day and twenty four hours one group.
fn fold(column: &Vector, rows: usize, hashes: &mut [u64], across: Across) {
    let validity = column.validity();
    if across == Across::OneInput {
        if let Some((codes, _)) = column.stable_dictionary_parts() {
            for (row, state) in hashes.iter_mut().enumerate().take(rows) {
                let one = if validity.is_valid(row) { u64::from(codes[row]) } else { NOTHING };
                *state = mix(*state, one);
            }
            return;
        }
    }
    // What the unpacked integer is read as, decided once for the column rather than once for every
    // row of it. The match was inside the loop, which made a pass over a packed column a logical
    // type comparison per row on top of the unpack.
    //
    // A decimal is wide only when its width says it is stored in 128 bits. It used to be wide at
    // every width, so a `DECIMAL(9, 2)` column hashed as two words when it was packed and as one
    // when it was flat, and the same value landed in two buckets.
    let wide = match column.logical_type() {
        rudb_common::LogicalType::HugeInt | rudb_common::LogicalType::UHugeInt => true,
        rudb_common::LogicalType::Decimal { width, .. } => wide_decimal(*width),
        _ => false,
    };
    if let Some(packed) = column.packed_parts() {
        fold_packed(&packed, wide, rows, hashes, |row| validity.is_valid(row).then_some(row));
        return;
    }
    if let Some(data) = column.data() {
        if fold_data(data, rows, hashes, |row| validity.is_valid(row).then_some(row)) {
            return;
        }
    }
    // The same pass with one indirection in it, for a dictionary or a run length column. Without it
    // a dictionary of `BIGINT`, which is what the Parquet reader hands back for a join key the
    // writer found worth coding, went to the row at a time path below and built a `Value` per row.
    // TPC-H q21's runtime filter reads `l_orderkey` in exactly that form, and hashing six million
    // rows of it a `Value` at a time was most of what the filter cost.
    //
    // Reading through the codes rather than hashing the dictionary once and gathering is deliberate.
    // A dictionary a Parquet reader hands over covers a whole column chunk and can hold far more
    // values than the thousand rows being hashed, so hashing it whole would be the slower of the two
    // exactly when the dictionary is doing its job.
    if let Some((at, values)) = column.positions() {
        if let Some(data) = values.data() {
            // A dictionary keeps its nulls in the vector it points at, so a row is null when either
            // the column says so or the value its code points at does.
            let inner = values.validity();
            let pick = |row: usize| {
                if !validity.is_valid(row) {
                    return None;
                }
                let code = *at.get(row)? as usize;
                inner.is_valid(code).then_some(code)
            };
            if fold_data(data, rows, hashes, pick) {
                return;
            }
        }
        // The same again where what the codes point at is a packed run rather than a run of `Data`.
        // That is the form our own storage writes for a column of few distinct numbers over a wide
        // range, and until this was here it fell all the way to the value at a time loop below,
        // because the arm above has `Data` to index and a packed run is not `Data`.
        //
        // TPC-H q20 is the query that shows it. It groups nine hundred thousand rows of lineitem by
        // two keys, one of which arrives in exactly that form, so a fifth of the whole query was
        // building a `Value` per row to hash it.
        if let Some(packed) = values.packed_parts() {
            let inner = values.validity();
            fold_packed(&packed, wide, rows, hashes, |row| {
                if !validity.is_valid(row) {
                    return None;
                }
                let code = *at.get(row)? as usize;
                inner.is_valid(code).then_some(code)
            });
            return;
        }
    }
    // Whether the bytes are a string, asked once for the column rather than once for every row of
    // it, which is what it was.
    let text = column.logical_type() == &rudb_common::LogicalType::Varchar;
    // row at a time: every other form and every type without an arm above. A dictionary string is
    // read as bytes because the input reader already validated the column and validating the same
    // bytes again for every row was most of the string group path. What is left after that is the
    // nested types and the intervals, which have no run of fixed width words to walk at all.
    for (row, state) in hashes.iter_mut().enumerate().take(rows) {
        *state = if text {
            match column.bytes_at(row) {
                Some(bytes) => mix(*state, bytes_word(bytes)),
                None => mix(*state, NOTHING),
            }
        } else {
            fold_value(*state, &column.value_at(row))
        };
    }
}

/// Folds one packed run into the running hash, with `pick` saying which code a row reads.
///
/// `None` from `pick` is a null, the same way it is for [`fold_data`]. `wide` says the type needs
/// both halves of the value mixed in rather than the low one, and it is the caller's because it is
/// a question about the column rather than about the run.
///
/// What this produces has to be what the value at a time path at the bottom of [`fold`] produces
/// for the same number, since the same column is a packed run in one chunk and something else in
/// the next, and the base plus the code is that number.
fn fold_packed(
    packed: &Packed<'_>,
    wide: bool,
    rows: usize,
    hashes: &mut [u64],
    pick: impl Fn(usize) -> Option<usize>,
) {
    let base = packed.base();
    for (row, state) in hashes.iter_mut().enumerate().take(rows) {
        let Some(code) = pick(row) else {
            *state = mix(*state, NOTHING);
            continue;
        };
        let value = base + i128::from(packed.code(code));
        *state = if wide {
            mix(mix(*state, value as u64), (value >> 64) as u64)
        } else {
            mix(*state, value as u64)
        };
    }
}

/// Folds one word per row into `hashes`, reading the values through `pick`.
///
/// `pick` says which index of `data` a row reads, and `None` says the row is null. That is what
/// makes this one copy of the type arms rather than two: a flat column picks the row itself, a
/// dictionary or a run length column picks the code, and the eleven arms below are written once.
/// The answer is whether there was an arm for the data at all, which is `false` for the nested
/// types and leaves the caller to fall through to whatever it has after this.
fn fold_data(
    data: &Data,
    rows: usize,
    hashes: &mut [u64],
    pick: impl Fn(usize) -> Option<usize>,
) -> bool {
    /// One pass over the rows, turning each into a word the same way the general path does.
    macro_rules! run {
        ($values:expr, $word:expr) => {{
            let values = $values.as_slice();
            let word = $word;
            for (row, state) in hashes.iter_mut().enumerate().take(rows) {
                let one = match pick(row).and_then(|at| values.get(at)) {
                    Some(value) => word(*value),
                    None => NOTHING,
                };
                *state = mix(*state, one);
            }
            return true;
        }};
    }
    match data {
        Data::Bool(values) => run!(values, |x: bool| u64::from(x)),
        Data::Int8(values) => run!(values, |x: i8| i64::from(x) as u64),
        Data::Int16(values) => run!(values, |x: i16| i64::from(x) as u64),
        Data::Int32(values) => run!(values, |x: i32| i64::from(x) as u64),
        Data::Int64(values) => run!(values, |x: i64| x as u64),
        Data::UInt8(values) => run!(values, |x: u8| u64::from(x)),
        Data::UInt16(values) => run!(values, |x: u16| u64::from(x)),
        Data::UInt32(values) => run!(values, |x: u32| u64::from(x)),
        Data::UInt64(values) => run!(values, |x: u64| x),
        Data::Float32(values) => run!(values, |x: f32| canonical(f64::from(x))),
        Data::Float64(values) => run!(values, canonical),
        // The bytes and not the string, so that a `BLOB` whose bytes are not text hashes as what it
        // is rather than as a null.
        Data::Varlen(strings) => {
            for (row, state) in hashes.iter_mut().enumerate().take(rows) {
                let one = match pick(row).and_then(|at| strings.bytes(at)) {
                    Some(bytes) => bytes_word(bytes),
                    None => NOTHING,
                };
                *state = mix(*state, one);
            }
            true
        }
        _ => false,
    }
}

/// Whether a decimal of this width is kept in 128 bits rather than in 64 or fewer.
///
/// The one place the hash asks, so that the run over a flat column, the pass over a packed one and
/// the value at a time fallback all take the same side of it. It is asked of the width rather than
/// of the type because a `Value` carries the width and not the type it was read out of, and it is
/// answered by [`rudb_common::LogicalType::physical`] rather than by a second copy of the digit
/// ranges so that the two cannot drift apart.
fn wide_decimal(width: u8) -> bool {
    matches!(
        rudb_common::LogicalType::Decimal { width, scale: 0 }.physical(),
        rudb_common::PhysicalType::Int128
    )
}

/// Folds one value into a running hash, for the forms and types that have no run to walk.
///
/// The nested types go through `Display`, which is slow and is the same honest answer `key.rs`
/// gives: a group key is a `Value` until section 7.4's row layout replaces it, and every type that
/// shows up in a ClickBench group key is written out above that fallback.
fn fold_value(state: u64, value: &Value) -> u64 {
    match value {
        Value::Null => mix(state, NOTHING),
        Value::Boolean(x) => mix(state, u64::from(*x)),
        Value::TinyInt(x) => mix(state, i64::from(*x) as u64),
        Value::SmallInt(x) => mix(state, i64::from(*x) as u64),
        Value::Integer(x) | Value::Date(x) => mix(state, i64::from(*x) as u64),
        Value::BigInt(x)
        | Value::Time(x)
        | Value::TimeTz(x)
        | Value::Timestamp(x)
        | Value::TimestampTz(x) => mix(state, *x as u64),
        Value::UTinyInt(x) => mix(state, u64::from(*x)),
        Value::USmallInt(x) => mix(state, u64::from(*x)),
        Value::UInteger(x) => mix(state, u64::from(*x)),
        Value::UBigInt(x) => mix(state, *x),
        Value::Float(x) => mix(state, canonical(f64::from(*x))),
        Value::Double(x) => mix(state, canonical(*x)),
        Value::Varchar(x) => mix(state, bytes_word(x.as_bytes())),
        Value::Blob(x) => mix(state, bytes_word(x)),
        // Two words, low first, the way the 128 bit layouts are read. The width and the scale of a
        // decimal are not mixed, because they are the column's and not the value's, and a flat
        // decimal column is a run of integers with no room to keep them.
        Value::HugeInt(x) => mix(mix(state, *x as u64), (*x >> 64) as u64),
        // A decimal is hashed as the layout its width puts it in and not always as a `HUGEINT`,
        // because a `DECIMAL(9, 2)` column is a run of `i32` and the run above hashes it as one
        // word. Hashing the value as two here and the run as one put the same value in two buckets
        // whenever the same column arrived flat in one chunk and packed in the next, which our own
        // format does page by page. The width is the column's, so every form of one column takes
        // the same side of this.
        Value::Decimal { unscaled: x, width, .. } => match wide_decimal(*width) {
            true => mix(mix(state, *x as u64), (*x >> 64) as u64),
            false => mix(state, *x as u64),
        },
        Value::UHugeInt(x) => mix(mix(state, *x as u64), (*x >> 64) as u64),
        // The one length the three counts add up to, read as two words the same way, because a
        // day and twenty four hours are one group and a hash that told them apart would put that
        // one group in two buckets.
        Value::Interval { months, days, micros } => {
            let length = interval_micros(*months, *days, *micros);
            mix(mix(state, length as u64), (length >> 64) as u64)
        }
        other => mix(state, bytes_word(other.to_string().as_bytes())),
    }
}

/// A run of bytes as one word, the way [`Digest`](crate::key::Digest) reads one.
///
/// The length goes in as well, so that `ab` and `ab\0` are two values rather than one.
fn bytes_word(bytes: &[u8]) -> u64 {
    let mut state = 0u64;
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        state = mix(state, u64::from_le_bytes(word.try_into().unwrap_or([0; 8])));
    }
    let rest = words.remainder();
    if !rest.is_empty() {
        let mut last = [0; 8];
        last[..rest.len()].copy_from_slice(rest);
        state = mix(state, u64::from_le_bytes(last));
    }
    mix(state, bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::*;

    #[test]
    fn a_stored_group_key_is_narrower_than_a_general_recursive_value() {
        assert!(size_of::<Stored>() < size_of::<Value>());
    }

    /// The hash of one column of values, in whatever form the vector is in.
    fn hashed(column: &Vector) -> Vec<u64> {
        let mut hashes = Vec::new();
        hash(std::slice::from_ref(column), column.len(), &mut hashes, Across::OneInput);
        hashes
    }

    fn flat(ty: LogicalType, values: &[Value]) -> Vector {
        Vector::from_values(ty, values).expect("a flat vector of these values")
    }

    /// The invariant the whole module rests on. A column read out of a parquet file is a dictionary
    /// in one chunk and flat in the next, and a group by that hashed the two differently would put
    /// the same string in two groups and return it twice.
    #[test]
    fn a_dictionary_hashes_the_same_as_the_flat_column_it_stands_for() {
        let long = "lovelace, and a string past the sixteen bytes a view holds inline";
        let values = [
            Value::Varchar("ada".into()),
            Value::Varchar(String::new()),
            Value::Null,
            Value::Varchar(long.into()),
        ];
        let plain = flat(LogicalType::Varchar, &values);
        let distinct = flat(LogicalType::Varchar, &values);
        let dictionary =
            Vector::dictionary(vec![0, 1, 2, 3], distinct).expect("a dictionary of those values");
        assert_eq!(hashed(&plain), hashed(&dictionary));
    }

    /// The same argument for the other two forms, which a literal and a `range` produce.
    #[test]
    fn a_constant_and_a_sequence_hash_the_same_as_the_values_they_stand_for() {
        let constant = Vector::constant(LogicalType::Integer, Value::Integer(7), 3);
        let plain = flat(LogicalType::Integer, &vec![Value::Integer(7); 3]);
        assert_eq!(hashed(&constant), hashed(&plain));

        let sequence = Vector::sequence(10, 2, 4);
        let counted = flat(
            LogicalType::BigInt,
            &[Value::BigInt(10), Value::BigInt(12), Value::BigInt(14), Value::BigInt(16)],
        );
        assert_eq!(hashed(&sequence), hashed(&counted));
    }

    /// Two NaNs are one group and the two zeros are one group, the same rule `key.rs` applies,
    /// because a group nobody can find again is worse than a group that follows IEEE.
    #[test]
    fn the_floats_that_group_together_hash_together() {
        let left = flat(LogicalType::Double, &[Value::Double(f64::NAN), Value::Double(0.0)]);
        let right = flat(LogicalType::Double, &[Value::Double(-f64::NAN), Value::Double(-0.0)]);
        assert_eq!(hashed(&left), hashed(&right));
    }

    /// Fills a table one row at a time, which is what a probe did before there was a batched one.
    fn one_at_a_time(keys: &[Vector], rows: usize, types: &[LogicalType]) -> (Table, Vec<usize>) {
        let mut table = Table::new(types);
        let mut hashes = Vec::new();
        hash(keys, rows, &mut hashes, Across::OneInput);
        let mut slots = Vec::new();
        for (row, &hash) in hashes.iter().enumerate() {
            slots.push(match table.probe(hash, keys, row) {
                Probe::Found(slot) => slot,
                Probe::Vacant(bucket) => {
                    table.insert(bucket, hash, keys, row).expect("room for this group")
                }
            });
        }
        (table, slots)
    }

    /// Fills a table the way the aggregate does now, a batch at a time with the misses finished off
    /// one at a time.
    fn a_batch_at_a_time(
        keys: &[Vector],
        rows: usize,
        types: &[LogicalType],
    ) -> (Table, Vec<usize>) {
        let mut table = Table::new(types);
        let mut hashes = Vec::new();
        hash(keys, rows, &mut hashes, Across::OneInput);
        let mut slots = vec![usize::MAX; rows];
        let mut walk = Walk::default();
        let mut from = 0;
        while from < rows {
            let upto = (from + BATCH).min(rows);
            table.probe_run(&hashes, keys, from, upto, &mut slots, &mut walk);
            from = upto;
            for &row in walk.pending() {
                slots[row] = match table.probe(hashes[row], keys, row) {
                    Probe::Found(slot) => slot,
                    Probe::Vacant(bucket) => {
                        table.insert(bucket, hashes[row], keys, row).expect("room for this group")
                    }
                };
            }
        }
        (table, slots)
    }

    /// The whole of what the batch has to promise. A slot is the order a group was first seen in and
    /// the operator above turns slots into the answer's rows, so a batch that agreed about which
    /// rows group together but not about their order would reorder every grouped query.
    ///
    /// Twelve thousand keys so that the table is past [`HOT`] and the batch is really taken, spread
    /// so that a batch of sixty four rows holds both repeats and new groups, with nulls among them.
    #[test]
    fn a_batch_at_a_time_finds_the_groups_one_at_a_time_found_in_the_order_it_found_them() {
        let values: Vec<Value> = (0..40_000)
            .map(|row: i64| match row % 97 {
                0 => Value::Null,
                _ => Value::BigInt((row * 7919) % 12_007),
            })
            .collect();
        let keys = [flat(LogicalType::BigInt, &values)];
        let types = [LogicalType::BigInt];
        let (was, before) = one_at_a_time(&keys, values.len(), &types);
        let (now, after) = a_batch_at_a_time(&keys, values.len(), &types);
        assert_eq!(before, after);
        assert_eq!(was.len(), now.len());
        assert!(now.buckets.len() > HOT, "the test has to reach the batched path");
    }

    /// The values as a dictionary of `seen`, with the nulls in the validity beside the codes rather
    /// than in the values, which is the shape the parquet reader hands a column over in.
    fn dictionary_of(ty: LogicalType, seen: &[Value], codes: Vec<u32>, values: &[Value]) -> Vector {
        let valid =
            rudb_vector::Validity::from_iter(values.len(), |row| values[row] != Value::Null);
        Vector::dictionary(codes, flat(ty, seen))
            .expect("a dictionary of those values")
            .with_validity(valid)
    }

    /// The batched compare is [`Column::holds`] with its two type matches lifted out of the row loop,
    /// so what has to be shown is that it did not change its mind about anything on the way up. An
    /// integer and a string cover both runs it has an arm for, the nulls cover the validity half of
    /// each arm, and the same values as a dictionary cover reading the run through a mapping rather
    /// than by row. All three have to put the same rows in the same groups in the same order.
    #[test]
    fn the_batched_key_compare_agrees_with_the_one_at_a_time_one_on_every_form() {
        let rows = 30_000;
        let number = |row: i64| (row * 7919) % 5003;
        let word = |row: i64| (row * 104_729) % 4001;
        let numbers: Vec<Value> = (0..rows)
            .map(|row| match row % 61 {
                0 => Value::Null,
                _ => Value::Integer(number(row) as i32),
            })
            .collect();
        let words: Vec<Value> = (0..rows)
            .map(|row| match row % 37 {
                0 => Value::Null,
                _ => Value::Varchar(format!("row {}", word(row))),
            })
            .collect();
        let types = [LogicalType::Integer, LogicalType::Varchar];
        let flatly = [flat(LogicalType::Integer, &numbers), flat(LogicalType::Varchar, &words)];
        let (was, before) = one_at_a_time(&flatly, numbers.len(), &types);
        let (now, after) = a_batch_at_a_time(&flatly, numbers.len(), &types);
        assert_eq!(before, after);
        assert_eq!(was.len(), now.len());
        assert!(now.buckets.len() > HOT, "the test has to reach the batched path");

        let digits: Vec<Value> = (0..5003).map(Value::Integer).collect();
        let phrases: Vec<Value> = (0..4001).map(|at| Value::Varchar(format!("row {at}"))).collect();
        let indirect = [
            dictionary_of(
                LogicalType::Integer,
                &digits,
                (0..rows).map(|row| number(row) as u32).collect(),
                &numbers,
            ),
            dictionary_of(
                LogicalType::Varchar,
                &phrases,
                (0..rows).map(|row| word(row) as u32).collect(),
                &words,
            ),
        ];
        let (_, through) = a_batch_at_a_time(&indirect, numbers.len(), &types);
        assert_eq!(before, through);
    }

    /// The two forms our own storage writes for a column of numbers keep their values where no run
    /// of `Data` reaches: a packed run, and a dictionary whose codes point into a packed run. Both
    /// have to hash as the flat column they stand for and both have to group the same rows the same
    /// way, since the same column is packed in one row group and flat in the next.
    #[test]
    fn a_packed_run_and_a_dictionary_over_one_group_as_the_flat_column_they_stand_for() {
        let rows = 30_000i64;
        let number = |row: i64| (row * 7919) % 5003;
        let values: Vec<Value> = (0..rows)
            .map(|row| match row % 61 {
                0 => Value::Null,
                _ => Value::BigInt(number(row) + 1_000_000),
            })
            .collect();
        let types = [LogicalType::BigInt];
        let flatly = [flat(LogicalType::BigInt, &values)];
        let (was, before) = one_at_a_time(&flatly, values.len(), &types);
        assert!(was.buckets.len() > HOT, "the test has to reach the batched path");

        let packed = [flatly[0].bit_packed().expect("a packed run of those values")];
        assert_eq!(
            packed[0].form(),
            rudb_vector::Form::BitPacked,
            "the test needs the packed form"
        );
        assert_eq!(hashed(&flatly[0]), hashed(&packed[0]));
        let (_, through_packed) = a_batch_at_a_time(&packed, values.len(), &types);
        assert_eq!(before, through_packed);

        let distinct: Vec<Value> = (0..5003).map(|at| Value::BigInt(at + 1_000_000)).collect();
        let held = flat(LogicalType::BigInt, &distinct)
            .bit_packed()
            .expect("a packed run of the distinct values");
        let valid =
            rudb_vector::Validity::from_iter(values.len(), |row| values[row] != Value::Null);
        let coded = [Vector::dictionary((0..rows).map(|row| number(row) as u32).collect(), held)
            .expect("a dictionary over that packed run")
            .with_validity(valid)];
        assert_eq!(hashed(&flatly[0]), hashed(&coded[0]));
        let (_, through_coded) = a_batch_at_a_time(&coded, values.len(), &types);
        assert_eq!(before, through_coded);
    }

    /// A dictionary that keeps its nulls in the values it points at rather than in a mask over its
    /// codes.
    ///
    /// The test above puts the nulls in the mask, which is the half of the question the batched
    /// compare answers off the column's own validity. This puts them where a dictionary built by a
    /// reader usually puts them, which is in the vector the codes name, and the batched compare has
    /// to reach through the code to find them. Both are the same column and the row at a time path
    /// answers both through one call, so the grouping has to come out the same.
    #[test]
    fn a_dictionary_whose_nulls_are_in_its_values_groups_as_the_flat_column_it_stands_for() {
        let rows = 30_000i64;
        let number = |row: i64| (row * 7919) % 5003;
        let word = |row: i64| (row * 104_729) % 4001;
        // Every seventy first code names a null, so the nulls arrive through the dictionary.
        let digit =
            |code: i64| if code % 71 == 0 { Value::Null } else { Value::BigInt(code + 1_000_000) };
        let phrase = |code: i64| {
            if code % 53 == 0 { Value::Null } else { Value::Varchar(format!("row {code}")) }
        };
        let numbers: Vec<Value> = (0..rows).map(|row| digit(number(row))).collect();
        let words: Vec<Value> = (0..rows).map(|row| phrase(word(row))).collect();
        let types = [LogicalType::BigInt, LogicalType::Varchar];
        let flatly = [flat(LogicalType::BigInt, &numbers), flat(LogicalType::Varchar, &words)];
        let (was, before) = one_at_a_time(&flatly, numbers.len(), &types);
        assert!(was.buckets.len() > HOT, "the test has to reach the batched path");

        let digits: Vec<Value> = (0..5003).map(digit).collect();
        let phrases: Vec<Value> = (0..4001).map(phrase).collect();
        let coded = [
            Vector::dictionary(
                (0..rows).map(|row| number(row) as u32).collect(),
                flat(LogicalType::BigInt, &digits),
            )
            .expect("a dictionary over those numbers"),
            Vector::dictionary(
                (0..rows).map(|row| word(row) as u32).collect(),
                flat(LogicalType::Varchar, &phrases),
            )
            .expect("a dictionary over those words"),
        ];
        assert_eq!(hashed(&flatly[0]), hashed(&coded[0]));
        assert_eq!(hashed(&flatly[1]), hashed(&coded[1]));
        let (_, through) = a_batch_at_a_time(&coded, numbers.len(), &types);
        assert_eq!(before, through);
    }

    /// The three paths the hash has for one column, over a decimal narrow enough to be stored in
    /// fewer than 128 bits.
    ///
    /// A `DECIMAL(9, 2)` is four bytes a value. The run over a flat column read it as one word, the
    /// pass over a packed one read it as two, and the value at a time fallback read it as two, so
    /// the same value hashed one way when the column arrived flat and another when it arrived
    /// packed. Our own format decides packing page by page, so that is one column of one table, and
    /// the group by would have returned the same decimal twice.
    #[test]
    fn a_decimal_narrower_than_a_hugeint_hashes_the_same_in_every_form() {
        for width in [4u8, 9, 18, 30] {
            let ty = LogicalType::Decimal { width, scale: 2 };
            let of = |unscaled: i128| Value::Decimal { unscaled, width, scale: 2 };
            let values: Vec<Value> = (0..600).map(of).collect();
            let plain = flat(ty.clone(), &values);
            let packed = plain.bit_packed().expect("a packed run of those values");
            assert_eq!(packed.form(), rudb_vector::Form::BitPacked, "DECIMAL({width}) has to pack");
            assert_eq!(hashed(&plain), hashed(&packed), "DECIMAL({width}) packed against flat");

            // A constant column is the value at a time path, which is the third reader of the same
            // column and has to agree with the other two.
            let one = Vector::constant(ty.clone(), of(7), 4);
            let same = flat(ty.clone(), &vec![of(7); 4]);
            assert_eq!(hashed(&one), hashed(&same), "DECIMAL({width}) constant against flat");
        }
    }

    /// One key column of the given type in the three forms a scan hands one over in, checked
    /// against the row at a time path and against the keys the table gives back.
    ///
    /// Flat is what a chunk built in memory looks like, packed is what a native file hands over,
    /// and a dictionary over a packed run is what the same file hands over behind a filter. The
    /// grouping has to come out the same in all three, and the keys the table hands back on the way
    /// out have to be the values that went in, under the type they went in as.
    fn a_key_of_every_form(ty: &LogicalType, of: impl Fn(i64) -> Value) {
        let rows = 30_000i64;
        let number = |row: i64| (row * 7919) % 5003;
        let values: Vec<Value> =
            (0..rows).map(|row| if row % 61 == 0 { Value::Null } else { of(number(row)) }).collect();
        let types = [ty.clone()];
        let flatly = [flat(ty.clone(), &values)];
        let (was, before) = one_at_a_time(&flatly, values.len(), &types);
        assert!(was.buckets.len() > HOT, "{ty} has to reach the batched path");
        let (now, after) = a_batch_at_a_time(&flatly, values.len(), &types);
        assert_eq!(before, after, "{ty} flat");
        assert_eq!(was.len(), now.len(), "{ty} flat");

        // The keys on the way out, which is the half of this a grouping check cannot see. A slot is
        // the order a group was first seen in, so the first row that reached a slot holds its key.
        let mut want = vec![Value::Null; now.len()];
        let mut filled = vec![false; now.len()];
        for (row, &slot) in after.iter().enumerate() {
            if !std::mem::replace(&mut filled[slot], true) {
                want[slot] = values[row].clone();
            }
        }
        let out = now.column(0, ty, 0..now.len()).expect("the keys of every group");
        let got: Vec<Value> = (0..out.len()).map(|slot| out.value_at(slot)).collect();
        assert_eq!(want, got, "{ty} keys on the way out");

        let packed = [flatly[0].bit_packed().expect("a packed run of those values")];
        assert_eq!(packed[0].form(), rudb_vector::Form::BitPacked, "{ty} has to pack");
        assert_eq!(hashed(&flatly[0]), hashed(&packed[0]), "{ty} packed");
        let (_, through_packed) = a_batch_at_a_time(&packed, values.len(), &types);
        assert_eq!(before, through_packed, "{ty} packed");

        let distinct: Vec<Value> = (0..5003).map(&of).collect();
        let held =
            flat(ty.clone(), &distinct).bit_packed().expect("a packed run of the distinct values");
        let valid =
            rudb_vector::Validity::from_iter(values.len(), |row| values[row] != Value::Null);
        let coded = [Vector::dictionary((0..rows).map(|row| number(row) as u32).collect(), held)
            .expect("a dictionary over that packed run")
            .with_validity(valid)];
        assert_eq!(hashed(&flatly[0]), hashed(&coded[0]), "{ty} coded");
        let (_, through_coded) = a_batch_at_a_time(&coded, values.len(), &types);
        assert_eq!(before, through_coded, "{ty} coded");
    }

    /// The types that are one signed integer without being one of the four integer types, which is
    /// what [`StoredData::Wide`] is for.
    ///
    /// A `DATE` behind a filter arrives as a dictionary over a packed run, and before there was a
    /// run for it the compare built a tagged value on each side of every probe step, which over a
    /// packed row means an allocation. What is checked here is the answer rather than the cost: the
    /// new runs have to group the rows exactly as the row at a time path does, in every form, and
    /// hand the keys back unchanged. The two decimals are two different physical widths, four bytes
    /// and sixteen, so both the widening compare and the same width one are covered.
    #[test]
    fn a_key_that_is_one_integer_without_being_an_integer_groups_the_same_in_every_form() {
        a_key_of_every_form(&LogicalType::Date, |code| Value::Date(code as i32));
        a_key_of_every_form(&LogicalType::Time, |code| Value::Time(code * 1_000));
        a_key_of_every_form(&LogicalType::Timestamp, |code| Value::Timestamp(code * 1_000_000));
        a_key_of_every_form(&LogicalType::HugeInt, |code| Value::HugeInt(i128::from(code)));
        a_key_of_every_form(&LogicalType::Decimal { width: 9, scale: 2 }, |code| Value::Decimal {
            unscaled: i128::from(code),
            width: 9,
            scale: 2,
        });
        a_key_of_every_form(&LogicalType::Decimal { width: 30, scale: 2 }, |code| Value::Decimal {
            unscaled: i128::from(code),
            width: 30,
            scale: 2,
        });
    }

    /// The reason a vacancy cannot be filled inside the batch. Every row of this batch is the first
    /// row of one group as far as the batched pass can tell, because none of them were in the table
    /// when it read the buckets, and they are one group.
    #[test]
    fn rows_of_one_new_group_in_one_batch_get_one_slot() {
        let values = vec![Value::Integer(4); BATCH * 3];
        let keys = [flat(LogicalType::Integer, &values)];
        let types = [LogicalType::Integer];
        let (table, slots) = a_batch_at_a_time(&keys, values.len(), &types);
        assert_eq!(table.len(), 1);
        assert!(slots.iter().all(|&slot| slot == 0));
    }

    /// A null is a word of its own rather than nothing at all, or a null would group with a zero.
    #[test]
    fn a_null_does_not_hash_as_a_zero() {
        let nulls = flat(LogicalType::BigInt, &[Value::Null]);
        let zeroes = flat(LogicalType::BigInt, &[Value::BigInt(0)]);
        assert_ne!(hashed(&nulls), hashed(&zeroes));
    }

    /// The order of the columns is part of the key, or `GROUP BY a, b` would put `(1, 2)` and
    /// `(2, 1)` in one group whenever the two columns held each other's values.
    #[test]
    fn the_same_values_in_a_different_order_hash_apart() {
        let ones = flat(LogicalType::Integer, &[Value::Integer(1)]);
        let twos = flat(LogicalType::Integer, &[Value::Integer(2)]);
        let mut forwards = Vec::new();
        let mut backwards = Vec::new();
        hash(&[ones.clone(), twos.clone()], 1, &mut forwards, Across::OneInput);
        hash(&[twos, ones], 1, &mut backwards, Across::OneInput);
        assert_ne!(forwards, backwards);
    }

    /// What the table is for, end to end: the same key finds the same slot and a different one does
    /// not, over a key of two columns of different types.
    #[test]
    fn a_key_that_has_been_seen_is_found_and_a_new_one_is_not() {
        let names = flat(
            LogicalType::Varchar,
            &[Value::Varchar("ada".into()), Value::Varchar("ada".into()), Value::Null],
        );
        let numbers =
            flat(LogicalType::Integer, &[Value::Integer(1), Value::Integer(1), Value::Integer(1)]);
        let keys = [names, numbers];
        let mut hashes = Vec::new();
        hash(&keys, 3, &mut hashes, Across::OneInput);

        let mut table = Table::new(&[LogicalType::Varchar, LogicalType::Integer]);
        let Probe::Vacant(bucket) = table.probe(hashes[0], &keys, 0) else {
            panic!("an empty table found a group");
        };
        let slot = table.insert(bucket, hashes[0], &keys, 0).expect("room for one group");
        assert!(matches!(table.probe(hashes[1], &keys, 1), Probe::Found(found) if found == slot));
        assert!(matches!(table.probe(hashes[2], &keys, 2), Probe::Vacant(_)));
        assert_eq!(table.len(), 1);
    }

    /// The same shape of mistake as the constant one below, over nulls rather than strings, and the
    /// reason [`Vector::is_null_at`] exists. A filter that drops a row hands the rows it kept on as
    /// dictionary vectors, and a dictionary keeps its nulls in the values its codes point at rather
    /// than in a mask of its own, so asking the vector's own validity whether a row is null answers
    /// no for every row of one. That put each null row of a group by in a group of its own.
    #[test]
    fn two_null_rows_are_one_group_when_they_arrive_behind_a_dictionary() {
        let values = flat(LogicalType::Integer, &[Value::Null, Value::Integer(1)]);
        let keys = [Vector::dictionary(vec![0, 0, 1], values).expect("a dictionary of those rows")];
        let (table, slots) = one_at_a_time(&keys, 3, &[LogicalType::Integer]);
        assert_eq!(slots, [0, 0, 1], "the two nulls did not find each other");
        assert_eq!(table.len(), 2);
    }

    /// The other half of it. A stored null key must not swallow a row that has a value, which is
    /// what reading the mask the other way around would do.
    #[test]
    fn a_null_group_does_not_take_a_row_that_has_a_value_behind_a_dictionary() {
        let values = flat(LogicalType::Varchar, &[Value::Null, Value::Varchar("ada".into())]);
        let keys = [Vector::dictionary(vec![0, 1, 0], values).expect("a dictionary of those rows")];
        let (table, slots) = one_at_a_time(&keys, 3, &[LogicalType::Varchar]);
        assert_eq!(slots, [0, 1, 0]);
        assert_eq!(table.len(), 2);
    }

    /// A string column that is a constant vector is still a string column. The hash of it already
    /// agreed with the flat form, and the comparison after the probe did not, so every row of a
    /// group by on the left side of a join opened a group of its own and the answer had the same
    /// string in it once per row.
    #[test]
    fn a_group_is_found_again_when_its_string_key_arrives_as_a_constant() {
        let long = "lovelace, and a string past the sixteen bytes a view holds inline";
        for text in ["ada", "", long] {
            let names = Vector::constant(LogicalType::Varchar, Value::Varchar(text.into()), 2);
            let keys = [names];
            let mut hashes = Vec::new();
            hash(&keys, 2, &mut hashes, Across::OneInput);

            let mut table = Table::new(&[LogicalType::Varchar]);
            let Probe::Vacant(bucket) = table.probe(hashes[0], &keys, 0) else {
                panic!("an empty table found a group");
            };
            let slot = table.insert(bucket, hashes[0], &keys, 0).expect("room for one group");
            assert!(
                matches!(table.probe(hashes[1], &keys, 1), Probe::Found(found) if found == slot),
                "{text:?} did not find itself"
            );
            assert_eq!(table.len(), 1);
        }
    }

    /// The other half of it, which is that falling through to `value_at` did not make everything
    /// one group. A constant of one string and a flat column of another are two groups.
    #[test]
    fn two_constants_of_different_strings_are_still_two_groups() {
        let ada = Vector::constant(LogicalType::Varchar, Value::Varchar("ada".into()), 1);
        let grace = Vector::constant(LogicalType::Varchar, Value::Varchar("grace".into()), 1);
        let mut first = Vec::new();
        let mut second = Vec::new();
        hash(std::slice::from_ref(&ada), 1, &mut first, Across::OneInput);
        hash(std::slice::from_ref(&grace), 1, &mut second, Across::OneInput);

        let mut table = Table::new(&[LogicalType::Varchar]);
        let keys = [ada];
        let Probe::Vacant(bucket) = table.probe(first[0], &keys, 0) else {
            panic!("an empty table found a group");
        };
        table.insert(bucket, first[0], &keys, 0).expect("room for one group");
        assert!(matches!(table.probe(second[0], &[grace], 0), Probe::Vacant(_)));
    }

    /// The two halves of a bucket do not read each other. The case worth naming is a salt of all
    /// ones, which is the bit pattern an empty slot has, and a slot that is every value a slot can
    /// take next to it.
    #[test]
    fn a_salt_of_all_ones_does_not_read_as_an_empty_bucket() {
        // row at a time: a handful of slots either side of the interesting bit patterns.
        for slot in [0usize, 1, 63, 64, 65_535, LIMIT - 1] {
            for salt in [0u32, 1, u32::MAX - 1, u32::MAX] {
                let bucket = bucket_of(salt, slot);
                assert_eq!(slot_of(bucket) as usize, slot, "slot {slot} under salt {salt}");
                assert_eq!(bucket_salt(bucket), salt, "salt {salt} over slot {slot}");
                assert_ne!(slot_of(bucket), EMPTY, "slot {slot} read as an empty bucket");
            }
        }
        assert_eq!(slot_of(VACANT), EMPTY);
    }

    /// The salt is the half of the hash the bucket number did not already say, which is the only
    /// reason keeping it is worth a load. Two hashes that land in one bucket of a table of any size
    /// this reaches still differ in their salt.
    #[test]
    fn the_salt_is_bits_the_bucket_number_does_not_cover() {
        let low = 0x0000_0000_dead_beef;
        let high = 0xffff_ffff_dead_beef;
        assert_eq!(low as u32, high as u32, "the test wants two hashes that share a bucket");
        assert_ne!(salt_of(low), salt_of(high));
    }

    /// Growing is where a table stops working quietly. Every key put in before a rehash has to be
    /// found after it, so this puts in more than the sixty four it starts with.
    #[test]
    fn every_group_is_still_found_after_the_buckets_have_doubled() {
        let values: Vec<Value> = (0..1000).map(Value::BigInt).collect();
        let column = flat(LogicalType::BigInt, &values);
        let keys = [column];
        let mut hashes = Vec::new();
        hash(&keys, values.len(), &mut hashes, Across::OneInput);

        let mut table = Table::new(&[LogicalType::BigInt]);
        for (row, &one) in hashes.iter().enumerate() {
            let Probe::Vacant(bucket) = table.probe(one, &keys, row) else {
                panic!("row {row} was found before it was inserted");
            };
            let slot = table.insert(bucket, one, &keys, row).expect("room");
            assert_eq!(slot, row);
        }
        assert_eq!(table.len(), values.len());
        for (row, &one) in hashes.iter().enumerate() {
            assert!(
                matches!(table.probe(one, &keys, row), Probe::Found(slot) if slot == row),
                "row {row} was lost by a rehash"
            );
        }
        let column = table.column(0, &LogicalType::BigInt, 0..values.len()).expect("a bigint key");
        assert_eq!(column.len(), values.len());
        assert_eq!(column.value_at(7), Value::BigInt(7));
    }

    /// Packed integer keys take the same typed path as flat keys and still group identically.
    #[test]
    fn a_packed_integer_column_groups_the_same_as_the_flat_one_it_stands_for() {
        let values: Vec<Value> = (0..256).map(|row| Value::BigInt(row % 7)).collect();
        let plain = flat(LogicalType::BigInt, &values);
        let packed = plain.bit_packed().expect("a column of seven small values packs");
        assert_eq!(packed.signed_at(0), Some(0), "a packed row stays in code space");
        assert_eq!(hashed(&packed), hashed(&plain), "packing does not change a key's hash");

        let mut grouped = Vec::new();
        for column in [&plain, &packed] {
            let keys = std::slice::from_ref(column);
            let hashes = hashed(column);
            let mut table = Table::new(&[LogicalType::BigInt]);
            let mut slots = Vec::new();
            for (row, &one) in hashes.iter().enumerate() {
                slots.push(match table.probe(one, keys, row) {
                    Probe::Found(slot) => slot,
                    Probe::Vacant(bucket) => table.insert(bucket, one, keys, row).expect("room"),
                });
            }
            assert_eq!(table.len(), 7, "seven distinct keys whichever form they arrived in");
            grouped.push(slots);
        }
        assert_eq!(grouped[0], grouped[1]);
    }

    /// Half of ClickBench's schema is `SMALLINT`, and a narrow integer key used to land in the
    /// general run. There the comparison that runs once per input row per probe step built a tagged
    /// value on each side of itself, and the batched comparison had no arm for it at all and fell
    /// back to the row at a time one. A run of their own has to group the way the values do, under
    /// the batched probe as well as the single one, and give the type back on the way out.
    ///
    /// `SMALLINT` gets enough distinct values to push the table past [`HOT`], which is what makes
    /// the batched path really taken. `TINYINT` cannot reach it, because two hundred and fifty six
    /// values is every group such a column can have.
    #[test]
    fn narrow_integer_keys_group_by_value_and_come_back_in_their_own_width() {
        for (ty, modulus) in [(LogicalType::TinyInt, 251i64), (LogicalType::SmallInt, 12_007i64)] {
            let tiny = ty == LogicalType::TinyInt;
            let held = |row: i64| {
                let value = (row * 7919) % modulus - modulus / 2;
                if tiny { Value::TinyInt(value as i8) } else { Value::SmallInt(value as i16) }
            };
            let values: Vec<Value> = (0..40_000)
                .map(|row: i64| if row % 53 == 0 { Value::Null } else { held(row) })
                .collect();
            let keys = [flat(ty.clone(), &values)];
            let types = [ty.clone()];
            let (was, before) = one_at_a_time(&keys, values.len(), &types);
            let (now, after) = a_batch_at_a_time(&keys, values.len(), &types);
            assert_eq!(
                before, after,
                "{ty:?} was batched into groups it did not make one at a time"
            );
            assert_eq!(was.len(), now.len());
            assert!(tiny || now.buckets.len() > HOT, "the test has to reach the batched path");

            let bytes = now.columns[0].footprint();
            assert!(
                bytes < now.len() * size_of::<Stored>(),
                "{bytes} bytes held {} keys",
                now.len()
            );
            let column = now.column(0, &ty, 0..now.len()).expect("a narrow key column");
            // row at a time: every row has to find its own value again under the slot it was given,
            // which is what says the emit narrowed back to the width the key arrived in.
            for (row, &slot) in before.iter().enumerate() {
                assert_eq!(column.value_at(slot), values[row], "row {row} of {ty:?}");
            }
        }
    }

    /// What `column` has to keep right now that it builds the vector itself rather than handing back
    /// values for the operator above to build one from. The second range is the part that is easy to
    /// get wrong, because the validity of a slice starts at the slice and not at the table.
    #[test]
    fn a_key_column_comes_back_as_a_vector_with_its_nulls_where_they_were() {
        let values = [Value::BigInt(5), Value::Null, Value::BigInt(9)];
        let keys = [flat(LogicalType::BigInt, &values)];
        let hashes = hashed(&keys[0]);
        let mut table = Table::new(&[LogicalType::BigInt]);
        for (row, &one) in hashes.iter().enumerate() {
            let Probe::Vacant(bucket) = table.probe(one, &keys, row) else {
                panic!("row {row} was found before it was inserted");
            };
            table.insert(bucket, one, &keys, row).expect("room");
        }
        let whole = table.column(0, &LogicalType::BigInt, 0..3).expect("a bigint key");
        assert_eq!(whole.value_at(0), Value::BigInt(5));
        assert_eq!(whole.value_at(1), Value::Null);
        assert_eq!(whole.value_at(2), Value::BigInt(9));
        let tail = table.column(0, &LogicalType::BigInt, 1..3).expect("a bigint key");
        assert_eq!(tail.len(), 2);
        assert_eq!(tail.value_at(0), Value::Null);
        assert_eq!(tail.value_at(1), Value::BigInt(9));
    }

    #[test]
    fn common_numeric_keys_keep_their_physical_width() {
        let values: Vec<Value> = (0..1000).map(Value::BigInt).collect();
        let keys = [flat(LogicalType::BigInt, &values)];
        let mut hashes = Vec::new();
        hash(&keys, values.len(), &mut hashes, Across::OneInput);
        let mut table = Table::new(&[LogicalType::BigInt]);
        for (row, &hash) in hashes.iter().enumerate() {
            let Probe::Vacant(bucket) = table.probe(hash, &keys, row) else {
                panic!("a unique key was already present");
            };
            table.insert(bucket, hash, &keys, row).expect("room for the group");
        }
        let key_bytes = table.columns[0].footprint();
        assert!(
            key_bytes < values.len() * 9,
            "{key_bytes} bytes stored a thousand eight-byte keys and their validity"
        );
    }

    /// The strings a key holds are charged, and they are charged once the group is in rather than
    /// per row, since a row that is not a new group copies nothing.
    #[test]
    fn string_key_bytes_are_counted_in_the_column() {
        let long = "a string well past the sixteen bytes a view holds inline".to_string();
        let column = flat(LogicalType::Varchar, &[Value::Varchar(long.clone())]);
        let keys = [column];
        let mut hashes = Vec::new();
        hash(&keys, 1, &mut hashes, Across::OneInput);
        let mut table = Table::new(&[LogicalType::Varchar]);
        assert_eq!(table.owned(), 0);
        let Probe::Vacant(bucket) = table.probe(hashes[0], &keys, 0) else {
            panic!("an empty table found a group");
        };
        table.insert(bucket, hashes[0], &keys, 0).expect("room");
        assert!(
            table.footprint() >= long.len() as u64,
            "{} bytes do not include the string",
            table.footprint()
        );
    }
    /// A dictionary of three letters, which is the form a Parquet scan hands `l_returnflag` over
    /// in, with `codes` saying which row takes which.
    fn coded_letters(codes: Vec<u32>, letters: &[&str]) -> Vector {
        let values: Vec<Value> = letters.iter().map(|&text| Value::Varchar(text.into())).collect();
        Vector::dictionary(codes, flat(LogicalType::Varchar, &values))
            .expect("a dictionary of those letters")
    }

    /// Every row's place in the map, which is what the caller reads once per chunk.
    fn placed(coded: &Coded<'_>, rows: usize) -> Vec<usize> {
        let mut places = Vec::new();
        coded.places(rows, &mut places);
        places
    }

    /// The pair TPC-H q1 groups by, and the six places their codes take between them.
    #[test]
    fn two_small_dictionaries_give_every_row_the_place_its_codes_say() {
        let flags = coded_letters(vec![0, 1, 2, 0], &["A", "N", "R"]);
        let status = coded_letters(vec![0, 1, 1, 0], &["F", "O"]);
        let keys = [flags, status];
        let coded = coded(&keys, 4).expect("two small dictionaries are read as codes");
        // Four codes in the first column and three in the second, one of each being the place a
        // null takes, which is twelve between them.
        assert_eq!(coded.combos(), 12);
        assert_eq!(placed(&coded, 4), [0, 1 + 4, 2 + 4, 0]);
    }

    /// Rows that are null take one place between them however they came to be null, because the
    /// table holds one group for all of them and a place per row would be a group per row.
    #[test]
    fn a_null_row_takes_the_place_past_the_last_code() {
        let inner = flat(LogicalType::Varchar, &[Value::Varchar("A".into()), Value::Null]);
        let column = Vector::dictionary(vec![0, 1, 0], inner)
            .expect("a dictionary whose second value is nothing")
            .with_validity(rudb_vector::Validity::Mask({
                let mut mask = rudb_vector::Bitmap::all_valid(3);
                mask.set(2, false);
                mask
            }));
        let keys = [column];
        let coded = coded(&keys, 3).expect("one small dictionary is read as codes");
        assert_eq!(coded.combos(), 3);
        // The second row's code points at a null and the third row's own bit says it is one, and
        // both of them are the same group.
        assert_eq!(placed(&coded, 3), [0, 2, 2]);
    }

    /// A flat column goes the long way, since it says nothing about what it can be holding and
    /// finding out would be the pass this exists to avoid.
    #[test]
    fn a_flat_column_is_not_read_as_codes() {
        let keys = [flat(LogicalType::Integer, &[Value::Integer(1), Value::Integer(2)])];
        assert!(coded(&keys, 2).is_none());
    }

    /// A packed column of small integers, which is the form the native file stores `l_linenumber`
    /// in, built at a width rather than measured into one so that the test says what it means.
    fn packed_numbers(values: &[i64], width: u32, base: i128) -> Vector {
        let bits = width as usize;
        let mut words = vec![0_u64; (values.len() * bits).div_ceil(u64::BITS as usize) + 1];
        for (row, &value) in values.iter().enumerate() {
            let code =
                u64::try_from(i128::from(value) - base).expect("a value at or above the base");
            let at = row * bits;
            let (word, offset) = (at / 64, at % 64);
            words[word] |= code << offset;
            if offset + bits > 64 {
                words[word + 1] |= code >> (64 - offset);
            }
        }
        Vector::packed(LogicalType::Integer, words, width, base, values.len())
            .expect("a packed column of those values")
    }

    /// A stored integer column with a small domain is read the way a dictionary is. That is the case
    /// `l_linenumber` is in: seven values, three bits, and no dictionary anywhere, so a group by on
    /// it used to hash six million rows to find one of seven answers.
    #[test]
    fn a_packed_column_is_read_as_codes_at_its_own_width() {
        let keys = [packed_numbers(&[1, 2, 7, 1], 3, 1)];
        let coded = coded(&keys, 4).expect("a narrow packed column is read as codes");
        // The eight codes three bits can take, and one more for the place a null takes.
        assert_eq!(coded.combos(), 9);
        assert_eq!(placed(&coded, 4), [0, 1, 6, 0]);
    }

    /// A packed column beside a dictionary, since a key is read this way only when every column of
    /// it can be and the two forms have to agree on what a place is.
    #[test]
    fn a_packed_column_and_a_dictionary_share_one_map() {
        let keys = [packed_numbers(&[1, 2, 1], 2, 1), coded_letters(vec![0, 1, 0], &["A", "N"])];
        let coded = coded(&keys, 3).expect("both columns are read as places");
        // Five places for the numbers, being the four codes and a null, times three for the letters.
        assert_eq!(coded.combos(), 15);
        assert_eq!(placed(&coded, 3), [0, 1 + 5, 0]);
    }

    /// A null row of a packed column takes the place past the last code, the way a dictionary's
    /// does, because the table holds one group for every null however it arose.
    #[test]
    fn a_null_row_of_a_packed_column_takes_the_place_past_the_last_code() {
        let column = packed_numbers(&[1, 2, 1], 3, 1).with_validity(rudb_vector::Validity::Mask({
            let mut mask = rudb_vector::Bitmap::all_valid(3);
            mask.set(1, false);
            mask
        }));
        let keys = [column];
        let coded = coded(&keys, 3).expect("one narrow packed column is read as codes");
        assert_eq!(coded.combos(), 9);
        assert_eq!(placed(&coded, 3), [0, 8, 0]);
    }

    /// And a packed column wide enough that the map would cost more than the probe it replaces,
    /// which is where a key like `l_suppkey` lands.
    #[test]
    fn a_packed_column_wider_than_the_map_allows_is_refused() {
        let keys = [packed_numbers(&[0, 1], 12, 0)];
        assert!(coded(&keys, 2).is_none());
    }

    /// A code only means a value against the base and width it was packed against, so those are
    /// what the caller holds. Unlike a dictionary, which is held by identity, a page packed the same
    /// way as the one before it is one the map can be carried into.
    #[test]
    fn a_chunk_packed_against_another_base_does_not_keep_the_map() {
        let first = [packed_numbers(&[1, 2], 3, 1)];
        let over_first = coded(&first, 2).expect("codes");
        let mut held = Vec::new();
        over_first.hold(&mut held);
        assert!(over_first.same_as(&held));

        let again = [packed_numbers(&[1, 2], 3, 1)];
        assert!(coded(&again, 2).expect("codes").same_as(&held), "the same base and width");
        let rebased = [packed_numbers(&[9, 10], 3, 9)];
        assert!(!coded(&rebased, 2).expect("codes").same_as(&held), "another base");
        let widened = [packed_numbers(&[1, 2], 4, 1)];
        assert!(!coded(&widened, 2).expect("codes").same_as(&held), "another width");
        let letters = [coded_letters(vec![0, 1], &["A", "N"])];
        assert!(!coded(&letters, 2).expect("codes").same_as(&held), "another form entirely");
    }

    /// And a dictionary large enough that the map would cost more than the probe it replaces.
    #[test]
    fn a_dictionary_wider_than_the_map_allows_is_refused() {
        let values: Vec<Value> = (0..COMBOS as i32).map(Value::Integer).collect();
        let column = Vector::dictionary(vec![0, 1], flat(LogicalType::Integer, &values))
            .expect("a dictionary of that many values");
        let keys = [column];
        assert!(coded(&keys, 2).is_none());
    }

    /// The map only means anything against the dictionaries it was filled from, which is what the
    /// caller throws it away on.
    #[test]
    fn a_chunk_under_other_dictionaries_does_not_keep_the_map() {
        let first = [coded_letters(vec![0, 1], &["A", "N"])];
        let over_first = coded(&first, 2).expect("codes");
        let mut held = Vec::new();
        over_first.hold(&mut held);
        assert!(over_first.same_as(&held));

        let second = [coded_letters(vec![0, 1], &["A", "N"])];
        let later = coded(&second, 2).expect("codes");
        assert!(!later.same_as(&held), "a dictionary built again is not the one the map holds");
        assert!(coded(&[], 0).is_none(), "no key columns are no codes");
    }

    /// Which rows `repeats` marks, asked with a threshold low enough that nothing is dropped for
    /// being too short a run.
    fn marked(keys: &[Vector], rows: usize) -> Vec<bool> {
        let mut same = Vec::new();
        let count = repeats(keys, rows, 0, &mut same);
        assert_eq!(count, same.iter().filter(|&&flag| flag).count(), "the count is what is marked");
        same
    }

    #[test]
    fn a_row_is_marked_when_the_row_before_it_holds_the_same_key() {
        let column = flat(
            LogicalType::Integer,
            &[Value::Integer(7), Value::Integer(7), Value::Integer(8), Value::Integer(7)],
        );
        // The first row is never marked, because there is nothing before it to be the same as, and
        // the last one is not either, since a key that came back is still not the key beside it.
        assert_eq!(marked(std::slice::from_ref(&column), 4), [false, true, false, false]);
    }

    #[test]
    fn a_row_is_marked_only_when_every_key_column_repeats() {
        let left =
            flat(LogicalType::Integer, &[Value::Integer(1), Value::Integer(1), Value::Integer(1)]);
        let right = flat(
            LogicalType::Varchar,
            &[Value::Varchar("a".into()), Value::Varchar("a".into()), Value::Varchar("b".into())],
        );
        assert_eq!(marked(&[left, right], 3), [false, true, false]);
    }

    /// The rule the table itself follows, which is that one stored null is the group every null row
    /// lands in.
    #[test]
    fn two_nulls_next_to_each_other_are_the_same_key_and_a_null_beside_a_value_is_not() {
        let column =
            flat(LogicalType::Integer, &[Value::Null, Value::Null, Value::Integer(4), Value::Null]);
        assert_eq!(marked(std::slice::from_ref(&column), 4), [false, true, false, false]);
    }

    /// A dictionary answers off its codes, and two rows under one code are one key whatever the
    /// values behind them look like.
    #[test]
    fn a_dictionary_is_read_through_its_codes() {
        let column = coded_letters(vec![0, 0, 1, 1, 0], &["A", "N"]);
        assert_eq!(marked(std::slice::from_ref(&column), 5), [false, true, false, true, false]);
    }

    /// Floats are left alone on purpose. Whether `-0.0` groups with `0.0` is settled where the keys
    /// are compared, and guessing it here would merge two groups that belong apart.
    #[test]
    fn a_float_column_gives_up_rather_than_deciding_what_counts_as_equal() {
        let column = flat(LogicalType::Double, &[Value::Double(1.0), Value::Double(1.0)]);
        assert_eq!(marked(std::slice::from_ref(&column), 2), [false, false]);
    }

    /// And a chunk that cannot reach the threshold is dropped whole, so a caller that asked for a
    /// run path gets nothing rather than a run path over four rows in a thousand.
    #[test]
    fn a_chunk_with_too_few_repeats_for_the_caller_is_dropped() {
        let column = flat(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        );
        let keys = std::slice::from_ref(&column);
        let mut same = Vec::new();
        assert_eq!(repeats(keys, 4, 2, &mut same), 0);
        assert_eq!(same, [false, false, false, false]);
        assert_eq!(repeats(keys, 4, 1, &mut same), 1);
        assert_eq!(repeats(&[], 0, 0, &mut same), 0);
    }

    /// The same values as a stable dictionary, which is the form a `VARCHAR` column read back out of
    /// a native file arrives in and the one form a key column borrows rather than copies. The nulls
    /// are in the validity beside the codes and not in the values, the same as on the page.
    fn stable_letters(codes: Vec<u32>, seen: &[&str], nulls: &[usize]) -> Vector {
        let values: Vec<Value> = seen.iter().map(|word| Value::Varchar((*word).into())).collect();
        let rows = codes.len();
        let vector =
            Vector::stable_dictionary(codes, Arc::new(flat(LogicalType::Varchar, &values)))
                .expect("a stable dictionary of those values");
        vector.with_validity(rudb_vector::Validity::from_iter(rows, |row| !nulls.contains(&row)))
    }

    /// #1265. A null row does not end a run, because a run keeps its nulls the way the page it came
    /// from does, in the validity beside the codes. Before this the null was handed to the general
    /// push, which had no arm for a column that was reading a run and said so as an internal error
    /// naming the value's type, which is what a nullable `VARCHAR` read back from a file raised.
    #[test]
    fn a_null_row_of_a_stable_dictionary_stays_in_the_run() {
        let column = stable_letters(vec![0, 0, 1], &["a", "bb"], &[1]);
        let mut key = Column::new(&LogicalType::Varchar);
        for row in 0..3 {
            key.push_from(&column, row).expect("every row goes in");
        }
        assert!(matches!(key.data, StoredData::StableText { .. }), "a null did not end the run");
        assert_eq!(
            key.values(0..3),
            [Value::Varchar("a".into()), Value::Null, Value::Varchar("bb".into()),]
        );
        // Read back out as a vector as well, since that is built from the whole run at once and a
        // code past the end of the dictionary is refused there rather than where it was stored.
        let vector = key.vector(&LogicalType::Varchar, 0..3).expect("the run comes back");
        assert!(vector.is_null_at(1));
        assert_eq!(vector.bytes_at(2), Some(b"bb".as_slice()));
    }

    /// And a run that starts on a null, which is the row the column has nothing to compare against.
    #[test]
    fn a_run_that_starts_on_a_null_holds_the_rows_after_it() {
        let column = stable_letters(vec![0, 0, 1], &["a", "bb"], &[0]);
        let mut key = Column::new(&LogicalType::Varchar);
        for row in 0..3 {
            key.push_from(&column, row).expect("every row goes in");
        }
        assert_eq!(
            key.values(0..3),
            [Value::Null, Value::Varchar("a".into()), Value::Varchar("bb".into()),]
        );
    }

    /// A code means something only next to the dictionary it was read from, so a row under another
    /// one ends the run. What it must not do is lose the rows the run was already holding.
    #[test]
    fn a_row_under_another_dictionary_ends_the_run_and_keeps_what_it_held() {
        let first = stable_letters(vec![0, 1, 0], &["a", "bb"], &[1]);
        let second = stable_letters(vec![1], &["a", "cc"], &[]);
        let mut key = Column::new(&LogicalType::Varchar);
        for row in 0..3 {
            key.push_from(&first, row).expect("every row goes in");
        }
        key.push_from(&second, 0).expect("the row under the other dictionary goes in too");
        assert!(matches!(key.data, StoredData::Varchar(_)), "the run ended");
        assert_eq!(
            key.values(0..4),
            [
                Value::Varchar("a".into()),
                Value::Null,
                Value::Varchar("a".into()),
                Value::Varchar("cc".into()),
            ]
        );
    }

    /// The same thing said where the operator above can see it. A column handed over as a run and
    /// the same column handed over flat have to put the same rows in the same groups in the same
    /// order, because a table read back from a file hands its strings over the first way and a
    /// table still in memory hands them over the second.
    #[test]
    fn a_stable_dictionary_groups_the_same_as_the_flat_column_it_stands_for() {
        let values = [
            Value::Varchar("a".into()),
            Value::Null,
            Value::Varchar("a".into()),
            Value::Varchar(String::new()),
            Value::Null,
            Value::Varchar("bb".into()),
        ];
        let types = [LogicalType::Varchar];
        let run = [stable_letters(vec![0, 0, 0, 1, 0, 2], &["a", "", "bb"], &[1, 4])];
        let plain = [flat(LogicalType::Varchar, &values)];
        let (over_run, run_slots) = one_at_a_time(&run, values.len(), &types);
        let (over_plain, plain_slots) = one_at_a_time(&plain, values.len(), &types);
        assert_eq!(run_slots, plain_slots);
        assert_eq!(over_run.len(), over_plain.len());
        let (batched, batched_slots) = a_batch_at_a_time(&run, values.len(), &types);
        assert_eq!(batched_slots, plain_slots);
        assert_eq!(batched.len(), over_plain.len());
    }
}

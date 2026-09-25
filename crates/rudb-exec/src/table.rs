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

use rudb_common::bounds::Bound;
use rudb_common::{Error, Result, Value, interval_micros};
use rudb_kernels::NOWHERE;
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
    /// The slot of each value in a key range, where the planner said the key has one.
    ///
    /// A shortcut to the buckets beside it and never a replacement for them. Every group is in both,
    /// and a row this cannot answer is answered by the buckets the way it always was.
    direct: Option<Direct>,
}

/// One slot per value of a key that is one integer column inside a known range.
///
/// The hash table answers which group a row belongs to with two trips to memory, one for the bucket
/// and one for the stored key the bucket pointed at, and the second address is only known once the
/// first has landed. When the key is one integer and the range it lies in is known, the value is the
/// address: subtract the smallest and read the cell. One trip, no salt, no key comparison and no
/// collisions.
///
/// # Why the buckets stay
///
/// Because a range is a statistic and a wrong answer is not an acceptable failure for one. The range
/// comes from the two ends a store wrote, through `rudb_opt`'s `dense` pass, and every link in that
/// chain is a place a range could come out narrower than the column. So this is only ever consulted
/// as a shortcut: a value with no cell for it, a key column in a form this cannot read, and a batch
/// with anything unusual in it all fall back to the buckets, and the buckets hold every group either
/// way. The cost of that promise is one array write per group, which happens once per group.
#[derive(Debug)]
struct Direct {
    /// The slot of the group whose key is this cell's value, [`EMPTY`] where there is no such group.
    ///
    /// Cell zero is the null key, so the value `v` is in cell `v - base + 1` and the array is one
    /// longer than the range. A null in a grouping key is a group of its own, which is what
    /// [`Column::holds`] says about a stored null, so it needs a cell like every other key.
    cells: Vec<u32>,
    /// The smallest value the range covers.
    base: i128,
}

impl Direct {
    /// The cell the value at `row` of a one column key belongs in, if the range has one.
    ///
    /// `None` for a value outside the range and for a key this cannot read as one integer, both of
    /// which send the caller back to the buckets.
    fn cell(&self, keys: &[Vector], row: usize) -> Option<usize> {
        let [column] = keys else { return None };
        if !column.validity().is_valid(row) {
            return Some(0);
        }
        let Bound::Int(value) = Bound::of_value(&column.value_at(row))? else {
            return None;
        };
        self.of(value)
    }

    /// The cell that value belongs in, if the range has one.
    fn of(&self, value: i128) -> Option<usize> {
        let step = value.checked_sub(self.base)?.checked_add(1)?;
        usize::try_from(step).ok().filter(|&cell| cell < self.cells.len())
    }

    /// What this has taken from the allocator, capacity rather than length for the reason
    /// [`Table::footprint`] gives.
    fn footprint(&self) -> usize {
        self.cells.capacity() * size_of::<u32>()
    }
}

/// Whether a key of this type can be read as the same integer by both sides of the direct index.
///
/// [`Table::insert`] writes a group's cell from a `Value`, through [`Bound::of_value`], because it
/// has to work for a key column in any form. [`Table::direct_at`] reads a batch off the stored run,
/// because reading a `Value` per row is most of what it exists to avoid. Those are two different
/// ways of getting to the same number and they only agree for some types: a `TIMESTAMP` and a
/// `DECIMAL` are both stored in an `i64` that `direct_at` could read happily, and both come back
/// from `of_value` as `Bound::Scaled` rather than `Bound::Int`, so the insert would write no cell
/// where the probe would read one. Every group would then look missing and be inserted twice.
///
/// Nothing in the planner can ask for a range over either of them today, because the pass that
/// writes one takes nothing but a pair of `Bound::Int` ends. This is here so that the table does not
/// depend on that: the disagreement is between two functions in this file and the check belongs
/// beside them.
fn addressable(ty: &rudb_common::LogicalType) -> bool {
    use rudb_common::LogicalType as Type;
    matches!(
        ty,
        Type::Boolean
            | Type::TinyInt
            | Type::SmallInt
            | Type::Integer
            | Type::BigInt
            | Type::UTinyInt
            | Type::USmallInt
            | Type::UInteger
            | Type::UBigInt
            | Type::Date
    )
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
        Self::sized(types, FIRST)
    }

    /// An empty table with room for `groups` groups already taken.
    ///
    /// The doubling in [`Table::insert`] is what this exists to skip. A table that ends with a
    /// million groups doubles fifteen times on the way there and each doubling rewrites every bucket
    /// it already had, so most of the bucket writes a group by does are writes of groups that were
    /// already in it. Asking for the room once is one allocation and no rewrites.
    ///
    /// The count is a number the planner is willing to stand behind rather than a guess, which is
    /// `rudb_opt`'s `presize` pass and the reasoning in its module documentation. A table that is
    /// given too small a number grows the way it always did, and nothing here depends on the number
    /// being right for the answer to be right: it is a capacity and the keys decide everything else.
    pub(crate) fn with_groups(types: &[rudb_common::LogicalType], groups: u64) -> Self {
        Self::sized(types, Self::buckets_for(groups))
    }

    /// The bytes the bucket array of [`Table::with_groups`] takes for `groups` groups, which is
    /// what an aggregate reserves before it asks for them.
    ///
    /// Exact, because the array is one `u64` a bucket and the count is worked out here the same
    /// way it is there. The key columns and the hashes grow as groups arrive and are charged as they
    /// do, so they are not in this.
    pub(crate) fn room(groups: u64) -> u64 {
        u64::try_from(Self::buckets_for(groups) * size_of::<u64>()).unwrap_or(u64::MAX)
    }

    /// How many buckets hold `groups` groups without growing.
    fn buckets_for(groups: u64) -> usize {
        // Half full is where `insert` grows, so the room for `groups` of them is twice that many
        // buckets, rounded up to the power of two the mask needs. `FIRST` is the floor because a
        // table smaller than the one every other table starts at is not an optimization.
        let wanted = usize::try_from(groups.saturating_mul(2)).unwrap_or(usize::MAX);
        // Halved before the rounding rather than after, so that the rounding cannot carry it past
        // the most slots a `u32` can address.
        wanted.clamp(FIRST, LIMIT / 2).next_power_of_two()
    }

    /// An empty table with `buckets` buckets, which is a power of two at or above [`FIRST`].
    fn sized(types: &[rudb_common::LogicalType], buckets: usize) -> Self {
        Self {
            buckets: vec![VACANT; buckets],
            columns: types.iter().map(Column::new).collect(),
            hashes: Vec::new(),
            owned: 0,
            direct: None,
        }
    }

    /// The same table with a direct index over a key that lies in a known range.
    ///
    /// `low` is the smallest value the key column can hold and `values` is how many values the range
    /// covers. Nothing about the table changes except that a probe of a value in the range can read
    /// its slot instead of walking to it, which is why this is a builder on top of the two
    /// constructors rather than a third one: a table that takes this and a table that does not hold
    /// the same groups in the same slots and answer the same rows.
    ///
    /// Ignored for a key of anything other than one column, for a key whose type is not
    /// [`addressable`], and for a range that will not fit in memory as a `Vec<u32>`, all of which
    /// leave the table exactly as it arrived.
    pub(crate) fn over_range(
        mut self,
        low: i128,
        values: u64,
        ty: &rudb_common::LogicalType,
    ) -> Self {
        if self.columns.len() != 1 || !addressable(ty) {
            return self;
        }
        // One longer than the range, for the null key in cell zero.
        let Ok(cells) = usize::try_from(values.saturating_add(1)) else {
            return self;
        };
        self.direct = Some(Direct { cells: vec![EMPTY; cells], base: low });
        self
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
        let direct = self.direct.as_ref().map_or(0, Direct::footprint);
        u64::try_from(buckets + hashes + keys + direct).unwrap_or(u64::MAX)
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
        if self.direct_at(keys, rows, slots, walk) {
            return;
        }
        if self.buckets.len() <= HOT {
            if let [column] = keys
                && self.hot_one(hashes, column, rows, slots, walk)
            {
                return;
            }
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

    /// The batch answered off the direct index, or `false` if it could not be.
    ///
    /// The whole batch or none of it. A row whose value has no cell would have to fall back to the
    /// buckets on its own, and a batch split between two paths is two passes over the same rows with
    /// a branch per row deciding which, which is most of what the batched probe exists to avoid. So
    /// the cells are worked out first, into a buffer, and one value the range does not cover sends
    /// the whole batch to the hash path. Nothing has been written to `slots` by then.
    ///
    /// The reasons it can answer nothing at all are the ordinary ones: no direct index, a key that
    /// is not one column, a key column in a form with no run of values to read, and a type that is
    /// not stored as an integer. Every one of them is decided once for the batch.
    fn direct_at(
        &self,
        keys: &[Vector],
        rows: Rows<'_>,
        slots: &mut [usize],
        walk: &mut Walk,
    ) -> bool {
        let Some(direct) = self.direct.as_ref() else { return false };
        let [column] = keys else { return false };
        let Some(data) = column.data() else { return false };
        let validity = column.validity();
        walk.cells.clear();
        /// One pass over a run of fixed width values, each place reading its own row's place in it.
        macro_rules! cells {
            ($values:expr) => {{
                let values = $values.as_slice();
                for out in 0..rows.len() {
                    let row = rows.at(out);
                    let cell = if !validity.is_valid(row) {
                        0
                    } else {
                        let Some(&value) = values.get(row) else { return false };
                        let Some(cell) = direct.of(i128::from(value)) else { return false };
                        cell
                    };
                    walk.cells.push(direct.cells[cell]);
                }
            }};
        }
        match data {
            Data::Int8(values) => cells!(values),
            Data::Int16(values) => cells!(values),
            Data::Int32(values) => cells!(values),
            Data::Int64(values) => cells!(values),
            Data::UInt8(values) => cells!(values),
            Data::UInt16(values) => cells!(values),
            Data::UInt32(values) => cells!(values),
            Data::UInt64(values) => cells!(values),
            // `Int128` and `UInt128` are left out and not because they cannot be read. A `HUGEINT`
            // key whose range is small enough to address is a key somebody stored in the widest
            // integer there is and then used a hundred values of, and carrying two more arms for it
            // is carrying them for nobody.
            _ => return false,
        }
        // Written only once every cell is known, so a batch that gave up above left `slots` alone
        // and the caller's own idea of what an unfilled slot means is what survives. In place order,
        // which is row order for both kinds of batch, so `pending` comes out sorted without sorting.
        for (out, &slot) in walk.cells.iter().enumerate() {
            if slot == EMPTY {
                walk.pending.push(out);
            } else {
                slots[out] = slot as usize;
            }
        }
        true
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
        // The direct index beside the buckets, so the two hold the same groups. Worked out before
        // the write because the read of the key needs the key columns and the write needs the index,
        // and a value the range does not cover simply has no cell and lives in the buckets alone.
        // Once per group and never per row, which is what makes keeping both cheap.
        let cell = self.direct.as_ref().and_then(|direct| direct.cell(keys, row));
        if let (Some(direct), Some(cell)) = (self.direct.as_mut(), cell) {
            direct.cells[cell] = u32::try_from(slot).unwrap_or(EMPTY);
        }
        // Half full rather than the seven eighths a `HashMap` allows, because this probes linearly
        // and a linear probe at seven eighths walks a run of about eight buckets to find a miss.
        // The buckets are eight bytes each, so the room the other half costs is small next to the
        // keys beside it.
        if self.hashes.len() * 2 >= self.buckets.len() {
            self.regrow();
        }
        Ok(slot)
    }

    /// Adds the key that `keys` holds at `row` as a group nobody will look up.
    ///
    /// For a group the aggregate closed as soon as it opened, because its key is past every row
    /// that could still arrive. Such a group is only ever read back out, so it takes no bucket and
    /// no hash, and the buckets stay the size they started. A table filled this way must not be
    /// probed, merged or scattered, which is why the stored hash is a zero that means nothing.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::OutOfMemory`] at [`LIMIT`] groups.
    pub(crate) fn append(&mut self, keys: &[Vector], row: usize) -> Result<usize> {
        let slot = self.hashes.len();
        if slot >= LIMIT {
            return Err(Error::out_of_memory(format!(
                "a single group by cannot hold more than {LIMIT} groups"
            )));
        }
        for (at, column) in keys.iter().enumerate() {
            self.owned += self.columns[at].push_from(column, row)?;
        }
        self.hashes.push(0);
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

    /// The row at a time probe of a table that fits in cache, for one flat integer key with no nulls.
    ///
    /// [`Self::probe`] asks [`Column::holds`] at every step, which matches on the stored type and
    /// then on the vector's form and widens both sides to 128 bits, all of it the same for every row
    /// of the batch. On a table this small there is no miss for that to hide behind, so it is most
    /// of the probe: ClickBench 43 groups half a million rows into 1440 minutes and spent more time
    /// in the comparison than in the walk. This settles both matches once and compares two slices.
    ///
    /// `false` for anything else, which the caller then probes the way it always did.
    fn hot_one(
        &self,
        hashes: &[u64],
        column: &Vector,
        rows: Rows<'_>,
        slots: &mut [usize],
        walk: &mut Walk,
    ) -> bool {
        let Some(data) = column.data() else { return false };
        if column.validity().has_nulls(column.len()) {
            return false;
        }
        let stored = &self.columns[0];
        let mask = self.buckets.len() - 1;
        macro_rules! walk {
            ($stored:expr, $values:expr, $widen:expr) => {{
                let (held, values) = ($stored, $values.as_slice());
                for (out, found) in slots.iter_mut().enumerate().take(rows.len()) {
                    let row = rows.at(out);
                    let hash = hashes[row];
                    let salt = salt_of(hash);
                    let mut at = (hash as usize) & mask;
                    loop {
                        let bucket = self.buckets[at];
                        let slot = slot_of(bucket);
                        if slot == EMPTY {
                            walk.pending.push(out);
                            break;
                        }
                        let slot = slot as usize;
                        if bucket_salt(bucket) == salt
                            && match values.get(row) {
                                Some(&value) => stored.valid[slot] && $widen(value) == held[slot],
                                None => stored.holds(slot, column, row),
                            }
                        {
                            *found = slot;
                            break;
                        }
                        at = (at + 1) & mask;
                    }
                }
                true
            }};
        }
        match (&stored.data, data) {
            (StoredData::TinyInt(held), Data::Int8(values)) => walk!(held, values, |v| v),
            (StoredData::SmallInt(held), Data::Int16(values)) => walk!(held, values, |v| v),
            (StoredData::Integer(held), Data::Int32(values)) => walk!(held, values, |v| v),
            (StoredData::BigInt(held), Data::Int64(values)) => walk!(held, values, |v| v),
            (StoredData::Wide { values: held, .. }, Data::Int128(values)) => {
                walk!(held, values, |v| v)
            }
            (StoredData::Wide { values: held, .. }, Data::Int64(values)) => {
                walk!(held, values, i128::from)
            }
            (StoredData::Wide { values: held, .. }, Data::Int32(values)) => {
                walk!(held, values, i128::from)
            }
            (StoredData::Wide { values: held, .. }, Data::Int16(values)) => {
                walk!(held, values, i128::from)
            }
            _ => false,
        }
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
        self.columns[at].vector_at(ty, slots)
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
    /// The slot each place of the last batch read straight out of the direct index.
    ///
    /// Its own buffer rather than writing into `slots`, because the direct path answers the whole
    /// batch or none of it and a batch it gave up on has to leave `slots` as it found it.
    cells: Vec<u32>,
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

/// The most places the direct map covers when the whole key is one dictionary.
///
/// Larger than [`COMBOS`] by a factor of a hundred and twenty eight, and the reason is that one
/// column's places are not a product. Every place in a one column map is a value that column's
/// dictionary actually holds, so the map is never bigger than the distinct values the chunk's source
/// could hand over. Put two columns together and the map is their product, most of which no row will
/// ever land in, which is what [`COMBOS`] is small for.
///
/// What it buys is measured in `spec/storage-v3/24`. A Parquet reader hands `Referer` over as a
/// dictionary of about a hundred and twenty eight thousand entries covering a row group of four
/// hundred and forty two thousand rows, and the rows a thread sees under one of those repeat a value
/// three times over. Under [`COMBOS`] that key was hashed, probed and compared once a row; a map
/// this wide answers two rows in three from a load, and the profile in that document has the probe
/// and the comparison it replaces at 21.5% of the query.
///
/// Two hundred and sixty two thousand places is a megabyte of `u32` per thread, which is the bound
/// worth stating: the map is per thread and is not charged against the memory budget, so this is a
/// number about what the engine may hold quietly rather than about what any chunk needs.
const WIDE_COMBOS: usize = 1 << 18;

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
    /// A packed run behind a list of rows, which is the shape a filter leaves on a packed column.
    ///
    /// The same identity as [`Self::Bits`], because it is the same run and a code means a value
    /// against the base and the width and nothing else. That is what lets one map serve a chunk
    /// the filter cut and the next chunk of the same page whole.
    CodedBits {
        /// The row of the run each row of the chunk reads, which is the filter's selection.
        at: &'a [u32],
        /// The run those rows are read out of.
        packed: Packed<'a>,
    },
    /// An integer column read by its value, as the distance from the bottom of a window.
    ///
    /// The form for an integer key that has no small places of its own. `CounterID` takes four
    /// thousand values between 17 and 262,029, which packs at eighteen bits, and that is too wide
    /// for [`Self::Bits`] once a second column could multiply it. After a filter it is not packed at
    /// all but copied out flat, and a flat column has nothing to read a place out of. Its values
    /// still fall inside a window a quarter of a million wide, and a value minus the bottom of that
    /// window is a place in the same way a code is.
    ///
    /// The window is the caller's rather than the page's, which is the other half of it. A page
    /// packs against its own base, so a map held by base and width is thrown away at every page,
    /// while a window is kept for as long as the chunks keep landing inside it.
    Values {
        /// This chunk's value per row, widened, and cut to the rows being asked about.
        values: &'a [i64],
        /// The value that takes place zero.
        low: i64,
        /// The runs of one value the pass that settled the window found on its way, each as the
        /// value and the row it ends before, or empty when it did not keep them.
        runs: &'a [(i64, usize)],
    },
}

/// Where the key columns a map reads by value are widened into, kept by the caller so that it is
/// not allocated once a chunk.
#[derive(Debug, Default)]
pub(crate) struct Widened {
    /// Each key column's values, one run per column.
    values: Vec<Vec<i64>>,
    /// Each key column's runs of one value, as [`Places::Values`] holds them.
    runs: Vec<Vec<(i64, usize)>>,
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
    /// The bottom of a window of values and how many places it takes, the null place included.
    Window(i64, usize),
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
                Places::CodedBits { at, packed } => {
                    for (row, place) in into.iter_mut().enumerate() {
                        let code = if column.is_null_at(row) {
                            nothing
                        } else {
                            packed.code(at[row] as usize) as usize
                        };
                        *place += code * stride;
                    }
                }
                Places::Values { values, low, .. } => {
                    for (row, place) in into.iter_mut().enumerate() {
                        let code = if column.is_null_at(row) {
                            nothing
                        } else {
                            values[row].wrapping_sub(low) as u64 as usize
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
            Places::CodedBits { at, packed } => {
                for (row, place) in into.iter_mut().enumerate() {
                    *place += packed.code(at[row] as usize) as usize * stride;
                }
            }
            Places::Values { values, low, .. } => {
                for (place, &value) in into.iter_mut().zip(values) {
                    *place += value.wrapping_sub(low) as u64 as usize * stride;
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
///
/// That last sentence is also why the map is worth having on a key nobody would call low
/// cardinality. `Referer` arrives out of a row group as a dictionary of about a hundred and twenty
/// eight thousand entries covering four hundred and forty two thousand rows, and the thirty two
/// thousand row morsel a thread takes out of that holds ten thousand distinct values, so two rows in
/// three are a value the map has already answered. See [`WIDE_COMBOS`].
pub(crate) struct Coded<'a> {
    columns: [Option<CodedColumn<'a>>; KEYS],
    combos: usize,
}

impl<'a> Coded<'a> {
    /// How many combinations of codes the key can take, which is how long the map has to be.
    pub(crate) fn combos(&self) -> usize {
        self.combos
    }

    /// Whether every key column is read by its value, so that [`Self::hash_of`] can answer a row.
    pub(crate) fn by_value(&self) -> bool {
        self.columns.iter().flatten().all(|column| matches!(column.places, Places::Values { .. }))
    }

    /// Whether any key column is read by its value, so that the map's places are a window of
    /// values the caller chose rather than codes a page came with.
    pub(crate) fn reads_values(&self) -> bool {
        self.columns.iter().flatten().any(|column| matches!(column.places, Places::Values { .. }))
    }

    /// The hash [`hash`] gives `row`, worked out for that row alone.
    ///
    /// Only for a key [`Self::by_value`] says is read by value, which is a narrow integer in every
    /// column, and for those [`hash`] folds each value in as its own word whatever form it came in.
    /// So the caller can hash the rows that miss the map rather than every row of the chunk. On
    /// `CounterID`, which arrives sorted, most chunks bring a few dozen values the map has not seen,
    /// and hashing the whole of each such chunk was a sixth of what the group by cost.
    pub(crate) fn hash_of(&self, row: usize) -> u64 {
        let mut state = 0;
        for column in self.columns.iter().flatten() {
            let word = match column.places {
                _ if column.nullable && column.column.is_null_at(row) => NOTHING,
                Places::Values { values, .. } => values[row] as u64,
                _ => NOTHING,
            };
            state = mix(state, word);
        }
        spread(state)
    }

    /// Fills `places` with the index in the map of each row's key, one pass per key column.
    pub(crate) fn places(&self, rows: usize, places: &mut Vec<usize>) {
        places.clear();
        places.resize(rows, 0);
        for column in self.columns.iter().flatten() {
            column.add_into(places);
        }
    }

    /// Answers each row's slot straight out of `map`, for a key of one or two dictionary columns
    /// with no nulls, and says whether any row found nothing there.
    ///
    /// That is the key of TPC-H q1, and [`Self::places`] followed by a lookup was two passes and a
    /// vector of places for it, one pass to write each row's place and one to read it back. Here a
    /// row's place is worked out in a register and used at once. The rows that found nothing are
    /// the first of each combination and need their place again, so when there are any the caller
    /// asks [`Self::places`] for them the long way. `None` is a key this does not answer.
    pub(crate) fn look_up(&self, map: &[u32], slots: &mut [usize]) -> Option<bool> {
        let mut plain = self.columns.iter().flatten().map(|column| match column.places {
            Places::Codes { codes, .. } if !column.nullable => Some((codes, column.stride)),
            _ => None,
        });
        let (first, stride) = plain.next()??;
        let second = plain.next();
        if plain.next().is_some() {
            return None;
        }
        let mut missed = false;
        match second {
            None => {
                for (slot, &code) in slots.iter_mut().zip(first) {
                    let found = map[code as usize * stride];
                    missed |= found == UNSEEN;
                    *slot = slot_at(found);
                }
            }
            Some(second) => {
                let (other, across) = second?;
                for ((slot, &code), &next) in slots.iter_mut().zip(first).zip(other) {
                    let found = map[code as usize * stride + next as usize * across];
                    missed |= found == UNSEEN;
                    *slot = slot_at(found);
                }
            }
        }
        Some(missed)
    }

    /// Cuts the chunk into runs of one place, each given as its place and the row it ends before,
    /// for a key of one column with no nulls, and says whether it could.
    ///
    /// A key the rows are sorted on, the way `CounterID` is, reaches a chunk as a few dozen runs.
    /// Found here, in one pass over the column where it already is, the map is read once a run
    /// rather than once a row, and the runs are what the aggregates fold by without a pass over
    /// the slots to find them again. `false`, with `into` cleared, for more than `most` runs, where
    /// a row at a time costs less, and for a key this does not read.
    pub(crate) fn place_runs(
        &self,
        rows: usize,
        most: usize,
        into: &mut Vec<(usize, usize)>,
    ) -> bool {
        into.clear();
        let mut columns = self.columns.iter().flatten();
        let (Some(column), None) = (columns.next(), columns.next()) else {
            return false;
        };
        if column.nullable {
            return false;
        }
        let stride = column.stride;
        match column.places {
            // Runs the window already found are placed as they are, without a second look at the
            // values. On ClickBench 28 the look was a fifth of the fold.
            Places::Values { runs, low, .. } if runs.last().is_some_and(|run| run.1 == rows) => {
                if runs.len() > most + 1 {
                    return false;
                }
                let place = |value: i64| value.wrapping_sub(low) as u64 as usize * stride;
                into.extend(runs.iter().map(|&(value, end)| (place(value), end)));
                true
            }
            Places::Values { values, low, .. } => values.get(..rows).is_some_and(|values| {
                runs_in(values, most, into, |value| {
                    value.wrapping_sub(low) as u64 as usize * stride
                })
            }),
            Places::Codes { codes, .. } => codes
                .get(..rows)
                .is_some_and(|codes| runs_in(codes, most, into, |code| code as usize * stride)),
            _ => false,
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
                (
                    Some(Origin::Bits(base, width)),
                    Places::Bits { packed } | Places::CodedBits { packed, .. },
                ) => *base == packed.base() && *width == packed.width(),
                (Some(Origin::Window(bottom, span)), Places::Values { low, .. }) => {
                    *bottom == low && *span == column.nothing + 1
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

    /// The span of the map `held` was built on, when this chunk's places are that map's grown
    /// upward.
    ///
    /// That is one key column read by value, against a window with the same bottom and more places
    /// than before, and a map of `built` places that is still the one `held` describes. Every value
    /// then has the place it had, and the one place that means something else is the old last one,
    /// which was the null place and is now a value's.
    pub(crate) fn grows(&self, held: &[Origin], built: usize) -> Option<usize> {
        let [Some(column), rest @ ..] = &self.columns else {
            return None;
        };
        if rest.iter().any(Option::is_some) {
            return None;
        }
        match (held, column.places) {
            ([Origin::Window(bottom, span)], Places::Values { low, .. })
                if *bottom == low && *span == built && *span < column.nothing + 1 =>
            {
                Some(*span)
            }
            _ => None,
        }
    }

    /// Records what the map about to be built reads its places out of.
    pub(crate) fn hold(&self, into: &mut Vec<Origin>) {
        into.clear();
        for column in self.columns.iter().flatten() {
            into.push(match column.places {
                Places::Codes { values, .. } => Origin::Dictionary(Arc::clone(values)),
                Places::Bits { packed } | Places::CodedBits { packed, .. } => {
                    Origin::Bits(packed.base(), packed.width())
                }
                Places::Values { low, .. } => Origin::Window(low, column.nothing + 1),
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
///
/// This one reads only the forms that carry places of their own. [`coded_within`] also reads an
/// integer column by its value, which needs somewhere to put the values and the windows the map
/// was last built on.
#[cfg(test)]
pub(crate) fn coded<'a>(keys: &'a [Vector], rows: usize) -> Option<Coded<'a>> {
    coded_within(keys, rows, &[], None)
}

/// The runs one key's window found, each a value and how many rows in a row hold it.
type KeyRuns = Vec<(i64, usize)>;

/// [`coded`], and an integer column with no places of its own read by its value against a window.
///
/// `held` is what the map beside the caller was last built on, so that a column read by value keeps
/// the window it had for as long as its chunks land inside it and the map lives on. `widened` is
/// where those columns are widened into. Without it a column read by value is refused the way
/// [`coded`] refuses it.
pub(crate) fn coded_within<'a>(
    keys: &'a [Vector],
    rows: usize,
    held: &[Origin],
    widened: Option<&'a mut Widened>,
) -> Option<Coded<'a>> {
    if keys.is_empty() || keys.len() > KEYS {
        return None;
    }
    // One column's places are the values it holds and several columns' places are their product,
    // which is why the two get different room. See [`WIDE_COMBOS`].
    let room = if keys.len() == 1 { WIDE_COMBOS } else { COMBOS };
    // The columns with places of their own first, because what they take out of the room is what
    // a window is allowed to be.
    let mut found = [None; KEYS];
    let mut fallback = [None; KEYS];
    let mut taken: usize = 1;
    let mut wanting = 0;
    for (at, key) in keys.iter().enumerate() {
        match places_of(key, rows, room) {
            // A packed page whose base is not the one the map was built on, which is every page
            // after the first when a key is sorted, since each packs against its own minimum. Its
            // codes would throw the map away, so it is read by value against the window instead,
            // and falls back on its codes only when the window will not have it.
            Some(read) if widened.is_some() && moved_off(&read.0, held.get(at)) => {
                fallback[at] = Some(read);
                wanting += 1;
            }
            Some(read) => {
                taken = taken.checked_mul(read.1).filter(|&taken| taken <= room)?;
                found[at] = Some(read);
            }
            None => wanting += 1,
        }
    }
    let mut windows = [None; KEYS];
    let (values, runs): (&'a [Vec<i64>], &'a [KeyRuns]) = if wanting == 0 {
        (&[], &[])
    } else {
        let Widened { values, runs } = widened?;
        values.resize_with(keys.len(), Vec::new);
        runs.resize_with(keys.len(), Vec::new);
        for (at, key) in keys.iter().enumerate() {
            if found[at].is_some() {
                continue;
            }
            let limit = room / taken;
            // Only a key of one column is ever folded by its runs. See [`Coded::place_runs`].
            let into = (&mut values[at], &mut runs[at], keys.len() == 1);
            let window = window_of(key, rows, held.get(at), limit, wanting == 1, into);
            match (window, fallback[at]) {
                (Some(window), _) => {
                    taken = taken.checked_mul(window.1).filter(|&taken| taken <= room)?;
                    windows[at] = Some(window);
                }
                (None, Some(read)) => {
                    taken = taken.checked_mul(read.1).filter(|&taken| taken <= room)?;
                    found[at] = Some(read);
                }
                (None, None) => return None,
            }
        }
        (values, runs)
    };
    let mut columns = [None; KEYS];
    let mut combos: usize = 1;
    for (at, key) in keys.iter().enumerate() {
        let (places, span, nullable) = match (found[at], windows[at]) {
            (Some(read), _) => read,
            (None, Some((low, span, nullable))) => {
                let runs = runs.get(at).map_or(&[][..], Vec::as_slice);
                let values = values.get(at)?.get(..rows)?;
                (Places::Values { values, low, runs }, span, nullable)
            }
            (None, None) => return None,
        };
        if combos.checked_mul(span)? > room {
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

/// What a place of the aggregate's map of codes holds before a group is found for it.
///
/// The map holds a slot in 32 bits rather than a `usize`. Every instance clears a map seeded on a
/// key's ends, up to a quarter of a million places, and a sorted key hands each instance a stretch
/// of the values, so the clear is most of what the map costs on six threads. Half the width is half
/// of that, and half the cache lines a key in no order reads its slots out of. A table that could
/// take a group past [`UNSEEN`] is not given a map. See [`fits_the_map`].
pub(crate) const UNSEEN: u32 = u32::MAX;

/// The slot a place of the map holds, `NOWHERE` for [`UNSEEN`].
#[inline(always)]
pub(crate) fn slot_at(held: u32) -> usize {
    if held == UNSEEN { NOWHERE } else { held as usize }
}

/// What a place of the map holds for `slot`, [`UNSEEN`] for `NOWHERE`.
#[inline(always)]
pub(crate) fn held_at(slot: usize) -> u32 {
    u32::try_from(slot).unwrap_or(UNSEEN)
}

/// Whether every slot a table of `groups` groups can give out over `rows` more rows fits a place of
/// the map, which is under [`UNSEEN`].
pub(crate) fn fits_the_map(groups: usize, rows: usize) -> bool {
    groups.saturating_add(rows) < UNSEEN as usize
}

/// The window a map of one integer key column's values starts on when the planner knows the
/// column's ends, being its bottom and its places with the null place counted, or `None` for a key
/// of another shape or a range wider than [`coded_within`] would give one column.
///
/// Only the integer types a chunk widens to their own values, so that a place is the value less
/// the bottom here the way it is in [`window_of`].
pub(crate) fn seeded_window(
    types: &[rudb_common::LogicalType],
    low: i128,
    values: u64,
) -> Option<(i64, usize)> {
    use rudb_common::LogicalType;
    let [ty] = types else { return None };
    if !matches!(
        ty,
        LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
    ) {
        return None;
    }
    let places = usize::try_from(values).ok()?.checked_add(1)?;
    (places <= WIDE_COMBOS).then_some((i64::try_from(low).ok()?, places))
}

/// Whether a column's own places come from a packed page other than the one `held` was built on.
///
/// Only once there is a map, since a first page is as good a place as any to start one. A shared
/// dictionary is the same dictionary from one chunk to the next and keeps its codes, and a string
/// column that moves off one is refused by value at once and keeps its codes too.
fn moved_off(places: &Places<'_>, held: Option<&Origin>) -> bool {
    match (places, held) {
        (Places::Bits { packed } | Places::CodedBits { packed, .. }, Some(held)) => {
            !matches!(held, Origin::Bits(base, width)
                if *base == packed.base() && *width == packed.width())
        }
        // A dictionary the map was not built on. The rows a filter kept out of a flat column are
        // one of these, with a payload of its own each chunk and the row number for a code, and
        // an integer column in that shape keeps a map across chunks only when it is read by value.
        (Places::Codes { values, .. }, Some(held)) => {
            !matches!(held, Origin::Dictionary(dictionary) if Arc::ptr_eq(dictionary, values))
        }
        _ => false,
    }
}

/// Widens an integer key column into `into` and settles the window its values are placed against.
///
/// The bottom of the window, how many places it takes with the null place counted, and whether a
/// row of the column can be null, or `None` for a column that is not an integer in a form that
/// widens as a block, or whose values spread wider than `limit` places.
///
/// The window `held` came from is kept whenever the chunk lands inside it, since a new window is a
/// new map and a new map is every value probed again. A chunk that reaches outside it gets a window
/// that takes the old one in as well when the two fit together, so that a key whose chunks wander
/// around one range settles on a window covering all of it after a rebuild or two, rather than
/// moving with every chunk. And when the column is the only one read this way, the window is made
/// twice as wide as it has to be, so that a chunk reaching a little past the last one lands inside.
fn window_of(
    key: &Vector,
    rows: usize,
    held: Option<&Origin>,
    limit: usize,
    alone: bool,
    (into, runs, keep_runs): (&mut Vec<i64>, &mut Vec<(i64, usize)>, bool),
) -> Option<(i64, usize, bool)> {
    runs.clear();
    let nullable = !key.none_null();
    // A marked chunk's key, when it is a sorted column, has its values and runs found on the column
    // and nothing is left for the pass below to find. See [`selected_runs`].
    let selected = keep_runs && !nullable && selected_runs(key, rows, into, runs);
    if !selected && !signed_rows(key, rows, into) {
        return None;
    }
    // A block at a time, so that a key spread wider than the map, a user id say, is given up
    // after a block rather than after the whole chunk. Its rows are hashed after all of this, and
    // a whole pass here that ends in a refusal was five percent of ClickBench 18.
    let (mut lowest, mut highest) = (i64::MAX, i64::MIN);
    for &(value, _) in runs.iter() {
        lowest = lowest.min(value);
        highest = highest.max(value);
    }
    // The last value taken in, so that a stretch repeating it is passed over. There is no compare of
    // two 64 bit integers on the baseline x86 this is built for, so the lowest and highest are a
    // compare and a branch a value, where a test for equal is a vector compare. A sorted key such
    // as `CounterID` comes in runs of hundreds, and this pass was a third of its fold.
    //
    // Where the value changes is where a run ends, so the runs are kept on the way for
    // [`Coded::place_runs`], up to one for every eight rows, when the key is this column alone.
    // Past that the key is not in runs worth folding by and they are dropped.
    let mut current = into.first().copied().unwrap_or_default();
    let mut keeping = keep_runs && !nullable;
    let most_runs = into.len() / 8 + 1;
    let scanned: &[i64] = if selected { &[] } else { into };
    for (block, values) in scanned.chunks(128).enumerate() {
        if nullable {
            for (row, &value) in values.iter().enumerate() {
                if !key.is_null_at(block * 128 + row) {
                    lowest = lowest.min(value);
                    highest = highest.max(value);
                }
            }
        } else {
            lowest = lowest.min(current);
            highest = highest.max(current);
            for (at, stretch) in values.chunks(16).enumerate() {
                if !stretch.iter().fold(false, |differ, &value| differ | (value != current)) {
                    continue;
                }
                if keeping {
                    for (row, &value) in stretch.iter().enumerate() {
                        if value != current {
                            runs.push((current, block * 128 + at * 16 + row));
                            current = value;
                            lowest = lowest.min(value);
                            highest = highest.max(value);
                        }
                    }
                    keeping = runs.len() < most_runs;
                } else {
                    // A key in no order, walked the way it was before the runs were kept.
                    for &value in stretch {
                        lowest = lowest.min(value);
                        highest = highest.max(value);
                    }
                    current = stretch[stretch.len() - 1];
                }
            }
        }
        if lowest <= highest && (i128::from(highest) - i128::from(lowest)) >= limit as i128 {
            return None;
        }
    }
    if selected {
        if (i128::from(highest) - i128::from(lowest)) >= limit as i128 {
            return None;
        }
    } else if keeping && !into.is_empty() {
        runs.push((current, into.len()));
    } else {
        runs.clear();
    }
    let kept = match held {
        Some(&Origin::Window(low, span)) => Some((low, span)),
        _ => None,
    };
    // The places the values can take, being all of them but the one a null takes.
    let most = i128::try_from(limit.checked_sub(1)?).ok()?;
    let top_of = |low: i64, span: usize| i128::from(low) + span as i128 - 2;
    if let Some((low, span)) = kept {
        // A chunk of nothing but nulls lands in any window at all.
        if lowest > highest
            || (lowest >= low && i128::from(highest) <= top_of(low, span) && span <= limit)
        {
            return Some((low, span, nullable));
        }
    }
    if lowest > highest {
        return Some((0, 2, nullable));
    }
    let (mut bottom, mut top) = (i128::from(lowest), i128::from(highest));
    if let Some((low, span)) = kept {
        let (wider_bottom, wider_top) = (bottom.min(i128::from(low)), top.max(top_of(low, span)));
        if wider_top - wider_bottom < most {
            (bottom, top) = (wider_bottom, wider_top);
        }
    }
    let width = top - bottom + 1;
    if width > most {
        return None;
    }
    let wanted = if alone { (width * 2).max(1024).min(most) } else { width };
    // A window that took the old one in from its bottom puts all of its slack above, so that a key
    // climbing the way a sorted one does keeps the bottom it had and its map can grow in place.
    let climbing = kept.is_some_and(|(low, _)| i128::from(low) == bottom);
    let slack_below = if climbing { 0 } else { (wanted - width) / 2 };
    let low = i64::try_from(bottom - slack_below).or_else(|_| i64::try_from(bottom)).ok()?;
    Some((low, usize::try_from(wanted).ok()?.checked_add(1)?, nullable))
}

/// The values and the runs of a key a filter cut out of a flat integer column, with the runs found
/// on the column rather than on the rows the filter kept.
///
/// A marked chunk's key is the column under the kept rows as codes. Gathering the kept values out
/// of it and then walking them for their runs is two passes over every kept row, where a sorted
/// column has the same runs over all of its rows and finds them in a pass of vector compares. Where
/// each run ends among the kept rows is a search of the codes, and the values are filled a run at a
/// time. `false`, with `runs` cleared, for any other form, for codes that do not climb, and for a
/// key with more runs than one in every eight rows.
///
/// Never inlined, so that [`window_of`] is the size it was for every key that is not one of these.
#[inline(never)]
fn selected_runs(
    key: &Vector,
    rows: usize,
    into: &mut Vec<i64>,
    runs: &mut Vec<(i64, usize)>,
) -> bool {
    runs.clear();
    let Some((at, values)) = key.dictionary_parts() else {
        return false;
    };
    let Some(at) = at.get(..rows) else {
        return false;
    };
    let (Some(&first), Some(&last)) = (at.first(), at.last()) else {
        return false;
    };
    let span = (first as usize, last as usize + 1);
    // Codes that climb over `rows` rows span at least that many, which a dictionary smaller than
    // the chunk never does, and that is most of the dictionaries that reach here.
    if span.1.saturating_sub(span.0) < rows {
        return false;
    }
    // Every pair of codes compared without stopping at the first that fails, which the compiler
    // makes vector compares of, since a filter's codes always climb and this is there to be sure.
    let climbing =
        || at.iter().zip(&at[1..]).fold(true, |up, (&before, &after)| up & (before < after));
    if !values.signed_runs(span, 8, runs) || !climbing() {
        runs.clear();
        return false;
    }
    // In place, since a run of the kept rows is written no later than the run of the column it came
    // from.
    let (mut kept, mut written) = (0, 0_usize);
    for read in 0..runs.len() {
        let (value, end) = runs[read];
        let upto = kept + below(&at[kept..], end);
        if upto == kept {
            continue;
        }
        kept = upto;
        match written.checked_sub(1).map(|last| &mut runs[last]) {
            Some(last) if last.0 == value => last.1 = kept,
            _ => {
                runs[written] = (value, kept);
                written += 1;
            }
        }
    }
    runs.truncate(written);
    if kept != rows || runs.len() > rows / 8 + 1 {
        runs.clear();
        return false;
    }
    into.clear();
    for &(value, end) in runs.iter() {
        into.resize(end, value);
    }
    true
}

/// How many of `at`, rows that climb, are below `end`.
///
/// Rows that climb are at least one apart, so no more than `end - at[0]` of them can be below
/// `end`, and that many are, exactly, wherever the filter kept every row in between. A filter that
/// keeps nearly every row, as `URL <> ''` does, leaves most runs like that, and a search of the
/// whole chunk a run was half of what [`selected_runs`] cost on ClickBench 28.
fn below(at: &[u32], end: usize) -> usize {
    let Some(&first) = at.first() else {
        return 0;
    };
    let most = end.saturating_sub(first as usize).min(at.len());
    if most == 0 || (at[most - 1] as usize) < end {
        return most;
    }
    at[..most].partition_point(|&row| (row as usize) < end)
}

/// The first `rows` values of an integer key column, widened to `i64`, into `into`.
///
/// The forms [`Vector::signed_block`] hands over as a block, and a filtered packed run, which is a
/// packed code per row the filter kept. `false` for anything else, which is read the long way.
fn signed_rows(key: &Vector, rows: usize, into: &mut Vec<i64>) -> bool {
    // A value [`hash`] folds in as two words, which [`Coded::hash_of`] would fold in as one.
    let wide = match key.logical_type() {
        rudb_common::LogicalType::HugeInt | rudb_common::LogicalType::UHugeInt => true,
        rudb_common::LogicalType::Decimal { width, .. } => wide_decimal(*width),
        _ => false,
    };
    if wide {
        return false;
    }
    if let Some((at, values)) = key.dictionary_parts() {
        let Some(at) = at.get(..rows) else {
            return false;
        };
        let Some(packed) = values.packed_parts() else {
            return gathered(at, values, into);
        };
        let Ok(base) = i64::try_from(packed.base()) else {
            return false;
        };
        if !rudb_vector::below(at, values.len()) {
            return false;
        }
        into.clear();
        into.extend(at.iter().map(|&row| base.wrapping_add(packed.code(row as usize) as i64)));
        return true;
    }
    if key.len() < rows || !key.signed_block(into) {
        return false;
    }
    into.truncate(rows);
    true
}

/// The runs of equal values in `values`, each as the place `place` gives its value and the row it
/// ends before, or `false` with `into` cleared once there are more than `most` of them.
///
/// Sixteen rows are compared against the current value at once, which the compiler turns into a
/// few vector compares, and only a block where something changed is walked a row at a time.
fn runs_in<T: Copy + Eq>(
    values: &[T],
    most: usize,
    into: &mut Vec<(usize, usize)>,
    place: impl Fn(T) -> usize,
) -> bool {
    let Some(&first) = values.first() else {
        return false;
    };
    let mut current = first;
    let mut row = 0;
    while row < values.len() {
        let end = (row + 16).min(values.len());
        let block = &values[row..end];
        if block.iter().fold(false, |differ, &value| differ | (value != current)) {
            for (at, &value) in block.iter().enumerate() {
                if value != current {
                    if into.len() >= most {
                        into.clear();
                        return false;
                    }
                    into.push((place(current), row + at));
                    current = value;
                }
            }
        }
        row = end;
    }
    into.push((place(current), values.len()));
    true
}

/// The values a filter kept out of a flat integer column, which is a dictionary whose codes are the
/// rows that got through.
///
/// A flat column is gathered and widened in one pass. Any other form is widened whole and then
/// gathered down in place, which is safe for as long as no row reads from before itself, and that
/// is what a selection's rows always do since they only ever go forward. `false` for codes that go
/// back, which a selection never hands over.
fn gathered(at: &[u32], values: &Vector, into: &mut Vec<i64>) -> bool {
    // A flat column is read at the kept rows and nowhere else, one pass rather than a copy of every
    // row and a second pass to pick the kept ones out of it.
    if values.signed_gather(at, into) {
        return true;
    }
    if at.iter().enumerate().any(|(row, &code)| (code as usize) < row) {
        return false;
    }
    if !values.signed_block(into) || !rudb_vector::below(at, into.len()) {
        into.clear();
        return false;
    }
    for (row, &code) in at.iter().enumerate() {
        into[row] = into[code as usize];
    }
    into.truncate(at.len());
    true
}

/// One key column read as places, with how many it can take and whether a row of it can be null.
///
/// The span counts the place a null takes as well as the places the values take, so it is one more
/// than the column has distinct values it could hold.
fn places_of(key: &Vector, rows: usize, room: usize) -> Option<(Places<'_>, usize, bool)> {
    // A dictionary whose values are a packed run, which is what a filter leaves behind on a column
    // our own format packed. The codes are the rows that got through and the payload is the whole
    // page, so the place is the packed code under the row rather than the row number itself.
    //
    // Before this, the branch below took this shape, because a selection is a dictionary as far as
    // the form goes. It read the payload's length as the span, which is the rows of the page and
    // not the values the column takes, and it keyed the map on the row number. So a group by on a
    // column of eleven discounts built a map of one entry per row of the page, and rebuilt it for
    // every chunk, because each filtered chunk points at a payload of its own and a map held by the
    // payload's identity cannot outlive it. Read this way the span is the width's, which is
    // sixteen, and the map's identity is the page's, so the chunk after this one reuses it.
    if let Some((at, values)) = key.dictionary_parts()
        && let Some(packed) = values.packed_parts()
    {
        // No code here needs checking against the payload. A dictionary vector is range checked
        // when it is built and nothing changes its codes after, so the check that used to sit
        // here was a second pass over every row of a chunk to learn what was already known.
        let at = at.get(..rows)?;
        let span = 1_usize.checked_shl(packed.width())?.checked_add(1)?;
        if span > room {
            return None;
        }
        // A packed run keeps its nulls in the vector's own validity, and a row here reads that
        // vector at the code rather than at the row, so the question is whether the page has a
        // null anywhere in it rather than whether this chunk does.
        let nullable = key.validity().has_nulls(rows) || values.validity().has_nulls(values.len());
        return Some((Places::CodedBits { at, packed }, span, nullable));
    }
    if let Some((codes, values)) = key.shared_dictionary_parts() {
        let codes = codes.get(..rows)?;
        let span = values.len().checked_add(1)?;
        if span > room {
            return None;
        }
        // Whether any row here can be null at all, asked once for the chunk. The cheap answer comes
        // from the two validities and covers the ordinary column, which has no null anywhere in it.
        //
        // The expensive answer is a pass over the dictionary. That was the only answer until the map
        // grew wide enough to cover a Parquet column chunk's dictionary, and at that width it is a
        // pass over a hundred and twenty eight thousand entries for every chunk of a few thousand
        // rows, which costs more than the probe the whole map exists to replace. So a dictionary too
        // large to scan and not known to be free of nulls gives the map up rather than paying for it
        // once a chunk, and such a key is hashed the way it was before.
        let nullable = if key.never_null() {
            false
        } else if values.len() > COMBOS {
            return None;
        } else {
            key.validity().has_nulls(rows) || (0..values.len()).any(|at| values.is_null_at(at))
        };
        return Some((Places::Codes { codes, values }, span, nullable));
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
    StableText {
        dictionary: Arc<Vector>,
        codes: Vec<u32>,
    },
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
        rudb_common::LogicalType::Date => Value::Date(i32::try_from(held).unwrap_or(i32::MAX)),
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
        if matches!(&self.data, StoredData::Varchar(values) if values.ends.is_empty())
            && let Some((codes, dictionary)) = column.stable_dictionary_parts()
        {
            let code = *codes
                .get(row)
                .ok_or_else(|| Error::internal("a stable dictionary row is missing"))?;
            self.data =
                StoredData::StableText { dictionary: Arc::clone(dictionary), codes: vec![code] };
            self.valid.push(!column.is_null_at(row));
            return Ok(0);
        }
        if column.is_null_at(row) {
            // A null does not end a run. Both sides keep their nulls in the validity beside the
            // codes rather than in a code of their own, so the row is stored the way every other
            // row of the run is and the validity says what it is. Nothing reads the code back,
            // because everything that reads a slot asks the validity first, but it still has to be
            // a code the dictionary has, since the vector handed back at the end is built from the
            // whole run at once and a code past the end of a dictionary is refused there.
            if let StoredData::StableText { dictionary, codes } = &mut self.data
                && let Some((incoming, values)) = column.stable_dictionary_parts()
                && Arc::ptr_eq(dictionary, values)
            {
                let code = *incoming
                    .get(row)
                    .ok_or_else(|| Error::internal("a stable dictionary row is missing"))?;
                codes.push(code);
                self.valid.push(false);
                return Ok(0);
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
        if let StoredData::StableText { dictionary, codes: stored } = &self.data
            && let Some((values, incoming)) = column.stable_dictionary_parts()
            && Arc::ptr_eq(dictionary, incoming)
        {
            for ((step, &bucket), flag) in here.iter().zip(seen).zip(same.iter_mut()) {
                if !*flag {
                    continue;
                }
                let slot = slot_of(bucket) as usize;
                *flag = match values.get(step.row) {
                    _ if !self.valid[slot] => !validity.is_valid(step.row),
                    Some(code) => validity.is_valid(step.row) && Some(code) == stored.get(slot),
                    None => false,
                };
            }
            return;
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
        if let Some(data) = column.data()
            && self.flat_run(
                here,
                seen,
                same,
                column,
                data,
                |row| !validity.is_valid(row),
                |row| row,
            )
        {
            return;
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

    /// The groups at `slots` of this column as a vector.
    ///
    /// A key that is a code into a file's dictionary stays a code. Reading it through a value here
    /// meant reading every selected group's text out of the dictionary, and a dictionary that reads
    /// its payload a block at a time keeps each block it reads for as long as the file is open. On
    /// ClickBench q40 that was two fifths of the query's time spent decoding `URL` and `Referer`
    /// blocks for groups the sort above then threw away, and the blocks stayed resident after.
    /// Handed over as codes, the text is read for the rows that make it into the answer, and q40
    /// went from 182 MB and 1.00 s of user time to 91 MB and 0.63 s.
    fn vector_at(&self, ty: &rudb_common::LogicalType, slots: &[usize]) -> Result<Vector> {
        if let StoredData::StableText { dictionary, codes } = &self.data {
            let picked = slots.iter().map(|&slot| codes[slot]).collect();
            let vector = Vector::stable_dictionary(picked, Arc::clone(dictionary))?;
            let valid = &self.valid;
            let validity =
                rudb_vector::Validity::from_iter(slots.len(), |index| valid[slots[index]]);
            return Ok(vector.with_validity(validity));
        }
        Vector::from_values(ty.clone(), &self.values_at(slots))
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
    if let ([column], Across::OneInput) = (keys, across)
        && let Some((codes, _)) = column.stable_dictionary_parts()
    {
        let validity = column.validity();
        // Two runs side by side when the column has no null in it, which is most columns, so
        // the row is a load, a mix and a spread and not a validity read and a branch as well.
        if let (false, Some(codes)) = (validity.has_nulls(rows), codes.get(..rows)) {
            for (state, &code) in hashes.iter_mut().zip(codes) {
                *state = spread(mix(0, u64::from(code)));
            }
            return;
        }
        for (row, state) in hashes.iter_mut().enumerate() {
            let word = if validity.is_valid(row) { u64::from(codes[row]) } else { NOTHING };
            *state = spread(mix(0, word));
        }
        return;
    }
    // The spread is folded into the last column's pass rather than made into a pass of its own.
    // It used to be a second walk of the whole run, which is a load, five operations and a store a
    // row on top of the one that did the work, and hashing is the largest single symbol on the
    // suite. With no key columns at all there is nothing to fold it into, and there is nothing to
    // do either, because every row is still the zero `resize` left and `spread(0)` is zero.
    let last = keys.len().saturating_sub(1);
    for (at, column) in keys.iter().enumerate() {
        fold(column, rows, hashes, across, at == last);
    }
}

/// The running hash of a row, spread if this was the last key column and left alone if it was not.
///
/// `finish` is the same for every row of a pass, so the branch is outside the loop by the time this
/// is compiled, and writing it this way is what keeps one copy of each of the passes below rather
/// than two.
#[inline]
fn end(state: u64, finish: bool) -> u64 {
    if finish { spread(state) } else { state }
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
fn fold(column: &Vector, rows: usize, hashes: &mut [u64], across: Across, finish: bool) {
    let validity = column.validity();
    if across == Across::OneInput
        && let Some((codes, _)) = column.stable_dictionary_parts()
    {
        // The same two runs side by side as in [`hash`], for the same reason.
        if let (false, Some(codes), Some(hashes)) =
            (validity.has_nulls(rows), codes.get(..rows), hashes.get_mut(..rows))
        {
            for (state, &code) in hashes.iter_mut().zip(codes) {
                *state = end(mix(*state, u64::from(code)), finish);
            }
            return;
        }
        for (row, state) in hashes.iter_mut().enumerate().take(rows) {
            let one = if validity.is_valid(row) { u64::from(codes[row]) } else { NOTHING };
            *state = end(mix(*state, one), finish);
        }
        return;
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
    // Whether a row reads the place it sits in and is never nothing, which is the shape a column
    // off our own format arrives in whenever the writer had no null to record. Asked once for the
    // column, because a validity that says `AllValid` says it for the whole of it.
    //
    // It is what lets the two runs below drop the per row question entirely. Without it every row
    // of every key column of every chunk paid a validity read, a branch, an `Option` and a bounds
    // check to say what the column already said once, and hashing is nine to twelve percent of
    // every query on the suite.
    let straight = !validity.has_nulls(rows);
    if let Some(packed) = column.packed_parts() {
        fold_packed(&packed, wide, rows, straight.then_some(Reads::Own), finish, hashes, |row| {
            validity.is_valid(row).then_some(row)
        });
        return;
    }
    if let Some(data) = column.data()
        && fold_data(data, rows, hashes, straight.then_some(Reads::Own), finish, |row| {
            validity.is_valid(row).then_some(row)
        })
    {
        return;
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
        // Whether no row is nothing, which for a column read through codes takes both sides saying
        // so: the column's own validity and that of what the codes point at. This is the shape a
        // filter hands on, since it keeps the rows that got through as codes into the chunk it was
        // given, so behind a filter this is what a flat key column with no nulls turns into. It
        // used to take the path that asks every row both questions and an `Option` besides.
        //
        // The side the codes point at is only asked when it is not much longer than the rows,
        // because counting its nulls is a pass over it, and a Parquet dictionary can cover a whole
        // column chunk. That is the case where it is also least likely to matter.
        let inner = values.validity();
        let clean = !validity.has_nulls(rows)
            && values.len() <= rows.saturating_mul(4)
            && !inner.has_nulls(values.len());
        let through = clean.then_some(Reads::Codes(&at[..]));
        if let Some(data) = values.data() {
            // A dictionary keeps its nulls in the vector it points at, so a row is null when either
            // the column says so or the value its code points at does.
            let pick = |row: usize| {
                if !validity.is_valid(row) {
                    return None;
                }
                let code = *at.get(row)? as usize;
                inner.is_valid(code).then_some(code)
            };
            if fold_data(data, rows, hashes, through, finish, pick) {
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
            fold_packed(&packed, wide, rows, through, finish, hashes, |row| {
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
        let one = if text {
            match column.bytes_at(row) {
                Some(bytes) => mix(*state, bytes_word(bytes)),
                None => mix(*state, NOTHING),
            }
        } else {
            fold_value(*state, &column.value_at(row))
        };
        *state = end(one, finish);
    }
}

/// Which place a row reads when every row reads one and none of them is nothing.
///
/// The two shapes a key column with no nulls arrives in. [`Reads::Own`] is a flat column, and
/// [`Reads::Codes`] is the same column behind a filter, which keeps the rows that got through as
/// codes into the chunk it was handed rather than copying them. Handing the passes below this
/// rather than a closure is what lets them walk two runs side by side with nothing to ask per row.
#[derive(Clone, Copy)]
enum Reads<'a> {
    /// Row `i` reads place `i`.
    Own,
    /// Row `i` reads the place its code names.
    Codes(&'a [u32]),
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
///
/// `straight` says no row is nothing and which place each row reads, which is `pick` answering
/// `Some` for every row it will be asked about. It is the caller's for the same reason `wide` is:
/// the caller holds the column and the column's validity says it once, where a closure can only be
/// asked a row at a time. Saying it wrongly is a wrong answer and not a slow one, so a caller that
/// cannot tell cheaply says `None` and every row asks `pick`.
fn fold_packed(
    packed: &Packed<'_>,
    wide: bool,
    rows: usize,
    straight: Option<Reads<'_>>,
    finish: bool,
    hashes: &mut [u64],
    pick: impl Fn(usize) -> Option<usize>,
) {
    let base = packed.base();
    match straight {
        Some(Reads::Own) => {
            let rows = rows.min(hashes.len());
            straight_packed(packed, wide, finish, &mut hashes[..rows], 0..rows);
            return;
        }
        Some(Reads::Codes(codes)) => {
            if let Some(codes) = codes.get(..rows) {
                let at = codes.iter().map(|&code| code as usize);
                let rows = rows.min(hashes.len());
                straight_packed(packed, wide, finish, &mut hashes[..rows], at);
                return;
            }
        }
        None => {}
    }
    for (row, state) in hashes.iter_mut().enumerate().take(rows) {
        let Some(code) = pick(row) else {
            *state = end(mix(*state, NOTHING), finish);
            continue;
        };
        let value = base + i128::from(packed.code(code));
        let one = if wide {
            mix(mix(*state, value as u64), (value >> 64) as u64)
        } else {
            mix(*state, value as u64)
        };
        *state = end(one, finish);
    }
}

/// The straight passes of [`fold_packed`], with the two questions that are the same for every row
/// of the pass taken out of the loop by the compiler rather than asked in it.
///
/// Written as one generic pass and four copies of it because that is what makes the copies. As a
/// closure over `wide` and `finish` the loop kept both as loads off the stack and a compare and a
/// branch each, per row, once `fold` stopped being inlined into `hash`, and TPC-H q17, which hashes
/// six million packed `l_partkey` values for its join filter, came out seven percent worse for it.
fn straight_packed(
    packed: &Packed<'_>,
    wide: bool,
    finish: bool,
    hashes: &mut [u64],
    at: impl Iterator<Item = usize>,
) {
    fn run<const WIDE: bool, const FINISH: bool>(
        packed: &Packed<'_>,
        hashes: &mut [u64],
        at: impl Iterator<Item = usize>,
    ) {
        let base = packed.base();
        for (state, code) in hashes.iter_mut().zip(at) {
            let value = base + i128::from(packed.code(code));
            let one = if WIDE {
                mix(mix(*state, value as u64), (value >> 64) as u64)
            } else {
                mix(*state, value as u64)
            };
            *state = end(one, FINISH);
        }
    }
    match (wide, finish) {
        (false, false) => run::<false, false>(packed, hashes, at),
        (false, true) => run::<false, true>(packed, hashes, at),
        (true, false) => run::<true, false>(packed, hashes, at),
        (true, true) => run::<true, true>(packed, hashes, at),
    }
}

/// Folds one word per row into `hashes`, reading the values through `pick`.
///
/// `pick` says which index of `data` a row reads, and `None` says the row is null. That is what
/// makes this one copy of the type arms rather than two: a flat column picks the row itself, a
/// dictionary or a run length column picks the code, and the eleven arms below are written once.
/// The answer is whether there was an arm for the data at all, which is `false` for the nested
/// types and leaves the caller to fall through to whatever it has after this.
///
/// `straight` says no row is nothing and which place each row reads, which is the shape a key
/// column with no nulls arrives in whether it is flat or behind a filter. It has the same meaning
/// and the same reason for being the caller's as it does in [`fold_packed`].
fn fold_data(
    data: &Data,
    rows: usize,
    hashes: &mut [u64],
    straight: Option<Reads<'_>>,
    finish: bool,
    pick: impl Fn(usize) -> Option<usize>,
) -> bool {
    /// One pass over the rows, turning each into a word the same way the general path does.
    macro_rules! run {
        ($values:expr, $word:expr) => {{
            let values = $values.as_slice();
            let word = $word;
            // The same pass with the row's own question taken out of it. Both ends are cut to the
            // rows, so the walk is two runs side by side and there is no validity read, no branch,
            // no `Option` and no bounds check left in the body. What is in it is the load, the
            // widen and the mix, which is the work.
            match straight {
                Some(Reads::Own) => {
                    if let (Some(values), Some(hashes)) =
                        (values.get(..rows), hashes.get_mut(..rows))
                    {
                        for (state, value) in hashes.iter_mut().zip(values) {
                            *state = end(mix(*state, word(*value)), finish);
                        }
                        return true;
                    }
                }
                // Through the codes, with the one question left being whether a code is inside
                // what it points at, which is a compare that always goes the same way.
                Some(Reads::Codes(codes)) => {
                    if let Some(codes) = codes.get(..rows) {
                        for (state, &code) in hashes.iter_mut().zip(codes) {
                            let one = match values.get(code as usize) {
                                Some(value) => word(*value),
                                None => NOTHING,
                            };
                            *state = end(mix(*state, one), finish);
                        }
                        return true;
                    }
                }
                None => {}
            }
            for (row, state) in hashes.iter_mut().enumerate().take(rows) {
                let one = match pick(row).and_then(|at| values.get(at)) {
                    Some(value) => word(*value),
                    None => NOTHING,
                };
                *state = end(mix(*state, one), finish);
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
                let at = match straight {
                    Some(Reads::Own) => Some(row),
                    Some(Reads::Codes(codes)) => codes.get(row).map(|&code| code as usize),
                    None => pick(row),
                };
                let one = match at.and_then(|at| strings.bytes(at)) {
                    Some(bytes) => bytes_word(bytes),
                    None => NOTHING,
                };
                *state = end(mix(*state, one), finish);
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

    /// Runs across the edges of the sixteen row blocks come out whole, and too many runs is a
    /// refusal with nothing left behind.
    #[test]
    fn runs_are_cut_where_the_value_changes_and_refused_past_the_limit() {
        let values: Vec<i64> = [5; 15].into_iter().chain([7; 20]).chain([5, 9]).collect();
        let mut runs = Vec::new();
        assert!(runs_in(&values, 8, &mut runs, |value| value as usize * 2));
        assert_eq!(runs, vec![(10, 15), (14, 35), (10, 36), (18, 37)]);

        assert!(!runs_in(&values, 3, &mut runs, |value| value as usize));
        assert!(runs.is_empty());

        assert!(runs_in(&[3u32; 40], 0, &mut runs, |code| code as usize));
        assert_eq!(runs, vec![(3, 40)]);
        assert!(!runs_in::<u32>(&[], 8, &mut runs, |code| code as usize));
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

    /// The same fill again over a table given a direct index, so the two can be compared.
    fn over_a_range(
        keys: &[Vector],
        rows: usize,
        types: &[LogicalType],
        low: i128,
        values: u64,
    ) -> (Table, Vec<usize>) {
        let mut table = Table::new(types).over_range(low, values, &types[0]);
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

    /// The whole of what the direct index has to promise, which is the same promise the batch makes:
    /// the same groups in the same slots. It is a shortcut to a slot and never a second opinion about
    /// which rows group together, so a table that has one and a table that does not are the same
    /// table, and this is the test that says so.
    ///
    /// Nulls among the values, because the null key has cell zero rather than no cell, and a range
    /// wider than the values actually used, because that is what a zone map hands over.
    #[test]
    fn a_direct_index_finds_the_same_groups_in_the_same_slots() {
        let values: Vec<Value> = (0..40_000)
            .map(|row: i64| match row % 97 {
                0 => Value::Null,
                _ => Value::BigInt((row * 7919) % 12_007),
            })
            .collect();
        let keys = [flat(LogicalType::BigInt, &values)];
        let types = [LogicalType::BigInt];
        let (was, before) = a_batch_at_a_time(&keys, values.len(), &types);
        let (now, after) = over_a_range(&keys, values.len(), &types, 0, 12_007);
        assert!(now.direct.is_some(), "the test has to reach the direct path");
        assert_eq!(before, after);
        assert_eq!(was.len(), now.len());
    }

    /// A range that starts below zero, which is the ordinary case for anything signed and the one an
    /// off by one in the base would show up in.
    #[test]
    fn a_range_below_zero_addresses_the_same_way() {
        let values: Vec<Value> =
            (0..4_000).map(|row: i32| Value::Integer((row % 601) - 300)).collect();
        let keys = [flat(LogicalType::Integer, &values)];
        let types = [LogicalType::Integer];
        let (_, before) = a_batch_at_a_time(&keys, values.len(), &types);
        let (now, after) = over_a_range(&keys, values.len(), &types, -300, 601);
        assert!(now.direct.is_some());
        assert_eq!(before, after);
        assert_eq!(now.len(), 601);
    }

    /// A value the range does not cover sends the whole batch to the buckets, and the answer is the
    /// answer either way. This is the case a store that wrote a narrow pair of ends produces, which
    /// is the failure this path is built to survive.
    #[test]
    fn a_value_outside_the_range_is_answered_by_the_buckets() {
        let values: Vec<Value> = (0..2_000).map(|row: i64| Value::BigInt(row % 500)).collect();
        let keys = [flat(LogicalType::BigInt, &values)];
        let types = [LogicalType::BigInt];
        let (_, before) = a_batch_at_a_time(&keys, values.len(), &types);
        // Ten values wide, so all but the first ten are outside it.
        let (now, after) = over_a_range(&keys, values.len(), &types, 0, 10);
        assert_eq!(before, after);
        assert_eq!(now.len(), 500);
    }

    /// A key column in a form with no run to read falls back the same way, and a chunk of each form
    /// in turn is what a parquet scan hands over.
    #[test]
    fn a_dictionary_key_is_answered_by_the_buckets() {
        let seen = [Value::Integer(3), Value::Integer(7), Value::Null];
        let values: Vec<Value> =
            (0..300).map(|row: usize| seen[row % seen.len()].clone()).collect();
        let codes: Vec<u32> = (0..300).map(|row| (row % seen.len()) as u32).collect();
        let keys = [dictionary_of(LogicalType::Integer, &seen, codes, &values)];
        let types = [LogicalType::Integer];
        let plain = [flat(LogicalType::Integer, &values)];
        let (_, before) = a_batch_at_a_time(&plain, values.len(), &types);
        let (now, after) = over_a_range(&keys, values.len(), &types, 0, 16);
        assert!(now.direct.is_some(), "the index is built and simply not read from");
        assert_eq!(before, after);
        assert_eq!(now.len(), 3);
    }

    /// A type stored as an integer that does not come back from `Bound::of_value` as one gets no
    /// index at all, because the insert and the probe would disagree about it. See `addressable`.
    #[test]
    fn a_type_the_two_sides_read_differently_gets_no_index() {
        let scaled = LogicalType::Decimal { width: 18, scale: 2 };
        assert!(
            Table::new(std::slice::from_ref(&scaled)).over_range(0, 100, &scaled).direct.is_none()
        );
        assert!(
            Table::new(&[LogicalType::Timestamp])
                .over_range(0, 100, &LogicalType::Timestamp)
                .direct
                .is_none()
        );
        assert!(
            Table::new(&[LogicalType::Date])
                .over_range(0, 100, &LogicalType::Date)
                .direct
                .is_some()
        );
    }

    /// A key of more than one column has no one value to address by, and asking for a range over one
    /// leaves the table as it arrived rather than indexing the first column of it.
    #[test]
    fn a_key_of_two_columns_gets_no_index() {
        let types = [LogicalType::Integer, LogicalType::Integer];
        assert!(Table::new(&types).over_range(0, 100, &types[0]).direct.is_none());
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
            // A hundred distinct values, so a two byte `DECIMAL(4)` still halves when it packs.
            let values: Vec<Value> = (0..600).map(|row| of(row % 100)).collect();
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
        let values: Vec<Value> = (0..rows)
            .map(|row| if row % 61 == 0 { Value::Null } else { of(number(row)) })
            .collect();
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
        a_key_of_every_form(&LogicalType::Timestamp, |code| Value::Timestamp(code * 1_000));
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

    /// The shape a filter hands on, which is codes naming the rows that got through over the whole
    /// chunk it was given, has to hash each kept row the way the chunk itself hashes that row. That
    /// is true whether the chunk is flat or packed, and whether the payload has a null in a row the
    /// filter dropped, in which case the pass without per row questions may not run, or has none.
    #[test]
    fn the_rows_a_filter_kept_hash_the_way_they_hashed_before_it() {
        let rows = 3_000usize;
        let kept: Vec<u32> = (0..rows as u32).filter(|row| row % 3 != 1).collect();
        for ty in [LogicalType::Integer, LogicalType::BigInt, LogicalType::Varchar] {
            let of = |row: usize| match &ty {
                LogicalType::Integer => Value::Integer(((row * 7919) % 5003) as i32),
                LogicalType::BigInt => Value::BigInt(1_000_000 + ((row * 7919) % 5003) as i64),
                _ => Value::Varchar(format!("k{}", (row * 7919) % 5003)),
            };
            let mut values: Vec<Value> = (0..rows).map(of).collect();
            let whole = hashed(&flat(ty.clone(), &values));
            let want: Vec<u64> = kept.iter().map(|&row| whole[row as usize]).collect();
            let mut payloads = vec![flat(ty.clone(), &values)];
            if ty != LogicalType::Varchar {
                let packed = flat(ty.clone(), &values).bit_packed().expect("a packed column");
                assert_eq!(packed.form(), rudb_vector::Form::BitPacked, "{ty} has to pack");
                payloads.push(packed);
            }
            // A null in a row the filter dropped, which the kept rows never read.
            values[1] = Value::Null;
            payloads.push(flat(ty.clone(), &values));
            for payload in payloads {
                let form = payload.form();
                let selected = Vector::dictionary(kept.clone(), payload).expect("codes into it");
                assert_eq!(want, hashed(&selected), "{ty} kept out of {form:?}");
            }
        }
    }

    /// The pass that skips the per row null question has to answer what the pass that asks it
    /// answers. A column with no null in it takes the first, a dictionary over the same values
    /// takes the second because a code can point at a null the column itself does not have, and a
    /// column with one null anywhere takes the second for all of its rows. All three are the same
    /// values and all three have to hash the same, or the same key would land in two buckets
    /// whenever a page with a null and a page without arrived one after the other.
    #[test]
    fn a_column_with_no_nulls_hashes_the_way_the_general_pass_hashes_it() {
        let rows = 3_000usize;
        for ty in [
            LogicalType::Integer,
            LogicalType::BigInt,
            LogicalType::Varchar,
            LogicalType::Date,
            LogicalType::Decimal { width: 9, scale: 2 },
        ] {
            let of = |row: usize| match &ty {
                LogicalType::Integer => Value::Integer(((row * 7919) % 5003) as i32),
                LogicalType::BigInt => Value::BigInt(((row * 7919) % 5003) as i64),
                LogicalType::Varchar => Value::Varchar(format!("k{}", (row * 7919) % 5003)),
                LogicalType::Date => Value::Date(((row * 7919) % 5003) as i32),
                _ => Value::Decimal { unscaled: ((row * 7919) % 5003) as i128, width: 9, scale: 2 },
            };
            let values: Vec<Value> = (0..rows).map(of).collect();
            let plain = flat(ty.clone(), &values);
            assert!(!plain.validity().has_nulls(rows), "{ty} has to have no nulls");
            let want = hashed(&plain);

            // The same values behind codes, which is the pass that asks about every row.
            let coded = Vector::dictionary((0..rows as u32).collect(), flat(ty.clone(), &values))
                .expect("a dictionary over those values");
            assert_eq!(want, hashed(&coded), "{ty} through codes");

            // And the same values with a null on the end, which puts every row of the run on the
            // pass that asks. The rows before the null are the ones being compared.
            let mut with_a_null = values.clone();
            with_a_null.push(Value::Null);
            let mixed = flat(ty.clone(), &with_a_null);
            assert!(mixed.validity().has_nulls(rows + 1), "{ty} has to have a null");
            assert_eq!(want, hashed(&mixed)[..rows], "{ty} beside a null");
        }
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

    /// The shape a filter leaves on a packed column: the rows that got through, as codes into the
    /// whole page.
    ///
    /// The place has to be the packed code the row names and not the row number, because the row
    /// number says nothing about what the row holds. Read the other way the span would be the rows
    /// of the page rather than the values the column takes, and the map would be one entry per row
    /// of a page that no second chunk can reuse.
    #[test]
    fn a_filtered_packed_column_is_read_at_the_width_of_the_page_it_came_from() {
        let page = packed_numbers(&[1, 2, 7, 1, 2, 7], 3, 1);
        let kept = Vector::dictionary(vec![4, 0, 2], page).expect("the rows a filter kept");
        let keys = [kept];
        let coded = coded(&keys, 3).expect("a filtered narrow packed column is read as codes");
        // The eight codes three bits can take and one more for a null, and not the six rows of the
        // page behind them.
        assert_eq!(coded.combos(), 9);
        assert_eq!(placed(&coded, 3), [1, 0, 6]);
    }

    /// And the map it builds is the page's, so the chunk after it reuses the same one.
    ///
    /// This is the half that pays. A filter hands out a chunk at a time and every one of them
    /// points at a payload of its own, so a map held by the payload's identity is thrown away and
    /// rebuilt on every chunk of the scan. Held by the base and the width it is built once for the
    /// page, whether the chunk arrived whole or cut.
    #[test]
    fn a_filtered_chunk_and_a_whole_one_off_the_same_page_keep_one_map() {
        let page = || packed_numbers(&[1, 2, 7, 1, 2, 7], 3, 1);
        let first = [Vector::dictionary(vec![0, 1], page()).expect("the rows a filter kept")];
        let over_first = coded(&first, 2).expect("codes");
        let mut held = Vec::new();
        over_first.hold(&mut held);

        let next =
            [Vector::dictionary(vec![3, 5], page()).expect("another chunk of the same page")];
        assert!(coded(&next, 2).expect("codes").same_as(&held), "the same page, cut again");
        let whole = [page()];
        assert!(coded(&whole, 2).expect("codes").same_as(&held), "the same page, not cut at all");
        let elsewhere = [Vector::dictionary(vec![0, 1], packed_numbers(&[9, 10], 3, 9))
            .expect("a chunk of another page")];
        assert!(!coded(&elsewhere, 2).expect("codes").same_as(&held), "another base");
    }

    /// A null in the page a filter cut is a null at the row that names it, which is the one thing
    /// reading through a code rather than by row can get wrong.
    #[test]
    fn a_null_in_the_page_behind_a_filter_takes_the_place_past_the_last_code() {
        let page = packed_numbers(&[1, 2, 7], 3, 1).with_validity(rudb_vector::Validity::Mask({
            let mut mask = rudb_vector::Bitmap::all_valid(3);
            mask.set(1, false);
            mask
        }));
        let keys = [Vector::dictionary(vec![2, 1, 0], page).expect("the rows a filter kept")];
        let coded = coded(&keys, 3).expect("codes");
        assert_eq!(coded.combos(), 9);
        assert_eq!(placed(&coded, 3), [6, 8, 0]);
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

    /// A dictionary of `entries` values, of which the chunk uses the first two.
    fn wide_dictionary(entries: usize) -> Vector {
        let values: Vec<Value> = (0..entries as i32).map(Value::Integer).collect();
        Vector::dictionary(vec![0, 1], flat(LogicalType::Integer, &values))
            .expect("a dictionary of that many values")
    }

    /// And a dictionary large enough that the map would cost more than the probe it replaces.
    #[test]
    fn a_dictionary_wider_than_the_map_allows_is_refused() {
        let keys = [wide_dictionary(WIDE_COMBOS)];
        assert!(coded(&keys, 2).is_none());
    }

    /// The width a Parquet column chunk's dictionary arrives at, which is past [`COMBOS`] and well
    /// inside [`WIDE_COMBOS`], and which is the whole point of the second bound.
    #[test]
    fn one_dictionary_the_size_of_a_row_groups_is_read_as_codes() {
        let keys = [wide_dictionary(128_000)];
        let coded = coded(&keys, 2).expect("one wide dictionary is read as codes");
        assert_eq!(coded.combos(), 128_001);
        assert_eq!(placed(&coded, 2), [0, 1]);
    }

    /// Two of them are not, because two columns' places are a product and most of it stays empty.
    #[test]
    fn two_dictionaries_that_wide_are_refused_even_though_one_would_not_be() {
        let keys = [wide_dictionary(128_000), wide_dictionary(4)];
        assert!(coded(&keys, 2).is_none());
        let narrow = [wide_dictionary(100), wide_dictionary(4)];
        assert!(coded(&narrow, 2).is_some(), "their product is still inside the small bound");
    }

    /// A wide dictionary the cheap null question cannot answer is given up rather than scanned,
    /// because that scan is a pass over the dictionary for every chunk of a few thousand rows.
    #[test]
    fn a_wide_dictionary_that_might_hold_a_null_is_refused() {
        let column = wide_dictionary(128_000).with_validity(rudb_vector::Validity::Mask({
            let mut mask = rudb_vector::Bitmap::all_valid(2);
            mask.set(1, false);
            mask
        }));
        assert!(coded(&[column], 2).is_none());
        let narrow = wide_dictionary(100).with_validity(rudb_vector::Validity::Mask({
            let mut mask = rudb_vector::Bitmap::all_valid(2);
            mask.set(1, false);
            mask
        }));
        let keys = [narrow];
        let coded = coded(&keys, 2).expect("a narrow one is still scanned");
        assert_eq!(placed(&coded, 2), [0, 100], "the null row takes the place past the last code");
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

    /// A flat column of integers, the shape a filter leaves when it copies the rows it kept out.
    fn integers(values: &[Option<i32>]) -> Vector {
        let values: Vec<Value> =
            values.iter().map(|value| value.map_or(Value::Null, Value::Integer)).collect();
        flat(LogicalType::Integer, &values)
    }

    /// The runs a column read by value is placed in are the runs of its rows' places, across the
    /// stretches and blocks the window reads it in, and a key in no order is not cut into runs.
    #[test]
    fn runs_found_by_the_window_are_the_runs_of_the_places() {
        let lengths = [(10, 5), (12, 130), (10, 1), (11, 40), (20, 124), (12, 16)];
        let rows: usize = lengths.iter().map(|&(_, length)| length).sum();
        let sorted: Vec<Option<i32>> = lengths
            .iter()
            .flat_map(|&(value, length)| std::iter::repeat_n(Some(value), length))
            .collect();
        let keys = [integers(&sorted)];
        let mut values = Widened::default();
        let coded = coded_within(&keys, rows, &[], Some(&mut values)).expect("read by value");
        let mut places = Vec::new();
        coded.places(rows, &mut places);
        let mut expected = Vec::new();
        for (row, &place) in places.iter().enumerate() {
            if row + 1 == rows || places[row + 1] != place {
                expected.push((place, row + 1));
            }
        }
        let mut runs = Vec::new();
        assert!(coded.place_runs(rows, rows, &mut runs));
        assert_eq!(runs, expected);
        assert_eq!(runs.len(), lengths.len());
        assert!(!coded.place_runs(rows, 2, &mut runs));
        let scattered: Vec<Option<i32>> = (0..rows as i32).map(|row| Some(row * 7 % 13)).collect();
        let keys = [integers(&scattered)];
        let coded = coded_within(&keys, rows, &[], Some(&mut values)).expect("read by value");
        assert!(!coded.place_runs(rows, rows / 8, &mut runs));
        assert!(runs.is_empty());
    }

    /// A flat column is read by its value once there is somewhere to widen it into, which is the
    /// shape `CounterID` arrives in after `URL <> ''` has copied out the rows it kept.
    #[test]
    fn a_flat_column_is_read_by_its_value_against_a_window() {
        let keys = [integers(&[Some(62), Some(1_000), Some(62), Some(-5)])];
        let mut values = Widened::default();
        let coded = coded_within(&keys, 4, &[], Some(&mut values)).expect("read by value");
        let places = placed(&coded, 4);
        assert_eq!(places[0], places[2], "one value is one place");
        assert_eq!(places.iter().collect::<std::collections::HashSet<_>>().len(), 3);
        assert!(
            places.iter().all(|&place| place < coded.combos() - 1),
            "no value takes the null place"
        );
        assert_eq!(
            places[1] - places[3],
            1_005,
            "a place is the value less the bottom of the window"
        );
    }

    /// The window outlives the chunk. The next chunk inside it keeps the map, one reaching past it
    /// gets a window that takes the old one in, and the map is only rebuilt for the second.
    #[test]
    fn a_window_is_kept_for_as_long_as_the_chunks_land_inside_it() {
        let mut values = Widened::default();
        let first = [integers(&[Some(100), Some(200)])];
        let mut held = Vec::new();
        coded_within(&first, 2, &[], Some(&mut values)).expect("read by value").hold(&mut held);

        let inside = [integers(&[Some(150), Some(101)])];
        let again = coded_within(&inside, 2, &held, Some(&mut values)).expect("read by value");
        assert!(again.same_as(&held), "a chunk inside the window keeps the map");

        let past = [integers(&[Some(90_000), Some(100)])];
        let wider = coded_within(&past, 2, &held, Some(&mut values)).expect("read by value");
        assert!(!wider.same_as(&held), "a chunk past the window builds a new one");
        let mut grown = Vec::new();
        wider.hold(&mut grown);
        let back = [integers(&[Some(200), Some(90_000)])];
        assert!(
            coded_within(&back, 2, &grown, Some(&mut values))
                .expect("read by value")
                .same_as(&grown),
            "and the new one still covers where the old one was"
        );
    }

    /// Every null row takes the one place past the values, and a chunk of nothing but nulls keeps
    /// whatever window there was.
    #[test]
    fn a_null_row_read_by_value_takes_the_place_past_the_window() {
        let mut values = Widened::default();
        let keys = [integers(&[Some(7), None, Some(9), None])];
        let coded = coded_within(&keys, 4, &[], Some(&mut values)).expect("read by value");
        let places = placed(&coded, 4);
        assert_eq!(places[1], coded.combos() - 1);
        assert_eq!(places[3], coded.combos() - 1);
        assert_eq!(places[2] - places[0], 2);
        let mut held = Vec::new();
        coded.hold(&mut held);
        let null_place = coded.combos() - 1;
        let nothing = [integers(&[None, None])];
        let over = coded_within(&nothing, 2, &held, Some(&mut values)).expect("read by value");
        assert!(over.same_as(&held));
        assert_eq!(placed(&over, 2), [null_place; 2]);
    }

    /// Values further apart than the map allows are hashed, and so is a pair of columns whose
    /// windows multiply past the small bound.
    #[test]
    fn values_spread_wider_than_the_map_allows_are_refused() {
        let mut values = Widened::default();
        let wide = [integers(&[Some(0), Some(WIDE_COMBOS as i32)])];
        assert!(coded_within(&wide, 2, &[], Some(&mut values)).is_none());
        let fits = [integers(&[Some(0), Some(WIDE_COMBOS as i32 - 2)])];
        assert!(coded_within(&fits, 2, &[], Some(&mut values)).is_some());
        let pair = [integers(&[Some(0), Some(100)]), integers(&[Some(0), Some(100)])];
        assert!(coded_within(&pair, 2, &[], Some(&mut values)).is_none());
        let small = [integers(&[Some(0), Some(10)]), integers(&[Some(0), Some(10)])];
        let coded = coded_within(&small, 2, &[], Some(&mut values)).expect("a product of 144");
        let places = placed(&coded, 2);
        assert_ne!(places[0], places[1]);
    }

    /// A filtered packed column too wide for its own places is read by value too, through the
    /// codes the filter kept.
    #[test]
    fn a_filtered_packed_column_too_wide_for_places_is_read_by_value() {
        let page = packed_numbers(&[17, 262_029, 62, 62], 18, 17);
        let kept = [Vector::dictionary(vec![2, 1, 3], page).expect("the rows a filter kept")];
        assert!(coded(&kept, 3).is_none(), "eighteen bits is too wide for places");
        let mut values = Widened::default();
        let coded = coded_within(&kept, 3, &[], Some(&mut values)).expect("read by value");
        let places = placed(&coded, 3);
        assert_eq!(places[0], places[2]);
        assert_eq!(places[1] - places[0], 262_029 - 62);
    }

    /// The rows a filter kept out of a flat integer column are a dictionary of their own every
    /// chunk, and the second of them is read by value so that the map outlives the first.
    #[test]
    fn a_selection_over_a_flat_column_is_read_by_value_once_it_moves() {
        let chunk = |values: &[Option<i32>], kept: Vec<u32>| {
            [Vector::dictionary(kept, integers(values)).expect("the rows a filter kept")]
        };
        let mut values = Widened::default();
        let mut held = Vec::new();
        let first = chunk(&[Some(5), Some(9), None, Some(5)], vec![0, 2, 3]);
        let coded = coded_within(&first, 3, &held, Some(&mut values)).expect("codes of its own");
        coded.hold(&mut held);
        let second = chunk(&[Some(1), Some(7), Some(5), None, Some(7)], vec![1, 2, 3, 4]);
        let coded = coded_within(&second, 4, &held, Some(&mut values)).expect("read by value");
        assert!(coded.by_value());
        let places = placed(&coded, 4);
        assert_eq!(places[0], places[3]);
        assert_eq!(places[0] - places[1], 2);
        assert_eq!(places[2], coded.combos() - 1, "the null takes the place past the window");
        coded.hold(&mut held);
        let third = chunk(&[Some(6), Some(8)], vec![0, 1]);
        let coded = coded_within(&third, 2, &held, Some(&mut values)).expect("read by value");
        assert!(coded.by_value() && coded.same_as(&held), "the window outlives the chunk");
        let back = [Vector::dictionary(vec![1, 0], integers(&[Some(3), Some(4)])).expect("codes")];
        // Codes that go back are read against the window the same as a filter's rows, because the
        // gather reads each row where it is and does not care which way the codes go.
        let coded = coded_within(&back, 2, &held, Some(&mut values)).expect("read by value");
        assert!(coded.by_value() && coded.same_as(&held), "codes that go back fit the window");
        let places = placed(&coded, 2);
        assert_eq!(places[0], places[1] + 1, "4 sits one place past 3");
        let text = |words: &[&str]| {
            let words: Vec<Value> = words.iter().map(|&word| Value::Varchar(word.into())).collect();
            [Vector::dictionary(vec![1, 0, 1], flat(LogicalType::Varchar, &words)).expect("codes")]
        };
        let first = text(&["a", "b"]);
        coded_within(&first, 3, &[], Some(&mut values)).expect("codes").hold(&mut held);
        let next = text(&["c", "d"]);
        let coded = coded_within(&next, 3, &held, Some(&mut values)).expect("codes of its own");
        assert!(!coded.by_value(), "a string column keeps its codes");
    }

    /// A second packed page is read by value against the window rather than by its own codes,
    /// which would mean a new map at every page, and the window it opens is kept by the page after.
    #[test]
    fn a_packed_page_other_than_the_one_held_is_read_by_value() {
        let first = [packed_numbers(&[100, 101, 102, 101], 2, 100)];
        let mut values = Widened::default();
        let mut held = Vec::new();
        let coded = coded_within(&first, 4, &held, Some(&mut values)).expect("places of its own");
        assert!(!coded.by_value(), "a first page keeps its codes");
        coded.hold(&mut held);
        let same = [packed_numbers(&[102, 100, 100, 103], 2, 100)];
        let coded = coded_within(&same, 4, &held, Some(&mut values)).expect("places of its own");
        assert!(!coded.by_value() && coded.same_as(&held), "the same base keeps the map");
        let next = [packed_numbers(&[104, 105, 106, 104], 2, 104)];
        let coded = coded_within(&next, 4, &held, Some(&mut values)).expect("read by value");
        assert!(coded.by_value());
        let places = placed(&coded, 4);
        assert_eq!(places[0], places[3]);
        assert_eq!(places[2] - places[0], 2);
        coded.hold(&mut held);
        let after = [packed_numbers(&[107, 105, 104, 106], 2, 104)];
        let coded = coded_within(&after, 4, &held, Some(&mut values)).expect("read by value");
        assert!(coded.by_value() && coded.same_as(&held), "the window outlives the page");
        let filtered =
            [Vector::dictionary(vec![3, 0], packed_numbers(&[108, 109, 110, 111], 2, 108))
                .expect("the rows a filter kept")];
        let coded = coded_within(&filtered, 2, &held, Some(&mut values)).expect("read by value");
        assert!(coded.by_value() && coded.same_as(&held), "a filtered page lands in it too");
        assert_eq!(placed(&coded, 2)[0] - placed(&coded, 2)[1], 3);
        let wide = [packed_numbers(&[0, 1 << 20, 0, 1], 21, 0)];
        assert!(coded_within(&wide, 4, &held, Some(&mut values)).is_none(), "no places either");
    }

    /// A row hashed on its own is the row hashed with its chunk, which is what lets a probe of one
    /// find the groups the other put in the table.
    #[test]
    fn a_row_hashed_by_value_is_the_row_hashed_with_its_chunk() {
        let page = packed_numbers(&[17, 262_029, 62, 62, -4], 18, -4);
        let forms: Vec<Vec<Vector>> = vec![
            vec![integers(&[Some(62), None, Some(-7), Some(62)])],
            vec![packed_numbers(&[3, 900, 3, 70_000], 17, 3)],
            vec![Vector::dictionary(vec![4, 1, 2, 3], page).expect("the rows a filter kept")],
            vec![
                integers(&[Some(1), Some(2), None, Some(1)]),
                integers(&[Some(5), None, Some(5), Some(9)]),
            ],
        ];
        for keys in &forms {
            let mut values = Widened::default();
            let coded = coded_within(keys, 4, &[], Some(&mut values)).expect("read by value");
            assert!(coded.by_value());
            let mut whole = Vec::new();
            hash(keys, 4, &mut whole, Across::OneInput);
            let alone: Vec<u64> = (0..4).map(|row| coded.hash_of(row)).collect();
            assert_eq!(alone, whole, "{keys:?}");
        }
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

    /// A key a filter cut out of a flat column has the values and the runs of the rows it kept,
    /// found on the column, and codes that do not climb are left to the gather.
    #[test]
    fn the_runs_of_a_cut_key_are_those_of_the_rows_the_filter_kept() {
        // Forty rows each of 5, 7, 9 and 2.
        let values: Vec<i32> = [5, 7, 9, 2].iter().flat_map(|&value| [value; 40]).collect();
        let column = Vector::flat(LogicalType::Integer, Data::Int32(values.into())).unwrap();
        let (mut into, mut runs) = (Vec::new(), Vec::new());
        // Every other row but the ones from 90 to 130, so the run of 9 keeps five rows.
        let kept: Vec<u32> = (0..160).step_by(2).filter(|row| !(90..130).contains(row)).collect();
        let cut = Vector::dictionary(kept.clone(), column.clone()).unwrap();
        assert!(selected_runs(&cut, kept.len(), &mut into, &mut runs));
        let wanted: Vec<i64> = kept.iter().map(|&row| [5, 7, 9, 2][row as usize / 40]).collect();
        assert_eq!(into, wanted);
        assert_eq!(runs, [(5, 20), (7, 40), (9, 45), (2, 60)]);
        // A run with no kept row is left out.
        let kept: Vec<u32> = (0..40).chain(80..160).collect();
        let cut = Vector::dictionary(kept.clone(), column.clone()).unwrap();
        assert!(selected_runs(&cut, kept.len(), &mut into, &mut runs));
        assert_eq!(runs, [(5, 40), (9, 80), (2, 120)]);
        let backwards = Vector::dictionary((0..40).rev().collect(), column).unwrap();
        assert!(!selected_runs(&backwards, 40, &mut into, &mut runs));
        assert!(runs.is_empty());
    }

    /// The count of rows below an end is the one a search of them all gives, whether the rows are
    /// every row or have gaps in them.
    #[test]
    fn rows_below_an_end_match_a_search() {
        let every: Vec<u32> = (10..60).collect();
        let gaps: Vec<u32> =
            (10..200).filter(|row| row % 7 != 3 && !(40..90).contains(row)).collect();
        for at in [&every[..], &gaps[..], &[][..], &[5][..]] {
            for end in 0..220 {
                let wanted = at.partition_point(|&row| (row as usize) < end);
                assert_eq!(below(at, end), wanted, "{end} in {at:?}");
            }
        }
    }
}

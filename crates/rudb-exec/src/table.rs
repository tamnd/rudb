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
use rudb_vector::{Data, Vector};

use crate::key::{canonical, mix, same, spread};
use crate::rows;

/// What a bucket holds when it holds nothing.
const EMPTY: u32 = u32::MAX;

/// The most groups one of these can hold.
///
/// A slot is a `u32` because the buckets are most of what a probe reads and half of them being
/// padding would halve the number that fit in a cache line. The bound that leaves is four billion
/// groups, which at the width of a key is a hundred gigabytes of them, so a query that reaches it
/// has run out of memory in every sense that matters and the only question is which error says so.
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
/// of eight thousand buckets is thirty two kilobytes and there are no misses to overlap, so what is
/// left is the cost of the batch, and a group by over a handful of groups is a query where that is
/// the whole of the time.
const HOT: usize = 8 * 1024;

/// The word a null contributes to the hash.
///
/// A constant rather than nothing at all, so that a null in a column of zeroes does not hash as a
/// zero. It can collide with a real value that happens to be this pattern, which costs one
/// comparison and no correctness, because the comparison is what decides.
const NOTHING: u64 = 0x9e37_79b9_7f4a_7c15;

/// A hash table from a row of key columns to the slot its group was given.
///
/// The slot is the number of groups seen before this one, so the answer comes out in the order the
/// groups were first seen, which is what the operator above this relies on and what makes a failing
/// test a diff rather than an investigation.
#[derive(Debug)]
pub(crate) struct Table {
    /// One slot per bucket, [`EMPTY`] where there is none. A power of two long, so the bucket a
    /// hash belongs to is a mask rather than a division.
    buckets: Vec<u32>,
    /// The stored keys, column at a time. `columns[column][slot]` is one group's value in one key
    /// column, which is the layout that lets a group be pushed without asking the allocator for a
    /// row to put it in.
    columns: Vec<Column>,
    /// The hash of each group's key, so that a probe compares one word before it compares a key.
    /// Worth its eight bytes on a string key, where the comparison it avoids is a memcmp.
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
            buckets: vec![EMPTY; FIRST],
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
        let buckets = self.buckets.capacity() * size_of::<u32>();
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
    pub(crate) fn probe(&self, hash: u64, keys: &[Vector], row: usize) -> Probe {
        let mask = self.buckets.len() - 1;
        let mut at = (hash as usize) & mask;
        loop {
            let slot = self.buckets[at];
            if slot == EMPTY {
                return Probe::Vacant(at);
            }
            let slot = slot as usize;
            if self.hashes[slot] == hash && self.holds(slot, keys, row) {
                return Probe::Found(slot);
            }
            at = (at + 1) & mask;
        }
    }

    /// Looks for a run of rows at once, which is where the time in a group by goes.
    ///
    /// [`Self::probe`] is three loads deep on every row. It reads the bucket, then the stored hash of
    /// whatever slot was in it, then the key beside that slot, and each address is only known once
    /// the load before it has landed. On a table larger than the cache all three are misses, so a row
    /// costs three trips to memory end to end and the core has nothing to get on with while it waits.
    /// A probe of a single `INTEGER` key measured at seventy two nanoseconds a row that way, which is
    /// not work, it is waiting.
    ///
    /// Rows are independent of each other, so this walks [`BATCH`] of them together and does one kind
    /// of load at a time across all of them: every bucket, then every stored hash, then every key.
    /// Inside a pass every address is known before the pass starts, so the misses are all outstanding
    /// at once and the batch waits about as long as one row used to.
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
        let mask = self.buckets.len() - 1;
        walk.pending.clear();
        if self.buckets.len() <= HOT {
            for row in from..upto {
                match self.probe(hashes[row], keys, row) {
                    Probe::Found(slot) => slots[row] = slot,
                    Probe::Vacant(_) => walk.pending.push(row),
                }
            }
            return;
        }
        walk.here.clear();
        walk.here.extend((from..upto).map(|row| Step { row, at: (hashes[row] as usize) & mask }));
        while !walk.here.is_empty() {
            // The bucket of every row still walking, and the only pass that is one load deep.
            walk.seen.clear();
            walk.seen.extend(walk.here.iter().map(|step| self.buckets[step.at]));
            // The stored hash of every bucket that holds a group. The slot each one reads came out of
            // the pass above, so these are independent of each other even though they depend on it.
            walk.same.clear();
            walk.same.extend(walk.here.iter().zip(&walk.seen).map(|(step, &slot)| {
                slot != EMPTY && self.hashes[slot as usize] == hashes[step.row]
            }));
            // The keys, which is the only pass that branches per row and the only one that can end a
            // row's walk. A row that is neither a hit nor a vacancy moves along one bucket and comes
            // back around, so a run of collisions costs passes rather than a serial walk per row.
            walk.next.clear();
            for ((step, &slot), &same) in walk.here.iter().zip(&walk.seen).zip(&walk.same) {
                if slot == EMPTY {
                    walk.pending.push(step.row);
                } else if same && self.holds(slot as usize, keys, step.row) {
                    slots[step.row] = slot as usize;
                } else {
                    walk.next.push(Step { row: step.row, at: (step.at + 1) & mask });
                }
            }
            std::mem::swap(&mut walk.here, &mut walk.next);
        }
        // In row order, because the slot a group is given is the order the answer comes out in, and
        // the passes above reach a vacancy in whatever order the walks happen to end.
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
        self.buckets[bucket] = slot as u32;
        // Half full rather than the seven eighths a `HashMap` allows, because this probes linearly
        // and a linear probe at seven eighths walks a run of about eight buckets to find a miss.
        // The buckets are four bytes each, so the room the other half costs is small next to the
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
        let mut buckets = vec![EMPTY; self.buckets.len() * 2];
        let mask = buckets.len() - 1;
        for (slot, &hash) in self.hashes.iter().enumerate() {
            let mut at = (hash as usize) & mask;
            while buckets[at] != EMPTY {
                at = (at + 1) & mask;
            }
            buckets[at] = slot as u32;
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
}

/// One row part way through a batched probe.
#[derive(Debug, Clone, Copy)]
struct Step {
    /// Its row in the chunk, which is where its hash and its key are.
    row: usize,
    /// The bucket its walk is looking at now.
    at: usize,
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
    /// What each row's bucket holds.
    seen: Vec<u32>,
    /// Whether each row's hash matches the hash stored for what its bucket holds.
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

/// One key column in its common physical width.
///
/// ClickBench's high-cardinality keys are mostly `BIGINT`, `INTEGER`, and `VARCHAR`. Keeping those
/// in a general tagged value made every number 32 bytes wide. The validity is separate because a
/// nullable integer represented as `Option<i64>` is 16 bytes, while `Vec<bool>` uses one bit.
#[derive(Debug)]
struct Column {
    valid: Vec<bool>,
    data: StoredData,
}

#[derive(Debug)]
enum StoredData {
    Integer(Vec<i32>),
    BigInt(Vec<i64>),
    Varchar(StringColumn),
    Other(Vec<Stored>),
}

impl Column {
    fn new(ty: &rudb_common::LogicalType) -> Self {
        let data = match ty {
            rudb_common::LogicalType::Integer => StoredData::Integer(Vec::new()),
            rudb_common::LogicalType::BigInt => StoredData::BigInt(Vec::new()),
            rudb_common::LogicalType::Varchar => StoredData::Varchar(StringColumn::default()),
            _ => StoredData::Other(Vec::new()),
        };
        Self { valid: Vec::new(), data }
    }

    fn push(&mut self, value: Value) -> Result<()> {
        let present = !matches!(value, Value::Null);
        match (&mut self.data, value) {
            (StoredData::Integer(values), Value::Integer(value)) => values.push(value),
            (StoredData::Integer(values), Value::Null) => values.push(0),
            (StoredData::BigInt(values), Value::BigInt(value)) => values.push(value),
            (StoredData::BigInt(values), Value::Null) => values.push(0),
            (StoredData::Varchar(values), Value::Varchar(value)) => values.push(value.as_bytes()),
            (StoredData::Varchar(values), Value::Null) => values.push(&[]),
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

    /// Adds the key that `column` holds at `row`, taking it where it lies when the widths agree.
    ///
    /// What comes back is what the key owns away from this column, which the table adds to its own
    /// total. It is what the key owns and not what it is: the value itself is in one of the runs
    /// below, whose capacity `footprint` counts, and counting it here as well would charge every
    /// group twice for the part of it that is not a string.
    ///
    /// The three stored widths read the row straight out of the vector, so an `INTEGER` or a
    /// `BIGINT` key costs a range check and a push and a `VARCHAR` key costs a copy of its bytes.
    /// Everything else builds a value, which is what all of this used to do.
    fn push_from(&mut self, column: &Vector, row: usize) -> Result<u64> {
        if !column.validity().is_valid(row) {
            return self.push(Value::Null).map(|()| 0);
        }
        let taken = match &mut self.data {
            StoredData::Integer(values) => {
                match column.signed_at(row).and_then(|value| i32::try_from(value).ok()) {
                    Some(value) => {
                        values.push(value);
                        true
                    }
                    None => false,
                }
            }
            StoredData::BigInt(values) => {
                match column.signed_at(row).and_then(|value| i64::try_from(value).ok()) {
                    Some(value) => {
                        values.push(value);
                        true
                    }
                    None => false,
                }
            }
            StoredData::Varchar(values) => match column.bytes_at(row) {
                Some(bytes) => {
                    values.push(bytes);
                    true
                }
                None => false,
            },
            StoredData::Other(_) => false,
        };
        if taken {
            self.valid.push(true);
            return Ok(0);
        }
        // A form that does not hand its rows over where they lie, which is the packed one and the
        // compressed one, or a type wider than the three runs above. The row becomes a value and
        // the general path takes it.
        let value = column.value_at(row);
        let owned = if self.stores_payload() { 0 } else { rows::owned(&value) };
        self.push(value)?;
        Ok(owned)
    }

    fn footprint(&self) -> usize {
        let values = match &self.data {
            StoredData::Integer(values) => values.capacity() * size_of::<i32>(),
            StoredData::BigInt(values) => values.capacity() * size_of::<i64>(),
            StoredData::Varchar(values) => values.footprint(),
            StoredData::Other(values) => values.capacity() * size_of::<Stored>(),
        };
        values + self.valid.capacity().div_ceil(8)
    }

    fn holds(&self, slot: usize, column: &Vector, row: usize) -> bool {
        if !self.valid[slot] {
            return !column.validity().is_valid(row);
        }
        match &self.data {
            // Read where it lies rather than through a value, because this is the one line in the
            // whole aggregate that runs once per input row per probe step. The fallback is not
            // decoration: a form that cannot hand its rows over as integers answers `None` here,
            // and treating that as a key that does not match would put every row of a packed
            // column in a group of its own.
            StoredData::Integer(values) => match column.signed_at(row) {
                Some(value) => value == i128::from(values[slot]),
                None => same(&Value::Integer(values[slot]), &column.value_at(row)),
            },
            StoredData::BigInt(values) => match column.signed_at(row) {
                Some(value) => value == i128::from(values[slot]),
                None => same(&Value::BigInt(values[slot]), &column.value_at(row)),
            },
            StoredData::Varchar(values) => column.bytes_at(row).map_or_else(
                || same(&Value::Varchar(values.string(slot)), &column.value_at(row)),
                |value| value == values.get(slot),
            ),
            StoredData::Other(values) => same(&values[slot].value(), &column.value_at(row)),
        }
    }

    /// A range of groups of this column as a vector, built from the run rather than through values.
    ///
    /// The two fixed widths are stored as exactly what a flat vector holds, so the slice is copied
    /// and the validity is read off the bits beside it. Everything else goes the long way, which is
    /// the string keys and the types that did not earn a run of their own. Strings are not here
    /// because the arena a group key lives in and the arena a vector reads are offset differently,
    /// and rebasing one onto the other is a change of its own rather than a line of this one.
    fn vector(
        &self,
        ty: &rudb_common::LogicalType,
        range: std::ops::Range<usize>,
    ) -> Result<Vector> {
        let (start, len) = (range.start, range.len());
        let data = match &self.data {
            StoredData::Integer(values) => Data::Int32(values[range.clone()].to_vec().into()),
            StoredData::BigInt(values) => Data::Int64(values[range.clone()].to_vec().into()),
            StoredData::Varchar(_) | StoredData::Other(_) => {
                return Vector::from_values(ty.clone(), &self.values(range));
            }
        };
        let valid = &self.valid;
        let validity = rudb_vector::Validity::from_iter(len, |index| valid[start + index]);
        Ok(Vector::flat(ty.clone(), data)?.with_validity(validity))
    }

    fn values(&self, range: std::ops::Range<usize>) -> Vec<Value> {
        range
            .map(|slot| {
                if !self.valid[slot] {
                    return Value::Null;
                }
                match &self.data {
                    StoredData::Integer(values) => Value::Integer(values[slot]),
                    StoredData::BigInt(values) => Value::BigInt(values[slot]),
                    StoredData::Varchar(values) => Value::Varchar(values.string(slot)),
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
    ends: Vec<usize>,
}

impl StringColumn {
    fn push(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
        self.ends.push(self.bytes.len());
    }

    fn get(&self, slot: usize) -> &[u8] {
        let start = slot.checked_sub(1).map_or(0, |before| self.ends[before]);
        &self.bytes[start..self.ends[slot]]
    }

    fn string(&self, slot: usize) -> String {
        String::from_utf8(self.get(slot).to_vec()).expect("a VARCHAR group key is valid UTF-8")
    }

    fn footprint(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * size_of::<usize>()
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
    Timestamp(i64),
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
            Value::Timestamp(v) => Self::Timestamp(v),
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
            Self::Timestamp(v) => Value::Timestamp(*v),
            Self::Interval { months, days, micros } => {
                Value::Interval { months: *months, days: *days, micros: *micros }
            }
            Self::Other(v) => (**v).clone(),
        }
    }
}

/// Hashes a chunk of key columns into one word per row.
///
/// This is the column at a time half of #237. The type of a column is matched on once per column
/// per chunk rather than once per value, so the hash of a thousand rows of `INTEGER` is a pass over
/// a run of `i32` with a multiply and a rotate in it.
///
/// `hashes` is the caller's buffer, kept between chunks so that this does not go to the allocator
/// once per chunk either.
pub(crate) fn hash(keys: &[Vector], rows: usize, hashes: &mut Vec<u64>) {
    hashes.clear();
    hashes.resize(rows, 0);
    for column in keys {
        fold(column, rows, hashes);
    }
    for state in hashes.iter_mut() {
        *state = spread(*state);
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
fn fold(column: &Vector, rows: usize, hashes: &mut [u64]) {
    let validity = column.validity();
    /// One pass over a run of values, turning each into a word the same way the general path does.
    macro_rules! run {
        ($values:expr, $word:expr) => {{
            let values = $values.as_slice();
            let word = $word;
            for (row, state) in hashes.iter_mut().enumerate().take(rows) {
                let one = match values.get(row) {
                    Some(value) if validity.is_valid(row) => word(*value),
                    _ => NOTHING,
                };
                *state = mix(*state, one);
            }
            return;
        }};
    }
    if let Some(data) = column.data() {
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
            // The bytes and not the string, so that a `BLOB` whose bytes are not text hashes as
            // what it is rather than as a null.
            Data::Varlen(strings) => {
                for (row, state) in hashes.iter_mut().enumerate().take(rows) {
                    let one = match strings.bytes(row) {
                        Some(bytes) if validity.is_valid(row) => bytes_word(bytes),
                        _ => NOTHING,
                    };
                    *state = mix(*state, one);
                }
                return;
            }
            _ => {}
        }
    }
    // row at a time: every other form and every type without an arm above. A dictionary string is
    // read as bytes because the input reader already validated the column and validating the same
    // bytes again for every row was most of the string group path. What is left after that is the
    // nested types and the intervals, which have no run of fixed width words to walk at all.
    for (row, state) in hashes.iter_mut().enumerate().take(rows) {
        *state = if column.logical_type() == &rudb_common::LogicalType::Varchar {
            match column.bytes_at(row) {
                Some(bytes) => mix(*state, bytes_word(bytes)),
                None => mix(*state, NOTHING),
            }
        } else {
            fold_value(*state, &column.value_at(row))
        };
    }
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
        Value::BigInt(x) | Value::Time(x) | Value::Timestamp(x) => mix(state, *x as u64),
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
        Value::HugeInt(x) | Value::Decimal { unscaled: x, .. } => {
            mix(mix(state, *x as u64), (*x >> 64) as u64)
        }
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
        hash(std::slice::from_ref(column), column.len(), &mut hashes);
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
        hash(keys, rows, &mut hashes);
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
        hash(keys, rows, &mut hashes);
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
        hash(&[ones.clone(), twos.clone()], 1, &mut forwards);
        hash(&[twos, ones], 1, &mut backwards);
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
        hash(&keys, 3, &mut hashes);

        let mut table = Table::new(&[LogicalType::Varchar, LogicalType::Integer]);
        let Probe::Vacant(bucket) = table.probe(hashes[0], &keys, 0) else {
            panic!("an empty table found a group");
        };
        let slot = table.insert(bucket, hashes[0], &keys, 0).expect("room for one group");
        assert!(matches!(table.probe(hashes[1], &keys, 1), Probe::Found(found) if found == slot));
        assert!(matches!(table.probe(hashes[2], &keys, 2), Probe::Vacant(_)));
        assert_eq!(table.len(), 1);
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
            hash(&keys, 2, &mut hashes);

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
        hash(std::slice::from_ref(&ada), 1, &mut first);
        hash(std::slice::from_ref(&grace), 1, &mut second);

        let mut table = Table::new(&[LogicalType::Varchar]);
        let keys = [ada];
        let Probe::Vacant(bucket) = table.probe(first[0], &keys, 0) else {
            panic!("an empty table found a group");
        };
        table.insert(bucket, first[0], &keys, 0).expect("room for one group");
        assert!(matches!(table.probe(second[0], &[grace], 0), Probe::Vacant(_)));
    }

    /// Growing is where a table stops working quietly. Every key put in before a rehash has to be
    /// found after it, so this puts in more than the sixty four it starts with.
    #[test]
    fn every_group_is_still_found_after_the_buckets_have_doubled() {
        let values: Vec<Value> = (0..1000).map(Value::BigInt).collect();
        let column = flat(LogicalType::BigInt, &values);
        let keys = [column];
        let mut hashes = Vec::new();
        hash(&keys, values.len(), &mut hashes);

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

    /// The fallback in `holds` and `push_from`, which is the risk the two fast paths carry. A packed
    /// column cannot hand a row over as an integer, and a probe that read that `None` as a key that
    /// did not match would put every row of one in a group of its own.
    #[test]
    fn a_packed_integer_column_groups_the_same_as_the_flat_one_it_stands_for() {
        let values: Vec<Value> = (0..256).map(|row| Value::BigInt(row % 7)).collect();
        let plain = flat(LogicalType::BigInt, &values);
        let packed = plain.bit_packed().expect("a column of seven small values packs");
        assert!(packed.signed_at(0).is_none(), "a packed row is not an integer a read can reach");

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
        hash(&keys, values.len(), &mut hashes);
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
        hash(&keys, 1, &mut hashes);
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
}

//! Fixed-width radix ownership for grouped `COUNT(DISTINCT BIGINT)` with a TopN parent.
//!
//! The group key here is four bytes wide whatever the query said it was. A signed integer column
//! already fits, and a string column fits when it arrives with a stable dictionary, because then
//! the code and the string it stands for pick out the same groups and the code is what this can put
//! in a record. That is the whole of why `GROUP BY SearchPhrase` reaches this at all: the dictionary
//! is written once for the column and shared by every chunk of it, so grouping on the code is
//! grouping on the string with none of the payload.
//!
//! Two columns fit in the same four bytes when their codes are laid out side by side the way digits
//! are laid out in a number, so the composite is the first column's code times the width of
//! everything after it plus the rest. Zero is reserved in every column for a null, which is what
//! makes the composite a group key on its own rather than half of one: a record carries a single
//! validity bit, and with more than one column that bit could say a key was null but never which of
//! the keys it was. One column is the case where the bit is enough, and it keeps using it, so
//! nothing about the queries that reached this before has changed.
//!
//! What bounds the composite is that the column widths multiplied together have to fit in four
//! signed bytes. `GROUP BY MobilePhone, MobilePhoneModel` fits because a `SMALLINT` has sixty five
//! thousand values and the model dictionary has under a hundred. Two wide columns do not, and an
//! aggregate whose keys do not fit falls through to the general table the way a string with no
//! dictionary always has.

use std::mem::size_of;
use std::sync::{Arc, Mutex, OnceLock};

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Stage, Value, stage};
use rudb_pipeline::Lease;
use rudb_vector::{Chunk, Vector};

use crate::group::signed_value;
use crate::pairs::{
    self, Counted, Grouped, Held, PARTITIONS, Run, distinct_pairs, in_parallel, scatter,
};
use crate::rows;
use crate::signed::SignedBlock;

const EMPTY: u32 = u32::MAX;

/// One group column, and what its code space is made of.
#[derive(Debug)]
enum Column {
    /// A signed integer column, whose code is the value itself.
    Signed(LogicalType),
    /// A string column, whose code is the position of its value in a stable dictionary.
    Dictionary(Arc<Vector>),
}

impl Column {
    fn kind(&self) -> LogicalType {
        match self {
            Self::Signed(kind) => kind.clone(),
            Self::Dictionary(_) => LogicalType::Varchar,
        }
    }

    /// The lowest code this column can produce, which is what a composite shifts it up by.
    fn low(&self) -> Option<i64> {
        match self {
            Self::Signed(LogicalType::TinyInt) => Some(i64::from(i8::MIN)),
            Self::Signed(LogicalType::SmallInt) => Some(i64::from(i16::MIN)),
            Self::Signed(LogicalType::Integer) => Some(i64::from(i32::MIN)),
            Self::Signed(_) => None,
            Self::Dictionary(_) => Some(0),
        }
    }

    /// How many codes this column has, not counting a null.
    fn width(&self) -> Option<i64> {
        match self {
            Self::Signed(LogicalType::TinyInt) => Some(1 << 8),
            Self::Signed(LogicalType::SmallInt) => Some(1 << 16),
            Self::Signed(LogicalType::Integer) => Some(1 << 32),
            Self::Signed(_) => None,
            Self::Dictionary(dictionary) => i64::try_from(dictionary.len()).ok(),
        }
    }

    /// One code put back into the value it stood for, where the code has had its low taken off.
    fn value(&self, code: i64) -> Result<Value> {
        match self {
            Self::Signed(kind) => signed_value(kind, code),
            Self::Dictionary(dictionary) => {
                let at = usize::try_from(code)
                    .map_err(|_| Error::internal("a group code is not a dictionary position"))?;
                dictionary.try_value_at(at)
            }
        }
    }
}

/// Several group columns laid out side by side in one code.
#[derive(Debug)]
struct Composite {
    columns: Vec<Column>,
    /// What each column's code has taken off it, so that its lowest value lands on zero.
    lows: Vec<i64>,
    /// How many codes each column has, counting the zero that stands for a null.
    spans: Vec<i64>,
    /// What each column's code is multiplied by, which is the width of everything after it.
    strides: Vec<i64>,
}

impl Composite {
    /// The layout these columns need, or nothing when four signed bytes cannot hold it.
    fn plan(columns: Vec<Column>) -> Option<Self> {
        let lows = columns.iter().map(Column::low).collect::<Option<Vec<_>>>()?;
        let spans = columns
            .iter()
            .map(|column| column.width()?.checked_add(1))
            .collect::<Option<Vec<_>>>()?;
        let mut strides = vec![1_i64; spans.len()];
        let mut width = 1_i64;
        for at in (0..spans.len()).rev() {
            strides[at] = width;
            width = width.checked_mul(spans[at])?;
        }
        if width > i64::from(i32::MAX) {
            return None;
        }
        Some(Self { columns, lows, spans, strides })
    }

    /// One composite code taken apart into the values its columns held.
    fn values(&self, code: i32) -> Result<Vec<Value>> {
        let code = i64::from(code);
        let mut out = Vec::with_capacity(self.columns.len());
        for (at, column) in self.columns.iter().enumerate() {
            let here = code / self.strides[at] % self.spans[at];
            out.push(match here {
                0 => Value::Null,
                held => column.value(held - 1 + self.lows[at])?,
            });
        }
        Ok(out)
    }
}

/// How a group key is held in the four bytes a record gives it.
#[derive(Debug)]
enum Shape {
    /// One column held as itself, with the record's validity saying whether the key was null.
    Alone(Column),
    /// Several columns composed into one code, with a null taking each column's zero.
    Many(Composite),
}

impl Shape {
    fn plan(keys: &[Key<'_>]) -> Option<Self> {
        let mut columns = Vec::with_capacity(keys.len());
        for key in keys {
            columns.push(match key.codes {
                Codes::Loose => return None,
                Codes::Signed => Column::Signed(key.kind.clone()),
                Codes::Dictionary(_, dictionary) => Column::Dictionary(Arc::clone(dictionary)),
            });
        }
        match columns.len() {
            0 => None,
            1 => columns.pop().map(Self::Alone),
            _ => Composite::plan(columns).map(Self::Many),
        }
    }

    fn columns(&self) -> &[Column] {
        match self {
            Self::Alone(column) => std::slice::from_ref(column),
            Self::Many(composite) => &composite.columns,
        }
    }

    fn kinds(&self) -> Vec<LogicalType> {
        self.columns().iter().map(Column::kind).collect()
    }

    /// One group's code put back into the values its columns held.
    fn values(&self, group: Grouped) -> Result<Vec<Value>> {
        match self {
            Self::Alone(_) if !group.valid => Ok(vec![Value::Null]),
            Self::Alone(column) => Ok(vec![column.value(i64::from(group.group))?]),
            Self::Many(composite) => composite.values(group.group),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Exchange {
    /// The code space the groups are in, held so that the emit can turn a code back into the value
    /// it stands for, and so that a later chunk arriving in a different code space is caught rather
    /// than counted as if the two agreed on what a code means.
    shape: Shape,
    partitions: Vec<Mutex<Held>>,
    held: Mutex<Vec<Reservation>>,
}

/// What stands in for one group column of one chunk.
pub(crate) enum Codes<'a> {
    /// The column is a signed integer, so the vector is read where it lies.
    Signed,
    /// The column is a string and its stable dictionary code stands in for it.
    Dictionary(&'a [u32], &'a Arc<Vector>),
    /// The column is a string with no stable dictionary, so there is no code to group on.
    Loose,
}

/// One group column of one chunk, as the caller found it.
pub(crate) struct Key<'a> {
    pub(crate) vector: &'a Vector,
    pub(crate) kind: &'a LogicalType,
    pub(crate) codes: Codes<'a>,
}

/// One chunk's group column, read as a flat run before any row is looked at.
enum ColumnReader<'a> {
    Signed(&'a [i64]),
    Dictionary(&'a [u32]),
}

impl ColumnReader<'_> {
    /// The column's code at one row.
    ///
    /// A dictionary code is in range because [`Exchange::buffer`] checks the whole run against the
    /// dictionary before reading any of it, and a signed value is in range because the shape only
    /// admits the widths that fit.
    #[inline]
    fn at(&self, row: usize) -> i64 {
        match self {
            Self::Signed(values) => values[row],
            Self::Dictionary(codes) => i64::from(codes[row]),
        }
    }
}

/// One chunk's whole group key.
enum GroupReader<'a> {
    /// The one column, read where it lies.
    Alone(ColumnReader<'a>),
    /// The composite of every column, built a column at a time before any row is scattered.
    ///
    /// Built up front rather than row by row because a composite has to check that each column's
    /// code is inside the range the layout gave it, and a column at a time that check is one
    /// predictable compare in a loop over a single vector. Row by row it would be a loop over the
    /// columns per row with a branch on each column's layout inside it.
    Many(&'a [i32]),
}

impl GroupReader<'_> {
    /// The group key at one row, narrowed to the four bytes a record holds.
    #[inline]
    fn at(&self, row: usize) -> i32 {
        match self {
            Self::Alone(column) => column.at(row) as i32,
            Self::Many(codes) => codes[row],
        }
    }
}

/// Lays one column's codes into the composite every row is being built in.
///
/// The range check is what keeps a column out of its neighbour's digits. A value that is wider than
/// the type it was read under would multiply up past its own stride and land on some other pair of
/// keys, which is a wrong answer rather than a failure, so it is caught here instead.
fn lay(
    codes: &mut [i32],
    reader: &ColumnReader<'_>,
    nulls: Option<&Vector>,
    at: usize,
    composite: &Composite,
) -> Result<()> {
    let (low, span, stride) = (composite.lows[at], composite.spans[at], composite.strides[at]);
    for (row, slot) in codes.iter_mut().enumerate() {
        let here = match nulls {
            Some(nulls) if nulls.is_null_at(row) => 0,
            _ => reader.at(row) - low + 1,
        };
        if !(0..span).contains(&here) {
            return Err(Error::internal("a group key is wider than the type it was read under"));
        }
        *slot += (here * stride) as i32;
    }
    Ok(())
}

/// The buffers one chunk's columns are read into, kept between chunks.
///
/// Every one of them lives for as long as the instance does, so a chunk after the first allocates
/// nothing for any of this. See [`SignedBlock`] for what reading a column this way saves.
#[derive(Debug, Default)]
struct Scratch {
    /// One buffer per group column, filled only for the columns held as numbers.
    keys: Vec<SignedBlock>,
    /// The distinct argument.
    user: SignedBlock,
    /// The composite every row's key is built in, when there is more than one column.
    codes: Vec<i32>,
}

#[derive(Debug)]
pub(crate) struct Local {
    used: bool,
    partitions: Vec<Run>,
    memory: Reservation,
    scratch: Scratch,
}

impl Local {
    pub(crate) fn new(memory: &Memory) -> Self {
        Self {
            used: false,
            partitions: (0..PARTITIONS).map(|_| Run::default()).collect(),
            memory: memory.reservation(),
            scratch: Scratch::default(),
        }
    }

    pub(crate) fn used(&self) -> bool {
        self.used
    }
}

impl Exchange {
    /// Buffers one chunk when its group representation can remain fixed width.
    ///
    /// Whether it can is decided by the first chunk and never asked again, which is what the
    /// `Option` inside the slot records. A string key with no stable dictionary leaves `None` there
    /// and every instance then falls through to the general table together, rather than some of the
    /// rows being counted here and the rest being counted there.
    pub(crate) fn buffer(
        slot: &OnceLock<Option<Self>>,
        keys: &[Key<'_>],
        user: &Vector,
        rows: usize,
        local: &mut Local,
    ) -> Result<bool> {
        let state = slot.get_or_init(|| Shape::plan(keys).map(Self::new));
        let Some(state) = state else { return Ok(false) };
        // The buffers come out of the instance for the length of the loop, because the readers
        // below hand out borrows of them while every row scattered borrows the instance again.
        let mut scratch = std::mem::take(&mut local.scratch);
        let outcome = state.scatter_blocks(keys, user, rows, &mut scratch, local);
        local.scratch = scratch;
        outcome?;
        Ok(true)
    }

    /// One chunk's columns read as blocks and then scattered a row at a time.
    ///
    /// Split out of [`Self::buffer`] only so that the buffers can be lent out while the instance is
    /// borrowed for the scatter.
    fn scatter_blocks(
        &self,
        keys: &[Key<'_>],
        user: &Vector,
        rows: usize,
        scratch: &mut Scratch,
        local: &mut Local,
    ) -> Result<()> {
        let Scratch { keys: blocks, user: values, codes } = scratch;
        values.read(rows, user)?;
        let nulled = values.nulled();
        let held_user = values.cut(rows)?;
        let reader = self.reader(keys, rows, blocks, codes)?;
        // With one column the record's validity carries the null, so the loop below has to know
        // where to look for it. With several the null is already inside the code.
        let group_nulls = match &self.shape {
            Shape::Alone(_) => nulls_of(keys[0].vector, rows),
            Shape::Many(_) => None,
        };
        let before = local.partitions.iter().map(Run::footprint).sum::<usize>();
        let shift = pairs::shift();
        if !nulled {
            match group_nulls {
                None => {
                    for (row, &user) in held_user.iter().enumerate() {
                        scatter(&mut local.partitions, shift, reader.at(row), true, user);
                    }
                }
                Some(nulls) => {
                    for (row, &user) in held_user.iter().enumerate() {
                        let valid = !nulls.is_null_at(row);
                        let group = if valid { reader.at(row) } else { 0 };
                        scatter(&mut local.partitions, shift, group, valid, user);
                    }
                }
            }
        } else {
            for (row, &held) in held_user.iter().enumerate() {
                if user.is_null_at(row) {
                    continue;
                }
                let valid = group_nulls.is_none_or(|nulls| !nulls.is_null_at(row));
                let group = if valid { reader.at(row) } else { 0 };
                scatter(&mut local.partitions, shift, group, valid, held);
            }
        }
        let after = local.partitions.iter().map(Run::footprint).sum::<usize>();
        local.memory.grow(width(after.saturating_sub(before)))?;
        local.used = true;
        Ok(())
    }

    /// One chunk's keys read the way the shape says they are held.
    fn reader<'a>(
        &self,
        keys: &[Key<'a>],
        rows: usize,
        blocks: &'a mut Vec<SignedBlock>,
        codes: &'a mut Vec<i32>,
    ) -> Result<GroupReader<'a>> {
        if keys.len() != self.shape.columns().len() {
            return Err(Error::internal(
                "a grouped distinct exchange received the wrong key width",
            ));
        }
        blocks.resize_with(keys.len(), SignedBlock::default);
        // Every buffer is filled before any reader is built, because a reader hands out a borrow of
        // the buffer it reads and nothing can be written into them while one of those is out.
        for ((key, column), block) in keys.iter().zip(self.shape.columns()).zip(blocks.iter_mut()) {
            if matches!(column, Column::Signed(_)) {
                block.read(rows, key.vector)?;
            }
        }
        let blocks: &[SignedBlock] = blocks;
        let mut readers = Vec::with_capacity(keys.len());
        for ((key, column), block) in keys.iter().zip(self.shape.columns()).zip(blocks) {
            readers.push(column_reader(key, column, rows, block)?);
        }
        match &self.shape {
            Shape::Alone(_) => readers
                .pop()
                .map(GroupReader::Alone)
                .ok_or_else(|| Error::internal("a grouped distinct exchange received no key")),
            Shape::Many(composite) => {
                codes.clear();
                codes.resize(rows, 0);
                for (at, (reader, key)) in readers.iter().zip(keys).enumerate() {
                    lay(codes, reader, nulls_of(key.vector, rows), at, composite)?;
                }
                Ok(GroupReader::Many(codes))
            }
        }
    }

    fn new(shape: Shape) -> Self {
        Self {
            shape,
            partitions: (0..PARTITIONS).map(|_| Mutex::new(Held::default())).collect(),
            held: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn combine(&self, mut local: Local) -> Result<()> {
        for (at, run) in local.partitions.iter_mut().enumerate() {
            if !run.rows.is_empty() {
                let run = std::mem::take(run);
                self.partitions[at].lock().map_err(poisoned)?.runs.push(run);
            }
        }
        self.held.lock().map_err(poisoned)?.push(local.memory);
        Ok(())
    }

    /// Throws duplicate pairs away a partition at a time and then counts groups a split at a time.
    ///
    /// The two passes are partitioned on different things and that is the point of there being two
    /// of them. Throwing duplicates away is the expensive pass, because its table holds a row per
    /// distinct pair and every probe into it is a cache miss, so the rows are partitioned on the
    /// pair and every partition gets an equal share of them whatever the grouping column looks
    /// like. Counting is the cheap pass, because its table holds a row per group and a query with
    /// few enough groups to be lopsided has a table small enough to sit in cache, so it is
    /// partitioned on the group, which puts every group in one split and lets each split take its
    /// own top rows with nobody to agree with afterwards.
    ///
    /// Partitioning on the group throughout is what this used to do, and it gave the whole of a
    /// group's deduplicating to one thread. On the million row ClickBench file one region holds
    /// eighteen percent of the distinct pairs, so one of sixteen partitions did three times the
    /// average share and the other fifteen waited for it.
    pub(crate) fn finish(
        &self,
        threads: &Lease<'_>,
        bound: usize,
        memory: &Memory,
    ) -> Result<Vec<Chunk>> {
        let input = self
            .partitions
            .iter()
            .map(|partition| partition.lock().map(|held| held.rows()).map_err(poisoned))
            .sum::<Result<usize>>()?;
        // The same rule a plain aggregate finishes on, for the same reason and rather more so. A row
        // costs more on this path: it probes a table that holds a slot per distinct pair, which is
        // most of the way to a slot per row, so the probe misses cache where a plain aggregate's
        // probe into a table of groups usually does not. That is what makes the second half of
        // [`pairs::finish_degree`] matter here, since a pass that is waiting on memory is the one a
        // thread past the machine's memory level parallelism does nothing for.
        let degree = pairs::finish_degree(input, PARTITIONS.min(threads.degree()));
        // How many of the scattered partitions are worth keeping apart, which the scatter itself
        // could not know. See [`pairs::used`].
        let used = pairs::used(input, degree);
        // Either every split or one of it. A split is a vector per pair partition, so there are as
        // many of them as the two counts multiplied, and a query that is going to finish on one
        // thread should not be paying for a hundred vectors to hand itself its own rows. Anything
        // that is worth a second thread is worth the full spread, because the counting pass is
        // skewed by the grouping column in a way the deduplicating pass no longer is.
        let splits = if degree > 1 { used } else { 1 };
        let counted = in_parallel(
            threads,
            used,
            degree,
            "deduplicated the pairs of radix partition",
            |at| {
                let mut partition = Held::default();
                for from in pairs::merged(at, used) {
                    let mut held = self.partitions[from].lock().map_err(poisoned)?;
                    partition.runs.append(&mut held.runs);
                }
                distinct_pairs(&mut partition, splits, memory)
            },
        )?;
        let pieces = counting_pieces(&sizes_of(&counted, splits), counted.len(), degree);
        let tallied = in_parallel(threads, pieces.len(), degree, "counted the groups of", |at| {
            let piece = &pieces[at];
            count_groups(&counted, piece, &self.shape, bound, memory)
        })?;
        // The distinct pairs are read for the last time by the pass above, so the room they took
        // goes back here rather than at the end of the query. Handing the four thousand vectors to
        // the leased threads to free instead was tried and is not worth it: on the million row
        // ClickBench file `COUNT(DISTINCT UserID) GROUP BY RegionID` frees them in 0.100 ms on this
        // thread and in 0.114 ms on sixteen, because the allocator sends a block freed by a thread
        // that did not allocate it down a slower path and because a pass this short is most of one
        // scatter and join.
        for part in counted {
            drop(part.held);
        }
        let mut chunks = Vec::new();
        let mut held = self.held.lock().map_err(poisoned)?;
        held.clear();
        for Output { chunks: mut part, held: charge } in
            gathered(tallied, &self.shape, bound, memory)?
        {
            chunks.append(&mut part);
            held.push(charge);
        }
        Ok(chunks)
    }
}

/// One chunk's column read the way the shape says that column is held.
fn column_reader<'a>(
    key: &Key<'a>,
    column: &Column,
    rows: usize,
    block: &'a SignedBlock,
) -> Result<ColumnReader<'a>> {
    match (column, &key.codes) {
        (Column::Signed(_), Codes::Signed) => Ok(ColumnReader::Signed(block.cut(rows)?)),
        (Column::Dictionary(held), Codes::Dictionary(codes, dictionary))
            if Arc::ptr_eq(held, dictionary) =>
        {
            // Checked for the whole run here rather than once a row, so that the read later is a
            // load and nothing else. A row whose key is null has whatever code the dictionary
            // vector happened to leave there, which is why the null rows are exempt.
            //
            // The width is checked against `i32::MAX` and not just against the run because a
            // record holds four signed bytes. A code above that would narrow to a negative
            // number and land on some other code's group.
            let width = dictionary.len();
            if i32::try_from(width).is_err() {
                return Err(Error::internal(
                    "a stable dictionary has more codes than a group record holds",
                ));
            }
            let loose = codes[..rows]
                .iter()
                .enumerate()
                .any(|(row, &code)| code as usize >= width && !key.vector.is_null_at(row));
            if loose {
                return Err(Error::internal("a stable dictionary code is out of range"));
            }
            Ok(ColumnReader::Dictionary(codes))
        }
        _ => Err(Error::internal("a grouped distinct exchange received two group code spaces")),
    }
}

/// A column's validity when the chunk has a null in it, and nothing when it has none.
fn nulls_of(vector: &Vector, rows: usize) -> Option<&Vector> {
    vector.validity().has_nulls(rows).then_some(vector)
}

struct Output {
    chunks: Vec<Chunk>,
    held: Reservation,
}

/// Adds up the groups a split has in the pair partitions this piece was given.
///
/// Every pair of a group lands in the same split, because the split is picked by the group hash, so
/// a piece holding a whole split has nothing to agree with anybody about and the top rows it picks
/// are final. A piece holding part of one hands its counts back instead. See [`counting_pieces`].
///
/// The table is sized by the groups it finds rather than by the pairs it is handed, which is the
/// difference between a probe that stays in the first level of cache and one that does not. The two
/// numbers are far apart on the query this path exists for: a split of `COUNT(DISTINCT UserID)
/// GROUP BY RegionID` over a million rows is fourteen thousand pairs and ten groups, so a table
/// sized by the pairs is a hundred and thirty kilobytes with ten things in it, spread so thinly that
/// almost every one of those fourteen thousand probes lands on a line nothing else touches. Sized by
/// the groups it is two hundred and fifty six bytes and every probe is a hit.
///
/// So it starts at [`SPLIT_SEED`] slots and doubles while it is less than half empty, up to the size
/// the input could need, which is the size this used to start at. The doubling is a rehash of the
/// groups found so far and nothing else, because the counts stay where they are and the buckets hold
/// an index into them rather than a key, so the whole cost of growing is bounded by the group count
/// and the group count is what was too small to be worth the big table in the first place.
fn count_groups(
    counted: &[Counted],
    piece: &Piece,
    shape: &Shape,
    bound: usize,
    memory: &Memory,
) -> Result<Tallied> {
    let split = piece.split;
    let parts = &counted[piece.parts.clone()];
    let reserving = stage::Timing::start(Stage::Reserve);
    let input = parts.iter().map(|part| part.splits[split].len()).sum::<usize>();
    let ceiling = input.saturating_mul(2).max(SPLIT_SEED).next_power_of_two();
    let mut capacity = SPLIT_SEED.min(ceiling);
    let mut working = memory.reservation();
    working.grow(width(capacity * size_of::<u32>()))?;
    let mut buckets = vec![EMPTY; capacity];
    let mut mask = capacity - 1;
    let mut groups: Vec<Grouped> = Vec::new();
    let mut counts: Vec<i64> = Vec::new();
    reserving.stop(0);

    let timing = stage::Timing::start(Stage::Fold);
    for part in parts {
        for pair in &part.splits[split] {
            let mut at = pair.group_hash as usize & mask;
            loop {
                let slot = buckets[at];
                if slot == EMPTY {
                    buckets[at] = u32::try_from(groups.len()).map_err(|_| {
                        Error::out_of_memory("a grouped distinct radix split is too large")
                    })?;
                    groups.push(*pair);
                    counts.push(1);
                    if groups.len().saturating_mul(2) > capacity && capacity < ceiling {
                        capacity *= 2;
                        mask = capacity - 1;
                        working.grow(width(capacity * size_of::<u32>()))?;
                        buckets = rehashed(capacity, &groups)?;
                    }
                    break;
                }
                let slot = slot as usize;
                // One index into each vector rather than three and two. The comparison used to name
                // `groups[slot]` once a field and the count named `counts[slot]` on both sides of
                // its own assignment, and each of those is a bounds check and a load that the one
                // before it already paid for. See [`Grouped`].
                if groups[slot] == *pair {
                    let count = &mut counts[slot];
                    *count = count
                        .checked_add(1)
                        .ok_or_else(|| Error::out_of_range("COUNT(DISTINCT BIGINT) overflowed"))?;
                    break;
                }
                at = (at + 1) & mask;
            }
        }
    }
    let kept =
        width(groups.capacity() * size_of::<Grouped>() + counts.capacity() * size_of::<i64>());
    working.grow(kept)?;
    timing.stop(0);

    if !piece.whole {
        // The table itself is finished with, but the two vectors it filled have to live until the
        // rest of this split arrives, so the room they take is charged on its own and the room the
        // buckets took goes back now.
        drop(buckets);
        let mut held = memory.reservation();
        held.grow(kept)?;
        drop(working);
        return Ok(Tallied::Part(Partial { split, groups, counts, held }));
    }
    emit(&groups, &counts, shape, bound, memory).map(Tallied::Whole)
}

/// The best `bound` groups of a finished tally, as the rows they stand for.
fn emit(
    groups: &[Grouped],
    counts: &[i64],
    shape: &Shape,
    bound: usize,
    memory: &Memory,
) -> Result<Output> {
    let timing = stage::Timing::start(Stage::Emit);
    let mut best: Vec<usize> = Vec::with_capacity(bound.min(groups.len()));
    for slot in 0..groups.len() {
        let at = best.partition_point(|&kept| counts[kept] >= counts[slot]);
        if at < bound {
            best.insert(at, slot);
            best.truncate(bound);
        }
    }
    best.sort_unstable();
    let mut output = Vec::with_capacity(best.len());
    for slot in best {
        // The code goes back to being the values it stood for here and nowhere earlier, so what is
        // copied is one row per group that reached the bound rather than one per row of input.
        let mut row = shape.values(groups[slot])?;
        row.push(Value::BigInt(counts[slot]));
        output.push(row);
    }
    let mut kinds = shape.kinds();
    kinds.push(LogicalType::BigInt);
    let mut held = memory.reservation();
    let chunks = rows::chunks(&kinds, &output, &mut held)?;
    timing.stop(0);
    Ok(Output { chunks, held })
}

/// What one piece of the counting pass made.
enum Tallied {
    /// The piece held a whole split, so its counts are final and it took its own top rows.
    Whole(Output),
    /// The piece held part of a split, so its counts still have to meet the rest of that split.
    Part(Partial),
}

/// One piece's counts for part of a split, on their way to meeting the other pieces of it.
struct Partial {
    split: usize,
    groups: Vec<Grouped>,
    counts: Vec<i64>,
    held: Reservation,
}

/// One piece of the counting pass, which is a split and which of the pair partitions to read of it.
///
/// See [`counting_pieces`] for why a split is ever read in more than one piece.
struct Piece {
    split: usize,
    parts: std::ops::Range<usize>,
    /// Whether this piece has the whole split and so can take its own top rows.
    whole: bool,
}

/// How many pairs each split was handed, which is what decides how the counting pass is cut up.
fn sizes_of(counted: &[Counted], splits: usize) -> Vec<usize> {
    let mut sizes = vec![0_usize; splits];
    for part in counted {
        for (split, held) in part.splits.iter().enumerate().take(splits) {
            sizes[split] += held.len();
        }
    }
    sizes
}

/// The counting pass cut into pieces, with a split that is too big for one thread cut further.
///
/// A split is all the pairs of some set of groups, and which groups land together is the group hash,
/// so a split is as lopsided as the grouping column is. On the million row ClickBench file
/// `COUNT(DISTINCT UserID) GROUP BY RegionID` puts eighteen percent of the distinct pairs in one
/// region, and that region is one group, so no number of splits divides it. Measured there, the
/// counting pass had 2.42 ms of work to do, finished in 0.403 ms, and the thread that drew the big
/// region took 0.376 ms of that against the 0.151 ms sixteen threads would have taken between them.
///
/// So a split whose share is more than one thread's worth is read in several pieces instead, each
/// taking a range of the pair partitions the deduplication left. Those pieces cannot pick top rows,
/// because none of them has all of any group's pairs, so they hand their counts back to be added up.
/// That is cheap precisely when it happens: a split is only ever oversized because a few groups in it
/// are huge, and it is the group count and not the pair count that the adding up walks.
///
/// A split that fits stays exactly as it was, one piece reading every partition and picking its own
/// top rows, so an even grouping column pays nothing for this and nothing about its output moves.
/// On that query five of the sixty seven pieces are cut ones and the pass finishes in 0.32 ms.
fn counting_pieces(sizes: &[usize], parts: usize, degree: usize) -> Vec<Piece> {
    let total = sizes.iter().sum::<usize>();
    let fair = total.div_ceil(degree.max(1)).max(1);
    let mut pieces = Vec::with_capacity(sizes.len());
    for (split, &size) in sizes.iter().enumerate() {
        let ways = if size > fair { size.div_ceil(fair).min(parts) } else { 1 };
        if ways < 2 {
            pieces.push(Piece { split, parts: 0..parts, whole: true });
            continue;
        }
        let per = parts.div_ceil(ways);
        let mut from = 0;
        while from < parts {
            let to = (from + per).min(parts);
            pieces.push(Piece { split, parts: from..to, whole: false });
            from = to;
        }
    }
    pieces
}

/// Every piece's counts turned into output, adding up the pieces that only had part of a split.
///
/// The pieces arrive in split order and a split's pieces are next to each other, so a run of parts
/// is gathered as it is reached and the splits that were read whole pass straight through. The
/// output is in the same order it would have been in had nothing been cut up.
fn gathered(
    tallied: Vec<Tallied>,
    shape: &Shape,
    bound: usize,
    memory: &Memory,
) -> Result<Vec<Output>> {
    let mut out = Vec::with_capacity(tallied.len());
    let mut run: Vec<Partial> = Vec::new();
    for piece in tallied {
        match piece {
            Tallied::Whole(output) => out.push(output),
            Tallied::Part(part) => {
                if run.first().is_some_and(|first| first.split != part.split) {
                    out.push(added_up(std::mem::take(&mut run), shape, bound, memory)?);
                }
                run.push(part);
            }
        }
    }
    if !run.is_empty() {
        out.push(added_up(run, shape, bound, memory)?);
    }
    Ok(out)
}

/// One split's pieces added together and then asked for its top rows.
///
/// The table is sized by the groups the pieces found, which is the whole reason cutting a split up
/// is affordable. A split is cut up because one of its groups holds a great many pairs, and a group
/// is one entry here however many pairs it had.
fn added_up(parts: Vec<Partial>, shape: &Shape, bound: usize, memory: &Memory) -> Result<Output> {
    let input = parts.iter().map(|part| part.groups.len()).sum::<usize>();
    let capacity = input.saturating_mul(2).max(SPLIT_SEED).next_power_of_two();
    let mask = capacity - 1;
    let mut working = memory.reservation();
    working.grow(width(capacity * size_of::<u32>()))?;
    let mut buckets = vec![EMPTY; capacity];
    let mut groups: Vec<Grouped> = Vec::with_capacity(input);
    let mut counts: Vec<i64> = Vec::with_capacity(input);
    for part in parts {
        for (pair, &by) in part.groups.iter().zip(&part.counts) {
            let mut at = pair.group_hash as usize & mask;
            loop {
                let slot = buckets[at];
                if slot == EMPTY {
                    buckets[at] = u32::try_from(groups.len()).map_err(|_| {
                        Error::out_of_memory("a grouped distinct radix split is too large")
                    })?;
                    groups.push(*pair);
                    counts.push(by);
                    break;
                }
                let slot = slot as usize;
                if groups[slot] == *pair {
                    let count = &mut counts[slot];
                    *count = count
                        .checked_add(by)
                        .ok_or_else(|| Error::out_of_range("COUNT(DISTINCT BIGINT) overflowed"))?;
                    break;
                }
                at = (at + 1) & mask;
            }
        }
        // This piece's counts have been read for the last time, so what they took goes back now
        // rather than at the end of a merge that could be holding several of them.
        drop(part.groups);
        drop(part.counts);
        drop(part.held);
    }
    emit(&groups, &counts, shape, bound, memory)
}

/// The slots a split's group table starts with, which is one cache line of them and then some.
///
/// Small enough that the table a query with a handful of groups builds fits in the first level of
/// cache beside everything else the fold is touching, and large enough that a query with a handful
/// of groups never has to grow it at all. See [`count_groups`].
const SPLIT_SEED: usize = 64;

/// A fresh table of `capacity` slots holding every group in `groups` at its index.
///
/// Only the groups are rehashed. The counts are indexed by the same number the buckets hold, so
/// nothing about them moves when the table grows.
fn rehashed(capacity: usize, groups: &[Grouped]) -> Result<Vec<u32>> {
    let mask = capacity - 1;
    let mut buckets = vec![EMPTY; capacity];
    for (slot, group) in groups.iter().enumerate() {
        let slot = u32::try_from(slot)
            .map_err(|_| Error::out_of_memory("a grouped distinct radix split is too large"))?;
        let mut at = group.group_hash as usize & mask;
        while buckets[at] != EMPTY {
            at = (at + 1) & mask;
        }
        buckets[at] = slot;
    }
    Ok(buckets)
}

fn width(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a grouped distinct radix lock was poisoned")
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;
    use std::sync::Arc;

    use rudb_common::{LogicalType, Memory, Value};
    use rudb_vector::Vector;

    use crate::pairs::{Held, Record, Run, distinct_pairs};

    use super::{Column, Composite, Shape, count_groups, counting_pieces, gathered, sizes_of};

    #[test]
    fn one_partition_deduplicates_pairs_and_counts_groups_across_the_runs_it_was_handed() {
        let row = |group, user, pair_hash| Record { user, group, pair_hash };
        // Three instances, and the pair (3, 10) arrives in two of them, which is the case the
        // deduplication has to see across a run boundary rather than only within one run.
        let mut first = Run::default();
        first.push(row(3, 10, 5), true);
        first.push(row(3, 10, 5), true);
        let mut second = Run::default();
        second.push(row(3, 11, 5), true);
        second.push(row(3, 10, 5), true);
        let mut third = Run::default();
        third.push(row(4, 10, 5), true);
        third.push(row(0, 10, 5), false);
        let mut partition = Held { runs: vec![first, Run::default(), second, third] };
        let rows = finished(&mut partition, &signed());
        assert_eq!(
            rows,
            [
                vec![Value::Integer(3), Value::BigInt(2)],
                vec![Value::Integer(4), Value::BigInt(1)],
                vec![Value::Null, Value::BigInt(1)],
            ]
        );
        assert_eq!(size_of::<Record>(), 16);
    }

    #[test]
    fn a_group_held_as_a_dictionary_code_comes_out_as_the_string_the_code_stands_for() {
        let row = |group, user, pair_hash| Record { user, group, pair_hash };
        let mut run = Run::default();
        run.push(row(2, 10, 5), true);
        run.push(row(2, 11, 5), true);
        run.push(row(2, 10, 5), true);
        run.push(row(1, 10, 5), true);
        run.push(row(0, 10, 5), false);
        let mut partition = Held { runs: vec![run] };
        let rows = finished(&mut partition, &Shape::Alone(Column::Dictionary(words())));
        // Code 0 is "zero" in the dictionary and the group whose key was null still answers NULL,
        // because what makes a group null is the key's validity and not what its code points at.
        assert_eq!(
            rows,
            [
                vec![Value::Null, Value::BigInt(1)],
                vec![Value::Varchar("one".to_string()), Value::BigInt(1)],
                vec![Value::Varchar("two".to_string()), Value::BigInt(2)],
            ]
        );
    }

    #[test]
    fn two_columns_composed_into_one_code_come_back_out_as_the_pair_they_were() {
        let shape = two_columns();
        // A `SMALLINT` beside a three word dictionary, so a column of four codes sits under a column
        // of sixty five thousand and seven. The codes below are what `GroupReader` would have built.
        let code = |phone: i64, word: i64| {
            let Shape::Many(composite) = &shape else { panic!("a composite") };
            ((phone - i64::from(i16::MIN) + 1) * composite.strides[0] + word + 1) as i32
        };
        let row = |group, user, pair_hash| Record { user, group, pair_hash };
        let mut run = Run::default();
        run.push(row(code(7, 2), 10, 5), true);
        run.push(row(code(7, 2), 11, 5), true);
        run.push(row(code(7, 2), 10, 5), true);
        run.push(row(code(7, 1), 10, 5), true);
        run.push(row(code(-3, 1), 10, 5), true);
        let mut partition = Held { runs: vec![run] };
        assert_eq!(
            finished(&mut partition, &shape),
            [
                vec![Value::SmallInt(-3), Value::Varchar("one".into()), Value::BigInt(1)],
                vec![Value::SmallInt(7), Value::Varchar("one".into()), Value::BigInt(1)],
                vec![Value::SmallInt(7), Value::Varchar("two".into()), Value::BigInt(2)],
            ]
        );
    }

    #[test]
    fn a_null_in_one_column_of_a_composite_is_a_group_of_its_own_per_other_column() {
        let shape = two_columns();
        // A null phone beside two different words, which is the case a single validity bit cannot
        // tell apart and the reserved zero can. A null phone is the zero of the top column, so all
        // that is left of the composite is the word's own code.
        let code = |word: i64| (word + 1) as i32;
        let (first, second) = (code(1), code(2));
        let row = |group, user, pair_hash| Record { user, group, pair_hash };
        let mut run = Run::default();
        run.push(row(first, 10, 5), true);
        run.push(row(second, 10, 5), true);
        run.push(row(second, 11, 5), true);
        let mut partition = Held { runs: vec![run] };
        assert_eq!(
            finished(&mut partition, &shape),
            [
                vec![Value::Null, Value::Varchar("one".into()), Value::BigInt(1)],
                vec![Value::Null, Value::Varchar("two".into()), Value::BigInt(2)],
            ]
        );
    }

    #[test]
    fn two_wide_columns_do_not_fit_in_a_group_code() {
        // Four billion values under three, which is over what four signed bytes hold.
        assert!(
            Composite::plan(vec![
                Column::Signed(LogicalType::Integer),
                Column::Dictionary(words()),
            ])
            .is_none()
        );
        // A `BIGINT` has no width this can put a bound on at all.
        assert!(
            Composite::plan(
                vec![Column::Signed(LogicalType::BigInt), Column::Dictionary(words()),]
            )
            .is_none()
        );
    }

    #[test]
    fn a_group_whose_pairs_landed_in_different_partitions_comes_out_with_one_count() {
        // What the two passes are for. The same group is counted separately by two pair partitions
        // and the merge has to add the two parts up rather than report a group twice, which is what
        // partitioning on the pair costs and what the second pass buys back.
        //
        // At one split as well as at several, because a query small enough to finish on one thread
        // asks for one split and that is the arithmetic in `split_of` that has no bits left to shift.
        let row = |group, user, pair_hash| Record { user, group, pair_hash };
        for splits in [1, SPLITS] {
            let mut first = Run::default();
            first.push(row(3, 10, 5), true);
            first.push(row(3, 11, 5), true);
            let mut second = Run::default();
            second.push(row(3, 12, 9), true);
            second.push(row(4, 12, 9), true);
            let mut left = Held { runs: vec![first] };
            let mut right = Held { runs: vec![second] };
            let memory = Memory::unlimited();
            let counted = vec![
                distinct_pairs(&mut left, splits, &memory).expect("a pair partition"),
                distinct_pairs(&mut right, splits, &memory).expect("a pair partition"),
            ];
            assert_eq!(
                rows_of(&counted, splits, &signed()),
                [
                    vec![Value::Integer(3), Value::BigInt(3)],
                    vec![Value::Integer(4), Value::BigInt(1)],
                ]
            );
        }
    }

    #[test]
    fn a_split_read_in_pieces_counts_the_same_as_a_split_read_whole() {
        // A split is cut up when it holds more pairs than one thread's fair share, and then no piece
        // of it has all of any group and the counts have to be added up afterwards. Asking for a
        // thread per pair makes every split too big for one, so every split here takes that path.
        let row = |group, user, pair_hash| Record { user, group, pair_hash };
        let mut first = Run::default();
        first.push(row(3, 10, 5), true);
        first.push(row(3, 11, 5), true);
        first.push(row(4, 12, 9), true);
        let mut second = Run::default();
        second.push(row(3, 12, 9), true);
        second.push(row(3, 14, 9), true);
        second.push(row(4, 13, 5), true);
        let mut left = Held { runs: vec![first] };
        let mut right = Held { runs: vec![second] };
        let memory = Memory::unlimited();
        let counted = vec![
            distinct_pairs(&mut left, SPLITS, &memory).expect("a pair partition"),
            distinct_pairs(&mut right, SPLITS, &memory).expect("a pair partition"),
        ];
        let whole = rows_of(&counted, SPLITS, &signed());
        assert_eq!(
            whole,
            [vec![Value::Integer(3), Value::BigInt(4)], vec![Value::Integer(4), Value::BigInt(2)],]
        );
        assert_eq!(rows_at(&counted, SPLITS, &signed(), 64), whole);
    }

    /// How many splits the tests count over, picked to be neither one nor the sixteen a big query gets.
    const SPLITS: usize = 4;

    fn signed() -> Shape {
        Shape::Alone(Column::Signed(LogicalType::Integer))
    }

    fn words() -> Arc<Vector> {
        let words = ["zero", "one", "two"].map(|word| Value::Varchar(word.to_string()));
        Arc::new(Vector::from_values(LogicalType::Varchar, &words).expect("a dictionary"))
    }

    fn two_columns() -> Shape {
        Shape::Many(
            Composite::plan(vec![
                Column::Signed(LogicalType::SmallInt),
                Column::Dictionary(words()),
            ])
            .expect("a composite"),
        )
    }

    /// One partition finished and flattened into rows, sorted so the partition order does not show.
    fn finished(partition: &mut Held, shape: &Shape) -> Vec<Vec<Value>> {
        let counted = vec![
            distinct_pairs(partition, SPLITS, &Memory::unlimited()).expect("a pair partition"),
        ];
        rows_of(&counted, SPLITS, shape)
    }

    /// Every split merged and flattened into rows, sorted so the split order does not show.
    fn rows_of(counted: &[crate::pairs::Counted], splits: usize, shape: &Shape) -> Vec<Vec<Value>> {
        rows_at(counted, splits, shape, 1)
    }

    /// The same, finishing on `degree` threads, which is what decides whether a split gets cut up.
    fn rows_at(
        counted: &[crate::pairs::Counted],
        splits: usize,
        shape: &Shape,
        degree: usize,
    ) -> Vec<Vec<Value>> {
        let memory = Memory::unlimited();
        let pieces = counting_pieces(&sizes_of(counted, splits), counted.len(), degree);
        let tallied = pieces
            .iter()
            .map(|piece| {
                count_groups(counted, piece, shape, 10, &memory).expect("a grouped distinct split")
            })
            .collect();
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for output in gathered(tallied, shape, 10, &memory).expect("the counted splits") {
            for chunk in output.chunks {
                for row in 0..chunk.len() {
                    rows.push(
                        (0..chunk.width()).map(|column| chunk.value_at(row, column)).collect(),
                    );
                }
            }
        }
        rows.sort_by_key(|row| format!("{row:?}"));
        rows
    }
}

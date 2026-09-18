//! Fixed-width radix ownership for grouped `COUNT(DISTINCT BIGINT)` with a TopN parent.
//!
//! The group key here is four bytes wide whatever the query said it was. An `INTEGER` key already
//! is, and a `VARCHAR` key becomes one when the column arrives with a stable dictionary, because
//! then the code and the string it stands for pick out the same groups and the code is what this
//! can put in a record. That is the whole of why `GROUP BY SearchPhrase` reaches this at all: the
//! dictionary is written once for the column and shared by every chunk of it, so grouping on the
//! code is grouping on the string with none of the payload.

use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Spent, Stage, Value, stage};
use rudb_vector::{Chunk, Vector};

use crate::key::{mix, spread};
use crate::rows;
use crate::signed::SignedReader;

const PARTITIONS: usize = 16;
const EMPTY: u32 = u32::MAX;
const NOTHING: u64 = 0x9e37_79b9_7f4a_7c15;

#[derive(Debug)]
pub(crate) struct Exchange {
    /// The code space the groups are in, and `None` when the key was an integer to begin with.
    ///
    /// Held so that the emit can turn a code back into the string it stands for, and so that a
    /// later chunk arriving in a different code space is caught rather than counted as if the two
    /// agreed on what a code means.
    dictionary: Option<Arc<Vector>>,
    partitions: Vec<Mutex<Held>>,
    held: Mutex<Vec<Reservation>>,
}

/// What stands in for the group key of one chunk.
pub(crate) enum Codes<'a> {
    /// The key is a signed integer, so the vector is read where it lies.
    Signed,
    /// The key is a string and its stable dictionary code stands in for it.
    Dictionary(&'a [u32], &'a Arc<Vector>),
    /// The key is a string with no stable dictionary, so there is no code to group on.
    Loose,
}

/// One chunk's group key, with the layout decided once instead of once a row.
enum GroupReader<'a> {
    Signed(SignedReader<'a>),
    Dictionary(&'a [u32]),
}

impl GroupReader<'_> {
    /// The group key at one row, narrowed to the four bytes a record holds.
    ///
    /// A code is in range because [`Exchange::buffer`] checks the whole run against the dictionary
    /// before reading any of it, and a signed key is in range because the binder typed it `INTEGER`.
    #[inline]
    fn at(&self, row: usize) -> i32 {
        match self {
            Self::Signed(reader) => reader.at(row) as i32,
            Self::Dictionary(codes) => codes[row] as i32,
        }
    }
}

/// One radix partition's rows, as the run each instance handed over rather than one flat run.
///
/// An instance that finishes used to append its run onto the shared one, which copies every record
/// it holds while holding that partition's lock. With sixteen instances that is fifteen sixteenths
/// of the whole column copied, sixteen bytes a row, serialised behind sixteen locks. Handing the run
/// over is a move instead, and the pass that deduplicates the pairs walks the runs one after another
/// and cannot tell the difference.
#[derive(Debug, Default)]
struct Held {
    runs: Vec<Run>,
}

impl Held {
    fn rows(&self) -> usize {
        self.runs.iter().map(|run| run.rows.len()).sum()
    }
}

#[derive(Debug, Clone, Copy)]
struct Record {
    user: i64,
    group: i32,
    pair_hash: u32,
}

fn group_hash(row: Record, valid: bool) -> u32 {
    let word = if valid { i64::from(row.group) as u64 } else { NOTHING };
    let wide = spread(mix(0, word));
    (wide ^ (wide >> 32)) as u32
}

/// One instance's rows for one radix partition.
#[derive(Debug, Default)]
struct Run {
    rows: Vec<Record>,
    /// Empty while every group key in this run is valid.
    validity: Vec<bool>,
}

impl Run {
    fn push(&mut self, row: Record, valid: bool) {
        if !valid && self.validity.is_empty() {
            self.validity.resize(self.rows.len(), true);
        }
        self.rows.push(row);
        if !self.validity.is_empty() {
            self.validity.push(valid);
        }
    }

    fn valid_at(&self, row: usize) -> bool {
        self.validity.is_empty() || self.validity[row]
    }

    fn footprint(&self) -> usize {
        self.rows.capacity() * size_of::<Record>() + self.validity.capacity() * size_of::<bool>()
    }
}

#[derive(Debug)]
pub(crate) struct Local {
    used: bool,
    partitions: Vec<Run>,
    memory: Reservation,
}

impl Local {
    pub(crate) fn new(memory: &Memory) -> Self {
        Self {
            used: false,
            partitions: (0..PARTITIONS).map(|_| Run::default()).collect(),
            memory: memory.reservation(),
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
        group: &Vector,
        codes: Codes<'_>,
        user: &Vector,
        rows: usize,
        local: &mut Local,
    ) -> Result<bool> {
        let state = slot.get_or_init(|| match codes {
            Codes::Loose => None,
            Codes::Signed => Some(Self::new(None)),
            Codes::Dictionary(_, dictionary) => Some(Self::new(Some(Arc::clone(dictionary)))),
        });
        let Some(state) = state else { return Ok(false) };
        let reader = match (&state.dictionary, codes) {
            (None, Codes::Signed) => GroupReader::Signed(SignedReader::new(group)),
            (Some(held), Codes::Dictionary(codes, dictionary)) if Arc::ptr_eq(held, dictionary) => {
                // Checked for the whole run here rather than once a row, so that the read below is a
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
                    .any(|(row, &code)| code as usize >= width && !group.is_null_at(row));
                if loose {
                    return Err(Error::internal("a stable dictionary code is out of range"));
                }
                GroupReader::Dictionary(codes)
            }
            _ => {
                return Err(Error::internal(
                    "a grouped distinct exchange received two group code spaces",
                ));
            }
        };
        let before = local.partitions.iter().map(Run::footprint).sum::<usize>();
        let shift = u32::BITS - PARTITIONS.ilog2();
        let all_valid = !group.validity().has_nulls(rows) && !user.validity().has_nulls(rows);
        if all_valid {
            // Neither column has a null, so the layout is the only thing that changes between rows
            // and it is picked once here rather than once a row. See `SignedReader`.
            let user = SignedReader::new(user);
            for row in 0..rows {
                scatter(&mut local.partitions, shift, reader.at(row), true, user.at(row) as i64);
            }
        } else {
            for row in 0..rows {
                if user.is_null_at(row) {
                    continue;
                }
                let user = i64::try_from(user.signed_at(row).ok_or_else(|| {
                    Error::internal("a distinct BIGINT value has no signed representation")
                })?)
                .map_err(|_| Error::internal("a distinct BIGINT value is out of range"))?;
                let valid = !group.is_null_at(row);
                let group = if valid { reader.at(row) } else { 0 };
                scatter(&mut local.partitions, shift, group, valid, user);
            }
        }
        let after = local.partitions.iter().map(Run::footprint).sum::<usize>();
        local.memory.grow(width(after.saturating_sub(before)))?;
        local.used = true;
        Ok(true)
    }

    fn new(dictionary: Option<Arc<Vector>>) -> Self {
        Self {
            dictionary,
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
    pub(crate) fn finish(&self, bound: usize, memory: &Memory) -> Result<Vec<Chunk>> {
        let input = self
            .partitions
            .iter()
            .map(|partition| partition.lock().map(|held| held.rows()).map_err(poisoned))
            .sum::<Result<usize>>()?;
        // Sixteen thousand rows is worth a thread here, where a plain aggregate asks for sixty five
        // thousand before it takes one. A row costs more on this path: it probes a table that holds
        // a slot per distinct pair, which is most of the way to a slot per row, so the probe misses
        // cache where a plain aggregate's probe into a table of groups usually does not. Measured on
        // the million row ClickBench file, dropping the ask from sixty five thousand to sixteen took
        // twelve percent off the two queries it moves and left the rest where they were, and asking
        // for less than sixteen thousand bought nothing back.
        let degree = input.div_ceil(16_384).clamp(1, PARTITIONS);
        // Either every split or one of it. A split is a vector per pair partition, so there are as
        // many of them as the two counts multiplied, and a query that is going to finish on one
        // thread should not be paying for a hundred vectors to hand itself its own rows. Anything
        // that is worth a second thread is worth the full spread, because the counting pass is
        // skewed by the grouping column in a way the deduplicating pass no longer is.
        let splits = if degree > 1 { PARTITIONS } else { 1 };
        let counted =
            in_parallel(PARTITIONS, degree, "deduplicated the pairs of radix partition", |at| {
                let mut partition = self.partitions[at].lock().map_err(poisoned)?;
                distinct_pairs(&mut partition, splits, memory)
            })?;
        let merged = in_parallel(splits, degree, "counted the groups of split", |at| {
            count_groups(&counted, at, self.dictionary.as_ref(), bound, memory)
        })?;
        // The distinct pairs are read for the last time by the pass above, so the room they took
        // goes back here rather than at the end of the query.
        for part in counted {
            drop(part.held);
        }
        let mut chunks = Vec::new();
        let mut held = self.held.lock().map_err(poisoned)?;
        held.clear();
        for Output { chunks: mut part, held: charge } in merged {
            chunks.append(&mut part);
            held.push(charge);
        }
        Ok(chunks)
    }
}

/// Runs `count` pieces of work across `degree` threads and hands back what they made, in order.
///
/// Both passes have the same shape, so they share this. The pieces are taken off one counter rather
/// than dealt out in advance, because they are not the same size and a thread that draws a cheap one
/// should pick up the next piece instead of finishing early. The calling thread takes a share too.
///
/// `what` only ever reaches an error message, and reads as "nothing <what> 3".
fn in_parallel<T: Send>(
    count: usize,
    degree: usize,
    what: &str,
    run: impl Fn(usize) -> Result<T> + Sync,
) -> Result<Vec<T>> {
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<Result<T>>>> = (0..count).map(|_| Mutex::new(None)).collect();
    let step = || {
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            if at >= count {
                return;
            }
            let done = run(at);
            if let Ok(mut slot) = slots[at].lock() {
                *slot = Some(done);
            }
        }
    };
    std::thread::scope(|scope| {
        let degree = degree.min(count);
        let mut handles = Vec::with_capacity(degree - 1);
        for _ in 1..degree {
            handles.push(scope.spawn(|| {
                step();
                stage::here()
            }));
        }
        step();
        let mut theirs = Spent::none();
        for handle in handles {
            let spent = handle
                .join()
                .map_err(|_| Error::internal("a grouped distinct radix worker panicked"))?;
            theirs.add(spent);
        }
        stage::gained(theirs);
        let mut out = Vec::with_capacity(count);
        for (at, slot) in slots.iter().enumerate() {
            out.push(
                slot.lock()
                    .map_err(poisoned)?
                    .take()
                    .unwrap_or_else(|| Err(Error::internal(format!("nothing {what} {at}"))))?,
            );
        }
        Ok::<_, Error>(out)
    })
}

struct Output {
    chunks: Vec<Chunk>,
    held: Reservation,
}

/// What one pair partition found, split by group hash so the count can take one split each.
struct Counted {
    splits: Vec<Vec<Grouped>>,
    /// What the splits cost, given back when [`Exchange::finish`] has read the last of them.
    held: Reservation,
}

/// The group of one distinct pair, on its way from the partition that found it to its split.
///
/// The hash rides along because the two sides want it once each, to pick the split the group belongs
/// in and then to find the group inside that split, and working it out again on the other side would
/// be the same arithmetic on the same number.
#[derive(Debug, Clone, Copy)]
struct Grouped {
    group: i32,
    group_hash: u32,
    valid: bool,
}

/// Which of `splits` a group belongs to, by the top bits of its hash.
///
/// The top bits, so that the table inside the split still has all the low ones to probe with. It is
/// a multiply rather than the shift the pair partitions use because the number of splits is decided
/// per query and can be one, and a shift that has to throw away all thirty two bits is not a shift
/// Rust will do.
#[inline]
fn split_of(group_hash: u32, splits: usize) -> usize {
    ((u64::from(group_hash) * splits as u64) >> u32::BITS) as usize
}

/// Deduplicates one pair partition and hands over the group of each pair that survived.
fn distinct_pairs(partition: &mut Held, splits: usize, memory: &Memory) -> Result<Counted> {
    let held_rows = partition.rows();
    let pair_capacity = held_rows.saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    working.grow(width(pair_capacity * size_of::<u32>()))?;
    let mut pair_buckets = vec![EMPTY; pair_capacity];
    let pair_mask = pair_capacity - 1;
    let all_valid = partition.runs.iter().all(|run| run.validity.is_empty());
    // The deduplicated pairs used to be compacted into the front of the one run the partition held.
    // There is no one run to compact into now, so they are collected here instead, and this is where
    // they are read from for the rest of the pass. It is one record per distinct pair rather than one
    // per row, which is the same bound the compaction had.
    let mut unique: Vec<Record> = Vec::new();
    let mut unique_validity: Vec<bool> = Vec::new();
    let timing = stage::Timing::start(Stage::Fold);
    for run in &partition.runs {
        for (source, &row) in run.rows.iter().enumerate() {
            let valid = all_valid || run.valid_at(source);
            let mut at = row.pair_hash as usize & pair_mask;
            loop {
                let slot = pair_buckets[at];
                if slot == EMPTY {
                    pair_buckets[at] = u32::try_from(unique.len()).map_err(|_| {
                        Error::out_of_memory("a grouped distinct radix partition is too large")
                    })?;
                    unique.push(row);
                    if !all_valid {
                        unique_validity.push(valid);
                    }
                    break;
                }
                let slot = slot as usize;
                let held = unique[slot];
                let held_valid = all_valid || unique_validity[slot];
                if held.pair_hash == row.pair_hash
                    && held.group == row.group
                    && held.user == row.user
                    && held_valid == valid
                {
                    break;
                }
                at = (at + 1) & pair_mask;
            }
        }
    }
    let pairs = unique.len();
    // The rows themselves are not read again, only the distinct pairs, so give the memory back
    // before the group pass rather than at the end of the query.
    partition.runs.clear();
    working.grow(width(
        unique.capacity() * size_of::<Record>() + unique_validity.capacity() * size_of::<bool>(),
    ))?;
    let partition = Run { rows: unique, validity: unique_validity };

    // Every distinct pair is one for its group to count, and the group goes to the split its hash
    // picks so that the pass below finds all of a group's pairs together. The user is not carried
    // over because nothing after this asks which user it was, only how many there were.
    //
    // Counting the groups here first, and leaving the pass below only the partial counts to add up,
    // was tried and is the wrong trade. It collapses a partition's pairs down to its groups, which
    // is worth a pass when a group has hundreds of pairs and is worth nothing when it has one, and
    // the second kind is `GROUP BY SearchPhrase`, where there are nearly as many phrases as there
    // are pairs. Doing it in both places cost ten percent there and bought two percent on the
    // lopsided queries it was meant for, because the pass below probes a table with one row per
    // group and a query with few enough groups to skew has a table small enough to sit in cache.
    //
    // Each split is asked for an even share of the pairs up front and not left to double its way
    // there. A hash spreads the groups evenly enough that the guess is close, and the alternative is
    // every one of the vectors reallocating five or six times on a pass whose whole job is to move
    // twelve bytes a pair.
    let even = pairs.div_ceil(splits);
    let share = (even + even.isqrt() * 4).min(pairs);
    let mut parts: Vec<Vec<Grouped>> = (0..splits).map(|_| Vec::with_capacity(share)).collect();
    for row in 0..pairs {
        let record = partition.rows[row];
        let valid = all_valid || partition.validity[row];
        let group_hash = group_hash(record, valid);
        parts[split_of(group_hash, splits)].push(Grouped {
            group: record.group,
            group_hash,
            valid,
        });
    }
    let mut held = memory.reservation();
    held.grow(width(
        parts.iter().map(|split| split.capacity() * size_of::<Grouped>()).sum::<usize>(),
    ))?;
    timing.stop(0);
    Ok(Counted { splits: parts, held })
}

/// Adds up one split's groups across every pair partition and takes the best `bound` of them.
///
/// Every pair of a group lands in the same split, because the split is picked by the group hash, so
/// nothing here has to agree with any other split about a count and the top rows it picks are final.
///
/// The table is made once at the size the input can need, rather than started small and doubled,
/// because the number of pairs coming in is known before any of them are read and a group cannot
/// appear more often than that.
fn count_groups(
    counted: &[Counted],
    split: usize,
    dictionary: Option<&Arc<Vector>>,
    bound: usize,
    memory: &Memory,
) -> Result<Output> {
    let timing = stage::Timing::start(Stage::Fold);
    let input = counted.iter().map(|part| part.splits[split].len()).sum::<usize>();
    let capacity = input.saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    working.grow(width(capacity * size_of::<u32>()))?;
    let mut buckets = vec![EMPTY; capacity];
    let mask = capacity - 1;
    let mut groups: Vec<Grouped> = Vec::new();
    let mut counts: Vec<i64> = Vec::new();
    for part in counted {
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
                    break;
                }
                let slot = slot as usize;
                if groups[slot].group_hash == pair.group_hash
                    && groups[slot].group == pair.group
                    && groups[slot].valid == pair.valid
                {
                    counts[slot] = counts[slot]
                        .checked_add(1)
                        .ok_or_else(|| Error::out_of_range("COUNT(DISTINCT BIGINT) overflowed"))?;
                    break;
                }
                at = (at + 1) & mask;
            }
        }
    }
    working.grow(width(
        groups.capacity() * size_of::<Grouped>() + counts.capacity() * size_of::<i64>(),
    ))?;
    timing.stop(0);

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
        let found = groups[slot];
        // The code goes back to being the string it stood for here and nowhere earlier, so what is
        // copied is one string per group that reached the bound rather than one per row.
        let group = match (found.valid, dictionary) {
            (false, _) => Value::Null,
            (true, None) => Value::Integer(found.group),
            (true, Some(dictionary)) => dictionary.try_value_at(found.group as usize)?,
        };
        output.push(vec![group, Value::BigInt(counts[slot])]);
    }
    let key = match dictionary {
        Some(_) => LogicalType::Varchar,
        None => LogicalType::Integer,
    };
    let mut held = memory.reservation();
    let chunks = rows::chunks(&[key, LogicalType::BigInt], &output, &mut held)?;
    timing.stop(0);
    Ok(Output { chunks, held })
}

/// One row into the radix partition its pair hash picks.
///
/// The pair and not the group, because the work the partitions are there to spread is deduplicating
/// pairs and a grouping column is allowed to be as lopsided as it likes. Grouping on the pair leaves
/// a group's pairs in several partitions, which is what the second pass in [`Exchange::finish`] is
/// for.
///
/// Pulled out of [`Exchange::buffer`] so that the loop that reads both columns where they lie and the
/// loop that asks the vectors a row at a time cannot drift apart on which partition a row belongs in
/// or on what its hash is.
///
/// The shift leaves exactly the bits that index [`PARTITIONS`] of them, so the index is always in
/// range and the bounds check never fires. The partition takes the top bits and the table inside it
/// probes with the low ones, so the bits the partition used are not the bits it then goes without.
#[inline]
fn scatter(partitions: &mut [Run], shift: u32, group: i32, valid: bool, user: i64) {
    let group_word = if valid { i64::from(group) as u64 } else { NOTHING };
    let wide_pair = spread(mix(spread(mix(0, group_word)), user as u64));
    let pair_hash = (wide_pair ^ (wide_pair >> 32)) as u32;
    partitions[(pair_hash >> shift) as usize].push(Record { user, group, pair_hash }, valid);
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

    use super::{Held, Record, Run, count_groups, distinct_pairs};

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
        let rows = finished(&mut partition, None);
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
        let words = ["zero", "one", "two"].map(|word| Value::Varchar(word.to_string()));
        let dictionary =
            Arc::new(Vector::from_values(LogicalType::Varchar, &words).expect("a dictionary"));
        let mut partition = Held { runs: vec![run] };
        let rows = finished(&mut partition, Some(&dictionary));
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
                rows_of(&counted, splits, None),
                [
                    vec![Value::Integer(3), Value::BigInt(3)],
                    vec![Value::Integer(4), Value::BigInt(1)],
                ]
            );
        }
    }

    /// How many splits the tests count over, picked to be neither one nor the sixteen a big query gets.
    const SPLITS: usize = 4;

    /// One partition finished and flattened into rows, sorted so the partition order does not show.
    fn finished(partition: &mut Held, dictionary: Option<&Arc<Vector>>) -> Vec<Vec<Value>> {
        let counted = vec![
            distinct_pairs(partition, SPLITS, &Memory::unlimited()).expect("a pair partition"),
        ];
        rows_of(&counted, SPLITS, dictionary)
    }

    /// Every split merged and flattened into rows, sorted so the split order does not show.
    fn rows_of(
        counted: &[super::Counted],
        splits: usize,
        dictionary: Option<&Arc<Vector>>,
    ) -> Vec<Vec<Value>> {
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for split in 0..splits {
            let output = count_groups(counted, split, dictionary, 10, &Memory::unlimited())
                .expect("a grouped distinct split");
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

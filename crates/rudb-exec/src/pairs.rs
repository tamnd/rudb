//! Radix partitioned deduplication of the pairs behind a grouped `COUNT(DISTINCT)`.
//!
//! A grouped distinct count is a grouping in its own right, over the pair of the group key and the
//! value being counted, and the expensive part of it is throwing the duplicate pairs away. That is
//! what this does, and it is all it does: the counting of the groups afterwards belongs to whoever
//! asked, because a plain grouped distinct wants a count and a row, and a mixed aggregate wants the
//! count folded into a group state it is already keeping.
//!
//! The rows are partitioned on the hash of the pair and not of the group. A grouping column is
//! allowed to be as lopsided as it likes, so partitioning the deduplication on the group hands one
//! thread every pair of the biggest group and no number of threads fixes it, because one group
//! cannot be split. On the million row ClickBench file one region holds eighteen percent of the
//! distinct pairs where an even share of sixteen partitions is six and a quarter.
//!
//! What that costs is that a group's pairs end up in several partitions, so whoever counts them has
//! to put the group back together. [`Counted`] is the shape that hands over, one list per split of
//! the group hash, so that the counting pass can take a split each and find all of a group's pairs
//! in the one it took.

use std::mem::size_of;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rudb_common::{Error, Memory, Reservation, Result, Spent, Stage, stage};

use rudb_pipeline::Lease;

use crate::key::{mix, spread};

/// How many radix partitions the pairs are spread over.
pub(crate) const PARTITIONS: usize = 16;

/// A pair bucket nobody has written to yet.
const EMPTY: u32 = u32::MAX;

/// What stands in for the group of a row whose group key is null.
///
/// Null groups together with null and with nothing else, so it needs a word of its own rather than
/// the zero the record carries, which a real group is allowed to be.
const NOTHING: u64 = 0x9e37_79b9_7f4a_7c15;

/// One row on its way to the partition its pair hash picks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Record {
    pub(crate) user: i64,
    pub(crate) group: i32,
    pub(crate) pair_hash: u32,
}

/// The hash of a group on its own, which is what the counting pass groups by.
pub(crate) fn group_hash(group: i32, valid: bool) -> u32 {
    let word = if valid { i64::from(group) as u64 } else { NOTHING };
    let wide = spread(mix(0, word));
    (wide ^ (wide >> 32)) as u32
}

/// One instance's rows for one radix partition.
#[derive(Debug, Default)]
pub(crate) struct Run {
    pub(crate) rows: Vec<Record>,
    /// Empty while every group key in this run is valid.
    pub(crate) validity: Vec<bool>,
}

impl Run {
    pub(crate) fn push(&mut self, row: Record, valid: bool) {
        self.rows.push(row);
        if self.validity.is_empty() {
            if valid {
                // No invalid key has reached this run yet, so there is nothing for a vector to say.
                return;
            }
            // The first invalid key, so the vector starts here and records that every row before
            // this one was valid. The length is taken after the push and one is taken off it, so
            // that a run whose very first row is the invalid one still gets a vector rather than
            // being left with an empty one that reads as all valid.
            self.validity = vec![true; self.rows.len() - 1];
        }
        self.validity.push(valid);
    }

    fn valid_at(&self, row: usize) -> bool {
        self.validity.is_empty() || self.validity[row]
    }

    pub(crate) fn footprint(&self) -> usize {
        self.rows.capacity() * size_of::<Record>() + self.validity.capacity() * size_of::<bool>()
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
pub(crate) struct Held {
    pub(crate) runs: Vec<Run>,
}

impl Held {
    pub(crate) fn rows(&self) -> usize {
        self.runs.iter().map(|run| run.rows.len()).sum()
    }
}

/// One row into the radix partition its pair hash picks.
///
/// The pair and not the group, for the reason the module documentation gives. Written once here so
/// that a caller's loop that reads both columns where they lie and its loop that asks the vectors a
/// row at a time cannot drift apart on which partition a row belongs in or on what its hash is.
///
/// The shift leaves exactly the bits that index [`PARTITIONS`] of them, so the index is always in
/// range and the bounds check never fires. The partition takes the top bits and the table inside it
/// probes with the low ones, so the bits the partition used are not the bits it then goes without.
#[inline]
pub(crate) fn scatter(partitions: &mut [Run], shift: u32, group: i32, valid: bool, user: i64) {
    let group_word = if valid { i64::from(group) as u64 } else { NOTHING };
    let wide_pair = spread(mix(spread(mix(0, group_word)), user as u64));
    let pair_hash = (wide_pair ^ (wide_pair >> 32)) as u32;
    partitions[(pair_hash >> shift) as usize].push(Record { user, group, pair_hash }, valid);
}

/// The shift that [`scatter`] wants, which is however many bits it takes to index [`PARTITIONS`].
pub(crate) fn shift() -> u32 {
    u32::BITS - PARTITIONS.ilog2()
}

/// Runs `count` pieces of work across the lease's threads and hands back what they made, in order.
///
/// The pieces are taken off one counter rather than dealt out in advance, because they are not the
/// same size and a thread that draws a cheap one should pick up the next piece instead of finishing
/// early. The calling thread takes a share too.
///
/// `what` only ever reaches an error message, and reads as "nothing <what> 3".
///
/// # Errors
///
/// Whatever `run` reported for the first piece that failed, in piece order rather than in the order
/// the threads finished, so that the same input reports the same error.
pub(crate) fn in_parallel<T: Send>(
    threads: &Lease<'_>,
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
    together(threads, degree.min(count), &step)?;
    let mut out = Vec::with_capacity(count);
    for (at, slot) in slots.iter().enumerate() {
        out.push(
            slot.lock()
                .map_err(poisoned)?
                .take()
                .unwrap_or_else(|| Err(Error::internal(format!("nothing {what} {at}"))))?,
        );
    }
    Ok(out)
}

/// Run `work` on the lease's threads and on this one, and collect what the borrowed ones measured.
///
/// Every finishing path an aggregate has looks the same from far enough away. There is a counter, a
/// slot per partition, and a function that takes whichever partition is next until there are none
/// left, so the only thing that differs between them is that function. What they also share is the
/// two things that are easy to get wrong: a borrowed thread's stage clock has to come back as a
/// difference rather than as a whole reading, because a pool worker carries what the queries before
/// this one spent, and a thread that panicked has to fail the query rather than leave a partition
/// looking like it was never reached.
///
/// What a caller gives up by using the lease is a finish wider than the lease. A pipeline's lease
/// is sized by the morsels its source has, so a scan of one morsel leases one thread and finishes
/// its aggregate on one thread even when the machine has thirty two and the aggregate has a hundred
/// thousand groups. That is the right answer for a session that asked for one thread and a
/// pessimistic one for a small table with a large group by, and the fix when it matters is for the
/// lease to be sized by the whole pipeline rather than by its source. On ClickBench it does not
/// come up, because a native scan of a hundred thousand rows already has five morsels.
///
/// # Errors
///
/// When a borrowed thread panicked, which is a bug in this engine rather than anything a query can
/// ask for.
pub(crate) fn together(threads: &Lease<'_>, degree: usize, work: &(dyn Fn() + Sync)) -> Result<()> {
    let theirs = Mutex::new(Spent::none());
    let task = || {
        let before = stage::here();
        work();
        let mine = stage::here().since(before);
        if let Ok(mut held) = theirs.lock() {
            held.add(mine);
        }
    };
    // The thread that asked takes partitions too rather than waiting on the ones it woke, for the
    // reason the parallel driver gives for doing the same. Its own reading needs no difference and
    // no adding, because it is already the clock the instrumentation shim reads.
    let (_, panicked) = threads.scatter_at_most(degree, &task, work);
    stage::gained(theirs.into_inner().map_err(poisoned)?);
    if panicked {
        return Err(Error::internal("a thread finishing an aggregate panicked"));
    }
    Ok(())
}

/// What one pair partition found, split by group hash so the count can take one split each.
#[derive(Debug)]
pub(crate) struct Counted {
    pub(crate) splits: Vec<Vec<Grouped>>,
    /// What the splits cost, given back when the counting pass has read the last of them.
    pub(crate) held: Reservation,
}

/// The group of one distinct pair, on its way from the partition that found it to its split.
///
/// The hash rides along because the two sides want it once each, to pick the split the group belongs
/// in and then to find the group inside that split, and working it out again on the other side would
/// be the same arithmetic on the same number.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Grouped {
    pub(crate) group: i32,
    pub(crate) group_hash: u32,
    pub(crate) valid: bool,
}

/// Which of `splits` a group belongs to, by the top bits of its hash.
///
/// The top bits, so that the table inside the split still has all the low ones to probe with. It is
/// a multiply rather than the shift the pair partitions use because the number of splits is decided
/// per query and can be one, and a shift that has to throw away all thirty two bits is not a shift
/// Rust will do.
#[inline]
pub(crate) fn split_of(group_hash: u32, splits: usize) -> usize {
    ((u64::from(group_hash) * splits as u64) >> u32::BITS) as usize
}

/// Deduplicates one pair partition and hands over the group of each pair that survived.
pub(crate) fn distinct_pairs(
    partition: &mut Held,
    splits: usize,
    memory: &Memory,
) -> Result<Counted> {
    let held_rows = partition.rows();
    let pair_capacity = held_rows.saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    working.grow(width(pair_capacity * size_of::<u32>()))?;
    let mut pair_buckets = vec![EMPTY; pair_capacity];
    let pair_mask = pair_capacity - 1;
    // A bucket says which pair it stands for as well as where that pair is, so that a probe landing
    // on somebody else's pair can tell from the bucket alone. The low bits are the index into
    // `unique` and the high bits are the part of the hash the bucket's own position did not already
    // fix, which is the only part of it worth comparing. Reading the record instead is a second
    // random load into a run of about a megabyte, and on ClickBench 8 nine rows in ten are a pair
    // nobody has seen before, so that load was paid on nearly every row to be told what the bucket
    // could have said.
    //
    // No real bucket reads as `EMPTY`, because that needs every index bit set and the table is twice
    // the rows it can hold, so the largest index there can be is below half of it. That also makes
    // the conversion here the only place a partition too large to index has to be caught.
    let index_mask = u32::try_from(pair_mask)
        .map_err(|_| Error::out_of_memory("a radix pair partition is too large"))?;
    let tag_mask = !index_mask;
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
            let tag = row.pair_hash & tag_mask;
            let mut at = row.pair_hash as usize & pair_mask;
            loop {
                let slot = pair_buckets[at];
                if slot == EMPTY {
                    pair_buckets[at] = tag | unique.len() as u32;
                    unique.push(row);
                    if !all_valid {
                        unique_validity.push(valid);
                    }
                    break;
                }
                if slot & tag_mask == tag {
                    // The group and the user are what the hash was taken of, so two rows that agree
                    // on both agree on the whole of it. The tag above is a filter and this is the
                    // answer, which is why the hash itself is not compared here at all.
                    let held_at = (slot & index_mask) as usize;
                    let held = unique[held_at];
                    let held_valid = all_valid || unique_validity[held_at];
                    if held.group == row.group && held.user == row.user && held_valid == valid {
                        break;
                    }
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
    // picks so that the counting pass finds all of a group's pairs together. The user is not carried
    // over because nothing after this asks which user it was, only how many there were.
    //
    // Counting the groups here first, and leaving the pass after only the partial counts to add up,
    // was tried and is the wrong trade. It collapses a partition's pairs down to its groups, which
    // is worth a pass when a group has hundreds of pairs and is worth nothing when it has one, and
    // the second kind is `GROUP BY SearchPhrase`, where there are nearly as many phrases as there
    // are pairs. Doing it in both places cost ten percent there and bought two percent on the
    // lopsided queries it was meant for, because the pass after probes a table with one row per
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
        let group_hash = group_hash(record.group, valid);
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

fn width(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a radix pair lock was poisoned")
}

#[cfg(test)]
mod tests {
    use super::{Held, Record, Run, distinct_pairs};

    /// A run whose very first row has a null key still says so.
    ///
    /// The vector that records validity only exists once a run has seen an invalid key, and it used
    /// to be sized from the rows already there, which is none of them when the invalid key is the
    /// first one. That left the run reading as all valid, and a null group came out as whatever
    /// number stood in for it, which is zero.
    #[test]
    fn a_run_that_opens_with_an_invalid_key_keeps_its_validity() {
        let mut run = Run::default();
        run.push(Record { user: 10, group: 0, pair_hash: 5 }, false);
        run.push(Record { user: 11, group: 4, pair_hash: 5 }, true);
        assert_eq!(run.validity, [false, true]);
        assert!(!run.valid_at(0));
        assert!(run.valid_at(1));
    }

    /// The null group and the group whose key really is zero are two groups, not one.
    #[test]
    fn a_null_key_does_not_join_the_group_whose_key_is_zero() {
        let mut run = Run::default();
        run.push(Record { user: 10, group: 0, pair_hash: 5 }, false);
        run.push(Record { user: 10, group: 0, pair_hash: 5 }, true);
        let mut partition = Held { runs: vec![run] };
        let counted = distinct_pairs(&mut partition, 1, &rudb_common::Memory::unlimited())
            .expect("a pair partition");
        let mut found: Vec<bool> = counted.splits[0].iter().map(|pair| pair.valid).collect();
        found.sort_unstable();
        assert_eq!(found, [false, true]);
    }
}

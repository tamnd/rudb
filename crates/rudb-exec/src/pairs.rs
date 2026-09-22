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
//! distinct pairs, which is more than a tenth of them however many partitions there are.
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
pub(crate) const PARTITIONS: usize = 64;

/// Rows one partition is worth, which is what [`used`] divides by.
///
/// The number this wants to be is whatever keeps the table a partition builds inside the cache the
/// thread building it has to itself. A row costs four bytes of bucket at the half load the table
/// keeps and sixteen bytes of record, so sixteen thousand rows is a table of about three hundred and
/// fifty kilobytes, and that is the largest one measured that was still worth having.
///
/// It is also the number the finishing passes ramp on. A partition is one thread's piece of work,
/// so the row count that is worth a partition is the row count that is worth the first few threads.
pub(crate) const ROWS_PER_PARTITION: usize = 16_384;

/// How many rows a finishing pass wants before it asks for a thread beyond the first few.
///
/// See [`finish_degree`].
pub(crate) const ROWS_PER_EXTRA_THREAD: usize = 65_536;

/// How many threads a finishing pass gets for `input` rows of radix partitions.
///
/// Two rules and the larger wins, which is the shape the scan uses to cut morsels and it is the same
/// argument. A small finish wants threads quickly, because a query that takes half a millisecond has
/// no time to ramp, so the first rule gives one per [`ROWS_PER_PARTITION`] up to eight. A large one
/// wants them slowly, and that is the part that is easy to get wrong.
///
/// It was one thread per [`ROWS_PER_PARTITION`] all the way up, which on a million rows asks for
/// sixty two. Nothing showed, because the lease this is capped by was the scan's and the scan cut
/// sixteen morsels, and because the sink's answer to how wide it could finish was being dropped
/// before it reached the lease at all. Both of those are fixed, so the ask is now what arrives.
///
/// What arrives at sixty two is worse than what arrived at sixteen. Measured on the million row
/// ClickBench file, `COUNT(DISTINCT UserID) GROUP BY RegionID` finishing on thirty two threads
/// instead of eight burns sixty eight percent more CPU to produce the same wall clock: the pass
/// probes a table with a slot per distinct pair and it is waiting on memory rather than on
/// arithmetic, so past the machine's memory level parallelism another thread adds a wake, a join and
/// a share of the bandwidth and takes nothing off the critical path. Eight threads finish the pass
/// in 1.858 ms and thirty two in 1.764, for 9.7 ms of CPU against 16.3.
///
/// So the second rule is one thread per [`ROWS_PER_EXTRA_THREAD`], which on a million rows asks for
/// sixteen. Swept over the queries this moves, sixty five thousand is the best or within noise of it
/// everywhere, and the queries that preferred the old number preferred it because the old number was
/// the only thing keeping them off one thread, which the first rule now does instead.
pub(crate) fn finish_degree(input: usize, ceiling: usize) -> usize {
    let quickly = input.div_ceil(ROWS_PER_PARTITION).min(8);
    let slowly = input.div_ceil(ROWS_PER_EXTRA_THREAD);
    quickly.max(slowly).clamp(1, ceiling)
}

/// How many pieces of work a thread should have to choose from, so that a slow one is absorbed.
///
/// Partitions are dealt off a counter rather than handed out in advance, which only helps when there
/// are more of them than there are threads. One each and a thread that draws the partition holding a
/// popular group finishes long after the rest, with nobody able to take any of it.
const PIECES_PER_THREAD: usize = 4;

/// How many of the [`PARTITIONS`] the pairs were scattered into are worth deduplicating separately.
///
/// The scatter has to pick a fan out before it has seen a row, so it picks the largest one any query
/// wants. That is the wrong number for a small query: a partition costs a table, a reservation and a
/// vector per split whatever is in it, and a query whose pairs would fit in one partition pays for
/// sixty four of all of those. Measured on the million row ClickBench file, going from sixteen
/// partitions to sixty four took eleven percent off `COUNT(DISTINCT UserID)` over every row and put
/// eight percent back on the same query behind a filter that leaves a tenth of them.
///
/// So the finishing pass picks the fan out instead, once it knows how many rows there really are,
/// and the partitions it does not want are merged into the ones it does. Merging is free because a
/// partition is the list of runs the instances handed it rather than one run of its own, so several
/// partitions become one by appending three pointers. The partition index is the top bits of the
/// pair hash and the table inside probes with the low ones, so merging adjacent partitions is the
/// same thing as having scattered on fewer top bits to begin with.
///
/// Two things want a say and the larger of them wins. The table wants to stay inside the cache, which
/// asks for a partition per [`ROWS_PER_PARTITION`] rows. The threads want something to take when they
/// run out, which asks for [`PIECES_PER_THREAD`] partitions each, and it is the one that binds on a
/// query too small for the first to ask for anything. A query finishing on one thread has nothing to
/// balance and is left with whatever the cache asked for, which for a small one is a single
/// partition.
pub(crate) fn used(rows: usize, degree: usize) -> usize {
    let cache = rows.div_ceil(ROWS_PER_PARTITION);
    let balance = if degree > 1 { degree.saturating_mul(PIECES_PER_THREAD) } else { 1 };
    cache.max(balance).max(1).next_power_of_two().min(PARTITIONS)
}

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

/// The hash of a group before it is folded to thirty two bits, which is where both hashes start.
///
/// A row of a mixed aggregate wants two hashes, the group's own to pick the owner that holds its
/// numeric state and the pair's to pick the partition that deduplicates it, and the second is the
/// first with the counted value mixed into it. Handing the wide form over lets that caller pay the
/// two multiplies underneath once a row rather than twice.
#[inline]
pub(crate) fn group_seed(group: i32, valid: bool) -> u64 {
    let word = if valid { i64::from(group) as u64 } else { NOTHING };
    spread(mix(0, word))
}

/// A wide hash cut to thirty two bits with the ones it loses folded into the ones it keeps.
#[inline]
pub(crate) fn folded(wide: u64) -> u32 {
    (wide ^ (wide >> 32)) as u32
}

/// The hash of a group on its own, which is what the counting pass groups by.
pub(crate) fn group_hash(group: i32, valid: bool) -> u32 {
    folded(group_seed(group, valid))
}

/// One instance's rows for one radix partition.
#[derive(Debug, Default)]
pub(crate) struct Run {
    pub(crate) rows: Vec<Record>,
    /// Empty while every group key in this run is valid.
    pub(crate) validity: Vec<bool>,
}

impl Run {
    #[inline]
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

/// The scatter partitions one piece of the finishing pass takes, when [`used`] wants fewer of them.
///
/// Both counts are powers of two, so every piece covers the same number of them and the whole set is
/// covered exactly once.
pub(crate) fn merged(at: usize, used: usize) -> std::ops::Range<usize> {
    let per = PARTITIONS / used;
    (at * per)..((at + 1) * per)
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
    scatter_seeded(partitions, shift, group_seed(group, valid), group, valid, user);
}

/// The same scatter for a caller that already asked [`group_seed`] about this row's group.
#[inline]
pub(crate) fn scatter_seeded(
    partitions: &mut [Run],
    shift: u32,
    seed: u64,
    group: i32,
    valid: bool,
    user: i64,
) {
    let pair_hash = folded(spread(mix(seed, user as u64)));
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
///
/// Comparing two of these is what a probe into a split's group table does on every pair it is
/// handed, and it compares all three fields because the hash is what rules a bucket out cheaply and
/// the other two are what prove it in. Deriving that rather than writing it out at each probe is
/// worth a load: the table is a `Vec`, so a probe that names the record three times indexes it three
/// times, and three bounds checks and three loads of the same twelve bytes become one of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    let reserving = stage::Timing::start(Stage::Reserve);
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
    //
    // That bound is also what it is asked for up front. A vector that doubles its way from nothing
    // to n copies about n on the way, and this pass is bandwidth bound rather than thread bound: on
    // ClickBench 8 the finishing half does not get any quicker between eight threads and sixteen,
    // and nine pairs in ten there are new, so the doubling was moving nearly as many bytes as the
    // loop itself was. Asking once costs no more memory than the doubling reached anyway, since a
    // vector that has just grown holds up to twice what is in it, and it is given back below as soon
    // as the real count is known.
    let mut unique: Vec<Record> = Vec::with_capacity(held_rows);
    let mut unique_validity: Vec<bool> =
        if all_valid { Vec::new() } else { Vec::with_capacity(held_rows) };
    working.grow(width(
        unique.capacity() * size_of::<Record>() + unique_validity.capacity() * size_of::<bool>(),
    ))?;

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
    // The splits are filled by the loop below rather than by a pass over its answer. A pair is
    // written into its split at the moment it turns out to be new, when the record and its validity
    // are both in registers already, and the pass that used to do it read all of `unique` back to
    // learn what the loop that wrote it had just known. On ClickBench 8 that is nine hundred
    // thousand records, sixteen bytes each, read out of memory for the second time to be turned into
    // twelve.
    //
    // Each split is asked for a share of the rows up front and not left to double its way there. A
    // hash spreads the groups evenly enough that the guess is close, and the alternative is every
    // one of the vectors reallocating five or six times on a pass whose whole job is to move twelve
    // bytes a pair. The share is measured against the rows the partition holds rather than against
    // the pairs it will find, since the pairs are not counted yet, and a partition whose rows are
    // mostly duplicates of each other gives the difference back below.
    let even = held_rows.div_ceil(splits);
    let share = (even + even.isqrt() * 4).min(held_rows);
    let mut parts: Vec<Vec<Grouped>> = (0..splits).map(|_| Vec::with_capacity(share)).collect();
    reserving.stop(0);

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
                    let group_hash = group_hash(row.group, valid);
                    let split = split_of(group_hash, splits);
                    parts[split].push(Grouped { group: row.group, group_hash, valid });
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
    timing.stop(0);

    // The rows themselves are not read again, only the distinct pairs, so give the memory back
    // before the group pass rather than at the end of the query.
    let reserving = stage::Timing::start(Stage::Reserve);
    partition.runs.clear();
    drop(unique);
    drop(unique_validity);

    // What the share above guessed too high, given back. A partition whose rows are nearly all new
    // pairs, which on ClickBench 8 is nine in ten of them, keeps what it asked for and copies
    // nothing. One whose rows are mostly repeats of each other is holding room for rows that turned
    // out to be the same pair, and the copy that gives it back is over the few pairs there were
    // rather than over the many rows there were.
    for split in &mut parts {
        if split.capacity() > split.len().saturating_mul(2) {
            split.shrink_to_fit();
        }
    }
    let mut held = memory.reservation();
    held.grow(width(
        parts.iter().map(|split| split.capacity() * size_of::<Grouped>()).sum::<usize>(),
    ))?;
    reserving.stop(0);
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
    use super::{
        Held, PARTITIONS, ROWS_PER_PARTITION, Record, Run, distinct_pairs, folded, group_hash,
        group_seed, merged, scatter, scatter_seeded, shift, used,
    };

    /// The fan out the finishing pass picks, and that what it drops is merged rather than lost.
    ///
    /// Every partition has to end up in exactly one piece of work whatever the row count is, since a
    /// partition that no piece takes is a set of pairs nobody counts.
    #[test]
    fn the_partitions_the_finish_does_not_want_are_merged_into_the_ones_it_does() {
        assert_eq!(used(0, 1), 1);
        assert_eq!(used(1, 1), 1);
        assert_eq!(used(ROWS_PER_PARTITION, 1), 1);
        assert_eq!(used(ROWS_PER_PARTITION + 1, 1), 2);
        assert_eq!(used(999_975, 32), PARTITIONS);
        assert_eq!(used(usize::MAX, 32), PARTITIONS);
        // A query with threads to keep busy takes more pieces than the table size alone asks for.
        assert_eq!(used(ROWS_PER_PARTITION, 4), 16);
        assert_eq!(used(0, 32), PARTITIONS);
        for rows in [0, 1, 40_000, 999_975, usize::MAX] {
            for degree in [1, 2, 4, 32] {
                let taken = used(rows, degree);
                let covered: Vec<usize> = (0..taken).flat_map(|at| merged(at, taken)).collect();
                assert_eq!(covered, (0..PARTITIONS).collect::<Vec<_>>(), "{rows} rows, {degree}");
            }
        }
    }

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

    /// The seeded scatter is the plain one with its first two multiplies handed in.
    ///
    /// Two ways of working out the same partition and the same hash is two ways for a row to end up
    /// somewhere a later pass does not look for it, so they have to agree on every input and not
    /// only on the ones the mixed aggregate happens to send.
    #[test]
    fn scattering_from_a_seed_puts_a_row_where_scattering_from_the_group_would() {
        for group in [i32::MIN, -7, 0, 1, 4096, i32::MAX] {
            for valid in [true, false] {
                assert_eq!(group_hash(group, valid), folded(group_seed(group, valid)));
                for user in [i64::MIN, -1, 0, 99, i64::MAX] {
                    let mut plain: Vec<Run> = (0..PARTITIONS).map(|_| Run::default()).collect();
                    let mut seeded: Vec<Run> = (0..PARTITIONS).map(|_| Run::default()).collect();
                    scatter(&mut plain, shift(), group, valid, user);
                    scatter_seeded(
                        &mut seeded,
                        shift(),
                        group_seed(group, valid),
                        group,
                        valid,
                        user,
                    );
                    let at = |runs: &[Run]| {
                        runs.iter().position(|run| !run.rows.is_empty()).expect("a row landed")
                    };
                    let left = at(&plain);
                    assert_eq!(left, at(&seeded), "{group} {valid} {user}");
                    assert_eq!(plain[left].rows[0].pair_hash, seeded[left].rows[0].pair_hash);
                    assert_eq!(plain[left].validity, seeded[left].validity);
                }
            }
        }
    }
}

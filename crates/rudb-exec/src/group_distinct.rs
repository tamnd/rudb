//! Fixed-width radix ownership for grouped `COUNT(DISTINCT BIGINT)` with a TopN parent.
//!
//! The group key here is four bytes wide whatever the query said it was. An `INTEGER` key already
//! is, and a `VARCHAR` key becomes one when the column arrives with a stable dictionary, because
//! then the code and the string it stands for pick out the same groups and the code is what this
//! can put in a record. That is the whole of why `GROUP BY SearchPhrase` reaches this at all: the
//! dictionary is written once for the column and shared by every chunk of it, so grouping on the
//! code is grouping on the string with none of the payload.

use std::mem::size_of;
use std::sync::{Arc, Mutex, OnceLock};

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Stage, Value, stage};
use rudb_vector::{Chunk, Vector};

use crate::pairs::{
    self, Counted, Grouped, Held, PARTITIONS, Run, distinct_pairs, in_parallel, scatter,
};
use crate::rows;
use crate::signed::SignedReader;

const EMPTY: u32 = u32::MAX;

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
        let shift = pairs::shift();
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

struct Output {
    chunks: Vec<Chunk>,
    held: Reservation,
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

    use super::count_groups;

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
        counted: &[crate::pairs::Counted],
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

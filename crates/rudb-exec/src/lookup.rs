//! The hash table a join finds its gathered rows in.
//!
//! A join and a group by ask a hash table two different questions. A group by asks which slot a key
//! has, one slot per distinct key, and [`crate::table::Table`] is exactly that. A join asks which
//! rows a key has, which is a list, and the two are one structure apart: a table from key to slot,
//! and a chain per slot that threads the rows of that key together. That is what this is. The table
//! does the hashing, the probing and the key comparison, all of it column at a time and a batch at
//! a time, and the chain beside it turns the slot it answers with into the rows the join wanted.
//!
//! The chain is two arrays and no allocation per key. `head[slot]` is the first row of a key and
//! `next[row]` is the row after that one, so a key with a thousand matches costs a thousand `u32`
//! in a run that was allocated once, rather than a `Vec` per distinct key that the allocator has to
//! be asked for and that a probe has to chase a pointer to reach. What this replaces was a
//! `HashMap<Vec<Value>, Vec<usize>>`, which asked the allocator twice per distinct key and once per
//! driving row, hashed a row of tagged values one value at a time, and compared keys the same way.
//!
//! The rows come out of a chain in the order the gathered side holds them, and that is deliberate
//! rather than incidental. The nested loop this replaces produced a driving row's matches in that
//! order, so keeping it makes this a faster way to the same answer instead of the same answer in a
//! different order, and makes a failing test a diff rather than an investigation. A chain that is
//! pushed onto at the front comes out backwards, so the build keeps a tail per slot and appends.
//!
//! # Nulls
//!
//! `NULL = NULL` is null and not true, so a row whose key holds a null in a column the join
//! compares with `=` matches nothing, on either side. Such a row is not put in the table and not
//! looked up in it, which is what says so. `IS NOT DISTINCT FROM` is the other rule for the same
//! value and its nulls are stored and compared, which the table already does, because a group by
//! puts every null in one group and that is the same question.

use rudb_common::{Error, LogicalType, Result};
use rudb_vector::{Form, Vector};

use crate::table::{Across, BATCH, Probe, Table, Walk};

/// The end of a chain, and the row a slot with nothing in it points at.
const NONE: u32 = u32::MAX;

/// What a probe of a row whose key is not in the table leaves behind.
///
/// Distinct from a slot rather than encoded as one, because slot zero is a real key and this has to
/// survive being written into the same run.
pub(crate) const MISS: usize = usize::MAX;

/// The gathered side's rows, by the values the key expressions produce from them.
#[derive(Debug, Default)]
pub(crate) struct Lookup {
    /// One slot per distinct key. Built on the first batch, because that is where the types of the
    /// key columns are, and left empty by a gathered side with no rows in it at all.
    table: Option<Table>,
    /// The first gathered row of each distinct key, by slot.
    head: Vec<u32>,
    /// The last one, by slot, which is what lets a row be appended rather than pushed on the front.
    /// Dropped by [`Self::seal`] when the build is over, because a probe never reads it.
    tail: Vec<u32>,
    /// The next gathered row with the same key, by gathered row, [`NONE`] at the end of a chain.
    next: Vec<u32>,
    /// How many gathered rows are in the table, which is not how many went past it: a row whose key
    /// holds a rejected null is neither stored nor counted.
    kept: usize,
    /// The hash per row of the batch being added, kept between batches.
    hashes: Vec<u64>,
    /// The slot per row of the batch being added, same.
    slots: Vec<usize>,
    /// Whether each row of the batch has a key at all, same.
    keyed: Vec<bool>,
    /// The buffers a batched probe walks with.
    walk: Walk,
}

impl Lookup {
    /// An empty table over a gathered side of `rows` rows.
    ///
    /// The row count is taken up front because the chain is indexed by gathered row and is
    /// allocated once rather than grown, and because it is where the one limit on this gets
    /// checked.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::OutOfMemory`] past [`NONE`] gathered rows, which is the row a
    /// chain uses to say it has ended.
    pub(crate) fn new(rows: usize) -> Result<Self> {
        if rows >= NONE as usize {
            return Err(Error::out_of_memory(format!(
                "a hash join cannot gather more than {} rows on one side",
                NONE - 1
            )));
        }
        Ok(Self { next: vec![NONE; rows], ..Self::default() })
    }

    /// Whether there is anything at all to look up.
    ///
    /// The driving side asks before it evaluates a key expression, so a gathered side with nothing
    /// keyed in it is a join that evaluates nothing on the driving side either. That is not only
    /// the work saved: a key expression that raises on a row is a key expression raising about a
    /// row that could not have matched anything, which the nested loop this replaces never did.
    pub(crate) fn is_empty(&self) -> bool {
        self.kept == 0
    }

    /// What this has taken from the allocator, capacity rather than length throughout.
    pub(crate) fn footprint(&self) -> u64 {
        let table = self.table.as_ref().map_or(0, |table| table.footprint() + table.owned());
        let chain =
            (self.head.capacity() + self.tail.capacity() + self.next.capacity()) * size_of::<u32>();
        let batch = self.hashes.capacity() * size_of::<u64>()
            + self.slots.capacity() * size_of::<usize>()
            + self.keyed.capacity();
        table + u64::try_from(chain + batch).unwrap_or(u64::MAX)
    }

    /// Adds one batch of the gathered side, whose keys are `keys` and whose first row is `base`.
    ///
    /// A batch rather than a row for the reason the whole file exists. The hash is one pass per key
    /// column with the type matched on once, the probe walks [`BATCH`] rows together so that the
    /// misses on a table larger than the cache are all outstanding at once, and the comparison that
    /// settles a bucket is one pass per column as well.
    ///
    /// `nulls` says, per key column, whether a null in it is a value to be stored rather than a row
    /// to be left out. See the module docs.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::OutOfMemory`] when there are more distinct keys than the table can
    /// hold, and whatever building a stored key raises.
    pub(crate) fn add(
        &mut self,
        keys: &[Vector],
        rows: usize,
        base: usize,
        nulls: &[bool],
    ) -> Result<()> {
        if rows == 0 {
            return Ok(());
        }
        let Self { table, head, tail, next, kept, hashes, slots, keyed, walk } = self;
        let table = table.get_or_insert_with(|| {
            let types: Vec<LogicalType> =
                keys.iter().map(|key| key.logical_type().clone()).collect();
            Table::new(&types)
        });
        // Two inputs, always. This side's dictionary and the driving side's are two dictionaries
        // even when the two sides are two scans of one table, so the codes cannot stand in for the
        // values. [`Across`] has the argument.
        crate::table::hash(keys, rows, hashes, Across::TwoInputs);
        which_are_keyed(keys, rows, nulls, keyed);
        slots.clear();
        slots.resize(rows, MISS);
        let mut from = 0;
        while from < rows {
            let upto = (from + BATCH).min(rows);
            table.probe_run(hashes, keys, from, upto, slots, walk);
            // The rows the batch could not settle, in row order, which is the order they have to go
            // in: two rows of one batch can be the first two rows of one key, and the second only
            // finds the first if the first went in before it was asked.
            for &row in walk.pending() {
                if !keyed[row] {
                    continue;
                }
                match table.probe(hashes[row], keys, row) {
                    Probe::Found(slot) => slots[row] = slot,
                    Probe::Vacant(bucket) => {
                        let slot = table.insert(bucket, hashes[row], keys, row)?;
                        debug_assert_eq!(
                            slot,
                            head.len(),
                            "a slot is the number of keys before it"
                        );
                        head.push(NONE);
                        tail.push(NONE);
                        slots[row] = slot;
                    }
                }
            }
            // In row order and after the whole batch has a slot, because the batched pass fills the
            // rows that were already keys and the loop above fills the rest, and a chain that was
            // appended to in that order would hold a key's rows in neither the order they arrived
            // in nor any other one.
            for row in from..upto {
                let slot = slots[row];
                if slot == MISS || !keyed[row] {
                    continue;
                }
                let at = u32::try_from(base + row).map_err(|_| too_many_rows())?;
                if tail[slot] == NONE {
                    head[slot] = at;
                } else {
                    next[tail[slot] as usize] = at;
                }
                tail[slot] = at;
                *kept += 1;
            }
            from = upto;
        }
        Ok(())
    }

    /// Gives back what only the build needed, now that it is over.
    ///
    /// The tail per slot is how a chain is appended to and nothing reads it afterwards, so a join
    /// over a side with ten million distinct keys holds forty megabytes of it for the length of the
    /// probe for no reason at all. The batch buffers go the same way and for the same reason.
    pub(crate) fn seal(&mut self) {
        self.tail = Vec::new();
        self.hashes = Vec::new();
        self.slots = Vec::new();
        self.keyed = Vec::new();
        self.walk = Walk::default();
    }

    /// The slot each driving row's key is in, [`MISS`] where it is in none.
    ///
    /// One call per driving chunk rather than one per driving row, which is the whole point: the
    /// hash is a pass per key column and the probe is a batch at a time, so a chunk of two thousand
    /// rows costs two thousand rows of arithmetic and one set of outstanding cache misses per batch
    /// rather than three dependent misses per row.
    ///
    /// The scratch buffers are the caller's because the caller is one instance of the probe and
    /// this table is shared by all of them.
    pub(crate) fn slots(
        &self,
        keys: &[Vector],
        rows: usize,
        nulls: &[bool],
        scratch: &mut Scratch,
        into: &mut Vec<usize>,
    ) {
        into.clear();
        into.resize(rows, MISS);
        let Some(table) = self.table.as_ref() else { return };
        if rows == 0 {
            return;
        }
        crate::table::hash(keys, rows, &mut scratch.hashes, Across::TwoInputs);
        which_are_keyed(keys, rows, nulls, &mut scratch.keyed);
        let mut from = 0;
        while from < rows {
            let upto = (from + BATCH).min(rows);
            table.probe_run(&scratch.hashes, keys, from, upto, into, &mut scratch.walk);
            from = upto;
        }
        // A row whose key holds a rejected null matches nothing, and the table was never told about
        // that rule. It would answer with a miss anyway, because no key holding such a null was
        // ever stored and the comparison against one that does not is false, but a lookup that is
        // right by two steps of reasoning rather than one is a lookup that stops being right when
        // somebody changes the other step.
        for (row, &keyed) in scratch.keyed.iter().enumerate().take(rows) {
            if !keyed {
                into[row] = MISS;
            }
        }
    }

    /// The gathered rows in one slot's chain, in the order the gathered side holds them.
    ///
    /// `into` is the caller's buffer so that a driving row does not cost an allocation, and it is
    /// cleared here rather than by the caller.
    ///
    /// Row numbers rather than indices, because what the caller does with them is hand them to
    /// [`Build::gather`](crate::side::Build::gather), and a gather takes a run of `u32`. The chain
    /// is a run of `u32` already, so this is a copy rather than a widening.
    pub(crate) fn matches(&self, slot: usize, into: &mut Vec<u32>) {
        into.clear();
        if slot == MISS {
            return;
        }
        let mut at = self.head[slot];
        while at != NONE {
            into.push(at);
            at = self.next[at as usize];
        }
    }
}

/// The buffers one instance of a probe walks a driving chunk with.
///
/// Held by the instance and reused, so a chunk costs the allocator nothing after the first one.
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    hashes: Vec<u64>,
    keyed: Vec<bool>,
    walk: Walk,
}

/// Which rows have a key at all, which is every row until a rejected null says otherwise.
///
/// Column at a time, and only the columns that can reject one, so a join on columns that are not
/// nullable costs a look at each column's mask and no pass over the rows at all.
fn which_are_keyed(keys: &[Vector], rows: usize, nulls: &[bool], keyed: &mut Vec<bool>) {
    keyed.clear();
    keyed.resize(rows, true);
    for (column, &stored) in keys.iter().zip(nulls) {
        if stored || !has_nulls(column, rows) {
            continue;
        }
        for (row, flag) in keyed.iter_mut().enumerate().take(rows) {
            *flag = *flag && !column.is_null_at(row);
        }
    }
}

/// Whether a column could hold a null at all, which is the mask for most forms and not for two.
///
/// A dictionary and a run length vector keep their nulls in the values they point at and are built
/// with every row marked present in the mask beside them, so asking the mask about one of those
/// gets a confident no about a column that is full of nulls. [`Vector::is_null_at`] is the one that
/// reads through, and this is only here to say when the pass that calls it can be skipped.
pub(crate) fn has_nulls(column: &Vector, rows: usize) -> bool {
    match column.form() {
        Form::Dictionary | Form::Rle => true,
        _ => column.validity().has_nulls(rows),
    }
}

/// What a gathered side too long to thread a chain through says.
fn too_many_rows() -> Error {
    Error::out_of_memory(format!(
        "a hash join cannot gather more than {} rows on one side",
        NONE - 1
    ))
}

/// A row of values, which is what the tests below build their key columns out of.
#[cfg(test)]
fn column(values: &[Option<i32>]) -> Vector {
    use rudb_common::Value;
    let values: Vec<Value> =
        values.iter().map(|value| value.map_or(Value::Null, Value::Integer)).collect();
    Vector::from_values(LogicalType::Integer, &values).expect("a column of integers")
}

#[cfg(test)]
mod tests {
    use super::{Lookup, MISS, Scratch, column};

    /// Builds a lookup over one integer key column, one batch, nulls rejected.
    fn built(values: &[Option<i32>]) -> Lookup {
        let mut lookup = Lookup::new(values.len()).expect("a lookup");
        lookup.add(&[column(values)], values.len(), 0, &[false]).expect("a build");
        lookup.seal();
        lookup
    }

    /// What one driving row of the same shape finds, in the order it finds it.
    fn found(lookup: &Lookup, values: &[Option<i32>]) -> Vec<Vec<u32>> {
        let mut scratch = Scratch::default();
        let mut slots = Vec::new();
        lookup.slots(&[column(values)], values.len(), &[false], &mut scratch, &mut slots);
        let mut chain = Vec::new();
        slots
            .iter()
            .map(|&slot| {
                lookup.matches(slot, &mut chain);
                chain.clone()
            })
            .collect()
    }

    #[test]
    fn a_key_with_no_rows_is_a_miss_and_a_key_with_one_is_that_row() {
        let lookup = built(&[Some(10), Some(20)]);
        assert_eq!(
            found(&lookup, &[Some(20), Some(30), Some(10)]),
            vec![vec![1], Vec::new(), vec![0]]
        );
    }

    /// The order a chain comes out in is the order the gathered side holds the rows, which is what
    /// the nested loop this replaces produced and what keeps a failing test a diff.
    #[test]
    fn a_keys_rows_come_out_in_the_order_the_gathered_side_holds_them() {
        let lookup = built(&[Some(7), Some(9), Some(7), Some(7), Some(9)]);
        assert_eq!(found(&lookup, &[Some(7), Some(9)]), vec![vec![0, 2, 3], vec![1, 4]]);
    }

    /// Two rows of one key in one batch, where the second only finds the first if the first went in
    /// before it was asked. A batch is settled together, so this is the case that says the rows the
    /// batch could not settle are finished in row order.
    #[test]
    fn a_key_first_seen_twice_inside_one_batch_is_one_key() {
        let lookup = built(&[Some(4), Some(4)]);
        assert_eq!(found(&lookup, &[Some(4)]), vec![vec![0, 1]]);
    }

    /// Past one batch, so that the chain is appended to across several of them and the rows of a key
    /// that spans two batches stay in order.
    #[test]
    fn a_chain_that_spans_several_batches_stays_in_order() {
        let values: Vec<Option<i32>> = (0..500).map(|row| Some(row % 3)).collect();
        let lookup = built(&values);
        let mut chain = Vec::new();
        let mut scratch = Scratch::default();
        let mut slots = Vec::new();
        lookup.slots(&[column(&[Some(1)])], 1, &[false], &mut scratch, &mut slots);
        lookup.matches(slots[0], &mut chain);
        let wanted: Vec<u32> = (0..500).filter(|row| row % 3 == 1).collect();
        assert_eq!(chain, wanted);
    }

    /// `NULL = NULL` is null and not true, so a null key is not stored and not looked up.
    #[test]
    fn a_rejected_null_is_neither_stored_nor_found() {
        let lookup = built(&[Some(1), None, Some(2)]);
        let mut scratch = Scratch::default();
        let mut slots = Vec::new();
        let driving = [Some(1), None];
        lookup.slots(&[column(&driving)], 2, &[false], &mut scratch, &mut slots);
        assert_ne!(slots[0], MISS, "a driving row with a key finds it");
        assert_eq!(slots[1], MISS, "a driving row whose key is null finds nothing");
    }

    /// `IS NOT DISTINCT FROM` is the other rule for the same value, and the table has always been
    /// able to hold it because a group by puts every null in one group.
    #[test]
    fn a_null_a_join_calls_a_value_is_stored_and_found() {
        let values = [Some(1), None, None];
        let mut lookup = Lookup::new(values.len()).expect("a lookup");
        lookup.add(&[column(&values)], values.len(), 0, &[true]).expect("a build");
        lookup.seal();
        let mut scratch = Scratch::default();
        let mut slots = Vec::new();
        let driving = [None];
        lookup.slots(&[column(&driving)], 1, &[true], &mut scratch, &mut slots);
        let mut chain = Vec::new();
        lookup.matches(slots[0], &mut chain);
        assert_eq!(chain, vec![1, 2]);
    }

    #[test]
    fn a_gathered_side_with_nothing_keyed_in_it_is_empty() {
        assert!(Lookup::new(0).expect("a lookup").is_empty());
        assert!(built(&[None, None]).is_empty(), "every row's key was a rejected null");
        assert!(!built(&[Some(1)]).is_empty());
    }

    /// The build holds a tail per slot and the probe does not, and a join over ten million distinct
    /// keys would otherwise carry forty megabytes of it for the length of the probe.
    #[test]
    fn sealing_gives_back_what_only_the_build_needed() {
        let mut lookup = Lookup::new(4).expect("a lookup");
        lookup
            .add(&[column(&[Some(1), Some(2), Some(1), Some(3)])], 4, 0, &[false])
            .expect("built");
        let before = lookup.footprint();
        lookup.seal();
        assert!(lookup.footprint() < before, "{} is not less than {before}", lookup.footprint());
        assert_eq!(found(&lookup, &[Some(1)]), vec![vec![0, 2]], "and it still answers");
    }
}

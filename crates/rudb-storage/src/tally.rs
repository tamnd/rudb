//! How many rows hold each value of a column, for the columns that hold few enough values.
//!
//! The sketch next door answers how many distinct values a column has. This answers how many rows
//! each of them takes, which is the question an equality filter and a `GROUP BY` are, and it is one
//! a table in memory could not answer at all. `Rows::exact_frequencies` returned `None` for every
//! in memory table, so `SELECT count(*) FROM t GROUP BY flag` over a table built by `CREATE TABLE
//! AS SELECT` built a hash table over every row to find the three groups the column has, and a
//! `WHERE flag = 2` over the same table estimated a fifth of the rows where the answer was written
//! down. A file has held this since the native format got a frequency synopsis. Memory had nothing.
//!
//! # Complete or absent, and nothing in between
//!
//! A file keeps the leading values of a wide column and a bound on the rest, because its writer
//! sees every row before it writes anything and can afford a heavy hitter pass. This cannot. A
//! chunk arrives, is counted, and is never looked at again, and the counters a streaming heavy
//! hitter pass ends with are lower bounds rather than counts. A lower bound is the wrong shape for
//! this: every reader of a frequency list here reads the counts as exact, because that is what
//! makes them an answer instead of an estimate.
//!
//! So the rule is all of the values or none of them. A column gets counted until it reaches
//! [`TALLY_VALUES`] distinct values and then gives up for good, dropping what it had. A column of
//! six flags is counted exactly and forever. A column of a million URLs costs five hundred and
//! twelve insertions in its first chunk and nothing after that.
//!
//! That is the half worth having. The column a grouping or an equality filter would otherwise walk
//! every row to answer is the narrow one, and the wide one has a distinct count from the sketch
//! already.
//!
//! # What it costs, which is less than nothing for the column it is for
//!
//! Nothing that a hash costs, because it does not compute one. [`count::walk`] already hashes every
//! value for the sketch and hands the hash here on its way past.
//!
//! It does not cost the sketch either, because while a column is being counted here the sketch does
//! not see it at all. A tally that is still counting is holding every distinct value of the column,
//! so it is the exact distinct count and the sketch beside it would only arrive at the same number
//! more slowly. The moment this gives up it hands over the hashes it had, and the sketch takes them
//! and then every row after them, so the count on the other side of the cap is the count it would
//! have reached had it been reading all along.
//!
//! That turns out to be the whole performance argument. A bottom-k sketch of a column narrower than
//! its k is a probe into a sixty four kilobyte table a row, because nothing is ever above the
//! threshold until the table fills and a narrow column never fills it. The slots here start at one
//! kilobyte, so forty narrow columns counted at once stay in the cache the row loop is already
//! using where forty sketches do not, and a load of forty such columns came out a little faster
//! with this in it than without.
//!
//! The lookup is an array index and not a hash map, and that part does matter. A `HashMap<u64, _>`
//! hashes the key it is given, so a tally behind one would pay a second hash a row on top of the
//! one the pass already paid, and measured over two million rows of twenty narrow columns that was
//! a load half again as long. What is here instead is an array of slots indexed by the low bits of
//! the hash the caller already has, never more than half of them taken, so a lookup is an index, a
//! comparison and usually nothing else. The array doubles from sixty four slots up to twice the
//! cap.
//!
//! The memory is bounded by the cap: five hundred and twelve values a column and sixteen kilobytes
//! of slots at the widest, and for a string column the values are the strings. A column that would
//! hold more values than the cap holds nothing at all, and the slots go with them.
//!
//! # The null
//!
//! Not here. The tally counts values and a null is not one, the same way the sketch beside it does
//! not count one. A reader that wants the null in the list adds it from the exact null count the
//! zone maps already keep, which is `MemoryTable::null_count`, and that is where the two halves are
//! put together.
//!
//! [`count::walk`]: crate::count

use std::cmp::Reverse;

use rudb_common::Value;

/// How many distinct values one column may hold before this stops counting it.
///
/// The five hundred and twelve the native writer's synopsis keeps, so that a narrow column answers
/// the same questions before a checkpoint and after one. A table that answered a `GROUP BY` from
/// its own counts and then stopped answering it the moment it was written to a file would be a
/// performance cliff nobody could see coming.
pub const TALLY_VALUES: usize = 512;

/// How many slots the values are looked up through at the widest.
///
/// Twice the cap and a power of two, which is what makes the lookup an index and a comparison.
/// Twice, so that half the slots are empty however full the tally is and a walk from the slot a
/// hash lands on reaches an empty one almost at once. A power of two, so that landing on one is a
/// mask.
const MOST_SLOTS: usize = 1024;

/// How many slots a column starts with.
///
/// Sixty four, so that a column of a handful of values reads one kilobyte a row rather than
/// sixteen. The slots double from here the moment the values fill half of them, so a column that
/// turns out to be wide reaches [`MOST_SLOTS`] after four doublings of an array nobody has read
/// yet.
const FIRST_SLOTS: usize = 64;

/// One slot: the hash that landed on it and how many rows have arrived under that hash.
///
/// A count of no rows is the empty slot, which is why nothing calls [`Tally::add`] with none. That
/// saves a third field and a second comparison, and a value really held by no rows is a value the
/// list is right to leave out.
///
/// The hash is here and the value is not, because this is the whole of the hot path: a row of a
/// column still being counted is one sixteen byte load, one comparison and one add, and the value
/// is only needed when the hash is new. That matters for a string column above all, where the value
/// is a string and building one a row is what a load cannot afford.
type Slot = (u64, u64);

/// How many rows hold each value of one column, while there are few enough values to hold them all.
#[derive(Debug, Clone, Default)]
pub struct Tally {
    /// The counts, at the slot the low bits of each hash land on, and empty until the first value.
    ///
    /// Open addressed, so a hash whose slot is taken by another walks forward to the next one. That
    /// walk always ends, because the values never fill more than half the slots and an empty one is
    /// therefore always ahead of it.
    slots: Vec<Slot>,
    /// The hash and the value of each distinct value, in the order they first arrived.
    ///
    /// Beside the counts rather than with them, because the counts are read a row at a time and
    /// these are read once when somebody asks for the list. Two values that collide in sixty four
    /// bits would be counted as one, which is the risk the exact distinct count beside this already
    /// takes for the same reason: at five hundred and twelve values the chance of a pair colliding
    /// is about one in ten to the fourteen.
    held: Vec<(u64, Value)>,
    /// The hashes this was holding when it gave up, waiting for the sketch to take them.
    ///
    /// Empty at every other moment, including before the first value and after the caller has taken
    /// them. While a column is counted here the sketch is not reading it, so this is the handover
    /// that keeps the distinct count on the other side of the cap the one it would have had.
    spill: Vec<u64>,
    /// Set by the value that would have been one past the cap, and never cleared.
    full: bool,
}

impl Tally {
    /// A tally holding nothing, which is a complete list of no values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `rows` rows hold one value, read only if the tally has not seen its hash.
    ///
    /// `true` when this counted it, which also means the value is in the list and the caller's
    /// sketch does not need to see it. `false` when it did not, and then the caller owes the hash
    /// to the sketch along with anything [`Tally::spilled`] hands back.
    ///
    /// `value` is a closure because every fast path in `count.rs` has a hash and no `Value`, and
    /// building one for a row whose value is already in the list is the work this exists to skip. A
    /// million rows of a six value string column build six strings.
    ///
    /// A value of `Value::Null` gives the column up. The callers do not pass one, because a null is
    /// not a value either half of this pass counts, and a list with a count filed under a null in
    /// it would read as a complete list that is missing rows.
    ///
    /// The value kept is the first one that arrived under its hash. `MemoryTable::append` refuses a
    /// chunk whose column types are not the table's, so every value of one column is one type and
    /// there is no second spelling of a value for the first to be kept instead of.
    #[inline]
    pub fn add(&mut self, hash: u64, rows: u64, value: impl FnOnce() -> Value) -> bool {
        if self.full || rows == 0 {
            return false;
        }
        self.count(hash, rows, value)
    }

    /// The body of [`Tally::add`], for a column that is still being counted.
    ///
    /// Split off so that the flag above it is the whole of what a column that gave up costs. A wide
    /// column gives up in its first chunk and then takes every row of the rest of the table past
    /// this point, so what is left in the row loop for it is a load and a branch that always goes
    /// the same way, rather than a call into a lookup it will never use.
    fn count(&mut self, hash: u64, rows: u64, value: impl FnOnce() -> Value) -> bool {
        if self.slots.is_empty() {
            // Not in the constructor, because a table is built with one of these per column and a
            // column that never gets a row should not cost a page for it.
            self.slots = vec![(0, 0); FIRST_SLOTS];
        }
        let mask = self.slots.len() - 1;
        let mut at = (hash as usize) & mask;
        loop {
            let Some(slot) = self.slots.get_mut(at) else { return false };
            if slot.1 == 0 {
                break;
            }
            if slot.0 == hash {
                slot.1 = slot.1.saturating_add(rows);
                return true;
            }
            at = (at + 1) & mask;
        }
        if self.held.len() >= TALLY_VALUES {
            self.give_up();
            return false;
        }
        let value = value();
        if matches!(value, Value::Null) {
            self.give_up();
            return false;
        }
        self.held.push((hash, value));
        self.slots[at] = (hash, rows);
        if self.held.len() * 2 >= self.slots.len() {
            self.widen();
        }
        true
    }

    /// Doubles the slots and puts back what was in them, once the values have filled half of them.
    ///
    /// Half is the line because the walk in [`Tally::add`] ends at the first empty slot, and a
    /// table fuller than that turns a lookup from one comparison into several. At the widest this
    /// does nothing, which is safe because the cap is half of [`MOST_SLOTS`].
    ///
    /// Each hash is still in its slot, so nothing is hashed again and the values are not read at
    /// all.
    fn widen(&mut self) {
        let wider = (self.slots.len() * 2).min(MOST_SLOTS);
        if wider <= self.slots.len() {
            return;
        }
        let mask = wider - 1;
        let narrow = std::mem::replace(&mut self.slots, vec![(0, 0); wider]);
        for (hash, rows) in narrow {
            if rows == 0 {
                continue;
            }
            let mut at = (hash as usize) & mask;
            while self.slots[at].1 != 0 {
                at = (at + 1) & mask;
            }
            self.slots[at] = (hash, rows);
        }
    }

    /// How many rows arrived under one hash, which is where the count of a value it names is.
    fn rows_of(&self, hash: u64) -> u64 {
        if self.slots.is_empty() {
            return 0;
        }
        let mask = self.slots.len() - 1;
        let mut at = (hash as usize) & mask;
        for _ in 0..self.slots.len() {
            let Some(slot) = self.slots.get(at) else { return 0 };
            if slot.1 == 0 {
                return 0;
            }
            if slot.0 == hash {
                return slot.1;
            }
            at = (at + 1) & mask;
        }
        0
    }

    /// Stops counting this column for good, leaving the hashes for the sketch to take.
    ///
    /// The counts are dropped rather than kept, because a partial list is not a shorter answer to
    /// the question this answers. Every reader of it reads the counts as exact and the list as
    /// whole, so a list with a value missing would report that no rows hold that value.
    ///
    /// The hashes are another matter. The sketch has not been reading this column, so these are the
    /// only record that those values were ever here, and dropping them would leave the distinct
    /// count short by up to the cap for the rest of the table's life. A caller that reaches for
    /// this rather than [`Tally::forget`] owes [`Tally::spilled`] to its sketch right afterwards.
    pub fn give_up(&mut self) {
        let spill: Vec<u64> = self.held.iter().map(|(hash, _)| *hash).collect();
        self.forget();
        self.spill = spill;
    }

    /// Stops counting this column for good and drops everything, the hashes with it.
    ///
    /// For a caller that is throwing the sketch away too, which is what a chunk this module could
    /// not walk does. Nothing is owed to a counter that is not counting.
    pub fn forget(&mut self) {
        self.full = true;
        self.slots = Vec::new();
        self.held = Vec::new();
        self.spill = Vec::new();
    }

    /// The hashes this was holding when it gave up, handed over once and then gone.
    ///
    /// `None` at every other moment, which is every call but the one right after the cap was
    /// passed. The caller adds them to its sketch, in any order, because a sketch does not have
    /// one.
    #[inline]
    pub fn spilled(&mut self) -> Option<Vec<u64>> {
        (!self.spill.is_empty()).then(|| std::mem::take(&mut self.spill))
    }

    /// Every value of the column with the rows holding it, most common first.
    ///
    /// `None` when the column gave up, which is the only way this is ever incomplete.
    ///
    /// The sort is stable, so two values holding the same number of rows come back in the order
    /// they first arrived rather than in whatever order a hash map happened to be walked in. A plan
    /// that prints one way on one run and another way on the next is a plan nobody can write a test
    /// against.
    #[must_use]
    pub fn list(&self) -> Option<Vec<(Value, u64)>> {
        if self.full {
            return None;
        }
        let mut held: Vec<(Value, u64)> =
            self.held.iter().map(|(hash, value)| (value.clone(), self.rows_of(*hash))).collect();
        held.sort_by_key(|(_, rows)| Reverse(*rows));
        Some(held)
    }

    /// Whether this is still counting, which is whether the sketch beside it has anything to do.
    ///
    /// Asked once a chunk a column rather than once a row, because the answer only ever changes the
    /// one way and a column that gave up in its first chunk is otherwise reading this a hundred
    /// million times to hear the same thing.
    #[must_use]
    pub fn counting(&self) -> bool {
        !self.full
    }

    /// How many distinct values are in the list, without building it.
    ///
    /// `None` when the column gave up. For a caller deciding whether the list is worth copying.
    #[must_use]
    pub fn values(&self) -> Option<usize> {
        (!self.full).then_some(self.held.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash of a small integer, the way `count.rs` would have hashed it.
    fn hashed(value: i64) -> u64 {
        crate::count::hash_value(&Value::BigInt(value)).expect("a bigint has a rule")
    }

    /// Adds `rows` rows of one integer.
    fn add(tally: &mut Tally, value: i64, rows: u64) {
        tally.add(hashed(value), rows, || Value::BigInt(value));
    }

    #[test]
    fn a_narrow_column_is_counted_exactly_and_comes_back_most_common_first() {
        let mut tally = Tally::new();
        add(&mut tally, 7, 10);
        add(&mut tally, 8, 5);
        add(&mut tally, 7, 90);
        assert_eq!(tally.list(), Some(vec![(Value::BigInt(7), 100), (Value::BigInt(8), 5)]));
        assert_eq!(tally.values(), Some(2));
    }

    #[test]
    fn two_values_with_the_same_count_come_back_in_the_order_they_arrived() {
        // Not in hash order, which is the order a map would have walked them in and the order that
        // changes between two runs over the same rows.
        let mut tally = Tally::new();
        for value in [3, 1, 2] {
            add(&mut tally, value, 4);
        }
        let held = tally.list().expect("nothing was left out");
        let values: Vec<i64> = held
            .iter()
            .map(|(value, _)| match value {
                Value::BigInt(held) => *held,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(values, [3, 1, 2]);
    }

    #[test]
    fn a_column_one_value_past_the_cap_answers_nothing_rather_than_answering_short() {
        // The 513th value is what stops it, and what it had already counted goes with it. A list of
        // 512 values out of 513 would say the one it dropped has no rows at all.
        let mut tally = Tally::new();
        for value in 0..i64::try_from(TALLY_VALUES).expect("512 fits") {
            add(&mut tally, value, 1);
        }
        assert_eq!(tally.values(), Some(TALLY_VALUES));
        add(&mut tally, 512, 1);
        assert_eq!(tally.list(), None);
        assert_eq!(tally.values(), None);
        // And it stays given up, including for a value it had already seen.
        add(&mut tally, 0, 1);
        assert_eq!(tally.list(), None);
    }

    #[test]
    fn the_hashes_of_a_column_that_gave_up_are_handed_over_once() {
        // What keeps the distinct count whole across the cap. The sketch reads nothing while the
        // tally is counting, so these are the only record that the first 512 values were here.
        let mut tally = Tally::new();
        for value in 0..i64::try_from(TALLY_VALUES).expect("512 fits") {
            add(&mut tally, value, 1);
        }
        assert!(tally.spilled().is_none(), "nothing has been given up yet");
        add(&mut tally, 512, 1);
        let spilled = tally.spilled().expect("the cap was passed");
        assert_eq!(spilled.len(), TALLY_VALUES);
        assert!(tally.spilled().is_none(), "handed over once and then gone");
    }

    #[test]
    fn a_column_exactly_at_the_cap_still_answers() {
        // The boundary in the other direction. 512 values is a list and 513 is nothing, so the test
        // above and this one are the pair that pins which side the cap falls on.
        let mut tally = Tally::new();
        for value in 0..i64::try_from(TALLY_VALUES).expect("512 fits") {
            add(&mut tally, value, 2);
        }
        let held = tally.list().expect("512 is the cap and not past it");
        assert_eq!(held.len(), TALLY_VALUES);
        assert!(held.iter().all(|(_, rows)| *rows == 2), "{held:?}");
    }

    #[test]
    fn a_count_taken_before_the_slots_doubled_is_still_there_after() {
        // Four doublings between the first value and the cap, and a count that is wrong on the other
        // side of one of them is a count nothing else in here would notice.
        let mut tally = Tally::new();
        add(&mut tally, 1, 7);
        for value in 2..300 {
            add(&mut tally, value, 1);
        }
        add(&mut tally, 1, 3);
        let held = tally.list().expect("299 values is under the cap");
        assert_eq!(held.len(), 299);
        assert_eq!(held[0], (Value::BigInt(1), 10));
        assert!(held[1..].iter().all(|(_, rows)| *rows == 1), "{:?}", &held[1..]);
    }

    #[test]
    fn the_value_is_only_built_for_a_hash_the_tally_has_not_seen() {
        // The argument for the closure. A string column of two values over a thousand rows allocates
        // two strings, and this is the test that fails if the closure is ever called eagerly.
        let mut built = 0;
        let mut tally = Tally::new();
        for row in 0..1000_u64 {
            let value = Value::Varchar(if row % 2 == 0 { "a".into() } else { "b".into() });
            let hash = crate::count::hash_value(&value).expect("a string has a rule");
            tally.add(hash, 1, || {
                built += 1;
                value
            });
        }
        assert_eq!(built, 2);
        let held = tally.list().expect("two values is a list");
        assert_eq!(
            held,
            vec![(Value::Varchar("a".into()), 500), (Value::Varchar("b".into()), 500)]
        );
    }

    #[test]
    fn a_null_gives_the_column_up_rather_than_taking_a_row_of_its_own() {
        // Nothing calls this with a null, because neither the sketch nor this counts one. If
        // something starts to, the list it produces has to stop being called complete, because the
        // rows under the null are rows the readers of this would count twice.
        let mut tally = Tally::new();
        add(&mut tally, 1, 3);
        tally.add(7, 1, || Value::Null);
        assert_eq!(tally.list(), None);
    }
}

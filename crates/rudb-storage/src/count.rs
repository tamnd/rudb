//! How many distinct values every column holds, counted as the rows arrive.
//!
//! The zone map next door answers where a value is not. This answers how many there are, which is
//! the number every cardinality estimate in the optimizer is built on and the one a table in memory
//! has never had. `Rows::distinct_values` answered `None` for an in memory table and always did, so
//! a join over a table built by `CREATE TABLE AS SELECT` was ordered from the default constants
//! however many times it had been queried, and the class histogram this is measured by recorded
//! `Unknown` for every column of it.
//!
//! The counter is [`rudb_encoding::sketch::Sketch`], which is a bottom-k sketch that was written for
//! the encoder's dictionary chooser and until now had no caller outside its own tests. Its module
//! doc has the argument for it over HyperLogLog and the derivation of the estimate. What matters
//! here is the one property that makes it safe to wire into a query answer rather than only into a
//! guess: a sketch that has seen at most k distinct values is holding all of them, so the count it
//! gives back is the count. [`Sketch::is_exact`] is that question and [`Counts::exact`] is what
//! passes it on.
//!
//! So a column with fewer than four thousand distinct values in it gets an exact answer that
//! `COUNT(DISTINCT c)` can be read straight out of, and a column with more gets an estimate at
//! about one and a half percent that only the estimator sees. Two methods and not one, because the
//! two uses are `Stat::answer` and `Stat::decide` and the difference between them is the difference
//! between a slow query and a wrong one.
//!
//! # And how many rows hold each of them, for a narrow column
//!
//! One pass over a column is one hash a value, and the hash is what the pass costs. So the frequency
//! tally in `tally.rs` rides along on this one rather than making a pass of its own: every arm below
//! hands the hash it computed to a `Sink`, which is the sketch and the tally together.
//!
//! Together, but only one of them at a time. The tally holds every distinct value of a column it is
//! still counting, so it is that column's exact distinct count and the sketch is not read at all
//! while it lasts. It gives up at five hundred and twelve values and hands its hashes to the sketch
//! on the way out, and from there the sketch reads the column as it always did. A narrow column
//! therefore costs less than it used to rather than more, because the tally's slots are a kilobyte
//! where the sketch's are sixty four.
//!
//! That is where `Rows::exact_frequencies` for a table in memory comes from. `tally.rs` has the
//! argument for the cap and for why a memory table's list is complete or absent where a file's is
//! allowed to be a prefix.
//!
//! # One value, one hash, whatever form it arrived in
//!
//! The same column arrives flat in one chunk and as a dictionary in the next, and a value counted
//! twice because the two forms hashed it differently is a distinct count that drifts upward the
//! more compressed the data is. So the rule is written once and it is about the value and never
//! about the layout:
//!
//! An integer, a date, a time, a timestamp and a boolean hash as the sixteen bytes of the 128 bit
//! pattern they widen to, signed or zero extended as their own type says. A decimal hashes as its
//! unscaled integer, which is the only part of it that varies inside one column. A float hashes as
//! the bits of the `f64` it widens to, with the negative zero folded onto the positive one and every
//! NaN folded onto one NaN, because SQL says those pairs are equal and a hash that disagreed would
//! report two distinct values where a `GROUP BY` produces one group. A string and a blob hash as
//! their bytes.
//!
//! [`hash_value`] is that rule and every fast path below has to agree with it. The test at the
//! bottom is the one that holds them to it: it feeds one column of the same values in four forms and
//! asserts one answer.
//!
//! # What it costs
//!
//! One hash per value is the worst case and most forms are not the worst case. A constant is one
//! hash for a whole chunk. A run length column is one hash per run. A dictionary column hashes each
//! dictionary entry at most once and then adds a `u64` per row, which is the same argument
//! `zone::coded` makes for reading a dictionary through its codes rather than over its values. A bit
//! packed column is a shift and a mask. A flat signed column whose chunk holds a range no wider than
//! its rows, which is a date, a quantity or a small decimal, adds a slot a row and a hash a value, see
//! `narrow`. Only the other flat columns and a string column pay a hash a row, and
//! [`Sketch::add_hash`] returns on a comparison for every value above the threshold, which after the
//! first few thousand rows is nearly all of them.
//!
//! Measured on server2 over a release build, three loads each, back when the sketch was the only
//! counter on this pass. ClickBench `hits`, a million rows of a hundred and five columns, takes
//! about 4.5 seconds wall, of which 2.4 is statistics and 1.8 of that 2.4 is this. TPC-H
//! `lineitem` at scale factor 1, six million rows of sixteen columns, takes about 3.3 seconds
//! wall, 2.0 statistics and 1.6 of it here. So a distinct count costs roughly three times what the
//! zone map beside it does, which is about what a hash against two comparisons should cost, and
//! both together are around half the load.
//!
//! The tally moved work between the two counters rather than adding a third, so those totals still
//! stand and the split inside the second of them no longer does: a column the tally is counting is
//! a column the sketch never sees, and most columns of both these tables are that kind. What was
//! measured after the tally went in is the total, on four million rows of forty columns with
//! nothing else running on the machine: 3.75 seconds against 3.85 for columns of two to forty one
//! values, and 4.27 against 4.41 for columns of a million. The per counter split on `hits` has not
//! been taken again.
//!
//! The memory is 128 KB a column at the default k, so a hundred and five column table like
//! ClickBench `hits` holds 13 MB of sketch. That is the price of the whole table's statistics and
//! it does not grow with the rows.
//!
//! # A column this cannot read says nothing
//!
//! A form or a type with no arm here, which today is an interval and the nested types, sets the
//! column's `blind` flag and the column answers `None` from then on. Not a wider count and not a
//! narrower one. A distinct count that is missing is a fact the estimator already knows how to
//! handle, because that is what every column of every memory table gave it until now, and a
//! distinct count that is wrong is one it has no defence against at all.

use std::sync::Arc;

use rudb_common::{LogicalType, Value};
use rudb_encoding::sketch::{DEFAULT_K, Sketch, hash64, hash128};
use rudb_vector::{Chunk, Data, Form, Validity, Vector};

use crate::tally::Tally;

/// The distinct count of every column of one table, built as chunks are appended.
#[derive(Debug, Clone)]
pub struct Counts {
    columns: Vec<Column>,
}

/// One column's sketch, and whether anything reached it that this module could not read.
#[derive(Debug, Clone)]
struct Column {
    sketch: Sketch,
    /// How many rows hold each value, while the column has few enough values to say. See `tally.rs`.
    ///
    /// Here rather than in a pass of its own because the hash is what a pass over a column costs and
    /// this one has already paid for it. Every arm below hands the hash it computed to both.
    tally: Tally,
    /// Set by the first chunk this could not walk, and never cleared.
    ///
    /// A sketch that missed some of its rows is not a sketch of the column, and the failure it would
    /// cause is a count that is too low, which is the direction that turns an estimate into a wrong
    /// plan rather than a cautious one. Once it is set the column answers nothing.
    blind: bool,
    /// What the last dictionary this column read has been worked out to hold, kept for the chunks
    /// after it that share the same one.
    entries: Entries,
}

/// The hashes of one dictionary's entries and a count a code, kept across the chunks that share it.
///
/// A Parquet dictionary covers a whole column chunk, which is a few dozen of the chunks a scan hands
/// up, and on `lineitem` a price or a key column has far more entries than a chunk has rows. Making
/// a count an entry and walking it for every chunk cost more than counting the rows did, and each
/// entry a row pointed at was built into a `Value` to be hashed. Now each entry is hashed once for
/// all the chunks that share its dictionary, and only the entries some row pointed at are walked.
#[derive(Debug, Clone, Default)]
struct Entries {
    /// The dictionary these are for, held so that its address stays its own while it is compared.
    values: Option<Arc<Vector>>,
    /// One slot an entry.
    slots: Vec<Entry>,
    /// The entries the chunk being walked pointed at, in the order it first did.
    touched: Vec<u32>,
}

/// What [`Entries`] knows about one entry.
///
/// The count and the hash sit side by side because the dictionary of a Parquet column chunk can be
/// far larger than the cache, and a row that points at an entry then costs a miss to count. Held in
/// two arrays that was a miss for the count and another for the hash when the chunk's entries were
/// walked. Held together the walk finds the line the count already brought in.
#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    hash: u64,
    /// How many rows of the chunk being walked point at the entry, back to zero after the walk. A
    /// chunk's rows are counted in `u32` like its codes are.
    seen: u32,
    /// Whether `hash` has been worked out yet, which is once some row has pointed at the entry.
    hashed: bool,
}

impl Entries {
    /// Ready for a chunk over `values`, starting afresh when that is not the dictionary held.
    fn over(&mut self, values: &Arc<Vector>) {
        if self.values.as_ref().is_some_and(|held| Arc::ptr_eq(held, values)) {
            return;
        }
        self.values = Some(Arc::clone(values));
        self.slots = vec![Entry::default(); values.len()];
        self.touched.clear();
    }
}

impl Column {
    /// A column that has seen nothing yet.
    fn new() -> Self {
        Self {
            // The default k is the only k here, so two of these can always be unioned. The
            // constructor only fails on a k of zero.
            sketch: Sketch::new(DEFAULT_K).unwrap_or_else(|_| Sketch::of(&[])),
            tally: Tally::new(),
            blind: false,
            entries: Entries::default(),
        }
    }

    /// Counts one vector, and gives up on the column if this is a form with no hash rule.
    fn add(&mut self, vector: &Vector) {
        if self.blind {
            return;
        }
        let mut sink = Sink::of(&mut self.sketch, &mut self.tally);
        if !walk(vector, &mut self.entries, &mut sink) {
            self.blind();
        }
    }

    /// Takes in what `later` counted over the rows that came after this column's.
    ///
    /// The column ends as it would have had it read those rows itself. The union of two bottom-k
    /// sketches is the sketch of the union of their values, and the tally takes the later values in
    /// the order they first arrived, so its list comes out in the same order and with the same row
    /// counts. A tally that goes past its cap on the way gives up the way it would have, and every
    /// value either side was holding goes to the sketch.
    fn absorb(&mut self, mut later: Self) {
        if later.blind {
            self.blind();
        }
        if self.blind {
            return;
        }
        let mut stray = Vec::new();
        if let Some(values) = later.tally.list_by_arrival() {
            let mut values = values.into_iter();
            if self.tally.counting() {
                for (hash, rows, value) in values.by_ref() {
                    // A tally only answers `false` with rows in hand when it has given up, which
                    // is the case the rest of this is for.
                    if rows > 0 && !self.tally.add(hash, rows, || value) {
                        stray.push(hash);
                        break;
                    }
                }
            }
            stray.extend(values.map(|(hash, _, _)| hash));
        } else if self.tally.counting() {
            self.tally.give_up();
        }
        stray.extend(self.tally.spilled().unwrap_or_default());
        stray.extend(later.tally.spilled().unwrap_or_default());
        match self.sketch.union(&later.sketch) {
            Ok(union) => self.sketch = union,
            Err(_) => return self.blind(),
        }
        for hash in stray {
            self.sketch.add_hash(hash);
        }
        later.tally.forget();
    }

    /// Gives up on the column, which is what a form with no hash rule leaves behind.
    fn blind(&mut self) {
        self.blind = true;
        // The hashes taken so far describe some of the rows and no question is going to be answered
        // from them, so they are dropped rather than carried. The tally forgets rather than gives
        // up, because there is no sketch left for it to hand anything to.
        self.sketch = Sketch::of(&[]);
        self.tally.forget();
        self.entries = Entries::default();
    }
}

/// One column of a [`Counts`], borrowed apart from the others. See [`Counts::columns_mut`].
#[derive(Debug)]
pub struct Counting<'a> {
    column: &'a mut Column,
}

impl Counting<'_> {
    /// Counts one vector into this column, the same as [`Counts::add_column`] does.
    pub fn add(&mut self, vector: &Vector) {
        self.column.add(vector);
    }

    /// Takes in a count of the rows that came after every row this column has seen.
    ///
    /// Parts have to be absorbed in the order of their rows, the way chunks have to be added in it,
    /// for the tally's list to come out the way reading the rows in order would have left it.
    pub fn absorb(&mut self, part: Partial) {
        self.column.absorb(part.column);
    }
}

/// One column's count over a run of its rows, taken apart from the column so that the runs can be
/// counted on threads of their own and absorbed afterwards. See [`Counting::absorb`].
///
/// A column split this way is split by rows and not by columns, which is the point. One wide string
/// column costs more to count than the rest of `lineitem` together, and counting a column a thread
/// left every other thread waiting on it (#1380).
#[derive(Debug)]
pub struct Partial {
    column: Column,
}

impl Partial {
    /// A count that has seen nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self { column: Column::new() }
    }

    /// Counts one vector, the same as [`Counting::add`] does.
    pub fn add(&mut self, vector: &Vector) {
        self.column.add(vector);
    }
}

impl Default for Partial {
    fn default() -> Self {
        Self::new()
    }
}

impl Counts {
    /// A counter for a table of `width` columns, holding nothing yet.
    #[must_use]
    pub fn new(width: usize) -> Self {
        Self { columns: (0..width).map(|_| Column::new()).collect() }
    }

    /// Counts one chunk, one column at a time.
    ///
    /// `MemoryTable::append` refuses a chunk whose columns are not the table's before it gets here,
    /// so a column this cannot find is a caller that has not done that. It blinds the column rather
    /// than skipping it, for the same reason a form with no arm does: a sketch that missed a chunk
    /// counts too few distinct values and says nothing about having done so.
    pub fn add(&mut self, chunk: &Chunk) {
        for at in 0..self.columns.len() {
            match chunk.column(at) {
                Ok(vector) => self.add_column(at, vector),
                Err(_) => self.blind(at),
            }
        }
    }

    /// Counts one vector into one column, for a caller that has the columns and not the chunk.
    ///
    /// The native writer is that caller. It buffers a stripe as chunks and then hands one column of
    /// all of them to each encode worker, so a worker has a vector and a column number and no chunk
    /// whose column numbering matches this counter's. Going through [`Self::add`] would mean giving
    /// every worker the whole chunk and a counter as wide as the table, and then every worker would
    /// walk every column and throw away all but one of them.
    ///
    /// Out of range is ignored rather than blinding anything, because there is no column to blind.
    pub fn add_column(&mut self, at: usize, vector: &Vector) {
        if let Some(column) = self.columns.get_mut(at) {
            column.add(vector);
        }
    }

    /// Every column on its own, in order, so that each can be counted on a thread of its own.
    ///
    /// The columns share nothing, and a chunk counted a column at a time on several threads leaves
    /// each column exactly as [`Self::add`] would have, as long as each column still sees the chunks
    /// in the order they arrived. `MemoryTable::append_all` is the caller.
    pub fn columns_mut(&mut self) -> Vec<Counting<'_>> {
        self.columns.iter_mut().map(|column| Counting { column }).collect()
    }

    /// Takes in what `later` counted, column by column, as though this had read its rows itself.
    ///
    /// The same rule as [`Counting::absorb`]: the tally lists come out in the order this had read
    /// the rows only when `later` counted the rows that came after these. The native writer counts
    /// a stripe on the thread that encodes it and absorbs the stripes as they reach the writer,
    /// which is the order it counted them in before it did that. A column `later` does not have is
    /// left as it is.
    pub fn absorb(&mut self, later: Counts) {
        for (column, later) in self.columns.iter_mut().zip(later.columns) {
            column.absorb(later);
        }
    }

    /// Gives up on a column, which is what a form with no hash rule leaves behind.
    fn blind(&mut self, at: usize) {
        if let Some(column) = self.columns.get_mut(at) {
            column.blind();
        }
    }

    /// How many distinct non-null values one column holds, when that number is exact.
    ///
    /// `None` when the sketch filled up, which means the column has at least [`DEFAULT_K`] distinct
    /// values and what is left is an estimate. This is the one a query result may be read out of.
    ///
    /// A column still being tallied is answered from the tally, because that is where its values
    /// are. The sketch has not read such a column at all, so asking it would give an empty answer.
    #[must_use]
    pub fn exact(&self, column: usize) -> Option<u64> {
        let held = self.columns.get(column)?;
        if held.blind {
            return None;
        }
        if let Some(values) = held.tally.values() {
            return Some(values as u64);
        }
        held.sketch.is_exact().then(|| held.sketch.len() as u64)
    }

    /// How many distinct non-null values one column holds, exactly or estimated, and which it is.
    ///
    /// The flag is `true` when the answer is exact, which is the same question [`Counts::exact`]
    /// asks and is returned here so a caller building a `Stat` does not have to ask twice.
    #[must_use]
    #[expect(clippy::cast_sign_loss, reason = "a distinct count is never negative")]
    #[expect(clippy::cast_possible_truncation, reason = "a count above u64 is a count of u64")]
    pub fn distinct(&self, column: usize) -> Option<(u64, bool)> {
        let held = self.columns.get(column)?;
        if held.blind {
            return None;
        }
        if let Some(values) = held.tally.values() {
            return Some((values as u64, true));
        }
        if held.sketch.is_exact() {
            return Some((held.sketch.len() as u64, true));
        }
        Some((held.sketch.distinct().round() as u64, false))
    }

    /// Every non-null value of one column with the exact number of rows holding it.
    ///
    /// `None` for a column with more distinct values than `tally.rs` keeps, and for a blind one.
    /// Most common value first. The null is not in here and the caller that wants it adds it from
    /// the null count the zone maps keep, for the reason `tally.rs` gives.
    #[must_use]
    pub fn frequencies(&self, column: usize) -> Option<Vec<(Value, u64)>> {
        let held = self.columns.get(column)?;
        if held.blind {
            return None;
        }
        held.tally.list()
    }

    /// How many distinct values one column's frequency list holds, without building it.
    ///
    /// `None` when there is no list, which is the same question [`Counts::frequencies`] answers and
    /// is asked separately by a caller deciding whether the copy is worth making.
    #[must_use]
    pub fn frequency_values(&self, column: usize) -> Option<usize> {
        let held = self.columns.get(column)?;
        if held.blind {
            return None;
        }
        held.tally.values()
    }

    /// The smallest and the largest non-null value of one column, when the tally holds them all.
    ///
    /// `None` for a column over the cap and for a blind one, the same two cases the list has. What
    /// this is for is the column whose zone maps cannot answer its ends: a string column that
    /// arrived as a dictionary narrower than its chunk gets ends that are a superset of its rows,
    /// because `zone::stringy` reads whichever of the rows and the values is shorter and the values
    /// can hold more than the rows point at. A tally that is still counting has only what the rows
    /// hold, so it answers where the zone gives up.
    #[must_use]
    pub fn extremes(&self, column: usize) -> Option<(Value, Value)> {
        let held = self.columns.get(column)?;
        if held.blind {
            return None;
        }
        held.tally.extremes()
    }

    /// One column's sketch, as a structure that can be written down.
    ///
    /// `None` for a blind column, which is a column whose sketch is missing rows and says nothing
    /// about it. That is the same answer [`Counts::distinct`] gives and for the same reason.
    ///
    /// A column still being tallied is answered by building a sketch out of the tally's hashes,
    /// rather than by handing back the sketch beside it, because that one is empty: while the tally
    /// is counting, it is the tally that has the column's hashes and the sketch has never seen it.
    /// Handing back the empty one would write a column of six values down as a column of none, and
    /// the failure would be invisible because an empty sketch is exact.
    ///
    /// The copy is why this returns a `Sketch` and not a `&Sketch`. It happens once a column at a
    /// checkpoint and never on the append path.
    #[must_use]
    pub fn sketch(&self, column: usize) -> Option<Sketch> {
        let held = self.columns.get(column)?;
        if held.blind {
            return None;
        }
        match held.tally.hashes() {
            Some(hashes) => {
                let mut sketch = Sketch::new(DEFAULT_K).ok()?;
                for hash in hashes {
                    sketch.add_hash(hash);
                }
                Some(sketch)
            }
            None => Some(held.sketch.clone()),
        }
    }

    /// How many columns this is counting.
    #[must_use]
    pub fn width(&self) -> usize {
        self.columns.len()
    }
}

/// Where one column's values go as a chunk is walked.
///
/// The distinct sketch and the frequency tally beside it both want the hash of the value and nothing
/// else about it, so the pass computes it once and hands it on. That is the whole reason the tally
/// is behind this rather than in a pass of its own: a second pass would pay a second hash a row,
/// and the hash is what this pass costs.
///
/// One at a time rather than both at once. A tally that is still counting holds every distinct value
/// of its column, so while it does the sketch has nothing to add and is not touched, and the moment
/// it gives up it hands over what it had and the sketch takes over from there. See `tally.rs` for
/// why that is faster than reading both, which is not the obvious way round.
struct Sink<'a> {
    sketch: &'a mut Sketch,
    tally: &'a mut Tally,
    /// Whether the tally is still counting, read once when this is built rather than once a row.
    ///
    /// A column that gave up in its first chunk is walked by every chunk after it, and what is left
    /// of this for those rows should be one flag in a register and not a look inside the tally.
    counting: bool,
}

impl<'a> Sink<'a> {
    /// The sink for one column of one chunk.
    fn of(sketch: &'a mut Sketch, tally: &'a mut Tally) -> Self {
        Self { counting: tally.counting(), sketch, tally }
    }

    /// One value, however many rows hold it, with the value itself read only when it is new.
    ///
    /// The rows are for the tally alone. A sketch counts a value once however many rows arrived with
    /// it, which is why every arm below that has a run or a dictionary entry can add it once.
    fn add(&mut self, hash: u64, rows: u64, value: impl FnOnce() -> Value) {
        if self.counting {
            if self.tally.add(hash, rows, value) {
                return;
            }
            self.counting = false;
            self.hand_over();
        }
        self.sketch.add_hash(hash);
    }

    /// Gives the tally up, for an arm that counted every value but cannot trust the row counts.
    ///
    /// The distinct count survives what this is called for and the frequency list does not, so the
    /// two halves part company here rather than the column losing both.
    fn stop_counting(&mut self) {
        self.tally.give_up();
        self.counting = false;
        self.hand_over();
    }

    /// Moves whatever the tally was holding into the sketch, once, on the way out of the tally.
    fn hand_over(&mut self) {
        if let Some(spilled) = self.tally.spilled() {
            for held in spilled {
                self.sketch.add_hash(held);
            }
        }
    }
}

/// Adds every non-null value of one vector to the sketch and the tally behind `sink`.
///
/// `false` means a form or a type with no arm here, which is what sets the column's `blind` flag.
/// The caller has to treat a `false` as poisoning the whole column and not as skipping one chunk,
/// because a sketch missing some of its rows counts too few distinct values and says nothing about
/// having done so.
fn walk(vector: &Vector, entries: &mut Entries, sink: &mut Sink<'_>) -> bool {
    match vector.form() {
        // One value repeated, so one hash for however many rows there are. A constant that is null
        // adds nothing, which is right: a distinct count does not count the null.
        Form::Constant => match vector.constant_value() {
            Some(Value::Null) | None => true,
            Some(value) => match hash_value(value) {
                Some(hash) => {
                    // A constant carries its value in the body and its nulls in the mask above it,
                    // like every other form, so the rows holding the value are the valid ones rather
                    // than all of them. Counted only when there are any, because the count is what
                    // the tally needs and the mask of a constant is almost always empty.
                    let validity = vector.validity();
                    let rows = if validity.has_nulls(vector.len()) {
                        (0..vector.len()).filter(|&row| validity.is_valid(row)).count()
                    } else {
                        vector.len()
                    };
                    sink.add(hash, rows as u64, || value.clone());
                    true
                }
                None => false,
            },
        },
        // A start and a step. Every row is a value of its own unless the step is zero, and a null
        // row has no value at all, so this is a hash a row with the nulls dropped.
        Form::Sequence => match vector.sequence_parts() {
            Some((start, step)) => {
                let validity = vector.validity();
                let nullable = validity.has_nulls(vector.len());
                for row in 0..vector.len() {
                    if nullable && !validity.is_valid(row) {
                        continue;
                    }
                    let value = i128::from(start) + i128::from(step) * row as i128;
                    sink.add(hash_signed(value), 1, || vector.value_at(row));
                }
                true
            }
            None => false,
        },
        Form::BitPacked => match vector.packed_parts() {
            Some(packed) => {
                let validity = vector.validity();
                let nullable = validity.has_nulls(vector.len());
                let base = packed.base();
                for row in 0..vector.len() {
                    if nullable && !validity.is_valid(row) {
                        continue;
                    }
                    let value = base + i128::from(packed.code(row));
                    sink.add(hash_signed(value), 1, || vector.value_at(row));
                }
                true
            }
            None => false,
        },
        Form::Dictionary => match vector.shared_dictionary_parts() {
            Some((codes, values)) => coded(vector, codes, values, entries, sink),
            None => false,
        },
        // One hash a run rather than one a row, because every row of a run holds the same value and
        // a sketch counts a value once however many times it arrives. A run over a vector that has
        // nulls of its own is the one case that has to go row by row, since then a run is partly a
        // value and partly nothing and the run boundaries no longer say which.
        Form::Rle => match vector.run_parts() {
            Some((stops, values)) => {
                if vector.validity().has_nulls(vector.len()) {
                    return rows(vector, sink);
                }
                let inner = values.validity();
                // The ends are exclusive and increasing, so the rows of a run are the distance from
                // the one before it, and the tally wants that distance rather than the run's number.
                let mut start = 0_u32;
                for (run, end) in stops.iter().take(values.len()).enumerate() {
                    let stop = (*end).max(start);
                    let held = u64::from(stop - start);
                    start = stop;
                    if !inner.is_valid(run) {
                        continue;
                    }
                    match value_hash(values, run) {
                        Some(hash) => sink.add(hash, held, || values.value_at(run)),
                        None => return false,
                    }
                }
                // The runs are meant to end where the vector does. A distinct count survives them
                // not doing so, because the values of the rows nobody walked are almost certainly
                // values some run already had, but a row count does not: it would come back short
                // and still call itself complete.
                if start as usize != vector.len() {
                    sink.stop_counting();
                }
                true
            }
            None => false,
        },
        Form::Flat => match vector.data() {
            Some(data) => flat(vector, data, sink),
            None => false,
        },
        // Strings that are not flat, read as bytes rather than as values so that a `URL` column is
        // not a heap allocation a row. This is the same reason `zone::text` reads them this way.
        Form::StringView | Form::Fsst => {
            let validity = vector.validity();
            let nullable = validity.has_nulls(vector.len());
            for row in 0..vector.len() {
                if nullable && !validity.is_valid(row) {
                    continue;
                }
                let Some(bytes) = vector.bytes_at(row) else { return false };
                sink.add(hash64(bytes), 1, || vector.value_at(row));
            }
            true
        }
        // A form added since this was written. The column stops answering, which is the failure that
        // is visible in a plan rather than the one that is visible in a wrong row count.
        _ => false,
    }
}

/// A dictionary column, counted once per row and hashed once per dictionary entry.
///
/// The rows are the cheap pass: an add into a slot of `seen` per row, with no hash and no value. The
/// entries are walked afterwards and only the ones some row pointed at, because the dictionary a
/// Parquet reader hands over covers a whole column chunk and can hold far more values than the rows
/// being counted. Hashing it whole would be the slower of the two exactly when the dictionary is
/// doing its job, and adding every entry would count values no row of this table has.
fn coded(
    vector: &Vector,
    codes: &[u32],
    values: &Arc<Vector>,
    entries: &mut Entries,
    sink: &mut Sink<'_>,
) -> bool {
    let validity = vector.validity();
    let nullable = validity.has_nulls(vector.len());
    let inner = values.validity();
    // Asked of the form rather than counted, since the mask covers the whole dictionary.
    let inner_nullable = !matches!(inner, Validity::AllValid);
    entries.over(values);
    for row in 0..vector.len() {
        if nullable && !validity.is_valid(row) {
            continue;
        }
        let Some(&code) = codes.get(row) else { return false };
        // A code pointing at a null is a null row, however the vector's own mask reads, which is the
        // same rule `Vector::is_null_at` follows through a dictionary.
        if inner_nullable && !inner.is_valid(code as usize) {
            continue;
        }
        let Some(slot) = entries.slots.get_mut(code as usize) else { return false };
        if slot.seen == 0 {
            entries.touched.push(code);
        }
        slot.seen += 1;
    }
    let Entries { slots, touched, .. } = entries;
    for code in touched.drain(..) {
        let Some(slot) = slots.get_mut(code as usize) else { return false };
        let held = u64::from(std::mem::take(&mut slot.seen));
        if !slot.hashed {
            match entry_hash(values, code as usize) {
                Some(found) => {
                    slot.hash = found;
                    slot.hashed = true;
                }
                None => return false,
            }
        }
        sink.add(slot.hash, held, || values.value_at(code as usize));
    }
    true
}

/// The hash of one dictionary entry, read from the typed run under it when it has one.
///
/// The same rules [`flat`] follows a row at a time, so an entry hashes as the row holding it would
/// have if the column had arrived flat. Values that are not flat go through [`value_hash`].
fn entry_hash(values: &Vector, at: usize) -> Option<u64> {
    match values.data() {
        Some(Data::Bool(held)) => held.get(at).map(|&v| hash_signed(i128::from(v))),
        Some(Data::Float32(held)) => held.get(at).map(|&v| hash_real(f64::from(v))),
        Some(Data::Float64(held)) => held.get(at).map(|&v| hash_real(v)),
        Some(Data::Varlen(_)) => values.bytes_at(at).map(hash64),
        Some(data) => {
            data.signed_at(at).map(hash_signed).or_else(|| data.unsigned_at(at).map(hash_unsigned))
        }
        None => value_hash(values, at),
    }
}

/// A flat column, one pass over its typed run.
///
/// The type is matched on once for the column rather than once for the value, which is what makes a
/// million rows of `INTEGER` a multiply a row rather than a match and a `Value` a row.
fn flat(vector: &Vector, data: &Data, sink: &mut Sink<'_>) -> bool {
    let validity = vector.validity();
    let nullable = validity.has_nulls(vector.len());
    let rows = vector.len();
    /// One layout, hashed by the rule its element type takes.
    macro_rules! pass {
        ($body:expr) => {{
            for row in 0..rows {
                if nullable && !validity.is_valid(row) {
                    continue;
                }
                match ($body)(row) {
                    Some(hash) => sink.add(hash, 1, || vector.value_at(row)),
                    None => return false,
                }
            }
            true
        }};
    }
    /// Every signed width, widened and hashed as one.
    macro_rules! signed {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(held) => {
                    let held: &[$native] = held;
                    if narrow(held, vector, sink) {
                        return true;
                    }
                    return pass!(|row: usize| held.get(row).map(|&v| hash_signed(i128::from(v))));
                })+
                _ => {}
            }
        };
    }
    /// Every unsigned width, the same way. `UInt128` is why this is not folded into the signed one:
    /// it does not widen into an `i128` and its own bits are the canonical pattern.
    macro_rules! unsigned {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(held) => {
                    let held: &[$native] = held;
                    return pass!(|row: usize| held.get(row).map(|&v| hash_unsigned(u128::from(v))));
                })+
                _ => {}
            }
        };
    }
    rudb_vector::for_each_layout!(signed, signed);
    rudb_vector::for_each_layout!(unsigned, unsigned);
    match data {
        Data::Bool(held) => pass!(|row: usize| held.get(row).map(|&v| hash_signed(i128::from(v)))),
        Data::Float32(held) => pass!(|row: usize| held.get(row).map(|&v| hash_real(f64::from(v)))),
        Data::Float64(held) => pass!(|row: usize| held.get(row).map(|&v| hash_real(v))),
        // A string arrives here as views into the arena. The column is read directly rather than
        // through `Vector::bytes_at`, which asks the form and the validity again for every row.
        Data::Varlen(held) => pass!(|row: usize| held.bytes(row).map(hash64)),
        // An interval and an empty column. Neither has a canonical pattern written down above, and
        // inventing one here rather than in `hash_value` is how the two stop agreeing.
        _ => false,
    }
}

/// A signed column whose values fall in a range no wider than its rows, counted into an array.
///
/// A quantity, a line number, a date or a price with two decimals is a column of this kind in most
/// chunks, and the hash a row is most of what counting it costs. So the rows are counted into a slot
/// per value in the range, and each value that some row holds is hashed once and handed on with its
/// rows. The values go on in the order they first arrived, and the tally keeps its list in that
/// order, so what the sketch and the tally end up holding is the same as a hash a row leaves.
///
/// `false` for a chunk too short or too wide for this, which the caller counts a row at a time.
fn narrow<T: Copy + Ord + Into<i128>>(held: &[T], vector: &Vector, sink: &mut Sink<'_>) -> bool {
    let rows = vector.len().min(held.len());
    let Some(held) = held.get(..rows) else { return false };
    let Some(&start) = held.first() else { return false };
    let (low, high) =
        held.iter().fold((start, start), |(low, high), &value| (low.min(value), high.max(value)));
    let low: i128 = low.into();
    let Some(span) = Into::<i128>::into(high).checked_sub(low) else { return false };
    if rows < NARROW_ROWS || span >= rows as i128 {
        return false;
    }
    let validity = vector.validity();
    let nullable = validity.has_nulls(vector.len());
    #[expect(clippy::cast_sign_loss, reason = "the span is below the rows, which is a usize")]
    let mut seen = vec![0u32; span as usize + 1];
    let mut first = Vec::new();
    for (row, &value) in held.iter().enumerate() {
        if nullable && !validity.is_valid(row) {
            continue;
        }
        #[expect(clippy::cast_sign_loss, reason = "every value is at least the lowest")]
        let Some(slot) = seen.get_mut((value.into() - low) as usize) else { return false };
        if *slot == 0 {
            first.push(row);
        }
        *slot += 1;
    }
    for row in first {
        // Every row in `first` was read out of `held` above, so this always finds it. A `false`
        // here would have the caller count again the values already handed on.
        let Some(&value) = held.get(row) else { continue };
        let value: i128 = value.into();
        #[expect(clippy::cast_sign_loss, reason = "every value is at least the lowest")]
        let rows = seen.get((value - low) as usize).copied().unwrap_or(0);
        sink.add(hash_signed(value), u64::from(rows), || vector.value_at(row));
    }
    true
}

/// The fewest rows a chunk has for [`narrow`] to count it, below which the array is not worth it.
const NARROW_ROWS: usize = 64;

/// One value of a vector, read as a `Value` and hashed by the rule.
///
/// The slow path, taken once per dictionary entry and once per run, which is why it is allowed to
/// build a `Value`. A column that reaches this once a row is a column with no fast path, and those
/// are the ones [`walk`] refuses rather than walks.
fn value_hash(values: &Vector, at: usize) -> Option<u64> {
    hash_value(&values.value_at(at))
}

/// Every non-null value of a vector, read a row at a time.
///
/// For a run length column that carries nulls in the vector above the runs, where a run no longer
/// says what a row holds.
fn rows(vector: &Vector, sink: &mut Sink<'_>) -> bool {
    // row at a time: a run that the vector above it has nulls in no longer says what a row holds,
    // so the runs cannot be walked and there is no typed slice under them to walk instead.
    for row in 0..vector.len() {
        if vector.is_null_at(row) {
            continue;
        }
        let value = vector.value_at(row);
        match hash_value(&value) {
            Some(hash) => sink.add(hash, 1, || value),
            None => return false,
        }
    }
    true
}

/// The hash of one value, which is the rule the module doc states and every fast path repeats.
///
/// `None` is a type with no rule, and it poisons the column rather than being skipped, because a
/// value nobody counted is a distinct count that is too low.
#[must_use]
pub fn hash_value(value: &Value) -> Option<u64> {
    Some(match value {
        // A null is not a distinct value, so a caller that reaches this with one has already gone
        // wrong. Hashing it as nothing rather than refusing keeps that from poisoning a column.
        Value::Null => return None,
        Value::Boolean(v) => hash_signed(i128::from(*v)),
        Value::TinyInt(v) => hash_signed(i128::from(*v)),
        Value::SmallInt(v) => hash_signed(i128::from(*v)),
        Value::Integer(v) | Value::Date(v) => hash_signed(i128::from(*v)),
        Value::BigInt(v)
        | Value::Time(v)
        | Value::TimeTz(v)
        | Value::Timestamp(v)
        | Value::TimestampTz(v) => hash_signed(i128::from(*v)),
        Value::HugeInt(v) => hash_signed(*v),
        Value::UTinyInt(v) => hash_unsigned(u128::from(*v)),
        Value::USmallInt(v) => hash_unsigned(u128::from(*v)),
        Value::UInteger(v) => hash_unsigned(u128::from(*v)),
        Value::UBigInt(v) => hash_unsigned(u128::from(*v)),
        Value::UHugeInt(v) => hash_unsigned(*v),
        Value::Float(v) => hash_real(f64::from(*v)),
        Value::Double(v) => hash_real(*v),
        // The unscaled integer alone, because the scale is a property of the column and every value
        // in one column carries the same one.
        Value::Decimal { unscaled, .. } => hash_signed(*unscaled),
        Value::Varchar(v) => hash64(v.as_bytes()),
        Value::Blob(v) => hash64(v),
        // An interval and the nested types. Nothing here counts them.
        _ => return None,
    })
}

/// A signed value as the sixteen bytes of its 128 bit pattern.
fn hash_signed(value: i128) -> u64 {
    hash128(value as u128)
}

/// An unsigned value the same way, which for every value below 2^127 is the same sixteen bytes a
/// signed one of the same number gives.
fn hash_unsigned(value: u128) -> u64 {
    hash128(value)
}

/// A float as the bits of the `f64` it widens to, with the two values SQL calls equal folded.
///
/// Negative zero onto positive zero, and every NaN onto one NaN. Without the folding a column
/// holding `0.0` and `-0.0` would count two distinct values where a `GROUP BY` over it produces one
/// group, and a column of NaNs would count as many distinct values as it has distinct bit patterns.
fn hash_real(value: f64) -> u64 {
    let folded = if value.is_nan() {
        f64::NAN
    } else if value == 0.0 {
        0.0
    } else {
        value
    };
    hash64(&folded.to_bits().to_le_bytes())
}

/// Whether this type has a rule above, asked of a type rather than of a value.
///
/// For a caller that wants to know before it builds anything, and for the test that holds the two
/// lists together.
#[must_use]
pub fn countable(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::Boolean
            | LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::HugeInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt
            | LogicalType::UHugeInt
            | LogicalType::Float
            | LogicalType::Double
            | LogicalType::Decimal { .. }
            | LogicalType::Varchar
            | LogicalType::Blob
            | LogicalType::Date
            | LogicalType::Time
            | LogicalType::TimeTz
            | LogicalType::Timestamp
            | LogicalType::TimestampTz
    )
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_encoding::sketch::{DEFAULT_K, Sketch};
    use rudb_vector::{Chunk, Form, Vector};

    use super::{Counts, Partial, countable, hash_value};
    use crate::tally::TALLY_VALUES;

    /// A flat `INTEGER` vector of `values`.
    fn flat(values: &[i32]) -> Vector {
        let held: Vec<Value> = values.iter().map(|n| Value::Integer(*n)).collect();
        Vector::from_values(LogicalType::Integer, &held).expect("a column")
    }

    /// What one vector counts, on its own, as a table of one column.
    fn count(vector: Vector) -> Option<(u64, bool)> {
        let mut counts = Counts::new(1);
        counts.add(&Chunk::new(vec![vector]).expect("a chunk"));
        counts.distinct(0)
    }

    /// How many rows the same one column holds of each of its values.
    fn held(vector: Vector) -> Option<Vec<(Value, u64)>> {
        let mut counts = Counts::new(1);
        counts.add(&Chunk::new(vec![vector]).expect("a chunk"));
        counts.frequencies(0)
    }

    /// The sketch of the same one column, for the writer that has to put one on disk.
    fn sketched(vector: Vector) -> Option<Sketch> {
        let mut counts = Counts::new(1);
        counts.add(&Chunk::new(vec![vector]).expect("a chunk"));
        counts.sketch(0)
    }

    #[test]
    fn a_narrow_column_gives_a_sketch_of_its_values_and_not_an_empty_one() {
        // The trap `Counts::sketch` is written around. While the tally is counting, the sketch
        // beside it has never seen the column, so handing that one back would write six values down
        // as none, and an empty sketch is exact and so says nothing about being empty.
        let values: Vec<i32> = (0..600).map(|n| n % 6).collect();
        let sketch = sketched(flat(&values)).expect("a sketch");
        assert!(sketch.is_exact());
        assert_eq!(sketch.len(), 6, "six values, however many rows hold them");
        #[expect(clippy::float_cmp, reason = "an exact sketch answers its own length")]
        {
            assert_eq!(sketch.distinct(), 6.0);
        }
    }

    #[test]
    fn a_column_past_the_tally_gives_the_sketch_that_took_it_over() {
        let values: Vec<i32> = (0..(TALLY_VALUES as i32 * 4)).collect();
        let sketch = sketched(flat(&values)).expect("a sketch");
        assert_eq!(
            sketch.len() as u64,
            count(flat(&values)).expect("a count").0,
            "the sketch a writer stores is the one the count came out of"
        );
    }

    #[test]
    fn a_blind_column_has_no_sketch_to_store() {
        // The same answer `distinct` gives, and for the same reason: a sketch missing rows counts
        // too few distinct values and says nothing about having done so, and storing one would make
        // that permanent.
        let held = [Value::Interval { months: 1, days: 0, micros: 0 }];
        let vector = Vector::from_values(LogicalType::Interval, &held).expect("a column");
        assert_eq!(sketched(vector), None);
    }

    /// A column narrow enough to be counted into an array leaves what a hash a row leaves.
    ///
    /// The same rows are counted as one chunk, which is long enough for the array, and as chunks of
    /// forty, which are not. The list has to come out in the same order with the same counts, which
    /// is the part the array could get wrong, and that includes a range wide enough for the tally to
    /// give up partway through a chunk.
    #[test]
    fn a_narrow_range_counted_into_an_array_matches_a_hash_a_row() {
        for (values, wide) in [(41_i64, false), (900, true)] {
            let held: Vec<Value> =
                (0..1000_i64)
                    .map(|n| {
                        if n % 7 == 3 {
                            Value::Null
                        } else {
                            Value::BigInt((n * 7919) % values - 20)
                        }
                    })
                    .collect();
            let mut whole = Counts::new(1);
            let vector = Vector::from_values(LogicalType::BigInt, &held).expect("a column");
            assert_eq!(vector.form(), Form::Flat);
            whole.add(&Chunk::new(vec![vector]).expect("a chunk"));
            let mut pieces = Counts::new(1);
            for part in held.chunks(40) {
                let vector = Vector::from_values(LogicalType::BigInt, part).expect("a column");
                pieces.add(&Chunk::new(vec![vector]).expect("a chunk"));
            }
            assert_eq!(whole.distinct(0), pieces.distinct(0));
            assert_eq!(whole.frequencies(0), pieces.frequencies(0));
            assert_eq!(whole.frequencies(0).is_none(), wide, "the tally gave up where it should");
            assert_eq!(whole.sketch(0), pieces.sketch(0));
        }
    }

    /// The property the module doc promises: the form is how the rows are written down and the
    /// count is about the values, so the five ways of writing the same column down agree.
    #[test]
    fn one_column_in_five_forms_gives_one_answer() {
        // Three hundred rows holding a hundred distinct values, three in a row each, which is a
        // shape all five forms can hold: the runs are real runs and the range packs into seven bits.
        let values: Vec<i32> = (0..300).map(|n| n / 3).collect();

        let flat = flat(&values);
        assert_eq!(flat.form(), Form::Flat);
        assert_eq!(count(flat), Some((100, true)));

        let packed = self::flat(&values).bit_packed().expect("a packing");
        assert_eq!(packed.form(), Form::BitPacked, "the test is not testing what it says");
        assert_eq!(count(packed), Some((100, true)));

        let runs = self::flat(&values).run_encoded().expect("a run encoding");
        assert_eq!(runs.form(), Form::Rle, "the test is not testing what it says");
        assert_eq!(count(runs), Some((100, true)));

        let codes: Vec<u32> = values.iter().map(|n| *n as u32).collect();
        let entries: Vec<i32> = (0..100).collect();
        let coded = Vector::dictionary(codes, self::flat(&entries)).expect("a dictionary");
        assert_eq!(coded.form(), Form::Dictionary);
        assert_eq!(count(coded), Some((100, true)));

        // And the two forms that hold no values at all. A sequence of a hundred steps holds a
        // hundred of them and a constant holds one however long it is.
        let sequence = Vector::sequence(0, 1, 100);
        assert_eq!(sequence.form(), Form::Sequence);
        assert_eq!(count(sequence), Some((100, true)));

        let one = Vector::constant(LogicalType::Integer, Value::Integer(7), 300);
        assert_eq!(one.form(), Form::Constant);
        assert_eq!(count(one), Some((1, true)));
    }

    /// The same property asked of the row counts, which are the half a wrong answer can be read out
    /// of. A form that counted a run once instead of once a row would report a third of the rows.
    #[test]
    fn one_column_in_five_forms_gives_one_set_of_row_counts() {
        let values: Vec<i32> = (0..300).map(|n| n / 3).collect();
        let three: Vec<(Value, u64)> = (0..100).map(|n| (Value::Integer(n), 3)).collect();

        assert_eq!(held(flat(&values)), Some(three.clone()));
        assert_eq!(held(flat(&values).bit_packed().expect("a packing")), Some(three.clone()));
        assert_eq!(held(flat(&values).run_encoded().expect("a run")), Some(three.clone()));

        let codes: Vec<u32> = values.iter().map(|n| *n as u32).collect();
        let entries: Vec<i32> = (0..100).collect();
        let coded = Vector::dictionary(codes, flat(&entries)).expect("a dictionary");
        assert_eq!(held(coded), Some(three));

        // A sequence is a row each and a constant is every row at once, so these two are where a
        // count that came from the form rather than from the rows would show up first. A sequence is
        // a `BIGINT` column and its values come back as its own type rather than as the type of the
        // column above, which is the same thing `Vector::value_at` would have said about it.
        let one: Vec<(Value, u64)> = (0..100).map(|n| (Value::BigInt(n), 1)).collect();
        assert_eq!(held(Vector::sequence(0, 1, 100)), Some(one));
        let repeated = Vector::constant(LogicalType::Integer, Value::Integer(7), 300);
        assert_eq!(held(repeated), Some(vec![(Value::Integer(7), 300)]));
    }

    /// Chunks that share one dictionary longer than any of them count as the same rows laid flat.
    ///
    /// The case the entries kept across chunks are for: the second chunk finds its hashes worked out
    /// by the first, and neither may carry the other's counts or touch an entry no row points at.
    #[test]
    fn chunks_sharing_a_long_dictionary_count_as_their_rows_do() {
        let tally = |chunks: &[Vector]| {
            let mut counts = Counts::new(1);
            for chunk in chunks {
                counts.add(&Chunk::new(vec![chunk.clone()]).expect("a chunk"));
            }
            (counts.distinct(0), counts.frequencies(0))
        };
        let entries: Vec<i32> = (0..1000).map(|n| n * 7).collect();
        let shared = std::sync::Arc::new(flat(&entries));
        let first: Vec<u32> = vec![5, 9, 5, 700, 9, 5];
        let second: Vec<u32> = vec![9, 42, 700, 42, 3];
        let coded: Vec<Vector> = [&first, &second]
            .iter()
            .map(|codes| {
                Vector::dictionary_over((*codes).clone(), std::sync::Arc::clone(&shared))
                    .expect("a dictionary")
            })
            .collect();
        let laid: Vec<Vector> = [&first, &second]
            .iter()
            .map(|codes| {
                flat(&codes.iter().map(|&code| entries[code as usize]).collect::<Vec<_>>())
            })
            .collect();
        let counted = tally(&coded);
        assert_eq!(counted, tally(&laid));
        assert_eq!(counted.0, Some((5, true)));

        let words: Vec<Value> = (0..300).map(|n| Value::Varchar(format!("word {n}"))).collect();
        let words = std::sync::Arc::new(
            Vector::from_values(LogicalType::Varchar, &words).expect("a column"),
        );
        let coded: Vec<Vector> = [&first[..3], &second[..2]]
            .iter()
            .map(|codes| {
                Vector::dictionary_over(codes.to_vec(), std::sync::Arc::clone(&words))
                    .expect("a dictionary")
            })
            .collect();
        let laid: Vec<Vector> = coded
            .iter()
            .map(|chunk| {
                let values: Vec<Value> = (0..chunk.len()).map(|row| chunk.value_at(row)).collect();
                Vector::from_values(LogicalType::Varchar, &values).expect("a column")
            })
            .collect();
        assert_eq!(tally(&coded), tally(&laid));
    }

    /// A null takes no row of anybody's list, in the three places the mask is read differently.
    #[test]
    fn a_null_holds_no_value_and_takes_no_row_of_one() {
        let held_by = |values: &[Option<i32>]| {
            let owned: Vec<Value> =
                values.iter().map(|v| v.map_or(Value::Null, Value::Integer)).collect();
            held(Vector::from_values(LogicalType::Integer, &owned).expect("a column"))
        };
        assert_eq!(
            held_by(&[Some(1), None, Some(2), None, Some(1)]),
            Some(vec![(Value::Integer(1), 2), (Value::Integer(2), 1)])
        );

        // Through a dictionary, where the null is an entry every code pointing at it shares.
        let entries = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Null, Value::Integer(2)],
        )
        .expect("a dictionary");
        let coded = Vector::dictionary(vec![0, 1, 2, 1, 0], entries).expect("a dictionary column");
        assert_eq!(held(coded), Some(vec![(Value::Integer(1), 2), (Value::Integer(2), 1)]));

        // And a constant column that is nothing but nulls, which holds no value at all.
        let nothing = Vector::constant(LogicalType::Integer, Value::Null, 40);
        assert_eq!(held(nothing), Some(Vec::new()));
    }

    /// The cap, end to end. Under it the counts are an answer and over it there is nothing, because
    /// a list of the first five hundred and twelve values would say the rest hold no rows.
    #[test]
    fn a_column_past_the_cap_has_no_frequencies_and_still_has_a_distinct_count() {
        let under: Vec<i32> = (0..i32::try_from(TALLY_VALUES).expect("512 fits")).collect();
        let mut counts = Counts::new(1);
        counts.add(&Chunk::new(vec![flat(&under)]).expect("a chunk"));
        assert_eq!(counts.frequency_values(0), Some(TALLY_VALUES));

        let over: Vec<i32> = (0..i32::try_from(TALLY_VALUES).expect("512 fits") + 1).collect();
        let mut counts = Counts::new(1);
        counts.add(&Chunk::new(vec![flat(&over)]).expect("a chunk"));
        assert_eq!(counts.frequencies(0), None);
        // The sketch is nowhere near full at 513 values, so the column that stopped saying how many
        // rows hold each value still says how many values there are. The two caps are different
        // numbers for different questions.
        assert_eq!(counts.exact(0), Some(TALLY_VALUES as u64 + 1));
    }

    /// A column arriving in pieces is one column, which is the reason the sketch is per table.
    #[test]
    fn chunks_of_the_same_column_are_counted_together_and_a_repeat_is_not_counted_twice() {
        let mut counts = Counts::new(1);
        for chunk in [&[1, 2, 3][..], &[3, 4, 5][..]] {
            let held: Vec<i32> = chunk.to_vec();
            counts.add(&Chunk::new(vec![flat(&held)]).expect("a chunk"));
        }
        assert_eq!(counts.exact(0), Some(5));
    }

    /// The line the whole design rests on. Below it the sketch is holding every hash there is and
    /// the count is the count, and above it there is an estimate and nothing a query may read.
    #[test]
    fn the_answer_is_exact_up_to_the_sketch_size_and_an_estimate_past_it() {
        let under: Vec<i32> = (0..DEFAULT_K as i32 - 1).collect();
        let mut counts = Counts::new(1);
        counts.add(&Chunk::new(vec![flat(&under)]).expect("a chunk"));
        assert_eq!(counts.exact(0), Some(DEFAULT_K as u64 - 1));
        assert_eq!(counts.distinct(0), Some((DEFAULT_K as u64 - 1, true)));

        // One more value and the sketch is full. It has thrown a hash away by then, so what is left
        // is an estimate, `exact` stops answering, and the estimate is close rather than right.
        let over: Vec<i32> = (0..DEFAULT_K as i32).collect();
        let mut counts = Counts::new(1);
        counts.add(&Chunk::new(vec![flat(&over)]).expect("a chunk"));
        assert_eq!(counts.exact(0), None);
        let (estimate, exact) = counts.distinct(0).expect("an estimate");
        assert!(!exact);
        let error =
            (estimate as f64 - f64::from(DEFAULT_K as u32)).abs() / f64::from(DEFAULT_K as u32);
        assert!(error < 0.05, "{estimate} against {DEFAULT_K}, which is {error} out");
    }

    /// The estimate is meant to be worth reading, so this checks the error at a size where the
    /// sketch is doing real work rather than at the boundary where it has only just filled.
    #[test]
    fn the_estimate_is_within_a_few_percent_of_a_column_far_past_the_sketch() {
        let values: Vec<i32> = (0..200_000).collect();
        let mut counts = Counts::new(1);
        for piece in values.chunks(8192) {
            counts.add(&Chunk::new(vec![flat(piece)]).expect("a chunk"));
        }
        let (estimate, exact) = counts.distinct(0).expect("an estimate");
        assert!(!exact);
        let error = (estimate as f64 - 200_000.0).abs() / 200_000.0;
        assert!(error < 0.05, "{estimate} against 200000, which is {error} out");
    }

    /// A null is not a value and a distinct count does not count it, in a flat column and through a
    /// dictionary, which are the two places the mask is read differently.
    #[test]
    fn a_null_is_not_a_distinct_value() {
        let held = [Some(1), None, Some(2), None, Some(1)]
            .map(|value| value.map_or(Value::Null, Value::Integer))
            .to_vec();
        let vector = Vector::from_values(LogicalType::Integer, &held).expect("a column");
        assert_eq!(count(vector), Some((2, true)));

        // The same through a dictionary that holds a null of its own, which every code pointing at
        // it is a null row rather than a value shared by those rows.
        let entries = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Null, Value::Integer(2)],
        )
        .expect("a dictionary");
        let coded = Vector::dictionary(vec![0, 1, 2, 1, 0], entries).expect("a dictionary column");
        assert_eq!(count(coded), Some((2, true)));
    }

    /// SQL calls these pairs equal and a `GROUP BY` over them makes one group, so a distinct count
    /// that made two would disagree with the answer to the query it is estimating.
    #[test]
    fn the_two_zeroes_are_one_value_and_every_nan_is_one_value() {
        let held = [0.0_f64, -0.0, 0.0].map(Value::Double).to_vec();
        let vector = Vector::from_values(LogicalType::Double, &held).expect("a column");
        assert_eq!(count(vector), Some((1, true)));

        // Two NaNs with different payloads, which is two bit patterns and one value.
        let other = f64::from_bits(f64::NAN.to_bits() | 1);
        assert!(other.is_nan() && other.to_bits() != f64::NAN.to_bits());
        let held = [f64::NAN, other].map(Value::Double).to_vec();
        let vector = Vector::from_values(LogicalType::Double, &held).expect("a column");
        assert_eq!(count(vector), Some((1, true)));
    }

    /// A column this cannot read says nothing rather than saying something low.
    #[test]
    fn a_blind_part_blinds_the_column_it_is_absorbed_into() {
        let held = [Value::Interval { months: 1, days: 0, micros: 0 }].to_vec();
        let unreadable = Vector::from_values(LogicalType::Interval, &held).expect("a column");
        let mut counts = Counts::new(1);
        counts.add(&Chunk::new(vec![flat(&[1, 2, 3])]).expect("a chunk"));
        let mut part = Partial::new();
        part.add(&unreadable);
        part.add(&flat(&[4]));
        let mut columns = counts.columns_mut();
        columns.first_mut().expect("one column").absorb(part);
        drop(columns);
        assert_eq!(counts.distinct(0), None, "the part missed rows, so the column did too");
    }

    #[test]
    fn a_type_with_no_rule_stops_the_column_answering_and_never_starts_again() {
        let held = [Value::Interval { months: 1, days: 0, micros: 0 }].to_vec();
        let vector = Vector::from_values(LogicalType::Interval, &held).expect("a column");
        let mut counts = Counts::new(1);
        counts.add(&Chunk::new(vec![vector]).expect("a chunk"));
        assert_eq!(counts.exact(0), None);
        assert_eq!(counts.distinct(0), None);

        // And a later chunk this could read does not bring it back, because the rows it missed are
        // still missing and a count built from what came after them is too low.
        counts.add(&Chunk::new(vec![flat(&[1, 2, 3])]).expect("a chunk"));
        assert_eq!(counts.distinct(0), None);
    }

    /// One blind column does not blind the rest of the table.
    #[test]
    fn the_columns_beside_a_blind_one_keep_counting() {
        let interval = Vector::from_values(
            LogicalType::Interval,
            &[Value::Interval { months: 1, days: 0, micros: 0 }],
        )
        .expect("a column");
        let mut counts = Counts::new(2);
        counts.add(&Chunk::new(vec![interval, flat(&[7])]).expect("a chunk"));
        assert_eq!(counts.width(), 2);
        assert_eq!(counts.distinct(0), None);
        assert_eq!(counts.exact(1), Some(1));
    }

    /// The two lists that have to agree: a type `countable` claims has a rule in `hash_value`, and
    /// one it does not claim has none.
    #[test]
    fn the_type_list_and_the_value_rule_say_the_same_thing() {
        let cases = [
            (LogicalType::Integer, Value::Integer(1)),
            (LogicalType::BigInt, Value::BigInt(1)),
            (LogicalType::HugeInt, Value::HugeInt(1)),
            (LogicalType::UBigInt, Value::UBigInt(1)),
            (LogicalType::Double, Value::Double(1.0)),
            (LogicalType::Varchar, Value::Varchar("a".into())),
            (LogicalType::Blob, Value::Blob(vec![1])),
            (LogicalType::Date, Value::Date(1)),
            (LogicalType::Timestamp, Value::Timestamp(1)),
            (LogicalType::Boolean, Value::Boolean(true)),
            (LogicalType::Interval, Value::Interval { months: 1, days: 0, micros: 0 }),
        ];
        for (ty, value) in cases {
            assert_eq!(
                countable(&ty),
                hash_value(&value).is_some(),
                "{ty} and the value rule disagree"
            );
        }
        // A null has no hash whatever its column's type is, which is what keeps a null row from
        // poisoning a column that every other row of is countable.
        assert_eq!(hash_value(&Value::Null), None);
    }
}

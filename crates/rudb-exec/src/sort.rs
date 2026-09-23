//! Sorting.
//!
//! A pipeline breaker: nothing can be emitted until the last row has arrived, because the last row
//! of the input can be the first row of the output. It reads everything, sorts, and then hands the
//! result out a chunk at a time.
//!
//! The sort is Rust's `sort_by`, which is stable. Stability is not something SQL promises and it is
//! kept anyway, because `ORDER BY a` over rows that tie on `a` producing a different order on two
//! runs of the same query is the kind of difference that makes a compatibility diff useless.
//!
//! # Keeping it stable on more than one thread
//!
//! A stable sort is stable in the order the rows were given to it, and on several threads that is
//! the order the threads happened to finish. So every row carries where it arrived from, which is
//! the morsel it came out of and its place in that morsel, and two rows that tie on every key are
//! separated by that instead. Reading it lexicographically is exactly the order one thread would
//! have produced, because one thread takes the morsels in the order they were cut and reads each
//! one through from the start, so a parallel sort answers what a serial sort answers rather than
//! answering something SQL also allows.
//!
//! A filter between the scan and the sort does not break that. The place a row carries is its place
//! among the rows that reached the sort rather than its row number in the file, and the rows that
//! reach the sort from one morsel still reach it in order.
//!
//! # Where the payload is while the sort runs
//!
//! Not in the sort. The chunks are kept as they arrived and a row is a chunk and a row in it, so
//! what the comparator moves is the keys and two pairs of numbers rather than a copy of every
//! column. The columns are moved once at the end, by [`lay`], which lays each column's pieces end
//! to end and reads them back in sorted order with [`interleave`], a typed copy per physical layout.
//!
//! This used to hold a `Vec<Value>` of the whole row per row. Sorting lineitem at SF1 on three
//! keys cost 197 billion instructions that way, of which most were the allocator: sixteen columns
//! a row over six million rows is around a hundred million `Value`s and thirty million of them
//! were strings. The same sort is 54 billion now, and the system time it spends asking the
//! operating system for memory went from 45.7 seconds to 2.8. See #1210.
//!
//! # And where the key is
//!
//! In the row, as bytes, whenever the key list allows it. [`crate::normal`] writes every key of a
//! row into one fixed width buffer in an encoding whose byte order is the sort order, with the
//! direction and the null placement already folded in, so the comparator is a byte compare and the
//! row is one flat thing rather than a pointer to a heap allocation per row. The `Vec<Value>` path
//! is still here and still handles everything, because a string key has no fixed width and a float
//! key does not order the way its bytes do. Which one a sort takes is decided once, from the types
//! of the key expressions, and [`Keyed`] is the two of them.
//!
//! # The shape a sink has
//!
//! [`Sort`] is a [`Sink`], so the rows arrive through `sink`, one instance's rows are handed over
//! through `combine`, and `finalize` does the sort once after every instance has combined. On one
//! thread that is the same work in the same order as reading the input in a loop would be. On
//! several it is the shape that makes the sort possible at all, and having it now is why F4 changes
//! no operator.
//!
//! The finished chunks go into a [`Sorted`], which is a separate source rather than something
//! `finalize` hands back, for the reason [`Sink::finalize`] gives.
//!
//! # When it does not fit
//!
//! Everything above is about a sort that fits, and a sort that does not used to die at the
//! allocator. What it does now is spill: when an instance is holding more than its share of the
//! memory limit, it sorts what it has, writes it out as a sorted run, and starts again empty. At
//! the end the runs are merged, by [`Sorted`] rather than here, as the rows downstream are asked
//! for.
//!
//! The merge is over there and not here for the reason section 15.6 of `tenx/15-the-partitioned-write.md`
//! gives. A sink's peak is inside its own `finalize`, holding what is left of the input and the
//! output it is building from it, so an operator that spills on the way in and then assembles the
//! whole answer on the way out has bounded nothing. The answer has to be produced as it is read,
//! which is the one thing a `finalize` cannot do.
//!
//! What a run carries beside its rows is one forty byte column, the row's normalized key and then
//! its arrival, in an encoding whose byte order is the sort order. That is what lets the merge be a
//! byte comparison with no key expressions in it, and it is why only the normalized path spills:
//! the valued path has no fixed width bytes to write. Every clustering declaration the loader takes
//! is fixed width, so that covers the case this was built for, and a string key on a table that
//! does not fit is still #1301.
//!
//! # Giving the input back while the output is being built
//!
//! A sort holds the rows that arrived and the rows it is handing out at the same time, and for a
//! moment near the end it holds both in full. That moment is what a big sort dies at: SF10 lineitem
//! is around ten gigabytes of payload, so two copies of it is twenty, and the limit on a machine
//! with 24 GiB lands at 19.1.
//!
//! It does not have to hold both. [`lay`] lays one column at a time, so the input's copy of a
//! column is finished with the moment that column has been laid, and the input is taken apart into
//! its columns up front so that each one can be dropped exactly then. What the sort holds is
//! therefore one payload and one column of headroom rather than two payloads, whichever column it
//! is on, and the charge against the memory limit comes down as the columns go.
//!
//! The rows themselves go before any of that. All [`lay`] wants from them is where each input
//! row lands, which is four bytes a row, against the forty eight a row that carries a normalized
//! key and an arrival. So the order is turned into that and the rows are dropped, which at SF10 is
//! another three gigabytes that is not held while the assembly runs.

use std::cmp::Ordering;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering as Atomic};

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Session, Value};
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_plan::{Plan, Slice, SortKey};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector, interleave};

use crate::merged::{ORDER, Sorted, order_of, ordering};
use crate::normal::{self, Normal};
use crate::pairs::in_parallel;
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::runs::Runs;
use crate::schema::Schema;

/// One row on its way through a sort: the values of its keys, where it arrived, and where it is.
///
/// The payload is not here. A row is a [`Source`] into the chunks the sink kept, and the columns
/// are moved once at the end by [`gathered`] rather than carried through the sort as a boxed value
/// a field. That is the second half of what the note on #63 asks for, and [`Normalized`] is the
/// first half: this arm is what a key list with a string, a float or too many bytes in it falls
/// back to.
pub(crate) type Sortable = (Vec<Value>, Arrival, Source);

/// Where a row arrived: the morsel it came from and its place among the rows of that morsel.
///
/// Sixteen bytes beside a `Vec` header and whatever it points at, which is why it is carried per
/// row rather than reconstructed. What it buys is that the answer does not depend on how many
/// threads ran.
pub(crate) type Arrival = (u64, u64);

/// Where a row is: the chunk the sink kept it in and its row in that chunk.
pub(crate) type Source = (u32, u32);

/// One row on its way through a sort with its keys written as bytes rather than held as values.
///
/// The same three fields as [`Sortable`] with the first one flattened. Nothing here points at
/// anything: the key is a fixed array in the row, so the whole vector is one allocation and a
/// comparison is a byte compare over that array. See [`crate::normal`] for what is in it and which
/// key lists can have one.
pub(crate) type Normalized = (Normal, Arrival, Source);

/// What one row costs beside the values of its keys, which [`rows::footprint`] already counts.
const BESIDE: u64 = (size_of::<Sortable>() - size_of::<Vec<Value>>()) as u64;

/// What one row of the normalized path costs, which is the whole of it since nothing is borrowed.
const NORMALIZED: u64 = size_of::<Normalized>() as u64;

/// An ordering over the input.
#[derive(Debug)]
pub(crate) struct Sort {
    keys: Vec<SortKey>,
    /// How wide each key writes into a normalized key, when the key list has one.
    ///
    /// `None` is the `Value` path, and it is what a string key, a float key or a key list that does
    /// not fit gets. See [`crate::normal::layout`].
    widths: Option<Vec<usize>>,
    /// The key expressions, evaluated against the input's schema.
    exprs: Prepared,
    /// The input's types, which are also the output's, since a sort changes no column.
    types: Vec<LogicalType>,
    memory: Memory,
    /// Every instance's rows and the chunks they point into, waiting for the sort.
    gathered: Mutex<Combined>,
    /// What those rows are charged, taken from the instances that gathered them and given back
    /// once the sorted chunks have been charged instead.
    charged: Mutex<Vec<Reservation>>,
    /// What the sorted chunks are charged, held for as long as they are readable.
    ///
    /// Nothing is charged here when the sort spilled, because then the chunks are read back one at
    /// a time and what is resident is a chunk a run rather than the answer.
    held: Mutex<Reservation>,
    /// The sorted runs the instances wrote, once they have combined.
    runs: Mutex<Vec<Runs>>,
    /// How many instances of this sort there are, which decides what share of the limit each gets.
    ///
    /// Every instance is made before any row moves, so this stops changing before it is first read.
    instances: AtomicUsize,
    out: Sorted,
}

/// Every instance's rows after they have been handed over, in one lock rather than two.
///
/// The two halves are read and written together and a row is an index into the chunks beside it,
/// so a pair of locks would be two that always have to be taken in the same order and nothing
/// would ever hold one of them alone.
#[derive(Debug, Default)]
struct Combined {
    /// The chunks as they arrived, which hold the payload of every row.
    chunks: Vec<Chunk>,
    /// One entry a row, pointing into `chunks`.
    rows: Keyed,
}

/// The rows of a sort, with their keys held whichever way this key list allows.
///
/// Two arms and not two operators, because everything either arm does differently is in this file
/// and everything else about a sort is the same: the same chunks, the same arrivals, the same
/// assembly at the end. Which arm a sort takes is decided once in [`Sort::new`], off the types of
/// the key expressions, so an instance never has to ask and the two can never be mixed.
#[derive(Debug)]
enum Keyed {
    /// Keys as bytes, which is the fast path and covers the fixed width types.
    Normal(Vec<Normalized>),
    /// Keys as values, which handles every type including the ones with no fixed width.
    Valued(Vec<Sortable>),
}

impl Default for Keyed {
    fn default() -> Self {
        Self::Valued(Vec::new())
    }
}

impl Keyed {
    /// An empty set of rows of the same arm as this one.
    fn empty(&self) -> Self {
        match self {
            Self::Normal(_) => Self::Normal(Vec::new()),
            Self::Valued(_) => Self::Valued(Vec::new()),
        }
    }

    /// Takes another instance's rows, moving every row's chunk along by `base`.
    ///
    /// The chunk an instance's row points at is its chunk among that instance's, so it moves along
    /// by however many chunks are already here.
    fn absorb(&mut self, other: Self, base: u32) -> Result<()> {
        match (self, other) {
            (Self::Normal(into), Self::Normal(from)) => {
                into.extend(from.into_iter().map(|(key, arrival, (chunk, row))| {
                    (key, arrival, (chunk.saturating_add(base), row))
                }));
                Ok(())
            }
            (Self::Valued(into), Self::Valued(from)) => {
                into.extend(from.into_iter().map(|(key, arrival, (chunk, row))| {
                    (key, arrival, (chunk.saturating_add(base), row))
                }));
                Ok(())
            }
            _ => Err(Error::internal("two instances of one sort holding their keys differently")),
        }
    }

    /// Puts the rows in order, keys first and where they arrived settling a tie.
    ///
    /// Unstable on the normalized arm and stable on the other, which is the same order either way:
    /// the arrival is unique per row and it is the last thing compared, so no two rows are ever
    /// equal and there is nothing for stability to decide.
    fn sort(&mut self, keys: &[SortKey]) -> Result<()> {
        match self {
            Self::Normal(rows) => {
                rows.sort_unstable_by(|left, right| {
                    left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1))
                });
                Ok(())
            }
            Self::Valued(rows) => {
                let mut failure: Option<Error> = None;
                rows.sort_by(|left, right| settled(keys, left, right, &mut failure));
                failure.map_or(Ok(()), Err)
            }
        }
    }

    /// The same order as [`Keyed::sort`], put in place on the lease's threads.
    ///
    /// Only the normalized arm is spread. The valued arm compares through a function that can fail
    /// and is left on this thread, which is also where it would be for the types it covers anyway.
    fn sort_across(&mut self, keys: &[SortKey], threads: &Lease<'_>) -> Result<()> {
        match self {
            Self::Normal(rows) => spread(rows, threads, |left, right| {
                left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1))
            }),
            Self::Valued(_) => self.sort(keys),
        }
    }

    /// Where each row sits, in the order the sort put them.
    fn sources(&self) -> Box<dyn ExactSizeIterator<Item = Source> + '_> {
        match self {
            Self::Normal(rows) => Box::new(rows.iter().map(|row| row.2)),
            Self::Valued(rows) => Box::new(rows.iter().map(|row| row.2)),
        }
    }

    /// How many rows there are.
    fn len(&self) -> usize {
        match self {
            Self::Normal(rows) => rows.len(),
            Self::Valued(rows) => rows.len(),
        }
    }

    /// The bytes each row is ordered by, in the order the sort put them.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) on the valued arm, which has no
    /// fixed width bytes to give and is never asked, because [`Sort::tight`] only ever says yes on
    /// the other one.
    fn orders(&self) -> Result<Vec<[u8; ORDER]>> {
        match self {
            Self::Normal(rows) => {
                Ok(rows.iter().map(|(key, arrival, _)| order_of(key, *arrival)).collect())
            }
            Self::Valued(_) => {
                Err(Error::internal("a sort holding its keys as values cannot spill"))
            }
        }
    }

    /// What these rows were charged when they were taken in, so that dropping them can give it back.
    ///
    /// Counted again rather than carried, because the valued arm's rows are not all the same size
    /// and a running total would have to be threaded through the instance handover as a fourth
    /// thing that has to stay in step with the other three. One walk over the rows at the end of a
    /// sort is nothing beside the sort.
    fn footprint(&self) -> u64 {
        match self {
            Self::Normal(rows) => {
                u64::try_from(rows.len()).unwrap_or(u64::MAX).saturating_mul(NORMALIZED)
            }
            Self::Valued(rows) => rows.iter().map(|row| rows::footprint(&row.0) + BESIDE).sum(),
        }
    }
}

/// What one instance of a sort gathers before it combines.
#[derive(Debug)]
pub(crate) struct Gathered {
    held: Combined,
    scratch: Scratch,
    charged: Reservation,
    /// The morsel this instance is reading and how many of its rows have arrived.
    place: Place,
    /// The sorted runs this instance has written, each one a batch that did not fit.
    runs: Vec<Runs>,
}

/// How far through a morsel an instance is, which is the second half of an [`Arrival`].
#[derive(Debug, Default)]
pub(crate) struct Place {
    pub(crate) morsel: u64,
    pub(crate) at: u64,
}

impl Place {
    /// Where the row at `row` of the chunk that starts here arrived.
    pub(crate) fn of(&self, row: usize) -> Arrival {
        (self.morsel, self.at.saturating_add(row as u64))
    }

    /// Moves past a chunk of `rows` rows of the same morsel.
    pub(crate) fn past(&mut self, rows: usize) {
        self.at = self.at.saturating_add(rows as u64);
    }

    /// Starts a new morsel.
    pub(crate) fn start(&mut self, morsel: u64) {
        self.morsel = morsel;
        self.at = 0;
    }
}

impl Sort {
    /// Applies the session semantics to the prepared sort keys.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.exprs = self.exprs.in_session(session);
        self
    }

    /// # Errors
    ///
    /// If a sort key does not resolve against the input's schema, which is a failure of the plan
    /// and is found here rather than on the first chunk.
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
        keys: Slice,
        memory: &Memory,
    ) -> Result<(Self, Sorted)> {
        let keys = plan.sort_key_list(keys).to_vec();
        let exprs: Vec<_> = keys.iter().map(|key| key.expr).collect();
        let types: Vec<_> = exprs.iter().map(|&expr| plan.expr_type(expr).clone()).collect();
        let out = Sorted::new();
        let widths = normal::layout(&types);
        let rows =
            if widths.is_some() { Keyed::Normal(Vec::new()) } else { Keyed::Valued(Vec::new()) };
        let sort = Self {
            widths,
            exprs: Prepared::new(plan, &exprs, input)?,
            keys,
            types: input.types(),
            memory: memory.clone(),
            gathered: Mutex::new(Combined { chunks: Vec::new(), rows }),
            charged: Mutex::new(Vec::new()),
            held: Mutex::new(memory.reservation()),
            runs: Mutex::new(Vec::new()),
            instances: AtomicUsize::new(0),
            out: out.clone(),
        };
        Ok((sort, out))
    }

    /// Whether it is time this instance wrote what it is holding out to a file.
    ///
    /// Two conditions, and the first one is the database rather than the sort. A run is a file on a
    /// disk that has to have room for it, so a sort that spills whenever it is holding a lot writes
    /// gigabytes a load that would have fitted never needed. The clustered SF10 load is sixteen
    /// gigabytes of rows on a machine with seven and a half free, so the rule that spills at a
    /// share of the limit fills the disk on a load that had memory to spare. The rule that spills
    /// when the database is close to its limit does not.
    ///
    /// Close is three quarters, and that is a measured answer rather than a round one. Writing a run
    /// used to cost 1.6 times the payload being spilled, because the run was built as chunks and no
    /// chunk is finished until every column is laid, so no fraction above three quarters left room
    /// to spill in. Runs are written a column at a time now (#1347) and the clustered SF1 load
    /// spills fine at fifteen sixteenths, but the clustered SF10 load at the default limit was
    /// fastest at three quarters, 2:30 against 3:02, and the room below the line is room the
    /// operators after the sort get to use.
    ///
    /// The second is that this instance is holding enough for a file to be worth opening, because
    /// otherwise a query that is short of memory for some other reason would turn every chunk that
    /// arrived into a run of one chunk, and a merge of ten thousand of those is slower than the
    /// query that ran out.
    ///
    /// No limit is no spilling, which is the right answer and not a missing case. A database opened
    /// without one is one whose answer to running out of memory is the allocator's, and a sort that
    /// started writing files anyway would be choosing for it.
    fn tight(&self, local: &Gathered) -> bool {
        if self.widths.is_none() {
            return false;
        }
        let Some(limit) = self.memory.limit() else { return false };
        if self.memory.used() < limit - limit / 4 {
            return false;
        }
        let instances = self.instances.load(Atomic::Relaxed).max(1) as u64;
        let worth = ((limit / 16) / instances).max(1 << 20);
        local.charged.bytes() >= worth
    }

    /// Sorts what this instance is holding, writes it out as a run, and leaves it empty.
    ///
    /// The instance's whole reservation goes in and comes back out, because everything it is
    /// charged for is what is being spilled: the chunks that arrived and the rows pointing into
    /// them. What is left in it afterwards is whatever rounding the give back did not account for,
    /// and it carries on from there.
    fn spill(&self, local: &mut Gathered) -> Result<()> {
        let empty = Combined { chunks: Vec::new(), rows: local.held.rows.empty() };
        let Combined { chunks, rows } = std::mem::replace(&mut local.held, empty);
        let mut charged = vec![std::mem::replace(&mut local.charged, self.memory.reservation())];
        let file = self.run(chunks, rows, &mut charged)?;
        if let Some(left) = charged.pop() {
            local.charged = left;
        }
        local.runs.push(file);
        Ok(())
    }

    /// One sorted run: these rows in order, in a file, with the bytes they are ordered by beside
    /// them.
    ///
    /// Everything up to the write is what [`Sink::finalize`] does to a sort that fits, in the same
    /// order and for the same reasons, so a run is a small sort that went to a file instead of to
    /// the source. The assembly it builds is charged to a reservation that lives for as long as
    /// this call does, since the chunks are written and dropped rather than held.
    ///
    /// # Errors
    ///
    /// If the rows hold their keys as values, if there are more of them than a sort addresses, or
    /// if the file cannot be written.
    fn run(
        &self,
        chunks: Vec<Chunk>,
        mut rows: Keyed,
        charged: &mut Vec<Reservation>,
    ) -> Result<Runs> {
        rows.sort(&self.keys)?;
        let total = rows.len();
        if u32::try_from(total).is_err() {
            return Err(too_many());
        }
        let orders = rows.orders()?;
        let order = order(&chunks, &rows)?;
        let taken = rows.footprint();
        drop(rows);
        give(charged, taken);
        let mut types = self.types.clone();
        types.push(LogicalType::Blob);
        let mut file = Runs::new("sort", types)?;
        file.begin(total)?;
        // Laid and written one column at a time, and this is the whole reason the run file is
        // written the way it is. The sort is spilling because it has run out of memory, so the one
        // thing it cannot do on the way out is hold a second copy of what it is holding, and a run
        // built as chunks would: no chunk is finished until every column has been laid. See #1347.
        lay(&self.types, chunks, &order, total, charged, |whole| file.column(whole))?;
        // The ordering column, a block at a time, because `orders` is already the bytes and turning
        // the whole of it into a column would be those bytes twice over.
        for block in 0..total.div_ceil(VECTOR_SIZE) {
            let start = block * VECTOR_SIZE;
            let Some(orders) = orders.get(start..(start + VECTOR_SIZE).min(total)) else {
                return Err(Error::internal("a sorted run with fewer keys in it than rows"));
            };
            file.part(&ordering(orders)?)?;
        }
        Ok(file)
    }
}

impl Sink for Sort {
    type Local = Gathered;

    fn local(&self) -> Gathered {
        let rows = if self.widths.is_some() {
            Keyed::Normal(Vec::new())
        } else {
            Keyed::Valued(Vec::new())
        };
        self.instances.fetch_add(1, Atomic::Relaxed);
        Gathered {
            held: Combined { chunks: Vec::new(), rows },
            scratch: self.exprs.scratch(),
            charged: self.memory.reservation(),
            place: Place::default(),
            runs: Vec::new(),
        }
    }

    fn at(&self, morsel: &rudb_pipeline::Morsel, local: &mut Gathered) -> Result<()> {
        local.place.start(morsel.index());
        Ok(())
    }

    fn sink(&self, chunk: &Chunk, local: &mut Gathered) -> Result<Progress> {
        if chunk.is_empty() {
            return Ok(Progress::More);
        }
        let mut keys = Vec::with_capacity(self.keys.len());
        self.exprs.evaluate(chunk, &mut local.scratch, &mut keys)?;
        let at = u32::try_from(local.held.chunks.len()).map_err(|_| too_many())?;
        let mut taken = u64::try_from(chunk.footprint()).unwrap_or(u64::MAX);
        // row at a time: the keys, and nothing else. The payload is not read here at all, which is
        // the point of `Sortable`.
        match (&self.widths, &mut local.held.rows) {
            (Some(widths), Keyed::Normal(rows)) => {
                for row in 0..chunk.len() {
                    let mut key: Normal = [0; normal::WIDTH];
                    let mut written = 0;
                    for (at, column) in keys.iter().enumerate() {
                        let wide = *widths.get(at).ok_or_else(mismatched)?;
                        let value = column.try_value_at(row)?;
                        normal::write(&mut key, written, wide, &value, self.keys[at])?;
                        written += wide;
                    }
                    taken += NORMALIZED;
                    let row = u32::try_from(row).map_err(|_| too_many())?;
                    rows.push((key, local.place.of(row as usize), (at, row)));
                }
            }
            (None, Keyed::Valued(rows)) => {
                for row in 0..chunk.len() {
                    let key: Vec<Value> = keys
                        .iter()
                        .map(|column| column.try_value_at(row))
                        .collect::<Result<_>>()?;
                    taken += rows::footprint(&key) + BESIDE;
                    let row = u32::try_from(row).map_err(|_| too_many())?;
                    rows.push((key, local.place.of(row as usize), (at, row)));
                }
            }
            _ => return Err(mismatched()),
        }
        local.held.chunks.push(chunk.clone());
        local.place.past(chunk.len());
        local.charged.grow(taken)?;
        if self.tight(local) {
            self.spill(local)?;
        }
        Ok(Progress::More)
    }

    fn combine(&self, local: Gathered) -> Result<()> {
        self.runs.lock().map_err(poisoned)?.extend(local.runs);
        let mut gathered = self.gathered.lock().map_err(poisoned)?;
        // Appended rather than merged, because the sort has not happened yet. The order the
        // instances combine in does not decide anything, since every row carries where it arrived
        // and the comparison falls back to that when the keys tie.
        let base = u32::try_from(gathered.chunks.len()).map_err(|_| too_many())?;
        gathered.rows.absorb(local.held.rows, base)?;
        gathered.chunks.extend(local.held.chunks);
        self.charged.lock().map_err(poisoned)?.push(local.charged);
        Ok(())
    }

    fn finalize(&self, threads: &Lease<'_>) -> Result<()> {
        let Combined { chunks, mut rows } = {
            let mut gathered = self.gathered.lock().map_err(poisoned)?;
            let empty = Combined { chunks: Vec::new(), rows: gathered.rows.empty() };
            std::mem::replace(&mut *gathered, empty)
        };
        // What the instances took, moved out so that it can be given back a column at a time rather
        // than all at once when this returns. Dropping what is left of it is what releases the rest.
        let mut charged = std::mem::take(&mut *self.charged.lock().map_err(poisoned)?);
        let mut files = std::mem::take(&mut *self.runs.lock().map_err(poisoned)?);
        if !files.is_empty() {
            // Something spilled, so what is left here is the last batch and it becomes the last
            // run. No chunks are built and nothing is held: the answer is produced by the merge as
            // the rows are read, which is the whole point and is why this returns before the path
            // below rather than sharing it.
            if rows.len() > 0 {
                files.push(self.run(chunks, rows, &mut charged)?);
            }
            return self.out.merge(files, self.types.clone());
        }
        rows.sort_across(&self.keys, threads)?;
        let total = rows.len();
        if u32::try_from(total).is_err() {
            return Err(too_many());
        }
        let order = order(&chunks, &rows)?;
        // The rows have said everything they had to say. Holding them through the assembly is
        // holding a key and an arrival a row for the sake of a number that is already in `order`.
        let taken = rows.footprint();
        drop(rows);
        give(&mut charged, taken);
        let mut held = self.held.lock().map_err(poisoned)?;
        let out = gathered(&self.types, chunks, &order, total, &mut held, &mut charged, threads)?;
        self.out.hold(out)?;
        Ok(())
    }
}

/// Sorts `rows` on the lease's threads, in place and without a second copy of them.
///
/// Cut at the median until there is a piece a thread, which leaves every row of a piece ordered
/// before every row of the piece after it, and then each piece is sorted on its own. Finding a
/// median is one pass over the piece, so the only step that reads every row on one thread is the
/// first cut. `order` has to be total with no two rows equal, which the arrival makes it, or the
/// halves would not be where the rows belong.
fn spread<T: Send>(
    rows: &mut [T],
    threads: &Lease<'_>,
    order: impl Fn(&T, &T) -> Ordering + Sync,
) -> Result<()> {
    let degree = threads.degree();
    let mut pieces: Vec<&mut [T]> = vec![rows];
    while pieces.len() < degree && pieces.iter().all(|piece| piece.len() > VECTOR_SIZE) {
        let held: Vec<Mutex<&mut [T]>> = pieces.into_iter().map(Mutex::new).collect();
        let halves = in_parallel(threads, held.len(), degree, "halved sorted piece", |at| {
            let slot =
                held.get(at).ok_or_else(|| Error::internal("a sorted piece past the end"))?;
            let piece = std::mem::take(&mut *slot.lock().map_err(poisoned)?);
            let middle = piece.len() / 2;
            piece.select_nth_unstable_by(middle, &order);
            Ok(piece.split_at_mut(middle))
        })?;
        pieces = halves.into_iter().flat_map(|(left, right)| [left, right]).collect();
    }
    let held: Vec<Mutex<&mut [T]>> = pieces.into_iter().map(Mutex::new).collect();
    in_parallel(threads, held.len(), degree, "sorted piece", |at| {
        let slot = held.get(at).ok_or_else(|| Error::internal("a sorted piece past the end"))?;
        slot.lock().map_err(poisoned)?.sort_unstable_by(&order);
        Ok(())
    })?;
    Ok(())
}

/// Where each row of the answer reads from, as a row of the chunks that arrived laid end to end.
///
/// Worked out once for the whole sort and read by every column, which is the point of it. It used
/// to be the other way round, a place for each row that arrived, which every column then had to
/// scatter into a map of its own before it could gather anything: on SF1 `lineitem` that was a
/// column taking 0.6s to 1.3s on its own, most of it in pages of maps being faulted in (#1365).
fn order(chunks: &[Chunk], rows: &Keyed) -> Result<Vec<usize>> {
    let mut starts = Vec::with_capacity(chunks.len());
    let mut start = 0;
    for chunk in chunks {
        starts.push(start);
        start += chunk.len();
    }
    rows.sources()
        .map(|(chunk, row)| {
            let (Some(&start), Some(len)) =
                (starts.get(chunk as usize), chunks.get(chunk as usize).map(Chunk::len))
            else {
                return Err(Error::internal(
                    "a sorted row pointing outside the chunks it came from",
                ));
            };
            if row as usize >= len {
                return Err(Error::internal(
                    "a sorted row pointing outside the chunks it came from",
                ));
            }
            Ok(start + row as usize)
        })
        .collect()
}

/// Gives `bytes` back across the reservations the instances handed over.
///
/// From the last one backwards, which is arbitrary and is fine: they all charge the same budget and
/// what matters is only that the total comes down. A reservation that has nothing left is dropped,
/// which is the same as shrinking it to nothing and is one fewer to walk next time.
fn give(charged: &mut Vec<Reservation>, mut bytes: u64) {
    while bytes > 0 {
        let Some(last) = charged.last_mut() else { return };
        let held = last.bytes();
        if held > bytes {
            last.shrink(bytes);
            return;
        }
        bytes -= held;
        charged.pop();
    }
}

/// The sorted rows, one whole column at a time, handed to `each` as they are finished.
///
/// This is where the sort stops being row shaped. The order is a permutation of the rows that
/// arrived, so what each column needs is for its values to be written out in that order, and
/// [`interleave`] is exactly that: the pieces are laid end to end and read back through the order,
/// one typed copy per physical layout rather than a `Value` a field. A string moves as sixteen
/// bytes of view over an arena its bytes were copied into once.
///
/// One column at a time, because laying a column holds a second copy of that column and holding
/// one of them at a time is a column of headroom rather than a table of it.
///
/// The chunks come in by value and are taken apart into their columns before anything is laid, so
/// that the input's copy of a column can be dropped the moment it has been laid and the charge
/// against the memory limit can come down with it. Held as chunks there is nowhere to put the
/// column that is finished with, and the sort ends up holding the whole input and the whole output
/// at once, which is what made a big one die at the allocator rather than get slower.
///
/// What `each` does with a finished column is the difference between the two callers. A sort that
/// fitted cuts it into chunk sized windows and keeps them, which for a page is a window and no
/// copy. A sort that is spilling writes it to a run file and drops it, which is why it can write a
/// run without holding one.
///
/// # Errors
///
/// If a column has no layout that can be laid, or whatever `each` fails with.
fn lay(
    types: &[LogicalType],
    chunks: Vec<Chunk>,
    order: &[usize],
    rows: usize,
    charged: &mut Vec<Reservation>,
    mut each: impl FnMut(&Vector) -> Result<()>,
) -> Result<()> {
    if rows == 0 {
        return Ok(());
    }
    for (ty, pieces) in types.iter().zip(transposed(types, chunks)?) {
        let (whole, given) = column(ty, pieces, order)?;
        give(charged, given);
        each(&whole)?;
    }
    Ok(())
}

/// The pieces of one column of every chunk, a list a column, so that a column is one thing to drop.
fn transposed(types: &[LogicalType], chunks: Vec<Chunk>) -> Result<Vec<Vec<Vector>>> {
    let mut pieces: Vec<Vec<Vector>> = vec![Vec::with_capacity(chunks.len()); types.len()];
    for chunk in chunks {
        for (position, column) in chunk.into_columns().into_iter().enumerate() {
            let Some(into) = pieces.get_mut(position) else {
                return Err(Error::internal("a sorted chunk wider than the schema it came from"));
            };
            into.push(column);
        }
    }
    Ok(pieces)
}

/// One column laid in sorted order, and how many bytes of input dropping its pieces gave back.
fn column(ty: &LogicalType, pieces: Vec<Vector>, order: &[usize]) -> Result<(Vector, u64)> {
    let whole = interleave(ty, &pieces, order)?.into_pages();
    let given = pieces.iter().map(Vector::footprint).sum::<usize>();
    drop(pieces);
    Ok((whole, u64::try_from(given).unwrap_or(u64::MAX)))
}

/// The sorted rows as chunks, for a sort that is going to hand them back rather than write them.
///
/// The columns are laid on the lease's threads, a column each, rather than one after another the
/// way [`lay`] does it. The columns do not share anything, and on one thread this was most of what
/// a sort that fitted spent after its input ran out: 1.2s of 1.55s on SF1 `lineitem`, with the
/// other threads idle (#1210). The headroom it costs is a column a thread rather than one column,
/// which a sort that fitted can afford and a sort that is spilling cannot, and that is why the
/// spilling path still goes through [`lay`].
///
/// # Errors
///
/// As [`lay`], or if the chunks pass the limit the database was opened with.
fn gathered(
    types: &[LogicalType],
    chunks: Vec<Chunk>,
    order: &[usize],
    rows: usize,
    held: &mut Reservation,
    charged: &mut Vec<Reservation>,
    threads: &Lease<'_>,
) -> Result<Vec<Chunk>> {
    if rows == 0 {
        return Ok(Vec::new());
    }
    let pieces = transposed(types, chunks)?;
    // The longest columns first. The threads take columns in the order they are handed out, and a
    // string column that is handed out last starts after a number column has finished on its
    // thread, so the whole thing takes the two of them end to end rather than the longer one. On
    // SF1 `lineitem` the strings come last in the schema and that was 150ms of 490.
    let mut longest: Vec<usize> = (0..types.len()).collect();
    longest.sort_by_key(|&position| {
        let stringy = types
            .get(position)
            .is_some_and(|ty| matches!(ty, LogicalType::Varchar | LogicalType::Blob));
        let bytes = pieces.get(position).map_or(0, |run| run.iter().map(Vector::footprint).sum());
        std::cmp::Reverse((stringy, bytes))
    });
    let pieces: Vec<Mutex<Vec<Vector>>> = pieces.into_iter().map(Mutex::new).collect();
    let charged = Mutex::new(charged);
    let laid = in_parallel(threads, types.len(), threads.degree(), "laid sorted column", |rank| {
        let position = longest.get(rank).copied().unwrap_or(rank);
        let (Some(ty), Some(pieces)) = (types.get(position), pieces.get(position)) else {
            return Err(Error::internal("a sorted column past the end of the schema"));
        };
        let pieces = std::mem::take(&mut *pieces.lock().map_err(poisoned)?);
        let (whole, given) = column(ty, pieces, order)?;
        give(*charged.lock().map_err(poisoned)?, given);
        Ok((position, whole))
    })?;
    let mut wholes: Vec<Option<Vector>> = vec![None; types.len()];
    for (position, whole) in laid {
        if let Some(slot) = wholes.get_mut(position) {
            *slot = Some(whole);
        }
    }
    let blocks = rows.div_ceil(VECTOR_SIZE);
    let mut columns: Vec<Vec<Vector>> = vec![Vec::with_capacity(types.len()); blocks];
    for whole in &wholes {
        let Some(whole) = whole else {
            return Err(Error::internal("a sorted column nobody laid"));
        };
        for (block, into) in columns.iter_mut().enumerate() {
            let start = block * VECTOR_SIZE;
            into.push(whole.slice(start, (rows - start).min(VECTOR_SIZE))?);
        }
    }
    drop(wholes);
    let mut built = Vec::with_capacity(blocks);
    for (block, columns) in columns.into_iter().enumerate() {
        let start = block * VECTOR_SIZE;
        let chunk = Chunk::with_rows(columns, (rows - start).min(VECTOR_SIZE))?;
        held.grow(u64::try_from(chunk.footprint()).unwrap_or(u64::MAX))?;
        built.push(chunk);
    }
    Ok(built)
}

/// More rows or more chunks than a sort addresses.
///
/// A row is found by a chunk and a row in it, both counted in a `u32`, and it lands at a position
/// the answer also counts in a `u32`. Four billion rows is a sort of something like a hundred
/// gigabytes, which is past where this operator should be asked anyway, and saying so is better
/// than an index that wrapped and an answer in the wrong order.
fn too_many() -> Error {
    Error::internal("a sort of more than 4294967295 rows")
}

/// A sort whose two halves disagree about how its keys are held.
///
/// Which way they are held is decided once, in [`Sort::new`], and every instance is built from that
/// decision, so the only way here is a bug in this file. It is an error rather than a fallback
/// because falling back means one instance's rows sorted one way and another's the other, which is
/// not an order.
fn mismatched() -> Error {
    Error::internal("a sort holding its keys two ways at once")
}

/// Where two rows sit relative to each other, with a tie on every key settled by where they arrived.
///
/// This is what makes the answer the same however many threads ran. [`compare`] on its own leaves
/// tied rows to the stability of the sort, which is the order they were handed over, and that is the
/// order the threads finished in.
pub(crate) fn settled(
    keys: &[SortKey],
    left: &Sortable,
    right: &Sortable,
    failure: &mut Option<Error>,
) -> Ordering {
    match compare(keys, &left.0, &right.0, failure) {
        Ordering::Equal => left.1.cmp(&right.1),
        ordering => ordering,
    }
}

/// Where two rows of keys sit relative to each other, under the whole key list in priority order.
///
/// The first key that separates them decides, and rows that agree on every key are equal, which is
/// where the stability of the sort does the rest.
///
/// A comparison that fails is reported as equal and remembered in `failure`, because `sort_by` wants
/// a total order and has nowhere to put an error. The order that comes out of a run that failed is
/// not an order anybody looks at, since the caller returns the error instead of the rows.
pub(crate) fn compare(
    keys: &[SortKey],
    left: &[Value],
    right: &[Value],
    failure: &mut Option<Error>,
) -> Ordering {
    for (at, key) in keys.iter().enumerate() {
        let ordering = match rank(&left[at], &right[at], *key) {
            Ok(ordering) => ordering,
            Err(error) => {
                failure.get_or_insert(error);
                Ordering::Equal
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// Where two values sit relative to each other under one sort key.
///
/// The direction and the null placement are independent, which is the detail worth being careful
/// about. `ORDER BY x DESC NULLS LAST` is not `ORDER BY x NULLS FIRST` reversed: reversing the
/// whole comparison would move the nulls too, and DuckDB's answer keeps them where the query put
/// them. So the direction is applied to the comparison of two values and never to the rule that
/// places a null.
pub(crate) fn rank(left: &Value, right: &Value, key: SortKey) -> Result<Ordering> {
    match (left.is_null(), right.is_null()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(if key.nulls_first { Ordering::Less } else { Ordering::Greater }),
        (false, true) => Ok(if key.nulls_first { Ordering::Greater } else { Ordering::Less }),
        (false, false) => {
            let ordering = rudb_kernels::order(left, right)?;
            Ok(if key.descending { ordering.reverse() } else { ordering })
        }
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the rows a sort is gathering")
}

#[cfg(test)]
mod tests {
    use rudb_pipeline::{Lease, Pool};
    use rudb_vector::VECTOR_SIZE;

    use super::spread;

    /// Rows with keys that repeat a lot and an arrival that settles every tie, shuffled.
    fn shuffled(count: usize) -> Vec<(u64, u64)> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        (0..count as u64)
            .map(|arrival| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % 97, arrival)
            })
            .collect()
    }

    /// Spread over threads, the order is the one a single sort gives, for sizes around the cuts.
    #[test]
    fn a_spread_sort_puts_the_rows_where_one_sort_would() {
        let pool = Pool::new(4);
        for threads in [Lease::alone(), pool.lease(4)] {
            for count in [0, 1, VECTOR_SIZE, VECTOR_SIZE + 1, VECTOR_SIZE * 9 + 7] {
                let mut spread_out = shuffled(count);
                let mut expected = spread_out.clone();
                expected.sort_unstable();
                spread(&mut spread_out, &threads, Ord::cmp).expect("sorted");
                assert_eq!(spread_out, expected, "{count} rows on {} threads", threads.degree());
            }
        }
    }
}

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
//! column. The columns are moved once at the end, by [`gathered`], which hands each column's
//! pieces to an [`Assembly`] and lets it do the interleave as a typed copy per physical layout.
//!
//! This used to hold a `Vec<Value>` of the whole row per row. Sorting lineitem at SF1 on three
//! keys cost 197 billion instructions that way, of which most were the allocator: sixteen columns
//! a row over six million rows is around a hundred million `Value`s and thirty million of them
//! were strings. The same sort is 54 billion now, and the system time it spends asking the
//! operating system for memory went from 45.7 seconds to 2.8. See #1210.
//!
//! # The shape a sink has
//!
//! [`Sort`] is a [`Sink`], so the rows arrive through `sink`, one instance's rows are handed over
//! through `combine`, and `finalize` does the sort once after every instance has combined. On one
//! thread that is the same work in the same order as reading the input in a loop would be. On
//! several it is the shape that makes the sort possible at all, and having it now is why F4 changes
//! no operator.
//!
//! The finished chunks go into a [`Buffered`], which is a separate source rather than something
//! `finalize` hands back, for the reason [`Sink::finalize`] gives.

use std::cmp::Ordering;
use std::sync::Mutex;

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Session, Value};
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_plan::{Plan, Slice, SortKey};
use rudb_vector::{Assembly, Chunk, VECTOR_SIZE, Vector};

use crate::buffer::Buffered;
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;

/// One row on its way through a sort: the values of its keys, where it arrived, and where it is.
///
/// The payload is not here. A row is a [`Source`] into the chunks the sink kept, and the columns
/// are moved once at the end by [`gathered`] rather than carried through the sort as a boxed value
/// a field. That is the second half of what the note on #63 asks for. The first half, a key that is
/// one comparable byte string rather than a `Vec<Value>`, is still to do.
pub(crate) type Sortable = (Vec<Value>, Arrival, Source);

/// Where a row arrived: the morsel it came from and its place among the rows of that morsel.
///
/// Sixteen bytes beside a `Vec` header and whatever it points at, which is why it is carried per
/// row rather than reconstructed. What it buys is that the answer does not depend on how many
/// threads ran.
pub(crate) type Arrival = (u64, u64);

/// Where a row is: the chunk the sink kept it in and its row in that chunk.
pub(crate) type Source = (u32, u32);

/// What one row costs beside the values of its keys, which [`rows::footprint`] already counts.
const BESIDE: u64 = (size_of::<Sortable>() - size_of::<Vec<Value>>()) as u64;

/// An ordering over the input.
#[derive(Debug)]
pub(crate) struct Sort {
    keys: Vec<SortKey>,
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
    held: Mutex<Reservation>,
    out: Buffered,
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
    rows: Vec<Sortable>,
}

/// What one instance of a sort gathers before it combines.
#[derive(Debug)]
pub(crate) struct Gathered {
    held: Combined,
    scratch: Scratch,
    charged: Reservation,
    /// The morsel this instance is reading and how many of its rows have arrived.
    place: Place,
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
    ) -> Result<(Self, Buffered)> {
        let keys = plan.sort_key_list(keys).to_vec();
        let exprs: Vec<_> = keys.iter().map(|key| key.expr).collect();
        let out = Buffered::new();
        let sort = Self {
            exprs: Prepared::new(plan, &exprs, input)?,
            keys,
            types: input.types(),
            memory: memory.clone(),
            gathered: Mutex::new(Combined::default()),
            charged: Mutex::new(Vec::new()),
            held: Mutex::new(memory.reservation()),
            out: out.clone(),
        };
        Ok((sort, out))
    }
}

impl Sink for Sort {
    type Local = Gathered;

    fn local(&self) -> Gathered {
        Gathered {
            held: Combined::default(),
            scratch: self.exprs.scratch(),
            charged: self.memory.reservation(),
            place: Place::default(),
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
        // row at a time: the keys, which are still a `Value` a key a row. The payload is not read
        // here at all, which is the point of `Sortable`.
        for row in 0..chunk.len() {
            let key: Vec<Value> =
                keys.iter().map(|column| column.try_value_at(row)).collect::<Result<_>>()?;
            taken += rows::footprint(&key) + BESIDE;
            let row = u32::try_from(row).map_err(|_| too_many())?;
            local.held.rows.push((key, local.place.of(row as usize), (at, row)));
        }
        local.held.chunks.push(chunk.clone());
        local.place.past(chunk.len());
        local.charged.grow(taken)?;
        Ok(Progress::More)
    }

    fn combine(&self, local: Gathered) -> Result<()> {
        let mut gathered = self.gathered.lock().map_err(poisoned)?;
        // Appended rather than merged, because the sort has not happened yet. The order the
        // instances combine in does not decide anything, since every row carries where it arrived
        // and the comparison falls back to that when the keys tie.
        //
        // The chunk an instance's row points at is its chunk among that instance's, so it moves
        // along by however many chunks are already here.
        let base = u32::try_from(gathered.chunks.len()).map_err(|_| too_many())?;
        gathered.rows.extend(
            local.held.rows.into_iter().map(|(key, arrival, (chunk, row))| {
                (key, arrival, (chunk.saturating_add(base), row))
            }),
        );
        gathered.chunks.extend(local.held.chunks);
        self.charged.lock().map_err(poisoned)?.push(local.charged);
        Ok(())
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        let Combined { chunks, mut rows } =
            std::mem::take(&mut *self.gathered.lock().map_err(poisoned)?);
        let mut failure: Option<Error> = None;
        rows.sort_by(|left, right| settled(&self.keys, left, right, &mut failure));
        if let Some(error) = failure {
            return Err(error);
        }
        let mut held = self.held.lock().map_err(poisoned)?;
        let out = gathered(&self.types, &chunks, &rows, &mut held)?;
        self.out.fill(out)?;
        // The gathered rows are gone and the chunks are charged instead, so what the instances
        // took is given back here and not before.
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
    }
}

/// The sorted rows as chunks, with every column moved once.
///
/// This is where the sort stops being row shaped. The order is a permutation of the rows that
/// arrived, so what each column needs is for its values to be written out in that order, and an
/// [`Assembly`] is exactly that: the chunks that arrived are placed into it, each row landing at
/// the position the sort gave it, and the interleave is one typed copy per physical layout rather
/// than a `Value` a field. A string moves as sixteen bytes of view over an arena its bytes were
/// copied into once.
///
/// One column at a time, because the assembly for a column holds a second copy of that column and
/// holding one of them at a time is a column of headroom rather than a table of it. The finished
/// column is then cut into chunk sized windows, which for a page is a window and no copy.
///
/// # Errors
///
/// If a column has no layout an assembly can lay, or if the chunks pass the limit the database was
/// opened with.
fn gathered(
    types: &[LogicalType],
    chunks: &[Chunk],
    order: &[Sortable],
    held: &mut Reservation,
) -> Result<Vec<Chunk>> {
    let rows = order.len();
    if rows == 0 {
        return Ok(Vec::new());
    }
    if u32::try_from(rows).is_err() {
        return Err(too_many());
    }
    // Where each row that arrived lands, kept the way an assembly wants to be handed it, which is
    // one run of positions a chunk.
    let mut at: Vec<Vec<u32>> = chunks.iter().map(|chunk| vec![0; chunk.len()]).collect();
    for (rank, &(_, _, (chunk, row))) in order.iter().enumerate() {
        let Some(place) = at.get_mut(chunk as usize).and_then(|run| run.get_mut(row as usize))
        else {
            return Err(Error::internal("a sorted row pointing outside the chunks it came from"));
        };
        *place = rank as u32;
    }
    let blocks = rows.div_ceil(VECTOR_SIZE);
    let mut columns: Vec<Vec<Vector>> = vec![Vec::with_capacity(types.len()); blocks];
    for (position, ty) in types.iter().enumerate() {
        let mut assembly = Assembly::new(ty.clone(), rows)?;
        for (chunk, places) in chunks.iter().zip(&at) {
            assembly.place(places, chunk.column(position)?)?;
        }
        let whole = assembly.finish()?.into_pages();
        for (block, into) in columns.iter_mut().enumerate() {
            let start = block * VECTOR_SIZE;
            into.push(whole.slice(start, (rows - start).min(VECTOR_SIZE))?);
        }
    }
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
/// an [`Assembly`] also counts in a `u32`. Four billion rows is a sort of something like a hundred
/// gigabytes, which is past where this operator should be asked anyway, and saying so is better
/// than an index that wrapped and an answer in the wrong order.
fn too_many() -> Error {
    Error::internal("a sort of more than 4294967295 rows")
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

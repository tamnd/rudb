//! Sorting when only the first few rows are wanted.
//!
//! `ORDER BY x LIMIT 10` over a hundred million rows has the same answer as a sort with a limit over
//! it and nothing like the same cost. A sort holds every row it was given, because the last row of
//! the input can be the first row of the output, so the memory it takes is the size of the input and
//! the time it takes is a full sort of it. Ten rows are wanted. This holds the rows that could still
//! come out and throws the rest away as it goes.
//!
//! What has to be held is `count + offset` rows, not `count`, since the rows that are skipped still
//! have to be found before there is anything to skip them from.
//!
//! # A sorted bound rather than a heap
//!
//! The textbook answer is a binary heap of the bound, pushing every row and popping the worst. This
//! keeps the candidates sorted instead. A row first compares with the worst candidate and is
//! discarded without materializing its payload when it loses. A winner is inserted after existing
//! equal keys, which preserves input order for ties the same way the stable full sort does.
//!
//! Binary search makes the comparison cost `O(m log n)`, as it is for a heap. Inserting moves `n`
//! small row handles, but only a shrinking share of the input wins after the first `n` rows. The
//! important saving is that almost every row pays for its key and never becomes a heap-allocated
//! payload row. A large offset makes those moves expensive, so bounds above 64 keep the batched
//! sort and trim path until normalized keys make a heap cheap.
//!
//! # Rejecting a chunk in one pass instead of a row at a time
//!
//! Almost every row of a million loses to the ten already held, and the cheap way to find that out
//! is the comparison kernel rather than a loop. Once the candidates are full, the worst of them is
//! a constant for the length of a chunk, so one vectorized comparison of the first key column
//! against that constant says which rows are still worth looking at, and on `ORDER BY EventTime
//! LIMIT 10` over ClickBench that is almost none of them after the first chunk. The rows it hands
//! back go down the same row at a time path as before, which is what keeps the answer the same.
//!
//! # Keeping a row without reading it
//!
//! A candidate that is kept is usually beaten later, and on ClickBench 24 an instance keeps about
//! sixty seven rows to hand ten back. Reading a row out of the chunk is what that costs, and for a
//! text column read out of a native file it is the most expensive read there is: the dictionary is
//! compressed in blocks and one value means one block decoded. A wide row pays that per string
//! column, for a row nobody ends up asking for.
//!
//! So a candidate holds a code where the column gave it one, and the value is read in `finalize`,
//! for the rows that came out on top and no others. See [`Cell`]. The chunk is gone by then and the
//! dictionary is not, because it belongs to the file rather than to the chunk, and holding it is one
//! pointer against the string it names.
//!
//! # Comparing two rows without reading either
//!
//! A key is not like the rest of the row. It is read because it is compared, so holding it as a code
//! only pays if the comparison can be made on codes too, and a code on its own cannot: two codes in
//! first appearance order say nothing about which of their two strings sorts first.
//!
//! A rank can. A native text column stores the sorted order of its dictionary, and the rank of a
//! code is where that code's value sits in it, so two ranks in the same dictionary compare exactly
//! as the two strings do and the comparison is one integer against another. A candidate's key is
//! kept as a code and a rank when the column gave it both, and a row is rejected against it by
//! looking its own rank up, which is two loads. Nothing decodes. On ClickBench 25, `ORDER BY
//! SearchPhrase LIMIT 10`, reading the row's value to compare it was forty three percent of the
//! whole query.
//!
//! Anything the dictionary cannot answer falls back to what it did before: the value is read on both
//! sides and compared as a value. That covers a key that is not a text column, a null, a chunk
//! arriving over a different dictionary from the one a candidate was kept from, and a file with no
//! stored order. Ranks from two different dictionaries are never compared, because a rank means
//! nothing outside the dictionary it was read out of.
//!
//! The bound moves while the chunk is being walked, since a winner replaces the worst candidate, so
//! what the pass produces is a superset of the rows that really win. That is the point: it is a
//! filter and not the decision, and every row it keeps is compared again properly.
//!
//! The batched path gets the same pass, one step behind. It does not hold its candidates in order,
//! so it only knows its worst one just after a trim, and that key is what it rejects against until
//! the next trim. It stays a true bound in between, because the running only improves: once the
//! bound of candidates are all at least as good as that key, nothing worse than it can finish
//! inside the bound. This matters more here than it does above, not less. A row this path keeps
//! costs a `Vec` for its key and a `Vec` for the whole row with a `Value` per column, so on
//! ClickBench's four `LIMIT 10 OFFSET 1000` queries, which come out of a group by with a `URL` in
//! the key, building those for every row was most of what the operator did.
//!
//! That key is also what the row under the pass rejects against, the same one comparison the sorted
//! path makes before it materializes anything, and the trim that sets it runs inside the row loop
//! rather than at the end of a chunk. Both of those are there for the instance that is handed fewer
//! rows than twice its bound. Thirteen thousand groups spread over thirty two instances is four
//! hundred rows each, and with the trim at the end of the chunk none of them reached it, so none of
//! them ever had a cut, so none of them rejected anything and all thirteen thousand rows were built
//! in full. That query cost four times what the same query one row under `SORTED_BOUND` cost, for
//! one more row of answer.
//!
//! Three things make it step aside and look at every row instead. A worst candidate whose first key
//! is null, because then what beats it depends on where the query puts nulls and the comparison
//! kernel answers null rather than true. A `NULLS FIRST` ordering over a column that has nulls, for
//! the same reason read the other way: those rows win and a comparison against a value says nothing
//! about them. And a comparison the kernel refuses, which is left to the row path so that the error
//! comes out of the same place it came out of before. With more than one sort key the pass keeps
//! ties on the first one, since the second key can still turn a tie into a win.
//!
//! # The shape a sink has
//!
//! Like the sort, this is a [`Sink`]. The difference between them is where the trimming happens: an
//! instance trims what it holds as it goes, and `combine` trims again over what two instances
//! brought, which is what keeps the bound the bound rather than the bound times the number of
//! threads. On one thread there is one instance and the second trim does nothing.
//!
//! Every candidate carries where it arrived, for the reason the sort's own documentation gives: a
//! tie on every key is settled by that rather than by which thread got there first, so the ten rows
//! this hands back are the ten a single thread would have handed back. It costs sixteen bytes per
//! candidate and the bound is ten.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::sync::{Arc, Mutex};

use rudb_common::bounds::Bound;
use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Session, Value};
use rudb_kernels::{
    Comparison, compare as compare_vectors, rank_at, rank_within, select_against_rank, selection,
};
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_plan::{Plan, Slice, SortKey};
use rudb_vector::{Chunk, Selection, Vector};

use crate::buffer::Buffered;
use crate::cutoff::Cutoff;
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;
use crate::sort::{Place, rank};

/// Above this bound, moving a sorted candidate array costs more than trimming in batches.
const SORTED_BOUND: usize = 64;

/// One row in the running: its keys, the row itself, and where it arrived.
#[derive(Debug)]
struct Candidate {
    /// The key cells and then the row's, in one `Vec` rather than two. A kept row costs one
    /// allocation instead of two, and a candidate is smaller to move when the running is trimmed.
    cells: Vec<Cell>,
    /// How many of `cells` are the key.
    keyed: usize,
    arrival: crate::sort::Arrival,
    /// The one key as the integer it is stored as, where the key is one column of a type whose
    /// order is the order of that integer and this row's value of it is not null.
    ///
    /// Two candidates that both have one compare on it and nothing else, see [`ordered`]. Anything
    /// else, a null on either side included, goes through the cells as it always has.
    ordinal: Option<i128>,
}

impl Candidate {
    /// The key cells, which are the first ones.
    fn key(&self) -> &[Cell] {
        &self.cells[..self.keyed]
    }
}

/// One column of a candidate row, which is either the value or what it takes to read it later.
///
/// A candidate is kept because it beats the worst of the bound held so far, and almost every one of
/// them is beaten in turn by something that arrives after it. Ten rows come out of an instance and
/// on ClickBench 24 about sixty seven go in, so most of the rows this holds are read out of the
/// columns, allocated, and thrown away.
///
/// A column read out of a native file arrives as codes over a dictionary the whole query shares, and
/// a code is four bytes that name the value without reading it. So a candidate coming off such a
/// column keeps the code, and the value is read in `finalize` for the rows that came out on top.
/// ClickBench 24 went from 130 million instructions to 87 million on that, a third of the query.
///
/// A key is the same cell with one more thing in it. It is held to be compared rather than to be
/// handed back, so a code alone would not save the read: two codes say nothing about which of their
/// values sorts first. A rank does, and where the dictionary knows its own order the rank of the
/// code is free at the moment the row is kept. See [`Cell::keyed`] and [`at_row`].
///
/// Everything else is read where it is met. A value that is not a code has to be copied out of the
/// chunk before the chunk goes, and a null is a value like any other here, since the dictionary has
/// no code for one.
#[derive(Debug, Clone)]
enum Cell {
    /// The value, read out of the chunk that carried it.
    Ready(Value),
    /// The code that names the value, and the dictionary to read it out of.
    ///
    /// `rank` is where the value sits in that dictionary's own order, and it is `None` for every
    /// cell of the row that is not a key. Working it out means inverting the order once per
    /// dictionary, and there is no reason to pay that for a column nobody is going to compare.
    Coded { dictionary: Arc<Vector>, code: u32, rank: Option<u32> },
}

impl Cell {
    /// The cell for one row of one column, keeping the code where the column has one.
    fn of(column: &Vector, row: usize) -> Result<Self> {
        if let Some((codes, dictionary)) = column.shared_dictionary_parts()
            && column.validity().is_valid(row)
            && let Some(code) = codes.get(row)
        {
            let dictionary = Arc::clone(dictionary);
            return Ok(Self::Coded { dictionary, code: *code, rank: None });
        }
        Ok(Self::Ready(column.try_value_at(row)?))
    }

    /// The cell for one row of one key column, which is a code only when it comes with a rank.
    ///
    /// The difference from [`Cell::of`] is that a code without a rank is no use here. The key of a
    /// candidate is read every time a row is rejected against it, so holding a code that has to be
    /// turned back into a value to be compared would read the same value over and over instead of
    /// once.
    fn keyed(column: &Vector, row: usize) -> Result<Self> {
        match placed(column, row) {
            Some(cell) => Ok(cell),
            None => Ok(Self::Ready(column.try_value_at(row)?)),
        }
    }

    /// The value, read now where it was not read when the row was kept.
    ///
    /// # Errors
    ///
    /// Whatever the dictionary raises when it cannot read the value the code names.
    fn value(self) -> Result<Value> {
        match self {
            Self::Ready(value) => Ok(value),
            Self::Coded { dictionary, code, .. } => dictionary.try_value_at(code as usize),
        }
    }

    /// The value without giving the cell up, borrowed where it is already there.
    ///
    /// For the callers that need a value out of a key and are not finished with the key: a bound to
    /// publish to the scan, and the fallback comparison for a pair of cells that ranks cannot
    /// settle. Nothing is copied unless the value has to be decoded.
    ///
    /// # Errors
    ///
    /// Whatever the dictionary raises when it cannot read the value the code names.
    fn read(&self) -> Result<Cow<'_, Value>> {
        match self {
            Self::Ready(value) => Ok(Cow::Borrowed(value)),
            Self::Coded { dictionary, code, .. } => {
                dictionary.try_value_at(*code as usize).map(Cow::Owned)
            }
        }
    }

    /// What this owns away from itself, which for a code is nothing.
    fn footprint(&self) -> usize {
        match self {
            Self::Ready(value) => value.footprint(),
            Self::Coded { .. } => 0,
        }
    }
}

/// The key cell for a row that arrived as a code in a dictionary that knows its own order.
///
/// `None` for anything else, which the caller reads as a value instead. The code is looked up a
/// second time rather than threaded out of `rank_at`, because this runs only when a row is kept and
/// what a rank is stays in one place.
fn placed(column: &Vector, row: usize) -> Option<Cell> {
    let (dictionary, rank) = rank_at(column, row)?;
    let (codes, _) = column.shared_dictionary_parts()?;
    let code = *codes.get(row)?;
    Some(Cell::Coded { dictionary, code, rank: Some(rank) })
}

/// What one candidate row of cells is charged.
///
/// The same count [`rows::footprint`] gives a row of values, with a code charged for the four bytes
/// it is rather than for the value it names. That is the honest number: the value is not there.
fn charge(values: &[Cell]) -> u64 {
    let bytes = size_of::<Vec<Cell>>()
        + values.iter().map(Cell::footprint).sum::<usize>()
        + size_of_val(values)
        + usize::try_from(rudb_common::ALLOCATION).unwrap_or(0);
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// Where two candidates sit relative to each other, ties settled by where they arrived.
///
/// What [`crate::sort::settled`] does for the sort's own row, which this cannot use because a
/// candidate holds its keys as cells rather than as values.
fn settled(
    keys: &[SortKey],
    left: &Candidate,
    right: &Candidate,
    failure: &mut Option<Error>,
) -> Ordering {
    let ordering = ordered(keys, left.ordinal, right.ordinal)
        .unwrap_or_else(|| compare(keys, left.key(), right.key(), failure));
    match ordering {
        Ordering::Equal => left.arrival.cmp(&right.arrival),
        ordering => ordering,
    }
}

/// Where two single keys sit by the integers they are stored as, when both of them have one.
///
/// The answer [`compare`] gives for the same two rows. A candidate only has an ordinal when the key
/// list is one column of a type that orders as its stored integer, see [`orders_as_stored`], and the
/// value is not null, so the null placement has nothing to say and only the direction does. On
/// ClickBench 43 the candidates are the 1423 minutes of a day and a half ordered by a `TIMESTAMP`,
/// and comparing them as values was a fifth of the query.
fn ordered(keys: &[SortKey], left: Option<i128>, right: Option<i128>) -> Option<Ordering> {
    let ordering = left?.cmp(&right?);
    Some(if keys.first()?.descending { ordering.reverse() } else { ordering })
}

/// Whether a key of this type sorts the way the integer [`Vector::signed_at`] reads out of it does.
///
/// The signed integers, and the types stored in one where a bigger number is a later or bigger
/// value. A decimal is here because one column has one scale, so its unscaled values order as the
/// decimals do. The unsigned integers are not, because `signed_at` does not read them.
fn orders_as_stored(logical: &LogicalType) -> bool {
    matches!(
        logical,
        LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::HugeInt
            | LogicalType::Decimal { .. }
            | LogicalType::Date
            | LogicalType::Time
            | LogicalType::Timestamp
            | LogicalType::TimestampS
            | LogicalType::TimestampMs
            | LogicalType::TimestampNs
            | LogicalType::TimestampTz
    )
}

/// The ordinal of one row's key, when the operator keeps them and the row's key is not null.
fn ordinal_at(ordinal: bool, keys: &[Vector], row: usize) -> Option<i128> {
    if !ordinal {
        return None;
    }
    keys.first()?.signed_at(row)
}

/// Where two rows of key cells sit relative to each other, under the key list in priority order.
///
/// [`crate::sort::compare`] over cells. The first key that separates them decides and the rest are
/// never looked at, which is the reason a key is compared rather than read.
fn compare(
    keys: &[SortKey],
    left: &[Cell],
    right: &[Cell],
    failure: &mut Option<Error>,
) -> Ordering {
    for (at, key) in keys.iter().enumerate() {
        let ordering = match place(&left[at], &right[at], *key) {
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

/// Where two key cells sit relative to each other under one sort key.
///
/// Two ranks in the same dictionary answer it outright, because a dictionary's order is the order of
/// its values and the ranks are positions in it. The identity check is what makes that true and it
/// is a pointer comparison: a rank is a position in one dictionary and means nothing in another.
///
/// Neither side can be null in that case, since a null row is one of the things that stops a cell
/// being held as a code at all, so the null placement of the sort key has nothing to say here and
/// only the direction does.
///
/// # Errors
///
/// Whatever reading either value raises, and whatever comparing two values of different types does.
fn place(left: &Cell, right: &Cell, key: SortKey) -> Result<Ordering> {
    match (left, right) {
        (Cell::Ready(here), Cell::Ready(there)) => rank(here, there, key),
        (
            Cell::Coded { dictionary: one, rank: Some(here), .. },
            Cell::Coded { dictionary: other, rank: Some(there), .. },
        ) if Arc::ptr_eq(one, other) => {
            let ordering = here.cmp(there);
            Ok(if key.descending { ordering.reverse() } else { ordering })
        }
        _ => {
            let (here, there) = (left.read()?, right.read()?);
            rank(&here, &there, key)
        }
    }
}

/// The first rows of an ordering, without holding the rest.
#[derive(Debug)]
pub(crate) struct TopN {
    keys: Vec<SortKey>,
    /// The key expressions, evaluated against the input's schema.
    exprs: Prepared,
    /// The input's types, which are also the output's.
    types: Vec<LogicalType>,
    /// Whether candidates carry their key as an integer as well, see [`Candidate::ordinal`].
    ordinal: bool,
    /// How many rows to emit, once the ones to skip have been skipped.
    count: usize,
    /// How many rows to skip first.
    offset: usize,
    /// `count + offset`, which is how many rows can still turn out to be wanted.
    bound: usize,
    memory: Memory,
    /// What every instance brought, already trimmed to the bound.
    rows: Mutex<Vec<Candidate>>,
    /// What those rows are charged, given back once the finished chunks are charged instead.
    charged: Mutex<Vec<Reservation>>,
    /// What the finished chunks are charged, held for as long as they are readable.
    held: Mutex<Reservation>,
    /// How good a row has to be to still be wanted, told to the scan below as this fills up.
    ///
    /// Empty for a top N with nothing under it that could use one, which is every shape but a scan
    /// under a filter under a projection. See [`crate::cutoff`].
    cutoff: Option<Arc<Cutoff>>,
    out: Buffered,
}

/// What one instance of a top N holds while it runs.
#[derive(Debug)]
pub(crate) struct Running {
    kept: Vec<Candidate>,
    scratch: Scratch,
    charged: Reservation,
    failure: Option<Error>,
    /// The morsel this instance is reading and how many of its rows have arrived.
    place: Place,
    /// The key of the worst candidate this instance has room for, once it has the bound of them.
    ///
    /// `None` until the first trim fills the running, and set at every trim after that. Only the
    /// batched path keeps it, since the sorted path reads the same key straight out of `kept`, which
    /// it holds in order at all times.
    ///
    /// It stays true between trims because the running only improves. Once the bound of candidates
    /// are all at least as good as this key, a row worse than it cannot finish inside the bound, and
    /// neither can one that ties it, because everything it ties arrived first.
    cut: Option<Vec<Cell>>,
    /// Whether the worst candidate has changed since the last time it was published to the cutoff.
    ///
    /// Publishing means reading the worst candidate's first key as a value, and a coded key is not
    /// read until somebody asks. Without this the sorted path would decode one dictionary block per
    /// chunk to hand the scan a bound it was given already. Every insert moves the worst candidate,
    /// whether the running was full before it or not, so the flag is set in the one place that
    /// inserts.
    moved: bool,
}

impl TopN {
    /// Applies the session semantics to the prepared sort keys.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.exprs = self.exprs.in_session(session);
        self
    }

    /// # Errors
    ///
    /// If a sort key does not resolve against the input's schema.
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
        keys: Slice,
        count: u64,
        offset: u64,
        memory: &Memory,
    ) -> Result<(Self, Buffered)> {
        // A limit past what a `Vec` can hold is a limit nothing reaches, so saturating here turns a
        // bound nobody can hit into the largest one this machine has room for, and the operator
        // degenerates into the sort it would have been.
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        let keys = plan.sort_key_list(keys).to_vec();
        let exprs: Vec<_> = keys.iter().map(|key| key.expr).collect();
        let ordinal = matches!(exprs.as_slice(), [only] if orders_as_stored(plan.expr_type(*only)));
        let out = Buffered::new();
        let top = Self {
            ordinal,
            exprs: Prepared::new(plan, &exprs, input)?,
            keys,
            types: input.types(),
            count,
            offset,
            bound: count.saturating_add(offset),
            memory: memory.clone(),
            rows: Mutex::new(Vec::new()),
            charged: Mutex::new(Vec::new()),
            held: Mutex::new(memory.reservation()),
            cutoff: None,
            out: out.clone(),
        };
        Ok((top, out))
    }

    /// Tells this to publish its worst candidate to `cutoff` as it goes.
    ///
    /// Taken whether the cutoff was armed or not, because the builder makes one before it walks into
    /// the input and only finds out afterwards whether the walk reached a scan. An unarmed one costs
    /// a load and a branch per chunk and is never read by anybody.
    #[must_use]
    pub(crate) fn telling(mut self, cutoff: Arc<Cutoff>) -> Self {
        self.cutoff = Some(cutoff);
        self
    }

    /// Says how good a row now has to be, given the worst of a full set of candidates.
    ///
    /// Only the first key, because a part of the file whose first key is all worse than this is worse
    /// whatever its later keys hold, and one that ties on the first key says nothing. A null first
    /// key publishes nothing, which under the `NULLS LAST` this is only ever armed for means the
    /// candidates do not yet rule out any value at all.
    ///
    /// The arming is checked before the key is read rather than after, because a coded key would
    /// decode a dictionary block to find out that nobody was listening. A read that fails publishes
    /// nothing and says nothing, since a cutoff is a hint and the same candidate is read again in
    /// `finalize` where an error has somewhere to go.
    fn reached(&self, worst: &[Cell]) {
        let Some(cutoff) = self.cutoff.as_ref().filter(|cutoff| cutoff.armed()) else { return };
        let Some(first) = worst.first() else { return };
        let Ok(value) = first.read() else { return };
        let Some(bound) = Bound::of_value(&value) else { return };
        cutoff.reached(bound);
    }

    /// Offers one row to the unordered candidates, and returns what holding it took.
    ///
    /// Two things happen before the row is read out of the columns, and both of them are the sorted
    /// path's, brought down here. The trim, so that an instance that never sees twice its bound
    /// still ends up with a cut. And the one comparison against that cut, which is the whole reason
    /// the sorted path is cheap: a row that loses costs one [`Value`] read and never allocates.
    ///
    /// The cut can be a trim or more behind, and rejecting against a stale one is still right. The
    /// candidates only improve, so a key that could not beat the worst of them then cannot beat the
    /// worst of them now. That is the same argument the pass above the loop already runs on.
    fn offer(
        &self,
        keys: &[Vector],
        chunk: &Chunk,
        row: usize,
        local: &mut Running,
        trimmed: &mut bool,
    ) -> Result<u64> {
        let Running { kept, failure, place, cut, .. } = local;
        if kept.len() > self.bound.saturating_mul(2) {
            trim(&self.keys, kept, self.bound, failure);
            *trimmed = true;
            *cut = (kept.len() == self.bound && self.bound > 0)
                .then(|| kept[self.bound - 1].key().to_vec());
            if let Some(reached) = cut.as_ref() {
                self.reached(reached);
            }
        }
        let lost = cut
            .as_ref()
            .is_some_and(|cut| against(&self.keys, keys, row, cut, failure) != Ordering::Less);
        if lost {
            return Ok(0);
        }
        let arrival = place.of(row);
        let ordinal = ordinal_at(self.ordinal, keys, row);
        hold(keys, chunk, row, (arrival, ordinal), kept)
    }
}

impl Sink for TopN {
    type Local = Running;

    fn local(&self) -> Running {
        Running {
            kept: Vec::new(),
            scratch: self.exprs.scratch(),
            charged: self.memory.reservation(),
            failure: None,
            place: Place::default(),
            cut: None,
            moved: false,
        }
    }

    fn at(&self, morsel: &rudb_pipeline::Morsel, local: &mut Running) -> Result<()> {
        local.place.start(morsel.index());
        Ok(())
    }

    fn sink(&self, chunk: &Chunk, local: &mut Running) -> Result<Progress> {
        let mut keys = Vec::with_capacity(self.keys.len());
        self.exprs.evaluate(chunk, &mut local.scratch, &mut keys)?;
        if self.bound <= SORTED_BOUND {
            let rows = chunk.len();
            let full = self.bound > 0 && local.kept.len() == self.bound;
            // The rank pass where the worst candidate's first key carries a rank and the value pass
            // where it does not. Both answer the same question and the second one searches the
            // dictionary to do it. The second is still tried when the first declines, because a
            // chunk that does not arrive over the dictionary the rank belongs to is a chunk the
            // rank says nothing about, and the search says as much as it ever did.
            let narrowed = full
                .then(|| {
                    let worst = local.kept[self.bound - 1].key();
                    let ranked = beats_rank(&self.keys, &keys, worst, rows);
                    ranked.or_else(|| worth_looking_at(&self.keys, &keys, worst, rows))
                })
                .flatten();
            let offer = |row: usize, local: &mut Running| {
                let arrival = local.place.of(row);
                let ordinal = ordinal_at(self.ordinal, &keys, row);
                keep(
                    Where { keys: &self.keys, columns: &keys, chunk, row, arrival, ordinal },
                    local,
                    self.bound,
                );
            };
            match narrowed {
                // row at a time: the rows the pass kept are the ones that can still win, and each
                // of them has to be placed among the candidates rather than counted.
                Some(kept) => {
                    for row in kept.iter() {
                        offer(row, local);
                    }
                }
                // row at a time: the key still has the same Value layout the sort holds, and 2i
                // (#63) replaces it with one normalized comparable byte string per row.
                None => {
                    for row in 0..rows {
                        offer(row, local);
                    }
                }
            }
            local.place.past(chunk.len());
            recharge(&local.kept, &mut local.charged)?;
            if local.moved && self.bound > 0 && local.kept.len() == self.bound {
                local.moved = false;
                self.reached(local.kept[self.bound - 1].key());
            }
            return Ok(Progress::More);
        }
        // The same question the sorted path asks, asked once the running is full rather than on
        // every chunk from the start, because this path only knows its worst candidate after a trim.
        // Without it a `LIMIT 10 OFFSET 1000` builds a `Vec<Value>` for the key and another for
        // every column of every row that reaches it, however hopeless the row is, and on a wide row
        // with a string in it that is most of what the operator does.
        let narrowed = local
            .cut
            .as_ref()
            .and_then(|cut| worth_looking_at(&self.keys, &keys, cut, chunk.len()));
        let mut taken = 0;
        let mut trimmed = false;
        match narrowed {
            // row at a time: the rows the pass kept are the ones that can still win, and each of
            // them has to be read out of the columns rather than counted.
            Some(rows) => {
                for row in rows.iter() {
                    taken += self.offer(&keys, chunk, row, local, &mut trimmed)?;
                }
            }
            // row at a time: the key still has the same Value layout the sort holds, and 2i (#63)
            // replaces it with one normalized comparable byte string per row.
            None => {
                for row in 0..chunk.len() {
                    taken += self.offer(&keys, chunk, row, local, &mut trimmed)?;
                }
            }
        }
        local.place.past(chunk.len());
        local.charged.grow(taken)?;
        // Only when a trim ran, since settling walks every candidate and the trims are inside the
        // loop now. Without one the reservation already holds exactly what the candidates weigh.
        if trimmed {
            recharge(&local.kept, &mut local.charged)?;
        }
        Ok(Progress::More)
    }

    fn combine(&self, mut local: Running) -> Result<()> {
        if let Some(error) = local.failure {
            return Err(error);
        }
        let mut rows = self.rows.lock().map_err(poisoned)?;
        rows.extend(local.kept);
        // Trimmed here as well as in the instance, so that combining thirty two instances holding
        // the bound each leaves the bound and not thirty two times it.
        let mut failure = None;
        trim(&self.keys, &mut rows, self.bound, &mut failure);
        if let Some(error) = failure {
            return Err(error);
        }
        recharge(&rows, &mut local.charged)?;
        self.charged.lock().map_err(poisoned)?.push(local.charged);
        Ok(())
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        let mut kept = std::mem::take(&mut *self.rows.lock().map_err(poisoned)?);
        // The one place the answer has to be in order. Everything before this keeps the best rows it
        // has seen without caring which of them is best, because a row that is thrown away later was
        // never worth placing among the ones that were not.
        let mut failure = None;
        settle(&self.keys, &mut kept, self.offset, self.count, &mut failure);
        if let Some(error) = failure {
            return Err(error);
        }
        let wanted = kept.into_iter().skip(self.offset).take(self.count);
        // The only place a candidate's row is read, which is why a candidate holds codes rather than
        // values: everything that got this far and lost was never read at all.
        let ordered: Vec<Vec<Value>> = wanted
            .map(|candidate| {
                let keyed = candidate.keyed;
                candidate.cells.into_iter().skip(keyed).map(Cell::value).collect()
            })
            .collect::<Result<_>>()?;
        let mut held = self.held.lock().map_err(poisoned)?;
        let chunks = rows::chunks(&self.types, &ordered, &mut held)?;
        self.out.fill(chunks)?;
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the rows a top N is keeping")
}

/// Keeps the best `bound` of what is held, in no particular order.
///
/// Ties are settled by where the rows arrived rather than by the order they were handed over, which
/// is what makes the trim in `combine` give the same answer whichever thread combined first. That
/// also makes the ordering a total one, since no two rows arrived in the same place, so the answer
/// does not depend on the trim being stable and the trim does not have to be a sort.
///
/// It used to be one. Ordering what is held answers which of it to keep, but it answers a great deal
/// more than that, and what is thrown away is most of the work: a partitioning around the `bound`th
/// best row walks the candidates a couple of times where a sort walks them the logarithm of their
/// count. On ClickBench 39, where a `LIMIT 10 OFFSET 1000` comes out of an aggregate that has
/// already cut itself to 1010 rows, that is 1010 comparisons and change against ten thousand.
///
/// Nothing downstream wants the order this no longer produces until the very end, where
/// [`settle`] puts the rows the offset and the count ask for into it and leaves the rest alone.
fn trim(keys: &[SortKey], kept: &mut Vec<Candidate>, bound: usize, failure: &mut Option<Error>) {
    if kept.len() <= bound {
        return;
    }
    if bound == 0 {
        kept.clear();
        return;
    }
    kept.select_nth_unstable_by(bound - 1, |left, right| settled(keys, left, right, failure));
    kept.truncate(bound);
}

/// Puts the rows an offset and a count ask for into order, and throws the rest away.
///
/// The rows before the offset are kept where they are rather than sorted, because nobody reads them
/// and the only thing wanted of them is that they really are the ones that sort first. A partition
/// around the offset says exactly that and says nothing else, which is the point.
fn settle(
    keys: &[SortKey],
    kept: &mut Vec<Candidate>,
    offset: usize,
    count: usize,
    failure: &mut Option<Error>,
) {
    trim(keys, kept, offset.saturating_add(count), failure);
    if offset >= kept.len() {
        kept.clear();
        return;
    }
    if offset > 0 {
        kept.select_nth_unstable_by(offset, |left, right| settled(keys, left, right, failure));
    }
    kept[offset..].sort_unstable_by(|left, right| settled(keys, left, right, failure));
}

/// Reads one row out of the columns and puts it among the candidates, unordered.
///
/// What the batched path does with a row it has decided to keep, and the reason it costs what it
/// costs: a `Vec` for the key and the row, and a value per column of each. A column that arrives as
/// codes is kept as a code, see [`Cell`]. The answer is what the cells are charged.
fn hold(
    keys: &[Vector],
    chunk: &Chunk,
    row: usize,
    (arrival, ordinal): (crate::sort::Arrival, Option<i128>),
    kept: &mut Vec<Candidate>,
) -> Result<u64> {
    let cells = read_row(keys, chunk, row)?;
    let taken = charge(&cells);
    kept.push(Candidate { cells, keyed: keys.len(), arrival, ordinal });
    Ok(taken)
}

/// One row's key cells followed by its own, read out of the columns into one `Vec`.
fn read_row(keys: &[Vector], chunk: &Chunk, row: usize) -> Result<Vec<Cell>> {
    let mut cells = Vec::with_capacity(keys.len() + chunk.columns().len());
    for column in keys {
        cells.push(Cell::keyed(column, row)?);
    }
    for column in chunk.columns() {
        cells.push(Cell::of(column, row)?);
    }
    Ok(cells)
}

/// Keeps one row when its key belongs in the ordered prefix.
///
/// The key is read out of the columns a value at a time and only as far as the first key that
/// separates it from the worst candidate, so a row that loses on the first of three keys costs one
/// value rather than three and never allocates the `Vec` that holds them. Almost every row loses.
fn keep(
    Where { keys, columns, chunk, row, arrival, ordinal }: Where<'_>,
    local: &mut Running,
    bound: usize,
) {
    if bound == 0 {
        return;
    }
    let failure = &mut local.failure;
    if local.kept.len() == bound
        && against(keys, columns, row, local.kept[bound - 1].key(), failure) != Ordering::Less
    {
        return;
    }
    let cells = match read_row(columns, chunk, row) {
        Ok(cells) => cells,
        Err(error) => {
            failure.get_or_insert(error);
            return;
        }
    };
    let key = &cells[..columns.len()];
    // After every candidate whose key it ties, which is where its arrival puts it too: an instance
    // reads the morsels it is given in order and each of them from the start, so a row reaching
    // here arrived after everything already held.
    let at = local.kept.partition_point(|candidate| {
        ordered(keys, candidate.ordinal, ordinal)
            .unwrap_or_else(|| compare(keys, candidate.key(), key, failure))
            != Ordering::Greater
    });
    let keyed = columns.len();
    local.kept.insert(at, Candidate { cells, keyed, arrival, ordinal });
    local.kept.truncate(bound);
    local.moved = true;
}

/// One row being offered to the candidates, which is six things that only travel together.
struct Where<'a> {
    keys: &'a [SortKey],
    columns: &'a [Vector],
    chunk: &'a Chunk,
    row: usize,
    arrival: crate::sort::Arrival,
    ordinal: Option<i128>,
}

/// Where one row of the key columns sits against a key already held.
///
/// The same answer [`compare`] gives for the same two keys, with the row read straight out of the
/// columns rather than out of a `Vec` built for the purpose. This is the reject, so it runs on every
/// row that reaches the operator and almost all of them lose on the first key.
fn against(
    keys: &[SortKey],
    columns: &[Vector],
    row: usize,
    held: &[Cell],
    failure: &mut Option<Error>,
) -> Ordering {
    for (at, key) in keys.iter().enumerate() {
        let ordering = match at_row(&columns[at], row, &held[at], *key) {
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

/// Where one row of one key column sits against one key cell already held.
///
/// [`place`] with the left side still in the chunk. A held key that is a rank asks the column where
/// this row sits in the same dictionary, which is two loads and an integer compare, and the row's
/// value is never read. Everything else reads it, which is what this did for every row before.
///
/// A key already sitting there as a value goes straight out the top, rather than through
/// [`Cell::read`], because that path is what a key that is not a text column out of a file does for
/// every row of the input and building a `Cow` around a borrow it already had was worth four percent
/// of ClickBench 24.
///
/// # Errors
///
/// Whatever reading either side raises, and whatever comparing two values of different types does.
fn at_row(column: &Vector, row: usize, held: &Cell, key: SortKey) -> Result<Ordering> {
    let (dictionary, code, place) = match held {
        Cell::Ready(there) => return rank(&column.try_value_at(row)?, there, key),
        Cell::Coded { dictionary, code, rank } => (dictionary, code, rank),
    };
    if let Some(there) = place
        && let Some(here) = rank_within(column, row, dictionary)
    {
        let ordering = here.cmp(there);
        return Ok(if key.descending { ordering.reverse() } else { ordering });
    }
    let (here, there) = (column.try_value_at(row)?, dictionary.try_value_at(*code as usize)?);
    rank(&here, &there, key)
}

/// The rows of a chunk that can still beat `worst`, or nothing when every row has to be looked at.
///
/// One comparison of the first key column against a constant, for the reasons in the module doc.
fn worth_looking_at(
    keys: &[SortKey],
    columns: &[Vector],
    worst: &[Cell],
    rows: usize,
) -> Option<Selection> {
    let key = *keys.first()?;
    let bound = worst.first()?.read().ok()?;
    let column = columns.first()?;
    if bound.is_null() || (key.nulls_first && column.validity().has_nulls(rows)) {
        return None;
    }
    let op = still_wanted(key, keys.len() == 1);
    let against = Vector::constant(column.logical_type().clone(), bound.into_owned(), rows);
    // The whole chunk is in play here, so this asks the kernel that reads its operands where they
    // lie rather than the one that reads them through a selection. Handing the threaded kernel an
    // identity selection would build a `u32` a row to say "all of them", check every one of them is
    // in range, and then put a load and an indirection in front of each of the comparisons that
    // loop is otherwise three instructions long. On `ORDER BY EventTime LIMIT 10` over ClickBench
    // that was more instructions than decoding the column cost.
    let flags = compare_vectors(op, column, &against).ok()?;
    Some(selection(&flags, rows))
}

/// [`worth_looking_at`] for a worst candidate whose place in the dictionary is already known.
///
/// The same question and the same answer, without the part that costs: placing the bound. A
/// comparison against a value in a sorted dictionary starts by finding out where the value sits,
/// which is about nineteen probes and a decoded block or two for each probe the stored head cannot
/// settle. The value did not come from the query though. It came out of the same dictionary, carried
/// by a row that arrived with its code, so its rank was free at the moment it was kept and searching
/// for it again is work that was already done. See `rudb_kernels::select_against_rank` for what is
/// left once the search is gone, which is two loads and an integer compare per row.
///
/// The two guards `worth_looking_at` starts with are here too, minus the one about a null bound,
/// which cannot happen because a null row is one of the things that stops a key being held as a
/// code.
fn beats_rank(
    keys: &[SortKey],
    columns: &[Vector],
    worst: &[Cell],
    rows: usize,
) -> Option<Selection> {
    let key = *keys.first()?;
    let column = columns.first()?;
    let Cell::Coded { dictionary, rank: Some(rank), .. } = worst.first()? else { return None };
    if key.nulls_first && column.validity().has_nulls(rows) {
        return None;
    }
    select_against_rank(still_wanted(key, keys.len() == 1), column, dictionary, *rank, rows)
}

/// The comparison against the worst candidate that a row still in the running satisfies.
///
/// With one sort key a row that ties the worst candidate has lost, because everything it ties
/// arrived first. With more than one it has not, because a later key can still separate them, so the
/// pass has to keep the ties and let the row path decide.
fn still_wanted(key: SortKey, single: bool) -> Comparison {
    match (key.descending, single) {
        (false, true) => Comparison::Less,
        (false, false) => Comparison::LessOrEqual,
        (true, true) => Comparison::Greater,
        (true, false) => Comparison::GreaterOrEqual,
    }
}

/// Charges the scratch reservation for what is still held after a trim.
///
/// Released and taken again rather than shrunk, because a reservation gives everything back at once
/// and has no partial release. Nothing else can be holding the difference at this point, since the
/// operator is between two reads of its input.
fn recharge(kept: &[Candidate], scratch: &mut Reservation) -> Result<()> {
    let footprint = kept.iter().map(|candidate| charge(&candidate.cells)).sum();
    scratch.release();
    scratch.grow(footprint)
}

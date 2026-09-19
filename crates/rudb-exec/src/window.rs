//! Window functions.
//!
//! A pipeline breaker, and one that breaks harder than a sort does. A sort needs every row before
//! it can emit the first one because the last row of the input can be the first row of the output.
//! A window needs every row before it can answer the first one because the value at row one can
//! depend on row a million, and unlike a sort it cannot even start until the rows are in order.
//!
//! So this reads everything, sorts by the partition keys and then the order keys, and walks each
//! partition once. Every row gets a fresh accumulator over its own frame, which is quadratic in the
//! size of a partition for a frame that grows with it. A running accumulator that is only reset
//! when the frame's start moves backwards would make the common frames linear, and it is not here
//! because it is correct for some frames and not others, and getting the general case right first
//! is what makes it safe to add. The milestone asks for this to work rather than to be fast.
//!
//! # Where the rows are ordered
//!
//! The partition keys sort ascending with nulls last and the order keys sort the way the query
//! wrote them. Ascending with nulls last is not a claim about semantics, since nothing can observe
//! the order of two partitions relative to each other. It is what makes partitions contiguous so
//! that one pass can find them, and any total order over the keys would do.
//!
//! Rows that tie on every key are separated by where they arrived, which is what the sort already
//! does and for the same reason. A window over tied rows has to pick some order, and picking the
//! one a single thread would have produced is the only choice that does not change with the number
//! of threads that ran.
//!
//! # Peers, and why `RANGE` is not `ROWS`
//!
//! Two rows are peers when they agree on every order key. This matters more than it sounds like,
//! because the default frame is `RANGE UNBOUNDED PRECEDING TO CURRENT ROW` and under `RANGE` the
//! current row means the whole peer group of the current row rather than the row itself. A running
//! total over rows that tie therefore gives every tied row the same total, which is what the
//! standard says and what DuckDB answers, while a `ROWS` frame over the same query gives each of
//! them a different one. A window with no `ORDER BY` has one peer group per partition, which is
//! why `sum(i) OVER ()` is a total over the partition rather than a running one.

use std::cmp::Ordering;
use std::sync::Mutex;

use rudb_common::{Error, Field, LogicalType, Memory, Reservation, Result, Session, Value};
use rudb_functions::resolve;
use rudb_kernels::Accumulator;
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_plan::{
    ColumnBinding, Expr, ExprRef, Plan, Slice, SortKey, WindowBound, WindowExclude, WindowFrame,
    WindowUnit,
};
use rudb_vector::Chunk;

use crate::buffer::Buffered;
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;
use crate::sort::{Arrival, Place, compare};

/// What a call reads to answer, which is five entirely different things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reads {
    /// The rows the frame covers, through an accumulator. Every aggregate.
    Frame,
    /// Where the row sits in its partition. The ranking windows, which have no arguments to read
    /// and no frame to read them over, and which answer the same whatever frame was written.
    Position(Ranking),
    /// One row of the frame, found by counting through it. `first_value`, `last_value` and
    /// `nth_value`.
    Picked(Picks),
    /// One row of the partition, a distance from this one. `lag` and `lead`, which read the
    /// partition and not the frame.
    Shifted(Looks),
    /// The values on either side of a gap, read along the sort key. `fill`, which is answered for
    /// the whole partition at once rather than a row at a time.
    Filled,
}

impl Reads {
    /// What a name reads, which is the name and nothing else. Anything unrecognised is an
    /// aggregate, since the binder has already refused every name that is neither.
    fn of(name: &str) -> Self {
        if let Some(ranking) = Ranking::of(name) {
            return Self::Position(ranking);
        }
        match name {
            "first_value" => Self::Picked(Picks::First),
            "last_value" => Self::Picked(Picks::Last),
            "nth_value" => Self::Picked(Picks::Nth),
            "lag" => Self::Shifted(Looks::Back),
            "lead" => Self::Shifted(Looks::Forward),
            "fill" => Self::Filled,
            _ => Self::Frame,
        }
    }
}

/// Which row of the frame a picking window answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Picks {
    First,
    Last,
    /// The one the second argument counts to, one-based and read off the current row.
    Nth,
}

/// Which way a shifting window counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Looks {
    Back,
    Forward,
}

/// The ranking windows, which count rather than aggregate.
///
/// Peer groups decide all of them. `row_number` is the only one that separates tied rows, `rank`
/// gives every row of a group the position of the group's first row, and `dense_rank` gives it the
/// number of the group. The two that divide are built out of those, and `ntile` is the one that
/// reads an argument, which is how many buckets to cut the partition into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ranking {
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile,
}

impl Ranking {
    /// The ranking a name stands for, or `None` for a name that is an aggregate.
    fn of(name: &str) -> Option<Self> {
        Some(match name {
            "row_number" => Self::RowNumber,
            "rank" => Self::Rank,
            "dense_rank" | "rank_dense" => Self::DenseRank,
            "percent_rank" => Self::PercentRank,
            "cume_dist" => Self::CumeDist,
            "ntile" => Self::Ntile,
            _ => return None,
        })
    }
}

/// One window call, resolved against the input.
#[derive(Debug)]
struct Call {
    /// The resolved function name, which the accumulator is built from.
    name: String,
    /// Whether it reads the frame or the row's position.
    reads: Reads,
    /// What the call returns, which is also the type of the column it produces.
    returns: LogicalType,
    /// Where this call's arguments start among the gathered values.
    args_at: usize,
    /// How many arguments it has.
    args: usize,
    /// Where its `FILTER` predicate landed, when it has one.
    filter_at: Option<usize>,
    /// Whether duplicate argument tuples are collapsed before aggregating.
    distinct: bool,
    /// Whether an argument that is null is passed over.
    ignore_nulls: bool,
}

/// One row on its way through a window, with everything the pass over it will need already read.
///
/// The gathered values are one flat vector rather than one per purpose, because they are evaluated
/// by a single prepared array in one pass over the chunk and cutting them apart per row would
/// allocate four vectors where one does.
type Windowed = (Vec<Value>, Vec<Value>, Arrival);

/// Where a frame's two ends were gathered, for the ends that were written as a distance.
#[derive(Debug, Clone, Copy)]
struct Offsets {
    start: Option<usize>,
    end: Option<usize>,
}

/// How to move the current row's order key by a distance, for a `RANGE` end.
///
/// A `RANGE` distance is not a count of rows, it is a distance in the values the query ordered by,
/// so the end of the frame is the place where the key reaches `key + offset` or `key - offset`.
/// DuckDB works that out by binding the addition as an ordinary call, which is why asking for
/// `ORDER BY a_varchar RANGE BETWEEN 1 PRECEDING` says there is no `-(VARCHAR, INTEGER_LITERAL)`.
/// The same resolution happens here, once when the operator is built rather than per row.
#[derive(Debug, Clone)]
struct Moved {
    /// `+` or `-`, which is the direction the sort key runs as much as the word that was written.
    name: &'static str,
    /// What the key is cast to before the call, which is what the overload takes.
    key: LogicalType,
    /// What the distance is cast to before the call.
    offset: LogicalType,
    /// What the call gives back, which is compared against the other rows' keys.
    returns: LogicalType,
}

/// Both ends of a `RANGE` frame that was written with a distance.
#[derive(Debug, Clone, Default)]
struct Ranged {
    start: Option<Moved>,
    end: Option<Moved>,
}

/// A window node as the plan wrote it, which is everything about the window except its input.
///
/// These five arrive together and are read together, and carrying them as one thing keeps the call
/// that turns them into an operator short enough to read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Written {
    /// The table index the window's own columns are bound under.
    pub(crate) index: u32,
    pub(crate) partition: Slice,
    pub(crate) order: Slice,
    pub(crate) frame: WindowFrame,
    /// The window calls themselves, which all share the partitioning, the order and the frame.
    pub(crate) expressions: Slice,
}

/// A window operator: one partitioning, one order, one frame, and the calls that share them.
#[derive(Debug)]
pub(crate) struct Window {
    /// Everything evaluated against the input, in one array: the partition keys, the order keys,
    /// each call's arguments and filter, and then the frame's offsets.
    values: Prepared,
    /// How many partition keys there are, which is also where the order keys start.
    partitions: usize,
    /// The order keys, which decide who is a peer of whom.
    order: Vec<SortKey>,
    /// What the gathered rows are sorted by: the partition keys ascending, then the order keys.
    sorting: Vec<SortKey>,
    calls: Vec<Call>,
    frame: WindowFrame,
    offsets: Offsets,
    ranged: Ranged,
    /// The input's types followed by one per call, which are the output's.
    types: Vec<LogicalType>,
    schema: Schema,
    memory: Memory,
    rows: Mutex<Vec<Windowed>>,
    /// What the gathered rows are charged, given back once the output chunks are charged instead.
    charged: Mutex<Vec<Reservation>>,
    /// What the output chunks are charged, held for as long as they are readable.
    held: Mutex<Reservation>,
    out: Buffered,
}

/// What one instance of a window gathers before it combines.
#[derive(Debug)]
pub(crate) struct Gathered {
    rows: Vec<Windowed>,
    scratch: Scratch,
    charged: Reservation,
    place: Place,
}

impl Window {
    /// Applies the session semantics to the prepared expressions.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.values = self.values.in_session(session);
        self
    }

    /// The columns this produces, which are the input's followed by one per call.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// # Errors
    ///
    /// If a key or an argument does not resolve against the input's schema, if an expression the
    /// plan lists is not a window call, or if the frame is one this cannot answer yet. All three
    /// are failures of the plan or gaps in this operator rather than failures of the data, which is
    /// why they are found here, once, rather than on some chunk in the middle of a scan.
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
        written: &Written,
        memory: &Memory,
    ) -> Result<(Self, Buffered)> {
        let Written { index, partition, order, frame, expressions } = *written;
        let order = plan.sort_key_list(order).to_vec();
        let mut gathered: Vec<ExprRef> = plan.expr_list(partition).to_vec();
        let partitions = gathered.len();
        // The partition keys sort ascending with nulls last, which nothing can observe, and the
        // order keys sort the way the query wrote them, which everything can.
        let mut sorting: Vec<SortKey> = gathered
            .iter()
            .map(|&expr| SortKey { expr, descending: false, nulls_first: false })
            .collect();
        sorting.extend(order.iter().copied());
        gathered.extend(order.iter().map(|key| key.expr));

        let mut calls = Vec::new();
        for &expr in plan.expr_list(expressions) {
            let Expr::Window { name, args, distinct, filter, ignore_nulls } = plan.expr(expr)
            else {
                return Err(Error::internal("a window node listing an expression that is not one"));
            };
            let arguments = plan.expr_list(*args).to_vec();
            let args_at = gathered.len();
            gathered.extend(arguments.iter().copied());
            let filter_at = filter.map(|predicate| {
                gathered.push(predicate);
                gathered.len() - 1
            });
            let written = plan.string(*name);
            calls.push(Call {
                reads: Reads::of(written),
                name: written.to_string(),
                returns: plan.expr_type(expr).clone(),
                args_at,
                args: arguments.len(),
                filter_at,
                distinct: *distinct,
                ignore_nulls: *ignore_nulls,
            });
        }
        let offsets = Offsets {
            start: distance(frame.start).map(|expr| {
                gathered.push(expr);
                gathered.len() - 1
            }),
            end: distance(frame.end).map(|expr| {
                gathered.push(expr);
                gathered.len() - 1
            }),
        };

        let mut fields = input.fields().to_vec();
        let mut bindings = input.bindings().to_vec();
        for (at, call) in calls.iter().enumerate() {
            fields.push(Field::new(call.name.clone(), call.returns.clone()));
            let at = u32::try_from(at).expect("a window node this wide cannot be built");
            bindings.push(ColumnBinding::new(index, at));
        }
        let schema = Schema::new(fields, bindings)?;
        let mut types = input.types();
        types.extend(calls.iter().map(|call| call.returns.clone()));

        // Worked out here rather than per row, because the types are the plan's and do not change
        // between rows. A `RANGE` end with no distance is the peer group and needs none of this.
        let ranged = if frame.unit == WindowUnit::Range {
            let key = order.first().map(|key| plan.expr_type(key.expr).clone());
            Ranged {
                start: moved(plan, key.as_ref(), frame.start, &order)?,
                end: moved(plan, key.as_ref(), frame.end, &order)?,
            }
        } else {
            Ranged::default()
        };

        let out = Buffered::new();
        let window = Self {
            values: Prepared::new(plan, &gathered, input)?,
            partitions,
            order,
            sorting,
            calls,
            frame,
            offsets,
            ranged,
            types,
            schema,
            memory: memory.clone(),
            rows: Mutex::new(Vec::new()),
            charged: Mutex::new(Vec::new()),
            held: Mutex::new(memory.reservation()),
            out: out.clone(),
        };
        Ok((window, out))
    }

    /// Whether two rows belong to the same partition.
    fn same_partition(&self, left: &Windowed, right: &Windowed) -> Result<bool> {
        for at in 0..self.partitions {
            if rudb_kernels::order_with_nulls(&left.0[at], &right.0[at], false)? != Ordering::Equal
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether two rows are peers, which means they agree on every order key.
    ///
    /// Not quite the same question as whether they sort equal, even though it has the same answer.
    /// A direction cannot make two values agree or disagree, so it is not consulted here.
    fn peers(&self, left: &Windowed, right: &Windowed) -> Result<bool> {
        for at in 0..self.order.len() {
            let at = self.partitions + at;
            if rudb_kernels::order_with_nulls(&left.0[at], &right.0[at], false)? != Ordering::Equal
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Whether a distance runs the wrong way, which `RANGE` refuses and `ROWS` and `GROUPS` accept.
///
/// `ROWS BETWEEN -1 PRECEDING` is an empty frame upstream and `RANGE BETWEEN -1 PRECEDING` is an
/// error, which is not an inconsistency: a row count that runs backwards still names a row, and a
/// distance in values that runs backwards names a frame whose start is past its end in the order
/// the query asked for. A distance this cannot read as a number is not negative, because the only
/// distances that are not numbers are intervals and an interval's sign is not one comparison.
fn negative(offset: &Value) -> bool {
    offset.as_i64().is_some_and(|written| written < 0)
}

/// The expression one end of a frame was written with, for the ends that were written as one.
fn distance(bound: WindowBound) -> Option<ExprRef> {
    match bound {
        WindowBound::Preceding(expr) | WindowBound::Following(expr) => Some(expr),
        _ => None,
    }
}

/// How one end of a `RANGE` frame moves the order key, or `None` when it does not move it at all.
///
/// The word that was written is only half of the direction. `1 PRECEDING` means a smaller key under
/// `ORDER BY x` and a larger one under `ORDER BY x DESC`, because preceding means earlier in the
/// order the query asked for and not smaller. So the sort direction decides the sign and the word
/// decides whether the direction is followed or reversed.
fn moved(
    plan: &Plan,
    key: Option<&LogicalType>,
    bound: WindowBound,
    order: &[SortKey],
) -> Result<Option<Moved>> {
    let (offset, back) = match bound {
        WindowBound::Preceding(offset) => (offset, true),
        WindowBound::Following(offset) => (offset, false),
        _ => return Ok(None),
    };
    // The binder refuses a `RANGE` distance with anything other than one order key, so a distance
    // that gets here without one is the binder and this disagreeing rather than a query's mistake.
    let (Some(key), Some(sort)) = (key, order.first()) else {
        return Err(Error::internal("a RANGE distance with no single order key"));
    };
    let name = if back == sort.descending { "+" } else { "-" };
    let offset = plan.expr_type(offset).clone();
    let resolved = resolve(name, &[key.clone(), offset])?;
    let [key, offset] = resolved.arguments.as_slice() else {
        return Err(Error::internal("an arithmetic overload that does not take two arguments"));
    };
    Ok(Some(Moved {
        name: resolved.name,
        key: key.clone(),
        offset: offset.clone(),
        returns: resolved.returns,
    }))
}

impl Sink for Window {
    type Local = Gathered;

    fn local(&self) -> Gathered {
        Gathered {
            rows: Vec::new(),
            scratch: self.values.scratch(),
            charged: self.memory.reservation(),
            place: Place::default(),
        }
    }

    fn at(&self, morsel: &rudb_pipeline::Morsel, local: &mut Gathered) -> Result<()> {
        local.place.start(morsel.index());
        Ok(())
    }

    fn sink(&self, chunk: &Chunk, local: &mut Gathered) -> Result<Progress> {
        let mut gathered = Vec::new();
        self.values.evaluate(chunk, &mut local.scratch, &mut gathered)?;
        let mut taken = 0;
        // row at a time: the same trade the sort makes and for now the same reason. A window that
        // holds its rows as chunks and its keys as one comparable byte string a row is what makes
        // the second pass cheap, and neither of those exists yet.
        for row in 0..chunk.len() {
            let held: Vec<Value> =
                gathered.iter().map(|column| column.try_value_at(row)).collect::<Result<_>>()?;
            let values: Vec<Value> = (0..chunk.width())
                .map(|column| chunk.try_value_at(row, column))
                .collect::<Result<_>>()?;
            taken += rows::footprint(&held) + rows::footprint(&values);
            local.rows.push((held, values, local.place.of(row)));
        }
        local.place.past(chunk.len());
        local.charged.grow(taken)?;
        Ok(Progress::More)
    }

    fn combine(&self, local: Gathered) -> Result<()> {
        let mut rows = self.rows.lock().map_err(poisoned)?;
        rows.extend(local.rows);
        self.charged.lock().map_err(poisoned)?.push(local.charged);
        Ok(())
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        let mut gathered = std::mem::take(&mut *self.rows.lock().map_err(poisoned)?);
        let keys = self.sorting.len();
        let mut failure: Option<Error> = None;
        gathered.sort_by(|left, right| {
            let ordering = compare(&self.sorting, &left.0[..keys], &right.0[..keys], &mut failure);
            match ordering {
                Ordering::Equal => left.2.cmp(&right.2),
                ordering => ordering,
            }
        });
        if let Some(error) = failure {
            return Err(error);
        }

        let mut answered: Vec<Vec<Value>> = Vec::with_capacity(gathered.len());
        let mut start = 0;
        while start < gathered.len() {
            let mut end = start + 1;
            while end < gathered.len() && self.same_partition(&gathered[start], &gathered[end])? {
                end += 1;
            }
            self.over(&gathered[start..end], &mut answered)?;
            start = end;
        }

        let mut held = self.held.lock().map_err(poisoned)?;
        let chunks = rows::chunks(&self.types, &answered, &mut held)?;
        self.out.fill(chunks)?;
        // The gathered rows are gone and the chunks are charged instead, so what the instances took
        // is given back here and not before.
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
    }
}

impl Window {
    /// Answers every row of one partition and appends the answered rows to `answered`.
    fn over(&self, rows: &[Windowed], answered: &mut Vec<Vec<Value>>) -> Result<()> {
        let peers = self.peer_groups(rows)?;
        // `fill` is answered for the whole partition in one go rather than a row at a time, because
        // every gap in it is read from the nearest value on either side and looking for those per
        // row would walk the partition again for each one.
        let filled: Vec<Option<Vec<Value>>> = self
            .calls
            .iter()
            .map(|call| (call.reads == Reads::Filled).then(|| self.filling(call, rows)))
            .collect();
        for at in 0..rows.len() {
            let frame = self.frame_of(rows, &peers, at)?;
            let mut row = rows[at].1.clone();
            for (which, call) in self.calls.iter().enumerate() {
                match &filled[which] {
                    Some(column) => row.push(column[at].clone()),
                    None => row.push(self.answer(call, rows, &peers, at, frame.clone())?),
                }
            }
            answered.push(row);
        }
        Ok(())
    }

    /// Which peer group each row of the partition belongs to, numbered from zero.
    ///
    /// Worked out once for the partition rather than per row, because every `RANGE` bound and every
    /// `GROUPS` bound asks the same question of it and asking per row would walk the partition
    /// again for each one.
    fn peer_groups(&self, rows: &[Windowed]) -> Result<Vec<usize>> {
        let mut groups = Vec::with_capacity(rows.len());
        let mut group = 0;
        for at in 0..rows.len() {
            if at > 0 && !self.peers(&rows[at - 1], &rows[at])? {
                group += 1;
            }
            groups.push(group);
        }
        Ok(groups)
    }

    /// The frame around `at`, as the half-open range of rows it covers.
    ///
    /// Half-open rather than inclusive so that an empty frame is an empty range rather than a pair
    /// that has to be read as one. An empty frame is ordinary: `ROWS BETWEEN 3 PRECEDING AND 2
    /// PRECEDING` covers nothing on the first row of a partition, and a `sum` over nothing is null
    /// rather than zero.
    fn frame_of(
        &self,
        rows: &[Windowed],
        peers: &[usize],
        at: usize,
    ) -> Result<std::ops::Range<usize>> {
        let last = rows.len();
        let from = match self.frame.start {
            WindowBound::UnboundedPreceding => 0,
            // Under `RANGE` and `GROUPS` the current row means its whole peer group, so the frame
            // starts at the first peer rather than at the row.
            WindowBound::CurrentRow => match self.frame.unit {
                WindowUnit::Rows => at,
                _ => first_of(peers, peers[at]),
            },
            WindowBound::Preceding(_) | WindowBound::Following(_)
                if self.frame.unit == WindowUnit::Range =>
            {
                self.reached(rows, peers, at, self.offsets.start, &self.ranged.start, false)?
            }
            WindowBound::Preceding(_) => {
                self.away(rows, peers, at, self.offsets.start, true, false)?
            }
            WindowBound::Following(_) => {
                self.away(rows, peers, at, self.offsets.start, false, false)?
            }
            WindowBound::UnboundedFollowing => {
                return Err(Error::internal("a frame starting after every row"));
            }
        };
        let to = match self.frame.end {
            WindowBound::UnboundedFollowing => last,
            WindowBound::CurrentRow => match self.frame.unit {
                WindowUnit::Rows => at + 1,
                _ => last_of(peers, peers[at]) + 1,
            },
            WindowBound::Preceding(_) | WindowBound::Following(_)
                if self.frame.unit == WindowUnit::Range =>
            {
                self.reached(rows, peers, at, self.offsets.end, &self.ranged.end, true)?
            }
            WindowBound::Preceding(_) => {
                self.away(rows, peers, at, self.offsets.end, true, true)?
            }
            WindowBound::Following(_) => {
                self.away(rows, peers, at, self.offsets.end, false, true)?
            }
            WindowBound::UnboundedPreceding => {
                return Err(Error::internal("a frame ending before every row"));
            }
        };
        Ok(from..to.min(last).max(from))
    }

    /// One end of a `RANGE` frame, as a row number.
    ///
    /// The distance is in the values and not in the rows, so this asks where the key would have to
    /// be and then finds that place. The rows of a partition are already sorted by the one order
    /// key a `RANGE` distance is allowed to have, so finding it is a binary search rather than a
    /// walk, which is what keeps a frame that moves with the row off the quadratic path.
    ///
    /// A row whose key is null is its own case and not an arithmetic failure. Nulls are all peers
    /// of each other and there is no distance from a null to anything, so the frame around one is
    /// the peer group, which is what `CURRENT ROW` would have given.
    fn reached(
        &self,
        rows: &[Windowed],
        peers: &[usize],
        at: usize,
        column: Option<usize>,
        moved: &Option<Moved>,
        after: bool,
    ) -> Result<usize> {
        let sort = *self
            .sorting
            .get(self.partitions)
            .ok_or_else(|| Error::internal("a RANGE distance with no order key to measure from"))?;
        let key = &rows[at].0[self.partitions];
        if key.is_null() {
            return Ok(if after {
                last_of(peers, peers[at]) + 1
            } else {
                first_of(peers, peers[at])
            });
        }
        let moved =
            moved.as_ref().ok_or_else(|| Error::internal("a RANGE distance with no arithmetic"))?;
        let column =
            column.ok_or_else(|| Error::internal("a frame distance the window did not gather"))?;
        let offset = &rows[at].0[column];
        if offset.is_null() {
            return Err(Error::binder("Window RANGE expressions cannot be NULL"));
        }
        if negative(offset) {
            let written = if after { self.frame.end } else { self.frame.start };
            let end = match written {
                WindowBound::Preceding(_) => "PRECEDING",
                _ => "FOLLOWING",
            };
            return Err(Error::out_of_range(format!("Invalid RANGE {end} value")));
        }
        let args = [
            rudb_kernels::cast_value(key, &moved.key, false)?,
            rudb_kernels::cast_value(offset, &moved.offset, false)?,
        ];
        let wanted = rudb_kernels::call_values(moved.name, &args, &moved.returns, None)?;
        // Half open on both ends: the start is the first row that is not before the place, and the
        // end is the first row that is past it, so a frame covering nothing comes out empty rather
        // than inverted.
        let mut low = 0;
        let mut high = rows.len();
        while low < high {
            let middle = low + (high - low) / 2;
            let held = &rows[middle].0[self.partitions];
            let ordering = crate::sort::rank(held, &wanted, sort)?;
            let before =
                if after { ordering != Ordering::Greater } else { ordering == Ordering::Less };
            if before {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low)
    }

    /// One end of a frame that was written as a distance, as a row number.
    ///
    /// `back` says which way the distance runs and `after` says whether the answer is the end of
    /// the range rather than its start, which is what decides whether the row the distance lands on
    /// is in the frame or one past it. The column is passed in rather than looked up from the
    /// expression, because the two ends of `ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING` can be the
    /// same expression and looking it up would find whichever end was gathered first.
    fn away(
        &self,
        rows: &[Windowed],
        peers: &[usize],
        at: usize,
        column: Option<usize>,
        back: bool,
        after: bool,
    ) -> Result<usize> {
        let offset = self.distance_at(rows, at, column, back)?;
        // A distance written as `1 PRECEDING` runs backwards and one written as `-1 PRECEDING` runs
        // forwards again, which upstream accepts rather than refusing. The frame it leaves usually
        // covers nothing, since a start after the end is an empty frame, and that is an answer of
        // null and not an error.
        let signed = if back { offset.saturating_neg() } else { offset };
        let landed = |from: usize| -> Option<usize> {
            let from = i64::try_from(from).unwrap_or(i64::MAX);
            usize::try_from(from.saturating_add(signed)).ok()
        };
        Ok(match self.frame.unit {
            WindowUnit::Rows => {
                // Everything that far back is before the partition. As a start that clamps to the
                // first row and as an end it leaves the frame covering nothing, which is what the
                // caller's `max` over the start turns a zero into.
                let Some(landed) = landed(at) else { return Ok(0) };
                if after { landed.saturating_add(1) } else { landed }
            }
            // A `GROUPS` distance counts peer groups, so it lands on a group and the frame takes
            // that whole group rather than one row of it.
            _ => {
                let Some(group) = landed(peers[at]) else { return Ok(0) };
                if after {
                    peers.iter().rposition(|&held| held <= group).map_or(0, |end| end + 1)
                } else {
                    peers.iter().position(|&held| held >= group).unwrap_or(rows.len())
                }
            }
        })
    }

    /// The distance one end of the frame was written with, read off the row it is measured from.
    ///
    /// Read per row and not once, because DuckDB accepts a column there. `ROWS BETWEEN j PRECEDING
    /// AND CURRENT ROW` gives every row a frame of its own size.
    fn distance_at(
        &self,
        rows: &[Windowed],
        at: usize,
        column: Option<usize>,
        back: bool,
    ) -> Result<i64> {
        let column =
            column.ok_or_else(|| Error::internal("a frame distance the window did not gather"))?;
        let value = &rows[at].0[column];
        let named = || {
            let unit = match self.frame.unit {
                WindowUnit::Rows => "ROWS",
                WindowUnit::Range => "RANGE",
                WindowUnit::Groups => "GROUPS",
            };
            let end = if back { "PRECEDING" } else { "FOLLOWING" };
            format!("Window {unit} {end} expression")
        };
        if value.is_null() {
            return Err(Error::invalid_input(format!("{} cannot be NULL", named())));
        }
        value.as_i64().ok_or_else(|| Error::invalid_input(format!("{} must be a number", named())))
    }

    /// One call's value over the rows the frame covers.
    fn answer(
        &self,
        call: &Call,
        rows: &[Windowed],
        peers: &[usize],
        at: usize,
        frame: std::ops::Range<usize>,
    ) -> Result<Value> {
        match call.reads {
            Reads::Position(ranking) => return ranked(ranking, call, rows, peers, at),
            Reads::Picked(pick) => return self.picked(pick, call, rows, peers, at, frame),
            Reads::Shifted(look) => return shifted(look, call, rows, at),
            // `over` answers this one for the whole partition before it asks about any row, so
            // getting here means the two of them disagree about which calls those are.
            Reads::Filled => return Err(Error::internal("fill is answered a partition at a time")),
            Reads::Frame => {}
        }
        let mut accumulator = Accumulator::new(&call.name, &call.returns)?;
        let mut seen: Vec<Vec<Value>> = Vec::new();
        for row in frame {
            if self.excluded(peers, at, row) {
                continue;
            }
            if let Some(filter) = call.filter_at {
                if rows[row].0[filter].as_bool() != Some(true) {
                    continue;
                }
            }
            let args: Vec<Value> = rows[row].0[call.args_at..call.args_at + call.args].to_vec();
            if call.ignore_nulls && args.iter().any(Value::is_null) {
                continue;
            }
            if call.distinct {
                if seen.contains(&args) {
                    continue;
                }
                seen.push(args.clone());
            }
            accumulator.update(&args)?;
        }
        accumulator.finish()
    }

    /// The row of the frame that `first_value`, `last_value` or `nth_value` answers with.
    ///
    /// These three read the frame, which is what separates them from `lag` and `lead`, and they
    /// obey `EXCLUDE` for the same reason. They count rows rather than values, so an argument that
    /// is null still takes up a place, unless `IGNORE NULLS` was written, which is what that clause
    /// means here: the nulls are passed over and the counting goes on around them.
    ///
    /// A count that does not reach a row is null and not an error. `nth_value(i, 10)` over a frame
    /// of five rows is null upstream, and so are `nth_value(i, 0)`, `nth_value(i, -1)` and
    /// `nth_value(i, NULL)`, which is three different reasons for the same answer.
    fn picked(
        &self,
        pick: Picks,
        call: &Call,
        rows: &[Windowed],
        peers: &[usize],
        at: usize,
        frame: std::ops::Range<usize>,
    ) -> Result<Value> {
        // The count is read off the current row rather than once for the partition, because
        // upstream reads it there: `nth_value(k, k)` gives each row a count of its own.
        let wanted = match pick {
            Picks::First => Some(1),
            Picks::Last => None,
            Picks::Nth => {
                let written = &rows[at].0[call.args_at + 1];
                if written.is_null() {
                    return Ok(Value::Null);
                }
                let count = written.as_i64().ok_or_else(|| {
                    Error::invalid_input("Argument for nth_value must be a number")
                })?;
                if count <= 0 {
                    return Ok(Value::Null);
                }
                Some(usize::try_from(count).unwrap_or(usize::MAX))
            }
        };
        let mut seen = 0_usize;
        let mut last = Value::Null;
        for row in frame {
            if self.excluded(peers, at, row) {
                continue;
            }
            let value = rows[row].0[call.args_at].clone();
            if call.ignore_nulls && value.is_null() {
                continue;
            }
            seen += 1;
            if wanted == Some(seen) {
                return Ok(value);
            }
            last = value;
        }
        // Reaching the end means the count ran past the frame for the two that count, and means the
        // answer for the one that wanted the end of it.
        Ok(if wanted.is_none() { last } else { Value::Null })
    }

    /// One `fill` call's whole column for one partition.
    ///
    /// A row that already has a value keeps it. A gap is read off the straight line through the
    /// nearest value before it and the nearest value after it, measured along the sort key rather
    /// than by counting rows, so an uneven key spaces the answers unevenly too. A gap that has
    /// nothing before it borrows the first two values in the partition and a gap that has nothing
    /// after it borrows the last two, which is what makes the ends extend the line rather than
    /// repeat the end value. With one value in the whole partition there is no line and that value
    /// is carried everywhere, and with none the column stays as it was.
    ///
    /// Only the stretch of rows whose sort key is usable takes part. A null key sorts to one end of
    /// the partition and an infinite one to the other, so that stretch is a single run in the
    /// middle, and a row outside it keeps whatever it already had.
    fn filling(&self, call: &Call, rows: &[Windowed]) -> Vec<Value> {
        let mut out: Vec<Value> = rows.iter().map(|row| row.0[call.args_at].clone()).collect();
        // The binder has already refused any `fill` whose `OVER` does not order by exactly one
        // expression, so the sort key is the one gathered value that follows the partition keys.
        let sorted = self.partitions;
        let keys: Vec<Option<f64>> = rows.iter().map(|row| placement(&row.0[sorted])).collect();
        let Some(first) = keys.iter().position(Option::is_some) else {
            return out;
        };
        let last =
            keys[first..].iter().position(Option::is_none).map_or(keys.len(), |past| first + past);
        let anchors: Vec<(usize, f64, f64)> = (first..last)
            .filter_map(|at| Some((at, keys[at]?, placement(&rows[at].0[call.args_at])?)))
            .collect();
        if anchors.is_empty() {
            return out;
        }
        let mut behind = 0;
        for at in first..last {
            while behind < anchors.len() && anchors[behind].0 <= at {
                behind += 1;
            }
            if !out[at].is_null() {
                continue;
            }
            // `behind` now counts the anchors before this row, so the pair is the one on each side
            // when there is one on each side, and the two nearest on the one side when there is not.
            let (from, to) = match (behind.checked_sub(1), behind < anchors.len()) {
                (Some(before), true) => (before, behind),
                (Some(before), false) => (before.saturating_sub(1), before),
                (None, _) => (0, usize::min(1, anchors.len() - 1)),
            };
            let (_, x0, y0) = anchors[from];
            let (_, x1, y1) = anchors[to];
            out[at] = blended(y0, y1, gradient(keys[at].unwrap_or(x0), x0, x1), &call.returns);
        }
        out
    }

    /// Whether `row` is left out of the frame around `at` by the frame's exclusion.
    fn excluded(&self, peers: &[usize], at: usize, row: usize) -> bool {
        match self.frame.exclude {
            WindowExclude::NoOthers => false,
            WindowExclude::CurrentRow => row == at,
            WindowExclude::Group => peers[row] == peers[at],
            // Ties keeps the current row and drops every other member of its group, which is the
            // one exclusion that does not cut a contiguous piece out of the frame.
            WindowExclude::Ties => peers[row] == peers[at] && row != at,
        }
    }
}

/// One ranking window's value for one row.
///
/// The frame is not consulted and that is the rule rather than a shortcut here. Upstream answers
/// `rank() OVER (ORDER BY i ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)` with the same column as
/// `rank() OVER (ORDER BY i)`, because a rank is about the partition and a frame is about a row's
/// neighbourhood, and the standard says a window that names one of these ignores the other.
fn ranked(
    ranking: Ranking,
    call: &Call,
    rows: &[Windowed],
    peers: &[usize],
    at: usize,
) -> Result<Value> {
    let total = rows.len();
    let first = first_of(peers, peers[at]);
    let last = last_of(peers, peers[at]);
    let count = |held: usize| {
        i64::try_from(held).map_err(|_| Error::internal("a partition longer than a BIGINT"))
    };
    Ok(match ranking {
        Ranking::RowNumber => Value::BigInt(count(at + 1)?),
        // Every row of a peer group gets the position of the group's first row, so a group of two
        // is followed by a gap and `1, 2, 2, 4` is a rank column and not a mistake.
        Ranking::Rank => Value::BigInt(count(first + 1)?),
        Ranking::DenseRank => Value::BigInt(count(peers[at] + 1)?),
        // The rank of the row over the rank of the last row, which is why it starts at zero and
        // reaches one. A partition of one row has nothing to divide by and upstream answers zero
        // there rather than a division by zero or a null.
        Ranking::PercentRank => {
            Value::Double(if total <= 1 { 0.0 } else { first as f64 / (total - 1) as f64 })
        }
        // How much of the partition is at or before this row, counting the whole peer group, so it
        // ends at one on every partition and starts above zero.
        Ranking::CumeDist => Value::Double((last + 1) as f64 / total as f64),
        Ranking::Ntile => ntile(call, rows, at, total)?,
    })
}

/// Which bucket of `buckets` the row at `at` falls in, numbered from one.
///
/// The buckets are as equal as they can be and the remainder goes to the front, which is upstream's
/// arrangement and the standard's: six rows in four buckets are two, two, one and one, and never
/// one, one, two and two. The count is read off the current row rather than once for the partition
/// because upstream reads it per row, so `ntile(i)` gives each row a cut of its own.
fn ntile(call: &Call, rows: &[Windowed], at: usize, total: usize) -> Result<Value> {
    let written = &rows[at].0[call.args_at];
    if written.is_null() {
        return Ok(Value::Null);
    }
    let buckets = written
        .as_i64()
        .ok_or_else(|| Error::invalid_input("Argument for ntile must be a number"))?;
    if buckets <= 0 {
        return Err(Error::invalid_input("Argument for ntile must be greater than zero"));
    }
    let buckets = usize::try_from(buckets).unwrap_or(total).min(total.max(1));
    let each = total / buckets;
    let wide = total % buckets;
    // The first `wide` buckets hold one row more than the rest. A row inside that stretch divides
    // by the wider size and a row past it starts counting again from where the stretch ended.
    let bucket = if at < wide * (each + 1) {
        at / (each + 1)
    } else {
        wide + (at - wide * (each + 1)) / each.max(1)
    };
    let bucket = i64::try_from(bucket + 1).map_err(|_| Error::internal("too many buckets"))?;
    Ok(Value::BigInt(bucket))
}

/// The value `lag` or `lead` answers with, which is another row of the partition.
///
/// The frame is not consulted and neither is `EXCLUDE`, which is the thing to know about these two
/// and is measured rather than assumed: `lag(i) OVER (ORDER BY i ROWS BETWEEN CURRENT ROW AND
/// CURRENT ROW)` answers the same column as `lag(i) OVER (ORDER BY i)` on the pin, and so does the
/// same call with `EXCLUDE CURRENT ROW` written on it. They are about where a row sits in its
/// partition, the way the ranking windows are, and a frame is about a row's neighbourhood.
///
/// Three arguments, of which two are optional. The count defaults to one, is read off the current
/// row so a column can supply it, and answers null when it is null. A negative count turns each of
/// these into the other rather than being refused, and a count of zero is the row itself. The
/// default is the third argument, also read off the current row, and it was cast to the column's
/// type when the call was bound.
fn shifted(look: Looks, call: &Call, rows: &[Windowed], at: usize) -> Result<Value> {
    let held = &rows[at].0[call.args_at..call.args_at + call.args];
    let count = match held.get(1) {
        None => 1,
        Some(value) if value.is_null() => return Ok(Value::Null),
        Some(value) => value.as_i64().ok_or_else(|| {
            Error::invalid_input(format!("Argument for {} must be a number", call.name))
        })?,
    };
    let back = match look {
        Looks::Back => count >= 0,
        Looks::Forward => count < 0,
    };
    let steps = usize::try_from(count.unsigned_abs()).unwrap_or(usize::MAX);
    let landed = if call.ignore_nulls {
        // `IGNORE NULLS` counts values rather than rows, so the walk steps over every null it meets
        // and does not spend a count on it. The current row is passed over whether it is null or
        // not, since a count of one means the one before this and never this one.
        away_over_nulls(call, rows, at, back, steps)
    } else if back {
        at.checked_sub(steps)
    } else {
        at.checked_add(steps).filter(|&row| row < rows.len())
    };
    Ok(match landed {
        Some(row) => rows[row].0[call.args_at].clone(),
        None => held.get(2).cloned().unwrap_or(Value::Null),
    })
}

/// The row `steps` non-null values away from `at`, or nothing when the partition runs out first.
fn away_over_nulls(
    call: &Call,
    rows: &[Windowed],
    at: usize,
    back: bool,
    steps: usize,
) -> Option<usize> {
    let mut left = steps;
    let mut row = at;
    while left > 0 {
        row = if back {
            row.checked_sub(1)?
        } else {
            row.checked_add(1).filter(|&row| row < rows.len())?
        };
        if rows[row].0[call.args_at].is_null() {
            continue;
        }
        left -= 1;
    }
    Some(row)
}

/// Where a value sits on the number line that `fill` interpolates over, or nothing when it cannot
/// be an anchor.
///
/// This is the value as it is stored and not as it reads, so a `DECIMAL(10,2)` gives its unscaled
/// integer and a `DATE` gives its day count. Both sides of the ratio are measured the same way and
/// the scale cancels, so nothing is lost by it and the arithmetic stays where upstream puts it. A
/// null is not an anchor and neither is a NaN or an infinity, since a line through one of those
/// leads nowhere.
fn placement(value: &Value) -> Option<f64> {
    let number = match *value {
        Value::TinyInt(held) => f64::from(held),
        Value::SmallInt(held) => f64::from(held),
        Value::Integer(held) => f64::from(held),
        Value::BigInt(held) => held as f64,
        Value::HugeInt(held) => held as f64,
        Value::UTinyInt(held) => f64::from(held),
        Value::USmallInt(held) => f64::from(held),
        Value::UInteger(held) => f64::from(held),
        Value::UBigInt(held) => held as f64,
        Value::UHugeInt(held) => held as f64,
        Value::Float(held) => f64::from(held),
        Value::Double(held) => held,
        Value::Decimal { unscaled, .. } => unscaled as f64,
        Value::Date(held) => f64::from(held),
        Value::Time(held)
        | Value::TimeTz(held)
        | Value::Timestamp(held)
        | Value::TimestampTz(held) => held as f64,
        _ => return None,
    };
    number.is_finite().then_some(number)
}

/// How far along the line from `x0` to `x1` the key `x` sits, which is 0 at the first and 1 at the
/// second and outside that range on either side of them.
///
/// Two keys in the same place have no line between them and answer 0, which makes the first of the
/// two values the answer. A spread wide enough to overflow a double is measured again with both
/// ends divided by the larger of them, which upstream does as well and which keeps the ratio when
/// the difference itself will not fit.
fn gradient(x: f64, x0: f64, x1: f64) -> f64 {
    let mut den = x1 - x0;
    if den == 0.0 {
        return 0.0;
    }
    let mut num = x - x0;
    if !den.is_finite() {
        let scale = x0.abs().max(x1.abs());
        num = x / scale - x0 / scale;
        den = x1 / scale - x0 / scale;
    }
    num / den
}

/// The point at `d` along the line from `y0` to `y1`, as a value of the type the call returns.
///
/// Between the two ends the answer is truncated toward zero and past them it is rounded, which
/// looks arbitrary and is measured: upstream interpolates with a lossy cast and extrapolates by
/// casting the distance it travels, and the two casts round differently. A result the type cannot
/// hold is null rather than an error, which is upstream's answer too.
///
/// One deliberate difference from the pinned binary lives here. Upstream extrapolates by putting
/// the smaller of the two values first and negating the distance with it, comparing the values
/// rather than the keys they are ordered by, so its line runs the wrong way for any column that
/// falls as the key rises and for every descending `ORDER BY`. This reads the line in the direction
/// the keys give it, which agrees with upstream wherever the values rise and disagrees where they
/// fall. Upstream's own tests cover only the rising case, so nothing in the corpus pins the values
/// this differs on.
fn blended(y0: f64, y1: f64, d: f64, returns: &LogicalType) -> Value {
    if matches!(*returns, LogicalType::Float | LogicalType::Double) {
        // Written out this way and not as `y0 + (y1 - y0) * d`, which is the same line and not the
        // same double: upstream weighs the two ends against each other and the last bit of the
        // answer follows from that, so `fill(1.0 .. 2.0)` a third of the way along is
        // 1.3333333333333335 there and 1.3333333333333333 the other way.
        let number = y0 * (1.0 - d) + y1 * d;
        return match *returns {
            LogicalType::Float => Value::Float(number as f32),
            _ => Value::Double(number),
        };
    }
    let delta = y1 - y0;
    let number = if (0.0..=1.0).contains(&d) {
        (y0 + delta * d).trunc()
    } else {
        let offset = (delta.abs() * d.abs()).round();
        if (delta >= 0.0) == (d >= 0.0) { y0 + offset } else { y0 - offset }
    };
    seated(number, returns)
}

/// A number the line arrived at, put back into the type it came from, or null when it does not fit.
fn seated(number: f64, returns: &LogicalType) -> Value {
    // A double outside this range has no `i128` to round to at all, and the cast below saturates
    // rather than refusing, so the range is checked before the cast and not after it.
    if !number.is_finite() || number.abs() >= 1.701_411_834_604_692_3e38 {
        return Value::Null;
    }
    let whole = number as i128;
    let fits = |low: i128, high: i128| (low..=high).contains(&whole);
    match *returns {
        LogicalType::TinyInt if fits(i128::from(i8::MIN), i128::from(i8::MAX)) => {
            Value::TinyInt(whole as i8)
        }
        LogicalType::SmallInt if fits(i128::from(i16::MIN), i128::from(i16::MAX)) => {
            Value::SmallInt(whole as i16)
        }
        LogicalType::Integer if fits(i128::from(i32::MIN), i128::from(i32::MAX)) => {
            Value::Integer(whole as i32)
        }
        LogicalType::BigInt if fits(i128::from(i64::MIN), i128::from(i64::MAX)) => {
            Value::BigInt(whole as i64)
        }
        LogicalType::HugeInt => Value::HugeInt(whole),
        LogicalType::UTinyInt if fits(0, i128::from(u8::MAX)) => Value::UTinyInt(whole as u8),
        LogicalType::USmallInt if fits(0, i128::from(u16::MAX)) => Value::USmallInt(whole as u16),
        LogicalType::UInteger if fits(0, i128::from(u32::MAX)) => Value::UInteger(whole as u32),
        LogicalType::UBigInt if fits(0, i128::from(u64::MAX)) => Value::UBigInt(whole as u64),
        LogicalType::UHugeInt if whole >= 0 => Value::UHugeInt(whole as u128),
        LogicalType::Decimal { width, scale } if digits(whole) <= u32::from(width) => {
            Value::Decimal { unscaled: whole, width, scale }
        }
        LogicalType::Date if fits(i128::from(i32::MIN), i128::from(i32::MAX)) => {
            Value::Date(whole as i32)
        }
        _ if !fits(i128::from(i64::MIN), i128::from(i64::MAX)) => Value::Null,
        LogicalType::Time => Value::Time(whole as i64),
        LogicalType::TimeTz => Value::TimeTz(whole as i64),
        LogicalType::Timestamp
        | LogicalType::TimestampS
        | LogicalType::TimestampMs
        | LogicalType::TimestampNs => Value::Timestamp(whole as i64),
        LogicalType::TimestampTz => Value::TimestampTz(whole as i64),
        // Every type the binder lets through is above, so this is the overflow arm for the ones
        // that named a range and missed it.
        _ => Value::Null,
    }
}

/// How many decimal digits an unscaled value takes, which is what a `DECIMAL` width counts.
fn digits(unscaled: i128) -> u32 {
    let mut left = unscaled.unsigned_abs();
    let mut counted = 1;
    while left >= 10 {
        left /= 10;
        counted += 1;
    }
    counted
}

/// The first row of the peer group numbered `group`.
fn first_of(peers: &[usize], group: usize) -> usize {
    peers.iter().position(|&held| held == group).unwrap_or(0)
}

/// The last row of the peer group numbered `group`.
fn last_of(peers: &[usize], group: usize) -> usize {
    peers.iter().rposition(|&held| held == group).unwrap_or(0)
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Error {
    Error::internal("a window lock a panicking thread left behind")
}

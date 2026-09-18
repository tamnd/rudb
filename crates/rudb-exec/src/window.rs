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
use rudb_kernels::Accumulator;
use rudb_pipeline::{Progress, Sink};
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

/// One window call, resolved against the input.
#[derive(Debug)]
struct Call {
    /// The resolved function name, which the accumulator is built from.
    name: String,
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
        refuse_unanswerable(frame)?;
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
            calls.push(Call {
                name: plan.string(*name).to_string(),
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

        let out = Buffered::new();
        let window = Self {
            values: Prepared::new(plan, &gathered, input)?,
            partitions,
            order,
            sorting,
            calls,
            frame,
            offsets,
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

/// The expression one end of a frame was written with, for the ends that were written as one.
fn distance(bound: WindowBound) -> Option<ExprRef> {
    match bound {
        WindowBound::Preceding(expr) | WindowBound::Following(expr) => Some(expr),
        _ => None,
    }
}

/// Says no to the frames this operator cannot answer yet, before any row has been read.
///
/// One gap, and it is the one the milestone keeps as a line of its own. A `RANGE` distance is
/// measured from the current row's order key, so answering it means adding the distance to that
/// key, and the key can be a number or a timestamp while the distance can be a number or an
/// interval. That arithmetic belongs to the scalar kernel and reaching it from here means building
/// an expression the plan does not contain. `RANGE` with no distance is the default frame and is
/// answered, because its ends are the peer group and the ends of the partition rather than a
/// distance from anything.
fn refuse_unanswerable(frame: WindowFrame) -> Result<()> {
    if frame.unit != WindowUnit::Range {
        return Ok(());
    }
    if distance(frame.start).is_some() || distance(frame.end).is_some() {
        return Err(Error::not_implemented("a RANGE frame with an offset"));
    }
    Ok(())
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

    fn finalize(&self) -> Result<()> {
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
        for at in 0..rows.len() {
            let frame = self.frame_of(rows, &peers, at)?;
            let mut row = rows[at].1.clone();
            for call in &self.calls {
                row.push(self.answer(call, rows, &peers, at, frame.clone())?);
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
        let offset = self.distance_at(rows, at, column)?;
        Ok(match self.frame.unit {
            WindowUnit::Rows => {
                let landed = if back {
                    match at.checked_sub(offset) {
                        Some(landed) => landed,
                        // Everything that far back is before the partition. As a start that clamps
                        // to the first row and as an end it leaves the frame covering nothing,
                        // which is what the caller's `max` over the start turns a zero into.
                        None => return Ok(0),
                    }
                } else {
                    at.saturating_add(offset)
                };
                if after { landed.saturating_add(1) } else { landed }
            }
            // A `GROUPS` distance counts peer groups, so it lands on a group and the frame takes
            // that whole group rather than one row of it.
            _ => {
                let group = if back {
                    match peers[at].checked_sub(offset) {
                        Some(group) => group,
                        None => return Ok(0),
                    }
                } else {
                    peers[at].saturating_add(offset)
                };
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
    fn distance_at(&self, rows: &[Windowed], at: usize, column: Option<usize>) -> Result<usize> {
        let column =
            column.ok_or_else(|| Error::internal("a frame distance the window did not gather"))?;
        let value = &rows[at].0[column];
        if value.is_null() {
            return Err(Error::binder("Invalid Input Error: Window frame offset cannot be NULL"));
        }
        let Some(offset) = value.as_i64() else {
            return Err(Error::binder("Invalid Input Error: Window frame offset must be a number"));
        };
        usize::try_from(offset).map_err(|_| {
            Error::binder("Invalid Input Error: Window frame offset must not be negative")
        })
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

//! The smallest and the largest value of every column of every chunk, kept so a scan can skip one.
//!
//! A filter that compares a column against a constant can be answered against two numbers instead of
//! against a thousand rows, and the rows it rules out are rows nothing has to read, copy, compare or
//! hand downstream. Parquet does this a row group at a time, in `rudb-parquet`'s `skips`, which is a
//! hundred thousand rows on the files this engine is measured against. This does it a chunk at a
//! time, which is a thousand, and the difference between those two granularities is most of what a
//! selective query costs.
//!
//! On the ClickBench `hits` sample at a million rows, six of the forty three queries have a filter
//! this can answer, and on those six it skips 75 percent of the chunks and they run between two and
//! two and a half times faster. Query 37 is the clearest: 999,975 rows scanned becomes 247,265, and
//! 11.05 milliseconds becomes 4.36.
//!
//! It is worth saying what that does not add up to, because the number that matters is the suite and
//! not the query. Over the whole of ClickBench at a million rows the six queries save 37
//! milliseconds and building the maps costs 60 to 90, so one pass over the suite is a wash. It pays
//! from the second pass on, and the real ClickBench protocol runs each query three times. The reason
//! it is not more is not the maps, it is that `hits` is not clustered on `CounterID`: the 7381 rows
//! with the value query 37 asks for are spread across 121 chunks of the 489. A sort key would put
//! them in four, and that is the F2 item this is the other half of.
//!
//! # Where the decision lives
//!
//! Not here. [`rudb_common::bounds`] holds what a minimum and a maximum rule out, because a Parquet
//! footer, a table in memory and a block of the storage format all keep the same two numbers and
//! all want the same answer from them. What is here is how to get those two numbers out of a vector
//! that may be in any of eight forms, and how to keep them next to the chunk they describe.
//!
//! # Forms this does not have to flatten
//!
//! A bound is allowed to be wider than the truth. It rules out a chunk that cannot hold a matching
//! row, so a range that covers more than the chunk really holds costs a chunk that is read and
//! yields nothing, and never costs a row. That is what makes a compressed form cheap here: a
//! dictionary column's bounds can come from its dictionary and not from its codes, a bit packed
//! column's from the base and the ceiling it was packed against, and a run encoded column's from
//! its run values, and none of those walks the rows at all.
//!
//! Cheap is not the only thing wanted from them any more. A bound that is allowed to be wide cannot
//! answer a `MIN` and cannot be added up, and a stored table that answers those out of its directory
//! is worth more than the load time it costs, so the three compressed forms are now read through
//! their codes, their packing and their runs rather than over their values. Each range says which of
//! the two it is in `exact`, so a form nobody has taught this about still skips chunks correctly and
//! simply does not answer the rest.
//!
//! # What it costs to build
//!
//! One pass over each column as it is appended. On `hits` at a million rows, one thread, that is
//! about 60 milliseconds on a load of 0.93 seconds, so a table with a full set of zone maps costs
//! 1.02 seconds to build against DuckDB's 1.18 for the same `CREATE TABLE AS SELECT`.
//!
//! Getting it to 60 milliseconds took two rounds. The first cost 260, and the comment on `extremes`
//! is where half of that went: a `Bound` allocated per value instead of a compare in the column's
//! own type. The other half was a shared dictionary rescanned once per chunk, which is `hits` and
//! its `SearchPhrase` column, whose dictionary is far larger than a chunk. Reading the codes costs
//! the rows instead, which is what happens now for every dictionary rather than only for that one.
//!
//! That cost is real and it belongs in the load time rather than hidden behind it, because a table
//! built once and queried forty three times and a table built once and queried once want different
//! answers, and the only way to have that conversation is with both numbers in front of you.
//! [`MemoryTable::stats_ns`] is what a load reports it spent here.

use rudb_common::Value;
use rudb_common::bounds::{Bound, Op, certain, excluded, scaled_as};
use rudb_vector::{Chunk, Data, Form, Packed, Vector};

#[cfg(doc)]
use crate::MemoryTable;

/// One comparison against one column of a table.
#[derive(Debug, Clone)]
pub struct Probe {
    /// Which column of the table.
    pub column: usize,
    /// The comparison, written with the column on the left.
    pub op: Op,
    /// The constant the column is compared against.
    pub value: Bound,
}

/// What one chunk of one column holds, as far as a filter needs to know.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Range {
    /// The smallest value, or `None` when the column has no ordered bound in this chunk.
    pub low: Option<Bound>,
    /// The largest value, same.
    pub high: Option<Bound>,
    /// How many rows are null, which is what an `IS NULL` would be answered against.
    pub nulls: usize,
    /// Whether `low` and `high` are the values the rows really hold rather than bounds that are
    /// allowed to be wider.
    ///
    /// A bound that is too wide is fine for skipping a chunk and useless for answering a `MIN`, so
    /// this is the difference between the two. It is set only where the walk actually looked at
    /// every row, which is every form here except the ones whose values hold something this cannot
    /// total or compare, and those fall back to the bound.
    pub exact: bool,
    /// The sum of the non-null values when they are integers, and `None` otherwise.
    ///
    /// Only integers, for two reasons that are both about answering the same thing twice. A float
    /// sum depends on the order it was added in, so one computed here at load and one computed at
    /// query time over the same rows can differ in the last bits, and a stored answer that is nearly
    /// the answer is worse than no stored answer. And a float column can hold a NaN, which makes
    /// every comparison undecidable, so the extremes beside this would not be extremes either.
    pub sum: Option<i128>,
}

impl Range {
    /// The range of one column, which is one pass over it.
    #[must_use]
    pub fn of(vector: &Vector) -> Self {
        range(vector)
    }

    /// Whether this range says no row of the chunk can pass `probe`.
    #[must_use]
    pub fn excludes(&self, op: Op, value: &Bound) -> bool {
        excluded(op, value, self.low.as_ref(), self.high.as_ref())
    }

    /// Whether this range says every row of the chunk passes `probe`.
    ///
    /// A chunk with a null in the column is never certain whatever the two ends say, because a
    /// comparison against null is null and a filter keeps the rows where its predicate is true. That
    /// is the one thing this adds to [`certain`], and it is the reason the question is asked here
    /// rather than of the two bounds on their own: the null count sits beside them and nowhere else.
    #[must_use]
    pub fn certain(&self, op: Op, value: &Bound) -> bool {
        self.nulls == 0 && certain(op, value, self.low.as_ref(), self.high.as_ref())
    }
}

/// The ranges of every column of one chunk.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Zone {
    columns: Vec<Range>,
}

impl Zone {
    /// Builds a zone from persisted ranges.
    #[must_use]
    pub fn from_ranges(columns: Vec<Range>) -> Self {
        Self { columns }
    }

    /// Persisted ranges in column order.
    #[must_use]
    pub fn columns(&self) -> &[Range] {
        &self.columns
    }

    /// The zone of a chunk, one pass per column.
    #[must_use]
    pub fn of(chunk: &Chunk) -> Self {
        Self { columns: chunk.columns().iter().map(range).collect() }
    }

    /// The range of one column, or `None` past the end.
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&Range> {
        self.columns.get(index)
    }

    /// How many columns this describes.
    #[must_use]
    pub fn width(&self) -> usize {
        self.columns.len()
    }

    /// Whether these probes, taken together, rule the chunk out.
    ///
    /// The probes are the conjuncts of one filter, so one of them ruling the chunk out rules it out.
    /// A probe naming a column this does not describe says nothing, which is the answer that keeps
    /// the rows.
    #[must_use]
    pub fn skips(&self, probes: &[Probe]) -> bool {
        probes.iter().any(|probe| {
            self.columns
                .get(probe.column)
                .is_some_and(|range| range.excludes(probe.op, &probe.value))
        })
    }

    /// Whether these probes, taken together, keep every row of the chunk.
    ///
    /// The mirror of [`Self::skips`], and the quantifier turns over with it. One probe ruling the
    /// chunk out rules it out, so that one is `any`. Every probe has to pass every row for the
    /// filter to keep every row, so this one is `all`, and a probe naming a column this does not
    /// describe says nothing here too, which now means the chunk is not certain and gets compared.
    ///
    /// No probes at all is `true` by the shape of `all`, and that is the right answer for the wrong
    /// reason: a filter with nothing in it does keep every row. Callers do not reach it, because a
    /// scan with no probes has no comparison to skip in the first place.
    #[must_use]
    pub fn certain(&self, probes: &[Probe]) -> bool {
        probes.iter().all(|probe| {
            self.columns
                .get(probe.column)
                .is_some_and(|range| range.certain(probe.op, &probe.value))
        })
    }
}

/// What one look at a column found.
///
/// The four answers are made together because they come from the same walk. Which walk that is
/// depends on the form the column arrived in, and the difference between a walk over the rows and a
/// read of a summary somebody else wrote is exactly what `exact` records.
#[derive(Default)]
struct Walked {
    low: Option<Bound>,
    high: Option<Bound>,
    exact: bool,
    sum: Option<i128>,
}

impl Walked {
    /// The two ends and nothing else, which is a string column: they are the rows, and strings do
    /// not add up.
    fn ends((low, high): (Option<Bound>, Option<Bound>)) -> Self {
        Self { low, high, exact: true, sum: None }
    }

    /// Two integer ends and a total, from a walk that saw every row.
    fn totalled(low: Option<i128>, high: Option<i128>, sum: i128) -> Self {
        Self { low: low.map(Bound::Int), high: high.map(Bound::Int), exact: true, sum: Some(sum) }
    }
}

/// The range of one vector, in whatever form it arrived in.
///
/// Public through [`Range::of`] because a writer that encodes a stripe one column at a time across
/// threads needs the range of the column it was handed, and building a whole [`Zone`] to read one
/// entry out of it would walk every other column on that thread as well.
///
/// The walk below compares the integers the column holds, because that is the only thing fast enough
/// to run over every column of a load. A decimal and a timestamp hold an integer with a power of ten
/// under it, and the walk cannot see that power: it sees an `INT64` either way. So the type puts it
/// back here, once per column rather than once per row. Without it the ends of every timestamp, time
/// and decimal column came out as a bare integer, which is a domain no constant of those types is
/// ever in, and every test against them answered nothing. See [`scaled_as`].
fn range(vector: &Vector) -> Range {
    let nulls = vector.len() - vector.validity().count_valid(vector.len());
    let walked = walk(vector);
    let ty = vector.logical_type();
    Range {
        low: walked.low.map(|bound| scaled_as(bound, ty)),
        high: walked.high.map(|bound| scaled_as(bound, ty)),
        nulls,
        exact: walked.exact,
        sum: walked.sum,
    }
}

/// One pass over one column, one arm per form.
fn walk(vector: &Vector) -> Walked {
    match vector.form() {
        // One value, which is both ends, and a total that is it times the rows that are not null.
        // That is the whole column with no loop at all.
        Form::Constant => constant(vector),
        // A start and a step, so the two ends are the first and the last, whichever way it runs. A
        // null row inside one has no value of its own, so the ends would then cover a row the column
        // does not hold, and that is the one case here that is a bound rather than the column.
        Form::Sequence => match vector.sequence_parts() {
            Some((start, step)) => {
                let (low, high) = ends(start, step, vector.len());
                let exact = !vector.validity().has_nulls(vector.len());
                Walked { low, high, exact, sum: None }
            }
            None => Walked::default(),
        },
        Form::BitPacked => match vector.packed_parts() {
            Some(packed) => unpacked(vector, &packed),
            None => Walked::default(),
        },
        Form::Dictionary => match vector.dictionary_parts() {
            Some((codes, values)) => coded(vector, codes, values),
            None => Walked::default(),
        },
        Form::Rle => match vector.run_parts() {
            Some((stops, values)) => runs(vector, stops, values),
            None => Walked::default(),
        },
        Form::Flat => match vector.data() {
            Some(data) => {
                let (low, high) = flat(vector, data);
                let (exact, sum) = summed(vector, data);
                Walked { low, high, exact, sum }
            }
            None => Walked::default(),
        },
        // Strings that are not flat. Walked as bytes rather than as values, because a `Value` per
        // row of a `URL` column is a heap allocation per row and this runs over every column of
        // every chunk of a load.
        Form::StringView | Form::Fsst => Walked::ends(text(vector)),
        // A form added since this was written. Saying nothing about it keeps its rows, which is the
        // answer that is wrong slowly rather than wrong.
        _ => Walked::default(),
    }
}

/// One value repeated, which is both ends of itself.
fn constant(vector: &Vector) -> Walked {
    match vector.constant_value() {
        // Every row is null, so there are no ends to get wrong and nothing to add up, and a total
        // of nothing is zero rather than unknown.
        Some(Value::Null) => Walked { exact: true, sum: Some(0), ..Walked::default() },
        Some(value) => match Bound::of_value(value) {
            Some(Bound::Int(only)) => {
                let rows = vector.validity().count_valid(vector.len()) as i128;
                Walked {
                    low: Some(Bound::Int(only)),
                    high: Some(Bound::Int(only)),
                    exact: true,
                    // Dropped rather than wrapped, which leaves exact ends and no total, and that is
                    // a true thing to say.
                    sum: only.checked_mul(rows),
                }
            }
            // A float, including a NaN, which is the second reason on `Range::exact`.
            Some(Bound::Real(real)) => Walked {
                low: Some(Bound::Real(real)),
                high: Some(Bound::Real(real)),
                exact: false,
                sum: None,
            },
            // A constant that is not a number still has exact ends. It is the same one value.
            Some(other) => Walked::ends((Some(other.clone()), Some(other))),
            None => Walked::default(),
        },
        None => Walked::default(),
    }
}

/// A bit packed column, unpacked a row at a time.
///
/// The range it was packed against is at least as wide as the column and often much wider, because
/// a width is a power of two number of bits and a column that fits in nine bits is packed in nine
/// bits whatever its largest value is. Unpacking costs a shift and a mask per row, so the ends this
/// gets are the column's and the total comes with them.
fn unpacked(vector: &Vector, packed: &Packed<'_>) -> Walked {
    let base = packed.base();
    let mut low: Option<i128> = None;
    let mut high: Option<i128> = None;
    let mut total = 0_i128;
    let nullable = vector.validity().has_nulls(vector.len());
    for row in 0..vector.len() {
        if nullable && vector.is_null_at(row) {
            continue;
        }
        let value = base + i128::from(packed.code(row));
        widen(value, &mut low, &mut high);
        total += value;
    }
    Walked::totalled(low, high, total)
}

/// A dictionary column, read through its codes rather than over its values.
///
/// The values are a superset of what the rows hold, so reading them is cheap and answers a skip.
/// It cannot answer a `MIN`, because a value no row points at is still in there, and it cannot
/// answer a `SUM` at all. Going through the codes costs a gather per row and answers both.
///
/// Only for numbers. A string column gets nothing out of being exact here and pays the same price
/// for it, which is why [`stringy`] still picks whichever of the rows and the values is shorter.
fn coded(vector: &Vector, codes: &[u32], values: &Vector) -> Walked {
    if strings(values) {
        return stringy(vector, values);
    }
    let Some(data) = values.data() else { return wider(values) };
    // A code pointing at a null is a row whose value this would have to invent, so the walk stops
    // and the superset answers instead. Fewer codes than rows is the same thing said differently,
    // and a walk that stopped short of the rows would report a total that is missing some of them.
    if values.validity().has_nulls(values.len()) || codes.len() < vector.len() {
        return wider(values);
    }
    /// One layout, gathered through the codes.
    macro_rules! gather {
        ($values:expr) => {{
            let held: &[_] = $values;
            let mut low: Option<i128> = None;
            let mut high: Option<i128> = None;
            let mut total = 0_i128;
            // The vector's own validity rather than `is_null_at`, which would follow the code into
            // the values on every row. The two say the same thing here because the values were just
            // checked for nulls, and this one is a bit read instead of a second indirection.
            let validity = vector.validity();
            let nullable = validity.has_nulls(vector.len());
            for (row, &code) in codes[..vector.len()].iter().enumerate() {
                if nullable && !validity.is_valid(row) {
                    continue;
                }
                let Some(&value) = held.get(code as usize) else { return wider(values) };
                let value = i128::from(value);
                widen(value, &mut low, &mut high);
                total += value;
            }
            Walked::totalled(low, high, total)
        }};
    }
    match data {
        Data::Bool(held) => gather!(held),
        Data::Int8(held) => gather!(held),
        Data::Int16(held) => gather!(held),
        Data::Int32(held) => gather!(held),
        Data::Int64(held) => gather!(held),
        Data::UInt8(held) => gather!(held),
        Data::UInt16(held) => gather!(held),
        Data::UInt32(held) => gather!(held),
        Data::UInt64(held) => gather!(held),
        // The widths that cannot be totalled and the floats that must not be, both covered by the
        // reasons on `Range::sum`. The superset still skips chunks for them.
        _ => wider(values),
    }
}

/// A run encoded column, read a run at a time.
///
/// The one form here where the exact answer is cheaper than the rows rather than the same price: a
/// run contributes its value once to the ends and its value times its length to the total, so a
/// thousand rows of one value cost one multiply.
fn runs(vector: &Vector, stops: &[u32], values: &Vector) -> Walked {
    if strings(values) {
        return stringy(vector, values);
    }
    let Some(data) = values.data() else { return wider(values) };
    // A null inside a run belongs to one row and the run's value belongs to the rest, and this walks
    // runs rather than rows, so it cannot tell which. The superset answers instead. So does a set of
    // runs that stops before the last row, because the rows past it would be left out of the total.
    if values.validity().has_nulls(values.len()) || vector.validity().has_nulls(vector.len()) {
        return wider(values);
    }
    if stops.last().copied() != u32::try_from(vector.len()).ok() {
        return wider(values);
    }
    /// One layout, weighted by how long each run is.
    macro_rules! weigh {
        ($values:expr) => {{
            let held: &[_] = $values;
            let mut low: Option<i128> = None;
            let mut high: Option<i128> = None;
            let mut total = 0_i128;
            let mut previous = 0_u32;
            for (run, &stop) in stops.iter().enumerate() {
                let Some(&value) = held.get(run) else { return wider(values) };
                let Some(length) = stop.checked_sub(previous).filter(|&rows| rows > 0) else {
                    continue;
                };
                previous = stop;
                let value = i128::from(value);
                widen(value, &mut low, &mut high);
                total += value * i128::from(length);
            }
            Walked::totalled(low, high, total)
        }};
    }
    match data {
        Data::Bool(held) => weigh!(held),
        Data::Int8(held) => weigh!(held),
        Data::Int16(held) => weigh!(held),
        Data::Int32(held) => weigh!(held),
        Data::Int64(held) => weigh!(held),
        Data::UInt8(held) => weigh!(held),
        Data::UInt16(held) => weigh!(held),
        Data::UInt32(held) => weigh!(held),
        Data::UInt64(held) => weigh!(held),
        _ => wider(values),
    }
}

/// The bounds of a column of strings that keeps its values somewhere else.
///
/// Exactness is worth paying for on a number and is worth nothing on a string. A whole table `MIN`
/// on a string column comes off the sorted dictionary the file writes rather than off these ends,
/// and strings do not add up, so all an exact end buys here is a slightly better chunk skip. So this
/// keeps the trade the column had before: walk the rows when there are fewer of them than there are
/// values, and scan the values otherwise.
///
/// The values being the longer side is not a strange case. One dictionary is shared by every chunk
/// of a column, so scanning it per chunk does the same work once per chunk. On `hits` at a million
/// rows that is `SearchPhrase`, whose dictionary is far larger than a chunk: 90 milliseconds of the
/// load went on that one column, against 10 for `URL`, which is plain and several times its size.
fn stringy(vector: &Vector, values: &Vector) -> Walked {
    if values.len() > vector.len() {
        return Walked::ends(text(vector));
    }
    wider(values)
}

/// Whether a set of values is text, which is walked by [`text`] rather than gathered.
fn strings(values: &Vector) -> bool {
    values.text_parts().is_some() || matches!(values.data(), Some(Data::Varlen(_)))
}

/// The bounds a column that points somewhere else falls back to, which are its values.
///
/// A superset of what the rows hold, so it skips a chunk correctly and answers nothing exactly.
/// This is where a layout the walks above do not understand ends up, and it is cheap: the values
/// are usually far fewer than the rows, and scanning them once is the whole cost.
fn wider(values: &Vector) -> Walked {
    let inner = range(values);
    Walked { low: inner.low, high: inner.high, exact: false, sum: None }
}

/// The sum of a flat column, one typed loop per physical layout, and whether it was walked at all.
///
/// The accumulator is an `i128` for every width, so a million `BIGINT` rows cannot overflow it and
/// nothing has to be checked per value. Adding the stripes together afterwards is checked, because
/// that is where enough rows to matter could finally pile up.
fn summed(vector: &Vector, data: &Data) -> (bool, Option<i128>) {
    /// One layout, summed in the widest integer there is.
    macro_rules! add {
        ($values:expr) => {{
            let held: &[_] = $values;
            let mut total = 0_i128;
            if vector.validity().has_nulls(vector.len()) {
                for (index, &value) in held.iter().enumerate() {
                    if !vector.is_null_at(index) {
                        total += i128::from(value);
                    }
                }
            } else {
                for &value in held {
                    total += i128::from(value);
                }
            }
            (true, Some(total))
        }};
    }
    match data {
        Data::Bool(values) => add!(values),
        Data::Int8(values) => add!(values),
        Data::Int16(values) => add!(values),
        Data::Int32(values) => add!(values),
        Data::Int64(values) => add!(values),
        Data::UInt8(values) => add!(values),
        Data::UInt16(values) => add!(values),
        Data::UInt32(values) => add!(values),
        Data::UInt64(values) => add!(values),
        // Exact ends, and no sum. A hundred and twenty eight bit column can overflow the widest
        // accumulator there is, which is the first reason on `Range::sum`.
        Data::Int128(_) | Data::UInt128(_) => (true, None),
        // Neither. A NaN is left out of the ends above, because a NaN at one of them rules nothing
        // out, and a column that holds one then has ends that are not its rows. The sum is the
        // other reason on `Range::sum`.
        Data::Float32(_) | Data::Float64(_) => (false, None),
        // Strings, walked a row at a time by `text` above.
        Data::Varlen(_) => (true, None),
        Data::Interval(_) | Data::Empty | _ => (false, None),
    }
}

/// The two ends of a sequence of `len` values starting at `start`.
fn ends(start: i64, step: i64, len: usize) -> (Option<Bound>, Option<Bound>) {
    if len == 0 {
        return (None, None);
    }
    let last = i128::from(start) + i128::from(step) * (len as i128 - 1);
    let first = i128::from(start);
    let (low, high) = if first <= last { (first, last) } else { (last, first) };
    (Some(Bound::Int(low)), Some(Bound::Int(high)))
}

/// The range of a flat vector, one typed loop per physical layout.
fn flat(vector: &Vector, data: &Data) -> (Option<Bound>, Option<Bound>) {
    /// The two ends of one layout, turned into bounds once each.
    ///
    /// `$into` is what turns the layout's Rust type into a bound, which is where the unsigned types
    /// widen rather than wrap, where the floats go to the real domain instead of the integer one,
    /// and where a value with no bound in either domain answers `None` rather than a wrong number.
    macro_rules! sweep {
        ($values:expr, $into:expr) => {{
            let (low, high) = extremes($values, vector);
            (low.and_then($into), high.and_then($into))
        }};
    }
    let int = |number: i128| Some(Bound::Int(number));
    match data {
        Data::Bool(values) => sweep!(values, |flag: bool| int(i128::from(flag))),
        Data::Int8(values) => sweep!(values, |n: i8| int(i128::from(n))),
        Data::Int16(values) => sweep!(values, |n: i16| int(i128::from(n))),
        Data::Int32(values) => sweep!(values, |n: i32| int(i128::from(n))),
        Data::Int64(values) => sweep!(values, |n: i64| int(i128::from(n))),
        Data::Int128(values) => sweep!(values, int),
        Data::UInt8(values) => sweep!(values, |n: u8| int(i128::from(n))),
        Data::UInt16(values) => sweep!(values, |n: u16| int(i128::from(n))),
        Data::UInt32(values) => sweep!(values, |n: u32| int(i128::from(n))),
        Data::UInt64(values) => sweep!(values, |n: u64| int(i128::from(n))),
        // A `UHUGEINT` past the signed ceiling has no bound in this domain, so that end answers
        // nothing rather than a number that would rule out rows the column really holds.
        Data::UInt128(values) => sweep!(values, |n: u128| i128::try_from(n).ok().and_then(int)),
        Data::Float32(values) => sweep!(values, |n: f32| Some(Bound::Real(f64::from(n)))),
        Data::Float64(values) => sweep!(values, |n: f64| Some(Bound::Real(n))),
        Data::Varlen(_) => text(vector),
        // An interval has no total order anybody agrees on, and an empty vector has no values. A
        // layout added since this was written lands here too, and says nothing for the same reason.
        Data::Interval(_) | Data::Empty | _ => (None, None),
    }
}

/// The smallest and the largest value of a fixed width column, skipping its nulls.
///
/// Compared in the layout's own type, with one conversion to a [`Bound`] at each end afterwards
/// rather than one per value. That is the difference between a zone map worth building at load time
/// and one that is not: the boxed form cost about 1.7 nanoseconds a row, which over a hundred and
/// five columns of a million rows is most of a second, and this is a compare and a branch.
///
/// There are two loops rather than one with a check in it. A column with no nulls is the common case
/// by a long way, and its loop has nothing in it but the two compares, which is what lets the
/// compiler put a whole register of values through at a time. Folding the null check into that loop
/// costs more than the check: it costs the vectorisation.
fn extremes<T: Copy + PartialOrd>(values: &[T], vector: &Vector) -> (Option<T>, Option<T>) {
    let mut low: Option<T> = None;
    let mut high: Option<T> = None;
    if vector.validity().has_nulls(vector.len()) {
        for (index, &value) in values.iter().enumerate() {
            if !vector.is_null_at(index) {
                widen(value, &mut low, &mut high);
            }
        }
    } else {
        for &value in values {
            widen(value, &mut low, &mut high);
        }
    }
    (low, high)
}

/// Opens the range far enough to hold `value`.
///
/// A value that does not order against itself is a NaN, and it is left out: a NaN at either end
/// makes every comparison against the range undecidable, which is a range that rules nothing out,
/// and a filter is false for a NaN row whichever way this goes. For every integer layout the check
/// folds away, since their `partial_cmp` never answers `None`.
#[inline]
fn widen<T: Copy + PartialOrd>(value: T, low: &mut Option<T>, high: &mut Option<T>) {
    if value.partial_cmp(&value).is_none() {
        return;
    }
    if low.is_none_or(|held| value < held) {
        *low = Some(value);
    }
    if high.is_none_or(|held| value > held) {
        *high = Some(value);
    }
}

/// The range of a column of strings, walked as bytes.
fn text(vector: &Vector) -> (Option<Bound>, Option<Bound>) {
    let mut low: Option<&[u8]> = None;
    let mut high: Option<&[u8]> = None;
    for index in 0..vector.len() {
        let Some(bytes) = vector.bytes_at(index) else { continue };
        if low.is_none_or(|held| bytes < held) {
            low = Some(bytes);
        }
        if high.is_none_or(|held| bytes > held) {
            high = Some(bytes);
        }
    }
    (low.map(|bytes| Bound::Bytes(bytes.to_vec())), high.map(|bytes| Bound::Bytes(bytes.to_vec())))
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::{Chunk, Vector};

    use super::{Bound, Op, Probe, Zone};

    /// A chunk of one `INTEGER` column holding `values`.
    fn chunk(values: &[i32]) -> Chunk {
        let held: Vec<Value> = values.iter().map(|n| Value::Integer(*n)).collect();
        let vector = Vector::from_values(LogicalType::Integer, &held).expect("a column");
        Chunk::new(vec![vector]).expect("a chunk")
    }

    #[test]
    fn a_chunk_of_integers_knows_its_two_ends() {
        let zone = Zone::of(&chunk(&[7, 2, 9, 4]));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.low, Some(Bound::Int(2)));
        assert_eq!(range.high, Some(Bound::Int(9)));
        assert_eq!(range.nulls, 0);
    }

    #[test]
    fn a_probe_outside_the_range_skips_the_chunk_and_one_inside_it_does_not() {
        let zone = Zone::of(&chunk(&[10, 20]));
        let outside = vec![Probe { column: 0, op: Op::Equal, value: Bound::Int(62) }];
        let inside = vec![Probe { column: 0, op: Op::Equal, value: Bound::Int(10) }];
        assert!(zone.skips(&outside));
        assert!(!zone.skips(&inside));
    }

    #[test]
    fn a_probe_the_whole_range_passes_is_certain_and_one_it_straddles_is_not() {
        let zone = Zone::of(&chunk(&[10, 20]));
        let whole = vec![Probe { column: 0, op: Op::GreaterOrEqual, value: Bound::Int(10) }];
        let part = vec![Probe { column: 0, op: Op::GreaterOrEqual, value: Bound::Int(15) }];
        assert!(zone.certain(&whole));
        assert!(!zone.certain(&part));
        // And neither of them skips it, which is the point: the three answers are different
        // answers and the middle one used to be the same as the last.
        assert!(!zone.skips(&whole));
        assert!(!zone.skips(&part));
    }

    /// The quantifier turns over between the two questions, which is the thing to get wrong.
    #[test]
    fn every_probe_has_to_pass_for_the_chunk_to_be_certain() {
        let zone = Zone::of(&chunk(&[10, 20]));
        let probes = vec![
            Probe { column: 0, op: Op::GreaterOrEqual, value: Bound::Int(10) },
            Probe { column: 0, op: Op::Less, value: Bound::Int(15) },
        ];
        assert!(!zone.certain(&probes), "the second probe throws rows away");
        assert!(!zone.skips(&probes), "and neither probe rules the chunk out");
    }

    /// A comparison against null is null and a filter keeps what is true, so one null is enough.
    #[test]
    fn a_null_in_the_column_makes_the_chunk_uncertain_whatever_the_ends_say() {
        let held = [Some(10), None, Some(20)]
            .map(|value| value.map_or(Value::Null, Value::Integer))
            .to_vec();
        let vector = Vector::from_values(LogicalType::Integer, &held).expect("a column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let whole = vec![Probe { column: 0, op: Op::GreaterOrEqual, value: Bound::Int(10) }];
        assert_eq!(zone.column(0).expect("one column").nulls, 1);
        assert!(!zone.certain(&whole));
    }

    #[test]
    fn a_probe_on_a_column_the_zone_does_not_describe_is_not_certain() {
        let zone = Zone::of(&chunk(&[10, 20]));
        let probes = vec![Probe { column: 7, op: Op::GreaterOrEqual, value: Bound::Int(0) }];
        assert!(!zone.certain(&probes));
    }

    /// The conjunction, which is where a query like ClickBench 37 gets its selectivity: four tests
    /// and any one of them is enough.
    #[test]
    fn one_probe_of_several_is_enough_to_skip() {
        let zone = Zone::of(&chunk(&[10, 20]));
        let probes = vec![
            Probe { column: 0, op: Op::GreaterOrEqual, value: Bound::Int(10) },
            Probe { column: 0, op: Op::Greater, value: Bound::Int(99) },
        ];
        assert!(zone.skips(&probes));
    }

    #[test]
    fn a_probe_on_a_column_the_zone_does_not_describe_keeps_the_chunk() {
        let zone = Zone::of(&chunk(&[1]));
        let probes = vec![Probe { column: 4, op: Op::Equal, value: Bound::Int(62) }];
        assert!(!zone.skips(&probes));
    }

    #[test]
    fn nulls_are_counted_and_do_not_move_the_ends() {
        let values = vec![Value::Integer(5), Value::Null, Value::Integer(3)];
        let vector = Vector::from_values(LogicalType::Integer, &values).expect("a column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.nulls, 1);
        assert_eq!(range.low, Some(Bound::Int(3)));
        assert_eq!(range.high, Some(Bound::Int(5)));
    }

    #[test]
    fn a_column_of_only_nulls_has_no_ends_and_rules_nothing_out() {
        let values = vec![Value::Null, Value::Null];
        let vector = Vector::from_values(LogicalType::Integer, &values).expect("a column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.nulls, 2);
        assert_eq!(range.low, None);
        let probes = vec![Probe { column: 0, op: Op::Equal, value: Bound::Int(1) }];
        assert!(!zone.skips(&probes));
    }

    #[test]
    fn a_column_of_strings_is_ordered_as_bytes() {
        let values = vec![
            Value::Varchar("grace".to_string()),
            Value::Varchar("ada".to_string()),
            Value::Varchar("turing".to_string()),
        ];
        let vector = Vector::from_values(LogicalType::Varchar, &values).expect("a column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.low, Some(Bound::Bytes(b"ada".to_vec())));
        assert_eq!(range.high, Some(Bound::Bytes(b"turing".to_vec())));
    }

    /// The `SearchPhrase` shape: more entries in the dictionary than rows in the chunk. The rows are
    /// walked instead, which costs the rows and gives the exact range rather than the dictionary's.
    #[test]
    fn a_dictionary_wider_than_its_chunk_is_read_through_its_codes() {
        let values: Vec<Value> = ["ada", "babbage", "grace", "hopper", "turing"]
            .iter()
            .map(|s| s.to_string())
            .map(Value::Varchar)
            .collect();
        let inner = Vector::from_values(LogicalType::Varchar, &values).expect("a dictionary");
        let vector = Vector::dictionary(vec![1, 2], inner).expect("two rows of five values");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.low, Some(Bound::Bytes(b"babbage".to_vec())), "not ada");
        assert_eq!(range.high, Some(Bound::Bytes(b"grace".to_vec())), "not turing");
        let probes = vec![Probe { column: 0, op: Op::Equal, value: Bound::Bytes(b"ada".to_vec()) }];
        assert!(zone.skips(&probes));
    }

    /// A dictionary holds values no row points at, so its own two ends are wider than the column.
    /// Reading the codes is what closes that gap, and the gap is the whole point: the wider bounds
    /// would keep a chunk this one skips, and could not have answered a `MIN` at all.
    #[test]
    fn a_dictionary_takes_its_bounds_from_its_codes_and_not_from_its_values() {
        let values = vec![Value::Integer(1), Value::Integer(50), Value::Integer(99)];
        let inner = Vector::from_values(LogicalType::Integer, &values).expect("a dictionary");
        let vector = Vector::dictionary(vec![1, 1, 1], inner).expect("a coded column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.low, Some(Bound::Int(50)), "every row is 50, and 1 and 99 are not rows");
        assert_eq!(range.high, Some(Bound::Int(50)));
        assert!(range.exact);
        assert_eq!(range.sum, Some(150));
        let probes = vec![Probe { column: 0, op: Op::Equal, value: Bound::Int(1) }];
        assert!(zone.skips(&probes), "a value in the dictionary that no row holds");
    }

    /// A dictionary of something this cannot add up or compare in its own type falls back to the
    /// values, which is the bound it always was, and says it is a bound.
    #[test]
    fn a_dictionary_of_floats_falls_back_to_the_bounds_its_values_give() {
        let values = vec![Value::Double(1.0), Value::Double(50.0), Value::Double(99.0)];
        let inner = Vector::from_values(LogicalType::Double, &values).expect("a dictionary");
        let vector = Vector::dictionary(vec![1, 1, 1], inner).expect("a coded column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.low, Some(Bound::Real(1.0)), "every row is 50, and the bound is wider");
        assert!(!range.exact);
        assert_eq!(range.sum, None);
        // Wider bounds keep chunks they could have skipped. They never skip one they should keep.
        let probes = vec![Probe { column: 0, op: Op::Equal, value: Bound::Real(1.0) }];
        assert!(!zone.skips(&probes));
    }

    /// A packed width is a whole number of bits, so the range a column was packed against covers
    /// values it does not hold. Unpacking costs a shift and a mask per row and closes that too.
    #[test]
    fn a_bit_packed_column_is_unpacked_rather_than_read_off_its_packing() {
        // Four rows of four bits each, holding 3, 1, 2 and 1, over a base of 10.
        let words = vec![0x1213_u64];
        let vector =
            Vector::packed(LogicalType::Integer, words, 4, 10, 4).expect("a packed column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.low, Some(Bound::Int(11)), "not the base of 10");
        assert_eq!(range.high, Some(Bound::Int(13)), "not the ceiling of 25");
        assert!(range.exact);
        assert_eq!(range.sum, Some(47));
    }

    /// A run is worth one multiply rather than one add per row, which is the one form here where
    /// the exact answer is cheaper than the rows rather than the same price.
    #[test]
    fn a_run_encoded_column_is_weighed_by_how_long_each_run_is() {
        let values = vec![Value::Integer(7), Value::Integer(2)];
        let inner = Vector::from_values(LogicalType::Integer, &values).expect("the run values");
        let vector = Vector::runs(vec![3, 5], inner).expect("three sevens and two twos");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.low, Some(Bound::Int(2)));
        assert_eq!(range.high, Some(Bound::Int(7)));
        assert!(range.exact);
        assert_eq!(range.sum, Some(25), "three sevens and two twos");
    }

    #[test]
    fn a_constant_column_is_both_ends_of_itself() {
        let vector = Vector::constant(LogicalType::Integer, Value::Integer(62), 100);
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.low, Some(Bound::Int(62)));
        assert_eq!(range.high, Some(Bound::Int(62)));
        let probes = vec![Probe { column: 0, op: Op::Equal, value: Bound::Int(63) }];
        assert!(zone.skips(&probes));
    }

    /// A NaN row does not poison the range. If it did, the range would answer nothing about every
    /// query on the column, and one bad row in a chunk would cost the whole column its zone map.
    #[test]
    fn a_nan_in_a_column_does_not_take_the_ends_with_it() {
        let values = vec![
            Value::Double(2.5),
            Value::Double(f64::NAN),
            Value::Double(9.0),
            Value::Double(1.0),
        ];
        let vector = Vector::from_values(LogicalType::Double, &values).expect("a column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.low, Some(Bound::Real(1.0)));
        assert_eq!(range.high, Some(Bound::Real(9.0)));
        let probes = vec![Probe { column: 0, op: Op::Greater, value: Bound::Real(9.0) }];
        assert!(zone.skips(&probes));
    }

    #[test]
    fn a_sequence_is_its_first_and_last_value() {
        let vector = Vector::sequence(100, -5, 4);
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.low, Some(Bound::Int(85)), "it counts down");
        assert_eq!(range.high, Some(Bound::Int(100)));
    }

    /// The range of one column of `values`, typed as `ty`.
    fn only(ty: LogicalType, values: &[Value]) -> super::Range {
        let vector = Vector::from_values(ty, values).expect("a column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        zone.column(0).expect("one column").clone()
    }

    #[test]
    fn a_walked_column_says_so_and_says_what_it_adds_up_to() {
        let values = vec![Value::Integer(7), Value::Integer(2), Value::Integer(9)];
        let range = only(LogicalType::Integer, &values);
        assert!(range.exact, "the rows were walked one at a time");
        assert_eq!(range.sum, Some(18));
    }

    #[test]
    fn a_null_row_is_left_out_of_the_total_the_way_it_is_left_out_of_the_ends() {
        let values = vec![Value::Integer(7), Value::Null, Value::Integer(9)];
        let range = only(LogicalType::Integer, &values);
        assert!(range.exact);
        assert_eq!(range.sum, Some(16), "the null added nothing");
    }

    /// A float total computed here and a float total computed by the operator can differ in the
    /// last bits, because addition in this domain depends on the order, and a NaN is left out of
    /// the ends above while `MAX` would have to answer with it. So neither is claimed.
    #[test]
    fn a_float_column_reports_neither_an_exact_end_nor_a_total() {
        let values = vec![Value::Double(2.5), Value::Double(9.0), Value::Double(1.0)];
        let range = only(LogicalType::Double, &values);
        assert!(!range.exact);
        assert_eq!(range.sum, None);
        assert_eq!(range.low, Some(Bound::Real(1.0)), "it is still a bound worth skipping on");
    }

    #[test]
    fn a_constant_column_adds_up_to_itself_times_the_rows_that_are_not_null() {
        let vector = Vector::constant(LogicalType::Integer, Value::Integer(62), 100);
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert!(range.exact);
        assert_eq!(range.sum, Some(6200));
    }

    #[test]
    fn a_column_of_nothing_but_nulls_adds_up_to_zero_rather_than_to_nothing_known() {
        let range = only(LogicalType::BigInt, &[Value::Null, Value::Null]);
        assert!(range.exact, "there is no end here to be wrong about");
        assert_eq!(range.sum, Some(0));
    }

    /// A timestamp is an `INT64` of microseconds, so the walk hands back the integer and nothing
    /// else. The ends have to say what that integer is a count of, because the constant a query
    /// compares them against says so, and two bounds that disagree about their domain do not
    /// compare at all.
    #[test]
    fn a_timestamp_column_reports_its_ends_in_the_domain_a_timestamp_constant_arrives_in() {
        let values = vec![Value::Timestamp(1_000_000), Value::Timestamp(3_000_000)];
        let range = only(LogicalType::Timestamp, &values);
        assert_eq!(range.low, Some(Bound::Scaled { unscaled: 1_000_000, scale: 6 }));
        assert_eq!(range.high, Some(Bound::Scaled { unscaled: 3_000_000, scale: 6 }));
    }

    /// The reason the domain matters: before this, a timestamp probe answered nothing at all and
    /// every ClickBench query with an `EventTime` range read all nine hundred and seventy four
    /// parts of the file.
    #[test]
    fn a_timestamp_probe_past_the_end_of_the_column_skips_it() {
        let values = vec![Value::Timestamp(1_000_000), Value::Timestamp(3_000_000)];
        let vector = Vector::from_values(LogicalType::Timestamp, &values).expect("a column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let past = Bound::of_value(&Value::Timestamp(9_000_000)).expect("a bound");
        let inside = Bound::of_value(&Value::Timestamp(2_000_000)).expect("a bound");
        assert!(zone.skips(&[Probe { column: 0, op: Op::Equal, value: past }]));
        assert!(!zone.skips(&[Probe { column: 0, op: Op::Equal, value: inside }]));
    }

    /// A decimal is the same story with a scale the type carries rather than one the unit names.
    #[test]
    fn a_decimal_column_reports_its_ends_at_the_scale_of_its_type() {
        let ty = LogicalType::Decimal { width: 10, scale: 2 };
        let values = vec![Value::Decimal { unscaled: 150, width: 10, scale: 2 }];
        let range = only(ty, &values);
        assert_eq!(range.low, Some(Bound::Scaled { unscaled: 150, scale: 2 }));
    }

    /// A date is a count of days with no power of ten under it, so it stays the integer it was.
    #[test]
    fn a_date_column_is_left_as_the_plain_integer_it_counts_in() {
        let range = only(LogicalType::Date, &[Value::Date(19_000)]);
        assert_eq!(range.low, Some(Bound::Int(19_000)));
    }
}

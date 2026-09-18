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
//! yields nothing, and never costs a row. That is what makes the compressed forms cheap here: a
//! dictionary column's bounds come from its dictionary and not from its codes, a bit packed
//! column's come from the base and the ceiling it was packed against, and a run encoded column's
//! come from its run values. None of those walk the rows at all.
//!
//! # What it costs to build
//!
//! One pass over each column as it is appended. On `hits` at a million rows, one thread, that is
//! about 60 milliseconds on a load of 0.93 seconds, so a table with a full set of zone maps costs
//! 1.02 seconds to build against DuckDB's 1.18 for the same `CREATE TABLE AS SELECT`.
//!
//! Getting it to 60 milliseconds took two rounds. The first cost 260, which is where the comments on
//! `extremes` and on `narrower` come from: one was a `Bound` allocated per value instead of a
//! compare in the column's own type, and the other was a shared dictionary rescanned once per chunk.
//!
//! That cost is real and it belongs in the load time rather than hidden behind it, because a table
//! built once and queried forty three times and a table built once and queried once want different
//! answers, and the only way to have that conversation is with both numbers in front of you.
//! [`MemoryTable::stats_ns`] is what a load reports it spent here.

use rudb_common::Value;
use rudb_common::bounds::{Bound, Op, excluded};
use rudb_vector::{Chunk, Data, Form, Vector};

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
    /// every row, which is the flat layouts and the two forms that hold one value.
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
    /// Whether this range says no row of the chunk can pass `probe`.
    #[must_use]
    pub fn excludes(&self, op: Op, value: &Bound) -> bool {
        excluded(op, value, self.low.as_ref(), self.high.as_ref())
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
}

/// The range of one vector, in whatever form it arrived in.
fn range(vector: &Vector) -> Range {
    let nulls = vector.len() - vector.validity().count_valid(vector.len());
    let (low, high) = match vector.form() {
        // One value, which is both ends of the range.
        Form::Constant => match vector.constant_value().and_then(Bound::of_value) {
            Some(only) => (Some(only.clone()), Some(only)),
            None => (None, None),
        },
        // A start and a step, so the two ends are the first and the last, whichever way it runs.
        Form::Sequence => match vector.sequence_parts() {
            Some((start, step)) => ends(start, step, vector.len()),
            None => (None, None),
        },
        // The range the column was packed against, which is at least as wide as the column.
        Form::BitPacked => match vector.packed_parts() {
            Some(packed) => (Some(Bound::Int(packed.base())), Some(Bound::Int(packed.ceiling()))),
            None => (None, None),
        },
        // The distinct values, which are a superset of the values the codes point at.
        Form::Dictionary => match vector.dictionary_parts() {
            Some((_, values)) => narrower(vector, values),
            None => (None, None),
        },
        // Same, for the value of each run.
        Form::Rle => match vector.run_parts() {
            Some((_, values)) => narrower(vector, values),
            None => (None, None),
        },
        Form::Flat => match vector.data() {
            Some(data) => flat(vector, data),
            None => (None, None),
        },
        // Strings that are not flat. Walked as bytes rather than as values, because a `Value` per
        // row of a `URL` column is a heap allocation per row and this runs over every column of
        // every chunk of a load.
        Form::StringView | Form::Fsst => text(vector),
        // A form added since this was written. Saying nothing about it keeps its rows, which is the
        // answer that is wrong slowly rather than wrong.
        _ => (None, None),
    };
    let (exact, sum) = counted(vector);
    Range { low, high, nulls, exact, sum }
}

/// Whether the two ends above are exact, and the sum of the values behind them.
///
/// Both answers come from the same place, which is whether this walked the rows or read a summary
/// somebody else wrote. A bit packed column's ends come from the range it was packed against and a
/// dictionary column's come from its dictionary, and neither of those is the column, so neither can
/// say what the smallest value in it is or what they add up to.
fn counted(vector: &Vector) -> (bool, Option<i128>) {
    match vector.form() {
        // One value repeated, so the ends are that value and the sum is it times the rows that are
        // not null. That is the whole column with no loop at all.
        Form::Constant => match vector.constant_value() {
            // Every row is null, so there are no ends to get wrong and nothing to add up, and a
            // total of nothing is zero rather than unknown.
            Some(Value::Null) => (true, Some(0)),
            Some(value) => match Bound::of_value(value) {
                Some(Bound::Int(only)) => {
                    let rows = vector.validity().count_valid(vector.len()) as i128;
                    (true, only.checked_mul(rows))
                }
                // A float, including a NaN, which is the second reason on `Range::exact`.
                Some(Bound::Real(_)) => (false, None),
                // A constant that is not a number still has exact ends. It is the same one value.
                _ => (true, None),
            },
            None => (false, None),
        },
        // The first and the last, and neither of them was guessed. A null row inside a sequence has
        // no value of its own, so the ends would then cover a row the column does not hold.
        Form::Sequence => {
            let walked = vector.sequence_parts().is_some();
            (walked && !vector.validity().has_nulls(vector.len()), None)
        }
        Form::Flat => match vector.data() {
            Some(data) => summed(vector, data),
            None => (false, None),
        },
        // A string walked as bytes is walked a row at a time, so those ends are the column's.
        Form::StringView | Form::Fsst => (true, None),
        _ => (false, None),
    }
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

/// The range of a column that points somewhere else, from whichever end is cheaper to walk.
///
/// Scanning the values instead of the rows is what makes a dictionary and a run encoded column
/// nearly free here, because the values are a superset of what the rows hold and a superset is a
/// bound that is still correct.
///
/// It stops being cheap when the values outnumber the rows, which happens because one dictionary is
/// shared by every chunk of a column. Scanning it per chunk then does the same work once per chunk.
/// On `hits` at a million rows that was `SearchPhrase`, whose dictionary is far larger than a chunk:
/// 90 milliseconds of the load went on that one column, against 10 for `URL`, which is plain and
/// several times its size. Walking the rows through the codes gives the exact range for the cost of
/// the rows, so that is what happens when there are fewer of them.
fn narrower(vector: &Vector, values: &Vector) -> (Option<Bound>, Option<Bound>) {
    // Only for strings, because `text` is the one row walk here that borrows rather than allocating.
    // A numeric dictionary this size is not a shape any reader in this engine produces.
    let strings = matches!(values.data(), Some(Data::Varlen(_))) || values.text_parts().is_some();
    if strings && values.len() > vector.len() {
        return text(vector);
    }
    let inner = range(values);
    (inner.low, inner.high)
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

    /// A dictionary is not walked, so its bounds cover values no row holds. That is allowed, and
    /// this pins it, because the alternative reading is that the bounds are wrong.
    #[test]
    fn a_dictionary_takes_its_bounds_from_its_values_and_not_from_its_codes() {
        let values = vec![Value::Integer(1), Value::Integer(50), Value::Integer(99)];
        let inner = Vector::from_values(LogicalType::Integer, &values).expect("a dictionary");
        let vector = Vector::dictionary(vec![1, 1, 1], inner).expect("a coded column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert_eq!(range.low, Some(Bound::Int(1)), "every row is 50, and the bound is wider");
        assert_eq!(range.high, Some(Bound::Int(99)));
        // Wider bounds keep chunks they could have skipped. They never skip one they should keep.
        let probes = vec![Probe { column: 0, op: Op::Equal, value: Bound::Int(1) }];
        assert!(!zone.skips(&probes));
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

    /// The bounds of a dictionary are allowed to be wider than its rows, which is fine for skipping
    /// a chunk and wrong for answering a `MIN`. This is the flag that tells the two apart.
    #[test]
    fn a_dictionary_does_not_claim_its_wider_bounds_are_the_column() {
        let values = vec![Value::Integer(1), Value::Integer(50), Value::Integer(99)];
        let inner = Vector::from_values(LogicalType::Integer, &values).expect("a dictionary");
        let vector = Vector::dictionary(vec![1, 1, 1], inner).expect("a coded column");
        let zone = Zone::of(&Chunk::new(vec![vector]).expect("a chunk"));
        let range = zone.column(0).expect("one column");
        assert!(!range.exact, "the bounds came from the values and not from the rows");
        assert_eq!(range.sum, None);
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
}

//! The smallest and the largest value of every column of every chunk, kept so a scan can skip one.
//!
//! A filter that compares a column against a constant can be answered against two numbers instead of
//! against a thousand rows, and the rows it rules out are rows nothing has to read, copy, compare or
//! hand downstream. Parquet does this a row group at a time in [`rudb_parquet::skips`], which is a
//! hundred thousand rows on the files this engine is measured against. This does it a chunk at a
//! time, which is a thousand, and the difference between those two granularities is most of what a
//! selective query costs.
//!
//! On the ClickBench `hits` sample that is not a small difference. Query 37 filters on `CounterID`,
//! which is written in sorted order, and 7381 rows of a million have the value it asks for. Row
//! group bounds cut nine groups to two, which is 222 thousand rows to scan for 6722 answers. Chunk
//! bounds cut the same query to the handful of chunks those rows actually sit in.
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
//! One pass over each column as it is appended, which is the cost this trades against every query
//! that follows. That cost is real and it belongs in the load time, not hidden behind it, because a
//! table built once and queried forty three times and a table built once and queried once want
//! different answers and the only way to have that conversation is with both numbers in front of
//! you. [`MemoryTable::stats_ns`] is what a load reports it spent here.

use rudb_common::bounds::{Bound, Op, excluded};
use rudb_vector::{Chunk, Data, Form, Vector};

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
            Some((_, values)) => {
                let inner = range(values);
                (inner.low, inner.high)
            }
            None => (None, None),
        },
        // Same, for the value of each run.
        Form::Rle => match vector.run_parts() {
            Some((_, values)) => {
                let inner = range(values);
                (inner.low, inner.high)
            }
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
    Range { low, high, nulls }
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
/// The null check is hoisted out of the loop rather than asked per row, because most columns of most
/// chunks have no nulls at all and a branch per row here is a branch per row of the whole load.
fn extremes<T: Copy + PartialOrd>(values: &[T], vector: &Vector) -> (Option<T>, Option<T>) {
    let mut low: Option<T> = None;
    let mut high: Option<T> = None;
    let nullable = vector.validity().has_nulls(vector.len());
    for (index, &value) in values.iter().enumerate() {
        // A value that does not order against itself is a NaN. It is left out because a NaN at
        // either end makes every comparison against the range undecidable, which is a range that
        // rules nothing out, and a filter is false for a NaN row whichever way this goes. For every
        // integer layout this folds away, since their `partial_cmp` never answers `None`.
        let comparable = value.partial_cmp(&value).is_some();
        if !comparable || (nullable && vector.is_null_at(index)) {
            continue;
        }
        if low.is_none_or(|held| value < held) {
            low = Some(value);
        }
        if high.is_none_or(|held| value > held) {
            high = Some(value);
        }
    }
    (low, high)
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
}

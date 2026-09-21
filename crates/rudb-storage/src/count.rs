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
//! packed column is a shift and a mask. Only a flat column and a string column pay a hash a row, and
//! [`Sketch::add_hash`] returns on a comparison for every value above the threshold, which after the
//! first few thousand rows is nearly all of them.
//!
//! The memory is 32 KB a column at the default k, so a hundred and five column table like ClickBench
//! `hits` holds 3.4 MB of sketch. That is the price of the whole table's statistics and it does not
//! grow with the rows.
//!
//! # A column this cannot read says nothing
//!
//! A form or a type with no arm here, which today is an interval and the nested types, sets the
//! column's `blind` flag and the column answers `None` from then on. Not a wider count and not a
//! narrower one. A distinct count that is missing is a fact the estimator already knows how to
//! handle, because that is what every column of every memory table gave it until now, and a
//! distinct count that is wrong is one it has no defence against at all.

use rudb_common::{LogicalType, Value};
use rudb_encoding::sketch::{DEFAULT_K, Sketch, hash64};
use rudb_vector::{Chunk, Data, Form, Vector};

/// The distinct count of every column of one table, built as chunks are appended.
#[derive(Debug, Clone)]
pub struct Counts {
    columns: Vec<Column>,
}

/// One column's sketch, and whether anything reached it that this module could not read.
#[derive(Debug, Clone)]
struct Column {
    sketch: Sketch,
    /// Set by the first chunk this could not walk, and never cleared.
    ///
    /// A sketch that missed some of its rows is not a sketch of the column, and the failure it would
    /// cause is a count that is too low, which is the direction that turns an estimate into a wrong
    /// plan rather than a cautious one. Once it is set the column answers nothing.
    blind: bool,
}

impl Counts {
    /// A counter for a table of `width` columns, holding nothing yet.
    #[must_use]
    pub fn new(width: usize) -> Self {
        Self {
            columns: (0..width)
                .map(|_| Column {
                    // The default k is the only k here, so two of these can always be unioned. The
                    // constructor only fails on a k of zero.
                    sketch: Sketch::new(DEFAULT_K).unwrap_or_else(|_| Sketch::of(&[])),
                    blind: false,
                })
                .collect(),
        }
    }

    /// Counts one chunk, one column at a time.
    ///
    /// A chunk with the wrong number of columns is a chunk the table has already refused, so the
    /// extra columns of a wider one are ignored rather than checked for again here.
    pub fn add(&mut self, chunk: &Chunk) {
        for (at, column) in self.columns.iter_mut().enumerate() {
            if column.blind {
                continue;
            }
            let Ok(vector) = chunk.column(at) else { continue };
            if !walk(vector, &mut column.sketch) {
                column.blind = true;
                // The hashes taken so far describe some of the rows and no question is going to be
                // answered from them, so they are dropped rather than carried.
                column.sketch = Sketch::of(&[]);
            }
        }
    }

    /// How many distinct non-null values one column holds, when that number is exact.
    ///
    /// `None` when the sketch filled up, which means the column has at least [`DEFAULT_K`] distinct
    /// values and what is left is an estimate. This is the one a query result may be read out of.
    #[must_use]
    pub fn exact(&self, column: usize) -> Option<u64> {
        let held = self.columns.get(column)?;
        if held.blind || !held.sketch.is_exact() {
            return None;
        }
        Some(held.sketch.len() as u64)
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
        if held.sketch.is_exact() {
            return Some((held.sketch.len() as u64, true));
        }
        Some((held.sketch.distinct().round() as u64, false))
    }

    /// How many columns this is counting.
    #[must_use]
    pub fn width(&self) -> usize {
        self.columns.len()
    }
}

/// Adds every non-null value of one vector to one sketch.
///
/// `false` means a form or a type with no arm here, which is what sets the column's `blind` flag.
/// The caller has to treat a `false` as poisoning the whole column and not as skipping one chunk,
/// because a sketch missing some of its rows counts too few distinct values and says nothing about
/// having done so.
fn walk(vector: &Vector, sketch: &mut Sketch) -> bool {
    match vector.form() {
        // One value repeated, so one hash for however many rows there are. A constant that is null
        // adds nothing, which is right: a distinct count does not count the null.
        Form::Constant => match vector.constant_value() {
            Some(Value::Null) | None => true,
            Some(value) => match hash_value(value) {
                Some(hash) => {
                    sketch.add_hash(hash);
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
                    sketch.add_hash(hash_signed(value));
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
                    sketch.add_hash(hash_signed(base + i128::from(packed.code(row))));
                }
                true
            }
            None => false,
        },
        Form::Dictionary => match vector.dictionary_parts() {
            Some((codes, values)) => coded(vector, codes, values, sketch),
            None => false,
        },
        // One hash a run rather than one a row, because every row of a run holds the same value and
        // a sketch counts a value once however many times it arrives. A run over a vector that has
        // nulls of its own is the one case that has to go row by row, since then a run is partly a
        // value and partly nothing and the run boundaries no longer say which.
        Form::Rle => match vector.run_parts() {
            Some((stops, values)) => {
                if vector.validity().has_nulls(vector.len()) {
                    return rows(vector, sketch);
                }
                let inner = values.validity();
                for run in 0..stops.len().min(values.len()) {
                    if !inner.is_valid(run) {
                        continue;
                    }
                    match value_hash(values, run) {
                        Some(hash) => sketch.add_hash(hash),
                        None => return false,
                    }
                }
                true
            }
            None => false,
        },
        Form::Flat => match vector.data() {
            Some(data) => flat(vector, data, sketch),
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
                sketch.add_hash(hash64(bytes));
            }
            true
        }
        // A form added since this was written. The column stops answering, which is the failure that
        // is visible in a plan rather than the one that is visible in a wrong row count.
        _ => false,
    }
}

/// A dictionary column, hashed once per dictionary entry and added once per row.
///
/// The dictionary is hashed lazily into `seen`, because a dictionary a Parquet reader hands over
/// covers a whole column chunk and can hold far more values than the rows being counted. Hashing it
/// whole would be the slower of the two exactly when the dictionary is doing its job, and adding
/// every entry would count values no row of this table points at.
fn coded(vector: &Vector, codes: &[u32], values: &Vector, sketch: &mut Sketch) -> bool {
    let validity = vector.validity();
    let nullable = validity.has_nulls(vector.len());
    let inner = values.validity();
    let mut seen: Vec<Option<u64>> = vec![None; values.len()];
    for row in 0..vector.len() {
        if nullable && !validity.is_valid(row) {
            continue;
        }
        let Some(&code) = codes.get(row) else { return false };
        let code = code as usize;
        // A code pointing at a null is a null row, however the vector's own mask reads, which is the
        // same rule `Vector::is_null_at` follows through a dictionary.
        if !inner.is_valid(code) {
            continue;
        }
        let Some(slot) = seen.get_mut(code) else { return false };
        let hash = match *slot {
            Some(hash) => hash,
            None => {
                let Some(hash) = value_hash(values, code) else { return false };
                *slot = Some(hash);
                hash
            }
        };
        sketch.add_hash(hash);
    }
    true
}

/// A flat column, one pass over its typed run.
///
/// The type is matched on once for the column rather than once for the value, which is what makes a
/// million rows of `INTEGER` a multiply a row rather than a match and a `Value` a row.
fn flat(vector: &Vector, data: &Data, sketch: &mut Sketch) -> bool {
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
                    Some(hash) => sketch.add_hash(hash),
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
        // A string arrives here as views into the arena, which `bytes_at` is the reader for.
        Data::Varlen(_) => pass!(|row: usize| vector.bytes_at(row).map(hash64)),
        // An interval and an empty column. Neither has a canonical pattern written down above, and
        // inventing one here rather than in `hash_value` is how the two stop agreeing.
        _ => false,
    }
}

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
fn rows(vector: &Vector, sketch: &mut Sketch) -> bool {
    for row in 0..vector.len() {
        if vector.is_null_at(row) {
            continue;
        }
        match hash_value(&vector.value_at(row)) {
            Some(hash) => sketch.add_hash(hash),
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
    hash64(&value.to_le_bytes())
}

/// An unsigned value the same way, which for every value below 2^127 is the same sixteen bytes a
/// signed one of the same number gives.
fn hash_unsigned(value: u128) -> u64 {
    hash64(&value.to_le_bytes())
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
    use rudb_encoding::sketch::DEFAULT_K;
    use rudb_vector::{Chunk, Form, Vector};

    use super::{Counts, countable, hash_value};

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
        let error = (estimate as f64 - f64::from(DEFAULT_K as u32)).abs() / f64::from(DEFAULT_K as u32);
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
            (LogicalType::Blob, Value::Blob(vec![1].into())),
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

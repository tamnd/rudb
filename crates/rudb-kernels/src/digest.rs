//! `approx_quantile`, answered from a t-digest that takes the same steps as the pin's.
//!
//! A t-digest keeps a group's values as a few hundred centroids, each a mean and a weight, packed
//! tight near the ends and loose in the middle so the tails stay sharp. Values collect unsorted
//! until there are 800 of them, then they are sorted into the centroids and the centroids are
//! packed again. The pin's digest is Ted Dunning's merging digest with a compression of 100, and
//! every step here is that one in the same order with the same floating point sums, so over the
//! same rows in the same order the answer is the pin's to the last digit. Over several threads the
//! answer depends on how the rows were split, in the pin as much as here.
//!
//! A value is held as a DOUBLE and the answer is cast back to the type it came in, rounded to the
//! nearest whole number with ties to even and held to the range of the type. A value that is not
//! finite is passed by. A fraction is a FLOAT, so 0.1 is 0.100000001490116.
//!
//! One step is not the pin's. The pin starts its running maximum at the smallest positive double
//! rather than the most negative one, so over values that are all negative the top quantile is
//! 2.2e-308, above every value, which is tamnd/duckdb#16. This starts it at the most negative one.

use rudb_common::{Error, Field, LogicalType, Result, Value};

use crate::quantile::Column;

/// How many centroids the digest aims for, about.
const COMPRESSION: f64 = 100.0;
/// How many packed centroids there can be before the digest is packed again.
const MOST_PACKED: usize = 200;
/// How many values collect before they are sorted in.
const MOST_WAITING: usize = 800;

/// A mean and how many values went into it.
#[derive(Debug, Clone, Copy)]
struct Centroid {
    mean: f64,
    weight: f64,
}

impl Centroid {
    /// Takes in another centroid, which moves the mean toward its mean by its share of the weight.
    fn add(&mut self, other: Self) {
        if self.weight == 0.0 {
            *self = other;
        } else {
            self.weight += other.weight;
            self.mean += other.weight * (other.mean - self.mean) / self.weight;
        }
    }
}

/// The digest itself.
#[derive(Debug, Clone)]
struct TDigest {
    min: f64,
    max: f64,
    packed_weight: f64,
    waiting_weight: f64,
    /// The centroids, sorted by mean.
    packed: Vec<Centroid>,
    /// The values not yet sorted in, each a centroid of weight 1, which is all they are held as.
    waiting: Vec<f64>,
}

impl Default for TDigest {
    fn default() -> Self {
        Self {
            min: f64::MAX,
            max: f64::MIN,
            packed_weight: 0.0,
            waiting_weight: 0.0,
            packed: Vec::new(),
            waiting: Vec::new(),
        }
    }
}

impl TDigest {
    fn add(&mut self, x: f64) {
        if x.is_nan() {
            return;
        }
        self.waiting.push(x);
        self.waiting_weight += 1.0;
        if self.dirty() {
            self.process();
        }
    }

    fn dirty(&self) -> bool {
        self.packed.len() > MOST_PACKED || self.waiting.len() > MOST_WAITING
    }

    /// Takes in another digest: its centroids merged with these in order of mean, and its waiting
    /// values added to these.
    fn merge(&mut self, other: &Self) {
        if !other.packed.is_empty() {
            self.packed_weight += other.packed_weight;
            let merged = merge_packed(&other.packed, &self.packed);
            self.packed = merged;
            self.bounds();
        }
        self.waiting.extend_from_slice(&other.waiting);
        self.waiting_weight += other.waiting_weight;
        if self.dirty() {
            self.process();
        }
    }

    /// Moves the minimum and maximum out to the first and last centroid.
    fn bounds(&mut self) {
        if let (Some(first), Some(last)) = (self.packed.first(), self.packed.last()) {
            if first.mean < self.min {
                self.min = first.mean;
            }
            if self.max < last.mean {
                self.max = last.mean;
            }
        }
    }

    /// Sorts the waiting values in with the centroids and packs them all again, so that no
    /// centroid spans more than one step of the scale that is tight at the ends.
    fn process(&mut self) {
        self.waiting.sort_unstable_by(f64::total_cmp);
        let merged = merge_sorted(&self.waiting, &self.packed);
        let Some((&first, rest)) = merged.split_first() else {
            return;
        };
        self.packed_weight += self.waiting_weight;
        self.waiting_weight = 0.0;
        self.packed.clear();
        self.packed.push(first);
        let total = self.packed_weight;
        let mut so_far = first.weight;
        let mut limit = total * integrated_q(1.0);
        for &centroid in rest {
            let projected = so_far + centroid.weight;
            if projected <= limit {
                so_far = projected;
                if let Some(last) = self.packed.last_mut() {
                    last.add(centroid);
                }
            } else {
                let k1 = integrated_location(so_far / total);
                limit = total * integrated_q(k1 + 1.0);
                so_far += centroid.weight;
                self.packed.push(centroid);
            }
        }
        self.waiting.clear();
        self.bounds();
    }

    /// For each packed centroid the weight below its middle, and the total weight at the end. The
    /// pin keeps this up to date as it goes, but only a quantile reads it, so it is made then.
    fn cumulative(&self) -> Vec<f64> {
        let mut cumulative = Vec::with_capacity(self.packed.len() + 1);
        let mut previous = 0.0;
        for centroid in &self.packed {
            cumulative.push(previous + centroid.weight / 2.0);
            previous += centroid.weight;
        }
        cumulative.push(previous);
        cumulative
    }

    /// The value at fraction `q` of the packed centroids, given their [`Self::cumulative`].
    fn quantile(&self, q: f64, cumulative: &[f64]) -> f64 {
        if !(0.0..=1.0).contains(&q) || self.packed.is_empty() {
            return f64::NAN;
        }
        let packed = &self.packed;
        if packed.len() == 1 {
            return packed[0].mean;
        }
        let n = packed.len();
        let index = q * self.packed_weight;
        if index <= packed[0].weight / 2.0 {
            return self.min + 2.0 * index / packed[0].weight * (packed[0].mean - self.min);
        }
        let at = cumulative.partition_point(|&c| c < index);
        if at + 1 != cumulative.len() {
            let z1 = index - cumulative[at - 1];
            let z2 = cumulative[at] - index;
            return weighted_average(packed[at - 1].mean, z2, packed[at].mean, z1);
        }
        let z1 = index - self.packed_weight - packed[n - 1].weight / 2.0;
        let z2 = packed[n - 1].weight / 2.0 - z1;
        weighted_average(packed[n - 1].mean, z1, self.max, z2)
    }
}

/// The sorted waiting values and the packed centroids as one run, a waiting value first when two
/// means are equal, which is how the pin's `inplace_merge` breaks a tie.
fn merge_sorted(waiting: &[f64], packed: &[Centroid]) -> Vec<Centroid> {
    let mut merged = Vec::with_capacity(waiting.len() + packed.len());
    let (mut a, mut b) = (0, 0);
    while a < waiting.len() && b < packed.len() {
        if packed[b].mean < waiting[a] {
            merged.push(packed[b]);
            b += 1;
        } else {
            merged.push(Centroid { mean: waiting[a], weight: 1.0 });
            a += 1;
        }
    }
    merged.extend(waiting[a..].iter().map(|&mean| Centroid { mean, weight: 1.0 }));
    merged.extend_from_slice(&packed[b..]);
    merged
}

/// The centroids of another digest merged with these, a tie broken the way the pin's priority
/// queue of two runs breaks it: the run that did not just give a centroid gives the next one.
fn merge_packed(theirs: &[Centroid], mine: &[Centroid]) -> Vec<Centroid> {
    let runs = [theirs, mine];
    let mut at = [0, 0];
    let mut top = usize::from(!mine.is_empty() && theirs[0].mean > mine[0].mean);
    let mut merged = Vec::with_capacity(theirs.len() + mine.len());
    loop {
        merged.push(runs[top][at[top]]);
        at[top] += 1;
        let other = 1 - top;
        let other_left = at[other] < runs[other].len();
        if at[top] == runs[top].len() {
            if !other_left {
                return merged;
            }
            top = other;
        } else if other_left && runs[other][at[other]].mean <= runs[top][at[top]].mean {
            top = other;
        }
    }
}

/// Where fraction `q` falls on the scale of centroids.
fn integrated_location(q: f64) -> f64 {
    COMPRESSION * ((2.0 * q - 1.0).asin() + std::f64::consts::PI / 2.0) / std::f64::consts::PI
}

/// The fraction step `k` of the scale of centroids falls at.
fn integrated_q(k: f64) -> f64 {
    let pi = std::f64::consts::PI;
    ((k.min(COMPRESSION) * pi / COMPRESSION - pi / 2.0).sin() + 1.0) / 2.0
}

/// The average of `x1` and `x2` weighted by `w1` and `w2`, kept between the two.
fn weighted_average(x1: f64, w1: f64, x2: f64, w2: f64) -> f64 {
    let (x1, w1, x2, w2) = if x1 <= x2 { (x1, w1, x2, w2) } else { (x2, w2, x1, w1) };
    let x = (x1 * w1 + x2 * w2) / (w1 + w2);
    x1.max(x.min(x2))
}

/// The state of an `approx_quantile` group: the digest, made on the first value, and how many
/// values went into it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Digest {
    digest: Option<TDigest>,
    count: u64,
}

impl Digest {
    /// Adds a value that is not null.
    pub(crate) fn push(&mut self, value: &Value) {
        if let Some(x) = encode(value) {
            self.push_number(x);
        }
    }

    /// Adds the row of a column.
    pub(crate) fn push_column(&mut self, column: Column<'_>, row: usize) {
        match column {
            Column::Reals(reals) => self.push_number(reals[row]),
            #[expect(
                clippy::cast_precision_loss,
                reason = "the digest holds a double, as the pin's does"
            )]
            Column::Wholes(_, numbers) => self.push_number(numbers.at(row) as f64),
            Column::Flags(_) => self.push(&column.value(row)),
        }
    }

    fn push_number(&mut self, x: f64) {
        if x.is_finite() {
            self.digest.get_or_insert_default().add(x);
            self.count += 1;
        }
    }

    /// Takes in the digest of another group of the same call.
    pub(crate) fn combine(&mut self, other: &Self) {
        if let Some(theirs) = &other.digest
            && other.count > 0
        {
            self.digest.get_or_insert_default().merge(theirs);
            self.count += other.count;
        }
    }

    /// The answer at `fraction`, or a list of answers for a list of fractions, as `returns`.
    pub(crate) fn finish(&self, fraction: Option<&Value>, returns: &LogicalType) -> Result<Value> {
        let Some(held) = self.digest.as_ref().filter(|_| self.count > 0) else {
            return Ok(Value::Null);
        };
        let mut digest = held.clone();
        digest.process();
        let cumulative = digest.cumulative();
        let share = |q: &Value| -> Result<f64> {
            match *q {
                Value::Float(q) => Ok(f64::from(q)),
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the pin holds the fraction as a FLOAT"
                )]
                Value::Double(q) => Ok(f64::from(q as f32)),
                _ => Err(Error::internal("an approx_quantile fraction that is not a FLOAT")),
            }
        };
        match (fraction, returns) {
            (Some(Value::List { values, .. }), LogicalType::List(element)) => {
                let values = values
                    .iter()
                    .map(|q| decode(digest.quantile(share(q)?, &cumulative), element))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Value::List { element: (**element).clone(), values })
            }
            (Some(q), returns) => decode(digest.quantile(share(q)?, &cumulative), returns),
            (None, _) => Err(Error::internal("an approx_quantile without its fraction")),
        }
    }
}

/// The layout of an `approx_quantile` state: how many values went in, the smallest and largest of
/// them, and the centroids.
pub(crate) fn digest_layout() -> LogicalType {
    let centroid = centroid_type();
    LogicalType::Struct(vec![
        Field::new("count", LogicalType::UBigInt),
        Field::new("min", LogicalType::Double),
        Field::new("max", LogicalType::Double),
        Field::new("centroids", LogicalType::List(Box::new(centroid))),
    ])
}

/// One centroid of the state, its mean and its weight.
fn centroid_type() -> LogicalType {
    LogicalType::Struct(vec![
        Field::new("mean", LogicalType::Double),
        Field::new("weight", LogicalType::Double),
    ])
}

impl Digest {
    /// The state written out the way the pin writes it, with the waiting values sorted in first,
    /// or null for a group that saw nothing.
    pub(crate) fn export(&self) -> Value {
        let Some(held) = self.digest.as_ref().filter(|_| self.count > 0) else {
            return Value::Null;
        };
        let mut digest = held.clone();
        digest.process();
        let centroids = digest.packed.iter().map(|centroid| {
            Value::Struct(vec![
                ("mean".to_string(), Value::Double(centroid.mean)),
                ("weight".to_string(), Value::Double(centroid.weight)),
            ])
        });
        Value::Struct(vec![
            ("count".to_string(), Value::UBigInt(self.count)),
            ("min".to_string(), Value::Double(digest.min)),
            ("max".to_string(), Value::Double(digest.max)),
            (
                "centroids".to_string(),
                Value::List { element: centroid_type(), values: centroids.collect() },
            ),
        ])
    }

    /// The state `export` wrote, read back with the pin's checks, or an empty one for a null.
    ///
    /// # Errors
    ///
    /// If a field is null, or a state that counted values has no centroids.
    pub(crate) fn import(value: &Value) -> Result<Self> {
        let Value::Struct(fields) = value else {
            return if value.is_null() {
                Ok(Self::default())
            } else {
                Err(Error::internal(format!("an approx_quantile state {value:?}")))
            };
        };
        let field = |name: &str| fields.iter().find(|(held, _)| held == name).map(|(_, v)| v);
        let (Some(Value::UBigInt(count)), Some(&Value::Double(min)), Some(&Value::Double(max))) =
            (field("count"), field("min"), field("max"))
        else {
            return Err(broken("the state fields cannot be NULL"));
        };
        let Some(Value::List { values: centroids, .. }) = field("centroids") else {
            return Err(broken("the state fields cannot be NULL"));
        };
        if *count != 0 && centroids.is_empty() {
            return Err(broken("non-zero count requires at least one centroid"));
        }
        let mut packed = Vec::with_capacity(centroids.len());
        for centroid in centroids {
            let Value::Struct(parts) = centroid else {
                return Err(broken("the centroids cannot be NULL"));
            };
            let part = |name: &str| parts.iter().find(|(held, _)| held == name).map(|(_, v)| v);
            let (Some(&Value::Double(mean)), Some(&Value::Double(weight))) =
                (part("mean"), part("weight"))
            else {
                return Err(broken("the centroids cannot be NULL"));
            };
            packed.push(Centroid { mean, weight });
        }
        let packed_weight = packed.iter().map(|centroid| centroid.weight).sum();
        let digest = TDigest {
            min,
            max,
            packed_weight,
            waiting_weight: 0.0,
            packed,
            waiting: Vec::new(),
        };
        Ok(Self { digest: Some(digest), count: *count })
    }
}

fn broken(what: &str) -> Error {
    Error::invalid_input(format!("Invalid approx_quantile state - {what}"))
}

/// A value as the double the digest holds, or `None` for a kind it does not hold.
#[expect(clippy::cast_precision_loss, reason = "the digest holds a double, as the pin's does")]
fn encode(value: &Value) -> Option<f64> {
    Some(match *value {
        Value::TinyInt(n) => f64::from(n),
        Value::SmallInt(n) => f64::from(n),
        Value::Integer(n) | Value::Date(n) => f64::from(n),
        Value::BigInt(n) | Value::Time(n) | Value::Timestamp(n) | Value::TimestampTz(n) => n as f64,
        Value::HugeInt(n) => n as f64,
        Value::Float(x) => f64::from(x),
        Value::Double(x) => x,
        Value::Decimal { unscaled, .. } => unscaled as f64,
        _ => return None,
    })
}

/// A double as a value of `ty`, rounded with ties to even and held to the range of the type, which
/// is the pin's cast with its clamp for an answer out of range.
#[expect(
    clippy::cast_possible_truncation,
    reason = "every whole number is clamped to its type before it narrows"
)]
fn decode(x: f64, ty: &LogicalType) -> Result<Value> {
    let whole = |min: i128, max: i128| (x.round_ties_even() as i128).clamp(min, max);
    let small = |min: i64, max: i64| whole(i128::from(min), i128::from(max)) as i64;
    Ok(match *ty {
        LogicalType::TinyInt => Value::TinyInt(small(i8::MIN.into(), i8::MAX.into()) as i8),
        LogicalType::SmallInt => Value::SmallInt(small(i16::MIN.into(), i16::MAX.into()) as i16),
        LogicalType::Integer => Value::Integer(small(i32::MIN.into(), i32::MAX.into()) as i32),
        LogicalType::Date => Value::Date(small(i32::MIN.into(), i32::MAX.into()) as i32),
        LogicalType::BigInt => Value::BigInt(small(i64::MIN, i64::MAX)),
        LogicalType::Time => Value::Time(small(i64::MIN, i64::MAX)),
        LogicalType::Timestamp => Value::Timestamp(small(i64::MIN, i64::MAX)),
        LogicalType::TimestampTz => Value::TimestampTz(small(i64::MIN, i64::MAX)),
        LogicalType::HugeInt => Value::HugeInt(whole(i128::MIN, i128::MAX)),
        LogicalType::Float => {
            Value::Float(x.clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32)
        }
        LogicalType::Double => Value::Double(x),
        LogicalType::Decimal { width, scale } => {
            let unscaled = match width {
                ..=4 => whole(i16::MIN.into(), i16::MAX.into()),
                5..=9 => whole(i32::MIN.into(), i32::MAX.into()),
                10..=18 => whole(i64::MIN.into(), i64::MAX.into()),
                _ => whole(i128::MIN, i128::MAX),
            };
            Value::Decimal { unscaled, width, scale }
        }
        _ => return Err(Error::internal(format!("an approx_quantile answer of type {ty}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn over(values: impl IntoIterator<Item = f64>) -> Digest {
        let mut digest = Digest::default();
        for x in values {
            digest.push(&Value::Double(x));
        }
        digest
    }

    fn at(digest: &Digest, q: f32) -> Value {
        digest.finish(Some(&Value::Float(q)), &LogicalType::Double).unwrap()
    }

    #[test]
    fn a_digest_answers_what_the_pin_answers() {
        // Each of these is the pin's answer over the same rows.
        let digest = over((0..1000).map(f64::from));
        assert_eq!(at(&digest, 0.5), Value::Double(499.5));
        let thirds = over((0..1000).map(|x| f64::from(x) / 3.0));
        assert_eq!(at(&thirds, 0.1), Value::Double(33.16666716337204));
        assert_eq!(at(&thirds, 0.33), Value::Double(109.83333770434064));
        assert_eq!(at(&thirds, 0.9), Value::Double(299.8333253860475));
        let many = over((0..100_000).map(f64::from));
        assert_eq!(at(&many, 0.123), Value::Double(12299.500339746475));
        assert_eq!(at(&over([f64::INFINITY, 1.0, 2.0]), 0.5), Value::Double(1.5));
        assert_eq!(at(&Digest::default(), 0.5), Value::Null);
    }

    #[test]
    fn the_top_of_values_that_are_all_negative_is_the_largest_of_them() {
        assert_eq!(at(&over([-5.0, -3.0]), 1.0), Value::Double(-3.0));
    }

    #[test]
    fn an_answer_rounds_to_even_and_stays_in_its_type() {
        assert_eq!(decode(4.5, &LogicalType::Date).unwrap(), Value::Date(4));
        assert_eq!(decode(499.5, &LogicalType::BigInt).unwrap(), Value::BigInt(500));
        assert_eq!(decode(300.0, &LogicalType::TinyInt).unwrap(), Value::TinyInt(127));
        assert_eq!(decode(9.3e18, &LogicalType::BigInt).unwrap(), Value::BigInt(i64::MAX));
        let decimal = LogicalType::Decimal { width: 4, scale: 1 };
        assert_eq!(
            decode(49.5, &decimal).unwrap(),
            Value::Decimal { unscaled: 50, width: 4, scale: 1 }
        );
    }

    #[test]
    fn merged_digests_hold_every_value() {
        let mut left = over((0..5000).map(f64::from));
        let right = over((5000..10_000).map(f64::from));
        left.combine(&right);
        left.combine(&Digest::default());
        assert_eq!(left.count, 10_000);
        let Value::Double(middle) = at(&left, 0.5) else { panic!("a DOUBLE answer") };
        assert!((4900.0..5100.0).contains(&middle), "{middle}");
        let ties = merge_packed(
            &[Centroid { mean: 1.0, weight: 1.0 }, Centroid { mean: 2.0, weight: 1.0 }],
            &[Centroid { mean: 1.0, weight: 2.0 }],
        );
        let weights: Vec<f64> = ties.iter().map(|c| c.weight).collect();
        assert_eq!(weights, [1.0, 2.0, 1.0]);
    }
}

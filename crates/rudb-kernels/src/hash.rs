//! The pin's hash of a value, which `hash()` answers and `approx_count_distinct` counts with.
//!
//! A whole number goes through the 64 bit finalizer the pin calls `MurmurHash64`, after widening
//! the way the pin's `static_cast` does, so a `TINYINT` of -1 and an `INTEGER` of -1 hash the same
//! and a `BIGINT` of -1 does not. Strings, blobs and bit strings hash their bytes eight at a time. A
//! struct hashes its first field and folds each of the others in, and a list hashes its first
//! element and folds each of the others in, so the nesting does not show: `[[1, 2], [3]]` hashes
//! the same as `[1, 2, 3]`, and `hash(1, [2, 3])` the same again, which is the pin's answer too.

use rudb_common::Value;

/// What the pin hashes a null to.
pub(crate) const NULL_HASH: u64 = 0xbf58_476d_1ce4_e5b9;

const MULTIPLIER: u64 = 0xd6e8_feb8_6659_fd93;

/// The pin's `MurmurHash64`, which is the finalizer and not the whole of MurmurHash.
const fn murmur(mut x: u64) -> u64 {
    x ^= x >> 32;
    x = x.wrapping_mul(MULTIPLIER);
    x ^= x >> 32;
    x = x.wrapping_mul(MULTIPLIER);
    x ^ (x >> 32)
}

/// A value narrower than 64 bits, widened to 32 bits with its sign and then to 64 without.
#[expect(clippy::cast_sign_loss, reason = "the pin casts the signed value to uint32_t")]
const fn narrow(value: i32) -> u64 {
    murmur(value as u32 as u64)
}

#[expect(clippy::cast_sign_loss, reason = "the pin hashes the bits of the signed value")]
const fn wide(value: i64) -> u64 {
    murmur(value as u64)
}

#[expect(clippy::cast_possible_truncation, reason = "the two halves are taken apart on purpose")]
#[expect(clippy::cast_sign_loss, reason = "the pin hashes the bits of the signed value")]
const fn huge(value: i128) -> u64 {
    murmur(value as u64) ^ murmur((value >> 64) as u64)
}

/// The pin's hash of some bytes.
fn bytes(data: &[u8]) -> u64 {
    let mut h = 0xe17a_1465_u64 ^ (data.len() as u64).wrapping_mul(0xc6a4_a793_5bd1_e995);
    let mut blocks = data.chunks_exact(8);
    for block in &mut blocks {
        let word = u64::from_le_bytes(block.try_into().unwrap_or_default());
        h = (h ^ word).wrapping_mul(MULTIPLIER);
    }
    let rest = blocks.remainder();
    if !rest.is_empty() {
        let mut word = [0; 8];
        word[..rest.len()].copy_from_slice(rest);
        h = (h ^ u64::from_le_bytes(word)).wrapping_mul(MULTIPLIER);
    }
    murmur(h)
}

/// How the pin puts a hash into the one before it.
const fn combine(left: u64, right: u64) -> u64 {
    let left = (left ^ (left >> 32)).wrapping_mul(MULTIPLIER);
    left ^ right
}

/// The hash of one value, which is the hash of the first argument of `hash()`.
pub(crate) fn hash(value: &Value) -> u64 {
    match value {
        Value::Null => NULL_HASH,
        Value::Boolean(v) => narrow(i32::from(*v)),
        Value::TinyInt(v) => narrow(i32::from(*v)),
        Value::SmallInt(v) => narrow(i32::from(*v)),
        Value::Integer(v) | Value::Date(v) => narrow(*v),
        Value::UTinyInt(v) => murmur(u64::from(*v)),
        Value::USmallInt(v) => murmur(u64::from(*v)),
        Value::UInteger(v) => murmur(u64::from(*v)),
        Value::BigInt(v)
        | Value::Time(v)
        | Value::TimeTz(v)
        | Value::Timestamp(v)
        | Value::TimestampTz(v) => wide(*v),
        Value::UBigInt(v) => murmur(*v),
        Value::HugeInt(v) => huge(*v),
        #[expect(clippy::cast_possible_wrap, reason = "only the bits are hashed")]
        Value::UHugeInt(v) => huge(*v as i128),
        Value::Float(v) => murmur(u64::from(float_bits(*v))),
        Value::Double(v) => murmur(double_bits(*v)),
        Value::Decimal { unscaled, width, .. } => match width {
            #[expect(clippy::cast_possible_truncation, reason = "the width says it fits")]
            0..=9 => narrow(*unscaled as i32),
            #[expect(clippy::cast_possible_truncation, reason = "the width says it fits")]
            10..=18 => wide(*unscaled as i64),
            _ => huge(*unscaled),
        },
        Value::Varchar(text) => bytes(text.as_bytes()),
        Value::Blob(data) | Value::Bit(data) => bytes(data),
        Value::Interval { months, days, micros } => {
            let (months, days, micros) = normalized(*months, *days, *micros);
            wide(days) ^ wide(months) ^ wide(micros)
        }
        Value::Struct(fields) => {
            let mut values = fields.iter().map(|(_, value)| value);
            let first = values.next().map_or(0x9e37_79b9_7f4a_7c15, hash);
            values.fold(first, fold)
        }
        Value::List { values, .. } => {
            let Some((first, rest)) = values.split_first() else { return NULL_HASH };
            rest.iter().fold(hash(first), |h, value| combine(h, hash(value)))
        }
        Value::Map { entries, .. } => {
            let Some(((key, value), rest)) = entries.split_first() else { return NULL_HASH };
            let entry = |key: &Value, value: &Value| fold(hash(key), value);
            rest.iter().fold(entry(key, value), |h, (key, value)| combine(h, entry(key, value)))
        }
        // A value of a kind added after these hashes its text, which keeps equal values equal.
        other => bytes(other.to_string().as_bytes()),
    }
}

/// Folds another value into a hash, which is how `hash()` takes its second argument on and how a
/// struct takes its second field on.
///
/// A struct folds each field in and a list each element, rather than folding in a hash of the
/// whole, and an empty list leaves the hash as it was.
pub(crate) fn fold(h: u64, value: &Value) -> u64 {
    match value {
        Value::Struct(fields) => fields.iter().fold(h, |h, (_, value)| fold(h, value)),
        Value::List { values, .. } => values.iter().fold(h, |h, value| combine(h, hash(value))),
        Value::Map { entries, .. } => {
            entries.iter().fold(h, |h, (key, value)| combine(h, fold(hash(key), value)))
        }
        other => combine(h, hash(other)),
    }
}

/// The hash `hash(a, b, ...)` answers.
pub(crate) fn hash_all(values: &[Value]) -> u64 {
    let Some((first, rest)) = values.split_first() else { return NULL_HASH };
    rest.iter().fold(hash(first), fold)
}

/// The bits of a float, with negative zero made zero and every NaN one NaN, since values that are
/// equal have to hash equal.
fn float_bits(v: f32) -> u32 {
    if v == 0.0 {
        0
    } else if v.is_nan() {
        f32::NAN.to_bits()
    } else {
        v.to_bits()
    }
}

fn double_bits(v: f64) -> u64 {
    if v == 0.0 {
        0
    } else if v.is_nan() {
        f64::NAN.to_bits()
    } else {
        v.to_bits()
    }
}

/// An interval with its microseconds carried into days and its days into 30 day months, with
/// remainders that are never negative, which is the pin's `Normalize`.
fn normalized(months: i32, days: i32, micros: i64) -> (i64, i64, i64) {
    const MICROS_PER_DAY: i64 = 86_400_000_000;
    let days = i64::from(days) + micros.div_euclid(MICROS_PER_DAY);
    let micros = micros.rem_euclid(MICROS_PER_DAY);
    let months = i64::from(months) + days.div_euclid(30);
    (months, days.rem_euclid(30), micros)
}

/// The sketch `approx_count_distinct` keeps: the pin's HyperLogLog with 1024 registers, which
/// estimates the same count from the same values in any order.
#[derive(Debug, Clone)]
pub(crate) struct Sketch {
    registers: Box<[u8; REGISTERS]>,
}

const PRECISION: u32 = 10;
const REGISTERS: usize = 1 << PRECISION;
const Q: usize = 64 - PRECISION as usize;

impl Default for Sketch {
    fn default() -> Self {
        Self { registers: Box::new([0; REGISTERS]) }
    }
}

impl Sketch {
    /// Counts a value that is not null.
    pub(crate) fn insert(&mut self, value: &Value) {
        self.insert_hash(hash(value));
    }

    pub(crate) fn insert_hash(&mut self, h: u64) {
        #[expect(clippy::cast_possible_truncation, reason = "masked to the register count")]
        let at = (h & (REGISTERS as u64 - 1)) as usize;
        let rest = (h >> PRECISION) | (1 << Q);
        #[expect(clippy::cast_possible_truncation, reason = "at most 55")]
        let run = rest.trailing_zeros() as u8 + 1;
        let register = &mut self.registers[at];
        *register = (*register).max(run);
    }

    pub(crate) fn combine(&mut self, other: &Self) {
        for (mine, theirs) in self.registers.iter_mut().zip(other.registers.iter()) {
            *mine = (*mine).max(*theirs);
        }
    }

    /// The estimate, by Ertl's improved estimator as the pin writes it.
    #[expect(clippy::cast_possible_truncation, reason = "the estimate is rounded to a count")]
    pub(crate) fn count(&self) -> i64 {
        let mut counts = [0_u32; Q + 2];
        for &register in self.registers.iter() {
            counts[usize::from(register)] += 1;
        }
        let m = REGISTERS as f64;
        let mut z = m * tau((m - f64::from(counts[Q])) / m);
        for k in (1..=Q).rev() {
            z += f64::from(counts[k]);
            z *= 0.5;
        }
        z += m * sigma(f64::from(counts[0]) / m);
        (0.721_347_520_444_481_7 * m * m / z).round() as i64
    }
}

#[expect(clippy::float_cmp, reason = "the series stops when a step no longer changes it")]
fn sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let (mut y, mut z) = (1.0, x);
    loop {
        x *= x;
        let before = z;
        z += x * y;
        y += y;
        if before == z {
            return z;
        }
    }
}

#[expect(clippy::float_cmp, reason = "the series stops when a step no longer changes it")]
fn tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let (mut y, mut z) = (1.0, 1.0 - x);
    loop {
        x = x.sqrt();
        let before = z;
        y *= 0.5;
        z -= (1.0 - x).powi(2) * y;
        if before == z {
            return z / 3.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_hash_to_what_the_pin_answers() {
        assert_eq!(hash(&Value::Integer(1)), 4_717_996_019_076_358_352);
        assert_eq!(hash(&Value::TinyInt(-1)), 4_739_667_815_145_166_545);
        assert_eq!(hash(&Value::Integer(-1)), 4_739_667_815_145_166_545);
        assert_eq!(hash(&Value::Boolean(true)), 4_717_996_019_076_358_352);
        assert_eq!(hash(&Value::Null), 13_787_848_793_156_543_929);
        assert_eq!(hash(&Value::Varchar(String::new())), 5_104_928_228_550_385_088);
        assert_eq!(hash(&Value::Varchar("a".into())), 12_561_829_011_207_016_135);
        assert_eq!(hash(&Value::Varchar("abcdefgh".into())), 10_586_210_142_991_718_607);
        assert_eq!(hash(&Value::Varchar("abcdefghijkl".into())), 5_942_002_808_662_391_075);
        assert_eq!(
            hash(&Value::Varchar("abcdefghijklmnopqrstu".into())),
            13_398_964_232_280_677_830
        );
        assert_eq!(hash(&Value::Double(1.5)), 1_706_605_666_616_485_939);
        assert_eq!(hash(&Value::Double(-0.0)), 0);
        assert_eq!(hash(&Value::Float(1.5)), 877_323_241_837_685_928);
        assert_eq!(hash(&Value::Double(f64::NAN)), 9_170_934_016_072_976_158);
        assert_eq!(hash(&Value::HugeInt(-1)), 0);
        assert_eq!(hash(&Value::Date(18_262)), 3_044_828_828_311_488_314);
        let interval = Value::Interval { months: 0, days: 40, micros: 0 };
        assert_eq!(hash(&interval), 994_387_204_874_416_290);
        for width in [4, 18, 30] {
            let decimal = Value::Decimal { unscaled: 125, width, scale: 2 };
            assert_eq!(hash(&decimal), 7_973_991_464_578_470_583);
        }
    }

    #[test]
    fn nesting_folds_into_one_hash_the_way_the_pin_does() {
        let list = |values: Vec<Value>| Value::List { element: values[0].logical_type(), values };
        let ints = |ns: &[i32]| list(ns.iter().copied().map(Value::Integer).collect());
        assert_eq!(hash(&ints(&[1, 2, 3])), 12_722_334_483_198_565_868);
        assert_eq!(hash(&list(vec![ints(&[1, 2]), ints(&[3])])), 12_722_334_483_198_565_868);
        assert_eq!(hash_all(&[Value::Integer(1), ints(&[2, 3])]), 12_722_334_483_198_565_868);
        let pair =
            Value::Struct(vec![("a".into(), Value::Integer(1)), ("b".into(), Value::Integer(2))]);
        assert_eq!(hash(&pair), 6_530_802_887_144_669_425);
        assert_eq!(hash_all(&[Value::Integer(1), Value::Null]), 17_970_267_147_294_058_266);
    }

    #[test]
    fn the_sketch_estimates_what_the_pin_does() {
        let mut sketch = Sketch::default();
        assert_eq!(sketch.count(), 0);
        for n in 0..1000 {
            sketch.insert(&Value::BigInt(n));
        }
        assert_eq!(sketch.count(), 978);
    }
}

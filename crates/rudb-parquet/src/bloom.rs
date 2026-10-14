//! The split block bloom filter of a Parquet column chunk, sized and filled the way the pin's
//! writer does, so a dictionary column gets the same filter bytes as it would from DuckDB.
//!
//! A filter is a power of two of 32 byte blocks. A value is hashed with XXH64 over its plain
//! bytes, without the length in front of a string, and the high half of the hash picks the block.
//! The low half sets one bit in each of the eight words of the block, through the eight salts of
//! the Parquet format.

use rudb_common::{Value, time_tz, uuid};

use crate::metadata::Unit;

/// The salts of the Parquet split block bloom filter.
const SALTS: [u32; 8] = [
    0x47B6_137B,
    0x4497_4D91,
    0x8824_AD5B,
    0xA2B7_289D,
    0x7054_95C7,
    0x2DF1_424B,
    0x9EFC_4947,
    0x5C6B_FB31,
];

/// The most blocks a filter takes, which is a filter of 512 MiB.
const MOST_BLOCKS: u64 = 1 << 24;

/// One filter, as the words of its blocks.
#[derive(Debug)]
pub(crate) struct Filter {
    blocks: Vec<[u32; 8]>,
}

impl Filter {
    /// A filter for `entries` values at a false positive rate of `ratio`, which takes the pin's
    /// number of blocks: the bits the rate asks for with eight bits a value, rounded up to a power
    /// of two of blocks, and never fewer than one. A rate so small it asks for more than
    /// [`MOST_BLOCKS`] gets that many, where the pin would fail to allocate it.
    pub(crate) fn new(entries: usize, ratio: f64) -> Self {
        let k = 8.0;
        let bits = -k * entries as f64 / (1.0 - ratio.powf(1.0 / k)).ln();
        let wanted = (bits / k).min((MOST_BLOCKS * 32) as f64) as u64;
        let blocks = (next_power_of_two(wanted) / 32).max(1);
        Self { blocks: vec![[0; 8]; blocks as usize] }
    }

    /// A filter read back from a file, which has to be whole blocks and at least one of them.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        let blocks = bytes
            .chunks_exact(32)
            .map(|block| {
                let mut words = [0_u32; 8];
                for (word, bytes) in words.iter_mut().zip(block.chunks_exact(4)) {
                    *word = u32::from_le_bytes(bytes.try_into().expect("4 bytes"));
                }
                words
            })
            .collect();
        Self { blocks }
    }

    /// Takes in a value by its hash.
    pub(crate) fn insert(&mut self, hash: u64) {
        let at = self.block(hash);
        for (word, salt) in self.blocks[at].iter_mut().zip(SALTS) {
            *word |= bit(hash, salt);
        }
    }

    /// Whether a value with this hash may have been taken in, which is every bit it would have set
    /// being set.
    pub(crate) fn contains(&self, hash: u64) -> bool {
        let block = &self.blocks[self.block(hash)];
        block.iter().zip(SALTS).all(|(word, salt)| word & bit(hash, salt) != 0)
    }

    /// The block a hash goes in, picked by its high half.
    fn block(&self, hash: u64) -> usize {
        (((hash >> 32) * self.blocks.len() as u64) >> 32) as usize
    }

    /// The bytes of the filter as the file holds them.
    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.blocks.iter().flatten().flat_map(|word| word.to_le_bytes()).collect()
    }
}

/// How a column holds its values, which is what hashing a value the way the file did needs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stored {
    /// The physical type, by its number in the format.
    pub(crate) physical: i64,
    /// The declared length of a fixed length column.
    pub(crate) width: i64,
    /// The unit of a time or a timestamp column, from its logical type or else its converted one.
    pub(crate) unit: Option<Unit>,
}

/// The physical types a hash depends on.
const INT32: i64 = 1;
const INT64: i64 = 2;
const FLOAT: i64 = 4;
const DOUBLE: i64 = 5;
const FIXED: i64 = 7;

/// The hashes a value cast to its column's type has in that column's filter, the pin's rules for
/// `parquet_bloom_probe` and for pruning a row group.
///
/// `None` is a value no filter can rule out, either because the pin does not hash its type or its
/// stored form, or because it is a NaN, which equals every NaN whatever its bits. A float zero has
/// two hashes, since `0.0` and `-0.0` are equal and their bits are not, and is ruled out only when
/// neither is in the filter.
pub(crate) fn probe_hashes(value: &Value, stored: Stored) -> Option<Vec<u64>> {
    let int = |value: i32| vec![xxh64(&value.to_le_bytes())];
    let long = |value: i64| vec![xxh64(&value.to_le_bytes())];
    // Infinity and minus infinity are the ends of the i64 and are stored as they are, unscaled.
    let finite = |value: i64| value != i64::MAX && value != -i64::MAX;
    let (int32, int64) = (stored.physical == INT32, stored.physical == INT64);
    let unit = stored.unit;
    Some(match *value {
        Value::TinyInt(value) => int(value.into()),
        Value::SmallInt(value) => int(value.into()),
        Value::Integer(value) | Value::Date(value) => int(value),
        Value::UTinyInt(value) => int(value.into()),
        Value::USmallInt(value) => int(value.into()),
        Value::UInteger(value) => vec![xxh64(&value.to_le_bytes())],
        Value::BigInt(value) => long(value),
        Value::UBigInt(value) => vec![xxh64(&value.to_le_bytes())],
        // A float read out of a half float or a wide decimal is not the bytes the file hashed,
        // where the pin hashes it all the same.
        Value::Float(_) if stored.physical != FLOAT => return None,
        Value::Double(_) if stored.physical != DOUBLE => return None,
        Value::Float(value) if value.is_nan() => return None,
        Value::Float(0.0) => {
            vec![xxh64(&0.0_f32.to_le_bytes()), xxh64(&(-0.0_f32).to_le_bytes())]
        }
        Value::Float(value) => vec![xxh64(&value.to_le_bytes())],
        Value::Double(value) if value.is_nan() => return None,
        Value::Double(0.0) => {
            vec![xxh64(&0.0_f64.to_le_bytes()), xxh64(&(-0.0_f64).to_le_bytes())]
        }
        Value::Double(value) => vec![xxh64(&value.to_le_bytes())],
        Value::Varchar(ref text) => vec![xxh64(text.as_bytes())],
        Value::Blob(ref bytes) => vec![xxh64(bytes)],
        // A decimal is hashed as the integer it is stored as, and one stored as bytes is not.
        Value::Decimal { unscaled, .. } if int32 => int(i32::try_from(unscaled).ok()?),
        Value::Decimal { unscaled, .. } if int64 => long(i64::try_from(unscaled).ok()?),
        Value::Uuid(held) if stored.physical == FIXED && stored.width == 16 => {
            vec![xxh64(&uuid::to_bytes(held))]
        }
        // A time of day is hashed at the unit the file holds, and a `TIME` has no nanoseconds to
        // give a file that holds them.
        Value::Time(micros) => match unit {
            Some(Unit::Millis) if int32 => int(i32::try_from(micros / 1_000).ok()?),
            Some(Unit::Micros) if int64 => long(micros),
            _ => return None,
        },
        Value::TimeNs(nanos) => match unit {
            Some(Unit::Millis) if int32 => int(i32::try_from(nanos / 1_000_000).ok()?),
            Some(Unit::Micros) if int64 => long(nanos / 1_000),
            Some(Unit::Nanos) if int64 => long(nanos),
            _ => return None,
        },
        // A zoned time is stored at UTC.
        Value::TimeTz(key) => match unit {
            Some(Unit::Millis) if int32 => int(i32::try_from(time_tz::at_utc(key) / 1_000).ok()?),
            Some(Unit::Micros) if int64 => long(time_tz::at_utc(key)),
            _ => return None,
        },
        Value::Timestamp(micros) | Value::TimestampTz(micros) => match unit {
            Some(Unit::Millis) if int64 && finite(micros) => long(micros / 1_000),
            Some(Unit::Millis | Unit::Micros) if int64 => long(micros),
            _ => return None,
        },
        Value::TimestampMs(millis) if int64 && unit == Some(Unit::Millis) => long(millis),
        Value::TimestampNs(nanos) | Value::TimestampTzNs(nanos)
            if int64 && unit == Some(Unit::Nanos) =>
        {
            long(nanos)
        }
        _ => return None,
    })
}

/// The bit of one word a hash sets, through that word's salt.
fn bit(hash: u64, salt: u32) -> u32 {
    1 << ((hash as u32).wrapping_mul(salt) >> 27)
}

/// The pin's next power of two, which is two for zero.
fn next_power_of_two(value: u64) -> u64 {
    if value == 0 { 2 } else { value.next_power_of_two() }
}

const PRIME_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME_3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME_4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME_5: u64 = 0x27D4_EB2F_1656_67C5;

/// XXH64 with a seed of zero, which is the hash a Parquet bloom filter takes.
pub(crate) fn xxh64(bytes: &[u8]) -> u64 {
    let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
    let round = |acc: u64, lane: u64| {
        acc.wrapping_add(lane.wrapping_mul(PRIME_2)).rotate_left(31).wrapping_mul(PRIME_1)
    };
    let merge =
        |acc: u64, lane: u64| (acc ^ round(0, lane)).wrapping_mul(PRIME_1).wrapping_add(PRIME_4);
    let mut at = 0;
    let mut hash = if bytes.len() >= 32 {
        let mut lanes = [PRIME_1.wrapping_add(PRIME_2), PRIME_2, 0, 0_u64.wrapping_sub(PRIME_1)];
        while at + 32 <= bytes.len() {
            for (lane, acc) in lanes.iter_mut().enumerate() {
                *acc = round(*acc, word(at + lane * 8));
            }
            at += 32;
        }
        let mut hash = lanes[0]
            .rotate_left(1)
            .wrapping_add(lanes[1].rotate_left(7))
            .wrapping_add(lanes[2].rotate_left(12))
            .wrapping_add(lanes[3].rotate_left(18));
        for lane in lanes {
            hash = merge(hash, lane);
        }
        hash
    } else {
        PRIME_5
    };
    hash = hash.wrapping_add(bytes.len() as u64);
    while at + 8 <= bytes.len() {
        hash =
            (hash ^ round(0, word(at))).rotate_left(27).wrapping_mul(PRIME_1).wrapping_add(PRIME_4);
        at += 8;
    }
    if at + 4 <= bytes.len() {
        let half = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        hash = (hash ^ u64::from(half).wrapping_mul(PRIME_1))
            .rotate_left(23)
            .wrapping_mul(PRIME_2)
            .wrapping_add(PRIME_3);
        at += 4;
    }
    for &byte in &bytes[at..] {
        hash = (hash ^ u64::from(byte).wrapping_mul(PRIME_5)).rotate_left(11).wrapping_mul(PRIME_1);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(PRIME_2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(PRIME_3);
    hash ^ (hash >> 32)
}

#[cfg(test)]
mod tests {
    use super::{Filter, xxh64};

    #[test]
    fn xxh64_is_the_reference_hash() {
        assert_eq!(xxh64(b""), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"a"), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(xxh64(b"abc"), 0x44BC_2CF5_AD77_0999);
        let long = b"Nobody inspects the spammish repetition";
        assert_eq!(xxh64(long), 0xFBCE_A83C_8A37_8BF1);
        let bytes: Vec<u8> = (1..=70).collect();
        let prefixes = [4, 8, 12, 31, 32, 63, 70].map(|len| xxh64(&bytes[..len]));
        let expected = [
            0x5426_20E3_A2A9_2ED1,
            0x814C_43EB_2964_6E14,
            0xC23D_AF14_A582_E9A5,
            0x68BF_A490_F875_2F55,
            0x8961_4B78_13C0_BD7F,
            0xDDB4_F290_C77F_8618,
            0x2EA5_12AB_3E16_924E,
        ];
        assert_eq!(prefixes, expected);
    }

    #[test]
    fn a_filter_takes_the_pins_number_of_blocks() {
        let blocks = |entries| Filter::new(entries, 0.01).bytes().len() / 32;
        assert_eq!([1, 3, 27, 28, 100, 1000].map(blocks), [1, 1, 1, 2, 4, 64]);
    }
}

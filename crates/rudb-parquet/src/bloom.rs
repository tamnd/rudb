//! The split block bloom filter of a Parquet column chunk, sized and filled the way the pin's
//! writer does, so a dictionary column gets the same filter bytes as it would from DuckDB.
//!
//! A filter is a power of two of 32 byte blocks. A value is hashed with XXH64 over its plain
//! bytes, without the length in front of a string, and the high half of the hash picks the block.
//! The low half sets one bit in each of the eight words of the block, through the eight salts of
//! the Parquet format.

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

    /// Takes in a value by its hash.
    pub(crate) fn insert(&mut self, hash: u64) {
        let at = ((hash >> 32) * self.blocks.len() as u64) >> 32;
        let block = &mut self.blocks[at as usize];
        for (word, salt) in block.iter_mut().zip(SALTS) {
            *word |= 1 << ((hash as u32).wrapping_mul(salt) >> 27);
        }
    }

    /// The bytes of the filter as the file holds them.
    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.blocks.iter().flatten().flat_map(|word| word.to_le_bytes()).collect()
    }
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

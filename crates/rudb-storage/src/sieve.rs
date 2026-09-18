//! Whether a stretch of a column can hold one particular value, answered without reading the rows.
//!
//! A range rules out an equality test only when the constant falls outside it, and on a column
//! whose values are spread over their whole domain that never happens. ClickBench query 19 is the
//! shape: `WHERE UserID = 435090932899640449` over a column of nine hundred thousand distinct hash
//! like numbers, where every chunk's range covers nearly the whole of `BIGINT` and so every chunk is
//! read. The scan reads 999,975 rows and the filter hands on none of them. A range is the wrong
//! summary for that question and this is the right one.
//!
//! # One sided, the same way [`rudb_common::bounds`] is
//!
//! [`Sieve::excludes`] says no row of the stretch holds the value, or it says nothing. It never says
//! a row does hold it. So a caller skips on `true` and reads on `false`, every case this cannot
//! decide answers `false`, and a stretch whose sieve could not be built has no sieve at all rather
//! than an empty one.
//!
//! That is also why a walk that meets a row it cannot read abandons the whole sieve. A summary that
//! saw some of the rows would exclude values the other rows hold, and that is a wrong answer arrived
//! at quickly.
//!
//! # Two implementations, chosen by what the column's range already says
//!
//! A column whose values live in a small integer range gets [`Dense`], which is one bit per value in
//! the range and is exact: it has no false positives at all, it is smaller than a filter over the
//! same values, and building it is a shift and an or with no hashing. A `GROUP BY`-sized column like
//! `AdvEngineID` or `EventDate` lands here and costs tens of bytes a chunk.
//!
//! Everything else gets [`Blocked`], a Bloom filter whose bits for one value all fall in one cache
//! line, which is the register blocked design from Lang, Neumann and Kemper's "Performance-Optimal
//! Filtering". One cache miss a probe instead of `k` of them, at a false positive rate a little
//! worse than the classic layout for the same bits, which is the trade every recent filter paper
//! makes because the misses are the cost and the bits are not.
//!
//! # What it costs
//!
//! Bytes proportional to the values, not to the rows, so a filter over a stretch of a column costs
//! the same whether that stretch is one chunk or sixty four of them. That is what makes it worth
//! keeping one per chunk rather than one per stripe: the finer one skips sixty four times more
//! precisely for the same space, and the only thing the extra granularity costs is the directory
//! entries pointing at them.

use rudb_common::LogicalType;
use rudb_common::bounds::Bound;
use rudb_vector::Vector;

use crate::zone::Range;

/// Bits a blocked filter spends on each value it expects to hold.
///
/// Ten bits and four lanes is about one percent false positives, which is a chunk read in a hundred
/// that yields nothing. Going to sixteen bits buys a tenth of that and costs sixty percent more
/// space, and the space is what decides whether this is kept for every column or only for the ones
/// somebody named.
const BITS_PER_VALUE: usize = 10;

/// The fewest bits per value a blocked filter is worth building at.
///
/// Under this a filter keeps nearly everything it is asked about, so it is bytes spent to answer
/// `false`, which is the answer a caller with no sieve at all already gets for nothing.
const LEAN_BITS_PER_VALUE: usize = 4;

/// Words in one block, which is 512 bits and one cache line.
const BLOCK_WORDS: usize = 8;

/// Bits in one block.
const BLOCK_BITS: usize = BLOCK_WORDS * 64;

/// How many bits of its block each value sets.
const LANES: usize = 4;

/// The widest integer range an exact bitmap is kept for.
///
/// Four thousand and ninety six bits is 512 bytes, which is what a column of a few thousand distinct
/// small numbers costs per chunk. Past that the bitmap is mostly zeroes and a filter sized by the
/// values rather than by the range is both smaller and better.
const DENSE_BITS: usize = 4096;

/// Odd constants with well spread bit patterns, one per lane.
///
/// These are part of the on disk format: a file written by one version of this and read by another
/// has to hash its values the same way, so they are fixed rather than chosen at build time.
const SALTS: [u64; LANES] =
    [0x9e37_79b9_7f4a_7c15, 0xc2b2_ae3d_27d4_eb4f, 0x1656_67b1_9e37_79f9, 0x85eb_ca77_c2b2_ae63];

/// The starting state of the value hash, which is part of the format for the same reason.
const SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// The tag byte an encoded [`Dense`] starts with.
const DENSE_TAG: u8 = 0;

/// The tag byte an encoded [`Blocked`] starts with.
const BLOCKED_TAG: u8 = 1;

/// Which values a stretch of one column holds, to the precision the stretch was worth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sieve {
    /// Exactly the values present, one bit each over a small integer range.
    Dense(Dense),
    /// A superset of the values present, as a blocked Bloom filter over their hashes.
    Blocked(Blocked),
}

/// One bit per integer in a range, set when the stretch holds that integer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dense {
    base: i128,
    width: usize,
    words: Vec<u64>,
}

/// A Bloom filter whose bits for one value all fall inside one cache line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    words: Vec<u64>,
}

impl Sieve {
    /// The sieve of one column of one chunk, or `None` when it is not worth one or cannot have one.
    ///
    /// `range` is the same range the zone map already built for this column, so the decision between
    /// the two implementations costs no extra pass. `budget` is the most bytes this may spend, and a
    /// filter that would have to be squeezed below four bits a value to fit answers `None`
    /// rather than returning something that keeps every chunk.
    ///
    /// `None` for a column with no rows, for one whose type no constant compares against in this
    /// domain, which is the floats and the temporal types, for one holding a row this cannot read,
    /// and for one whose bitmap came out mostly set, which is the case where the range already says
    /// everything this would.
    #[must_use]
    pub fn of(vector: &Vector, range: &Range, budget: usize) -> Option<Self> {
        if !probed(vector.logical_type()) {
            return None;
        }
        let rows = vector.validity().count_valid(vector.len());
        if rows == 0 {
            return None;
        }
        let mut sieve = match dense(range) {
            Some(dense) => Self::Dense(dense),
            None => Self::Blocked(blocked(distinct_bound(vector).min(rows), budget)?),
        };
        if !fill(vector, &mut sieve) {
            return None;
        }
        if !sieve.worth_keeping() {
            return None;
        }
        Some(sieve)
    }

    /// Whether this sieve rules out enough to be worth the bytes it takes.
    ///
    /// A column of a few dozen small numbers has a range covering nearly every value it holds, and
    /// the bitmap over that range comes out nearly all ones. Every probe inside the range is then
    /// kept, which is what a caller with no sieve at all already gets, so this is where most of the
    /// columns of a wide table stop paying for one. The `hits` table is 105 columns and the ones
    /// this is really for are the handful holding identifiers.
    ///
    /// Only the bitmap is judged. A filter is built only when the range was too wide for a bitmap,
    /// and a column whose values are spread over a range that wide is the case this exists for.
    fn worth_keeping(&self) -> bool {
        match self {
            Self::Dense(dense) => dense.set() * 2 <= dense.width,
            Self::Blocked(_) => true,
        }
    }

    /// Whether no row of the stretch holds `value`.
    ///
    /// The one sided answer of the module doc: `true` rules the stretch out, `false` says nothing.
    #[must_use]
    pub fn excludes(&self, value: &Bound) -> bool {
        match value {
            Bound::Int(number) => match self {
                Self::Dense(dense) => !dense.holds(*number),
                Self::Blocked(blocked) => !blocked.holds(hash_int(*number)),
            },
            Bound::Bytes(bytes) => match self {
                // A bitmap over an integer range was built from a column of integers, and a
                // constant of bytes never equals one of those. Saying nothing is still correct and
                // the caller is asking about a column it has confused with another.
                Self::Dense(_) => false,
                Self::Blocked(blocked) => !blocked.holds(hash_bytes(bytes)),
            },
            // Two floats that are equal can have different bits and two that have the same bits can
            // be unequal, so a hash of a float decides nothing about equality between floats. No
            // sieve is built over one either, and this is the other end of that decision.
            Bound::Real(_) => false,
        }
    }

    /// How many bytes [`Self::to_bytes`] will produce.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Dense(dense) => 1 + 16 + 4 + dense.words.len() * 8,
            Self::Blocked(blocked) => 1 + 4 + blocked.words.len() * 8,
        }
    }

    /// Whether this sieve holds no bits at all, which it never does.
    ///
    /// Here because a type with a `len` and no `is_empty` is a type clippy asks about, and the
    /// honest answer is that a sieve with nothing in it is never built.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// This sieve as the bytes a file keeps.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len());
        match self {
            Self::Dense(dense) => {
                out.push(DENSE_TAG);
                out.extend_from_slice(&dense.base.to_le_bytes());
                out.extend_from_slice(&(dense.width as u32).to_le_bytes());
                words(&mut out, &dense.words);
            }
            Self::Blocked(blocked) => {
                out.push(BLOCKED_TAG);
                let blocks = (blocked.words.len() / BLOCK_WORDS) as u32;
                out.extend_from_slice(&blocks.to_le_bytes());
                words(&mut out, &blocked.words);
            }
        }
        out
    }

    /// The sieve `bytes` encodes, or `None` when they are not one.
    ///
    /// `None` rather than an error because a caller that cannot read a sieve reads the rows, which
    /// is slow and right. The bytes this rejects are a file written by a later version or a page
    /// that a checksum has already been asked about.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let (&tag, rest) = bytes.split_first()?;
        match tag {
            DENSE_TAG => {
                let base = i128::from_le_bytes(rest.get(..16)?.try_into().ok()?);
                let width = u32::from_le_bytes(rest.get(16..20)?.try_into().ok()?) as usize;
                let held = read_words(rest.get(20..)?)?;
                if held.len() != width.div_ceil(64) {
                    return None;
                }
                Some(Self::Dense(Dense { base, width, words: held }))
            }
            BLOCKED_TAG => {
                let blocks = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
                let held = read_words(rest.get(4..)?)?;
                if held.len() != blocks.checked_mul(BLOCK_WORDS)? || blocks == 0 {
                    return None;
                }
                Some(Self::Blocked(Blocked { words: held }))
            }
            _ => None,
        }
    }

    /// Records one integer the stretch holds, answering whether this sieve could take it.
    fn add_int(&mut self, value: i128) -> bool {
        match self {
            Self::Dense(dense) => dense.add(value),
            Self::Blocked(blocked) => {
                blocked.add(hash_int(value));
                true
            }
        }
    }

    /// Records one string or blob the stretch holds, answering whether this sieve could take it.
    fn add_bytes(&mut self, value: &[u8]) -> bool {
        match self {
            // A bitmap over an integer range cannot hold a string, and a column that has produced
            // both is one this has misread. Abandoning the sieve is the safe end of that.
            Self::Dense(_) => false,
            Self::Blocked(blocked) => {
                blocked.add(hash_bytes(value));
                true
            }
        }
    }
}

impl Dense {
    /// Whether the bit for `value` is set, which for this implementation is whether a row holds it.
    fn holds(&self, value: i128) -> bool {
        match self.offset(value) {
            Some(bit) => self.words[bit / 64] & (1 << (bit % 64)) != 0,
            None => false,
        }
    }

    /// Sets the bit for `value`, or refuses a value the range does not cover.
    fn add(&mut self, value: i128) -> bool {
        match self.offset(value) {
            Some(bit) => {
                self.words[bit / 64] |= 1 << (bit % 64);
                true
            }
            None => false,
        }
    }

    /// How many values the stretch holds, which is how many bits are set.
    fn set(&self) -> usize {
        self.words.iter().map(|word| word.count_ones() as usize).sum()
    }

    /// Which bit stands for `value`, or `None` when it is outside the range this covers.
    fn offset(&self, value: i128) -> Option<usize> {
        let bit = usize::try_from(value.checked_sub(self.base)?).ok()?;
        if bit >= self.width { None } else { Some(bit) }
    }
}

impl Blocked {
    /// An empty filter sized for `values` entries, inside `budget` bytes.
    ///
    /// `None` when `budget` leaves under four bits an entry, which is a filter that keeps nearly
    /// everything it is asked about and so is bytes spent to answer nothing.
    ///
    /// This is here for a caller outside the file format: a hash join builds one of these over its
    /// build side's keys while the query runs and hands it to the scan under its other side. Such a
    /// filter is never written down, so it is free to hash its values however the operator filling
    /// it already hashes them, which is why this and the two below speak in hashes rather than in
    /// values. A filter built one way and probed the other answers nonsense, so one caller has to
    /// own both ends of it, and every caller here does.
    #[must_use]
    pub fn sized(values: usize, budget: usize) -> Option<Self> {
        blocked(values, budget)
    }

    /// Whether every bit `hash` names is set, which is what a Bloom filter can say.
    ///
    /// One sided in the direction the rest of this file is: `false` says no entry hashed to this,
    /// and `true` says one may have.
    #[must_use]
    pub fn holds(&self, hash: u64) -> bool {
        let block = self.block(hash);
        lanes(hash).iter().all(|&bit| self.words[block + bit / 64] & (1 << (bit % 64)) != 0)
    }

    /// Sets every bit `hash` names.
    pub fn add(&mut self, hash: u64) {
        let block = self.block(hash);
        for bit in lanes(hash) {
            self.words[block + bit / 64] |= 1 << (bit % 64);
        }
    }

    /// How many bytes of bits this holds.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.words.len() * 8
    }

    /// Where the block for `hash` starts, as an index into the words.
    ///
    /// A multiply and a shift rather than a remainder, so the block count is free to be any number
    /// rather than a power of two, and the high bits of the hash choose it while the low ones choose
    /// the bits inside it.
    fn block(&self, hash: u64) -> usize {
        let blocks = (self.words.len() / BLOCK_WORDS) as u64;
        (((hash >> 32) * blocks) >> 32) as usize * BLOCK_WORDS
    }
}

/// Which bits of its block one hash names.
///
/// Each lane multiplies by its own odd constant and takes the top nine bits of the result, which is
/// a number under 512 and so a bit of the block. The multiply is what makes the four lanes
/// independent of each other rather than four slices of the same hash.
#[inline]
fn lanes(hash: u64) -> [usize; LANES] {
    let mut bits = [0; LANES];
    for (bit, salt) in bits.iter_mut().zip(SALTS) {
        *bit = (hash.wrapping_mul(salt) >> 55) as usize;
    }
    bits
}

/// Whether a constant of this type ever reaches a sieve, which is whether [`Bound::of_value`] puts
/// it in the integer or the byte domain.
///
/// The floats are out because equality between them is not equality between their bits. The temporal
/// types past `DATE` and the decimals are out because a bound carries no unit or scale, so the
/// number a file stored and the number a query typed are not comparable without one, and that is the
/// gap the module doc of [`rudb_common::bounds`] names.
fn probed(ty: &LogicalType) -> bool {
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
            | LogicalType::Date
            | LogicalType::Varchar
            | LogicalType::Blob
    )
}

/// The exact bitmap a small integer range gets, or `None` when the range is too wide for one.
fn dense(range: &Range) -> Option<Dense> {
    let (Some(Bound::Int(low)), Some(Bound::Int(high))) = (&range.low, &range.high) else {
        return None;
    };
    let width = usize::try_from(high.checked_sub(*low)?.checked_add(1)?).ok()?;
    if width > DENSE_BITS {
        return None;
    }
    Some(Dense { base: *low, width, words: vec![0; width.div_ceil(64)] })
}

/// The blocked filter `values` distinct entries get inside `budget` bytes, or `None` when the budget
/// leaves it too little to be worth keeping.
fn blocked(values: usize, budget: usize) -> Option<Blocked> {
    let wanted = values.checked_mul(BITS_PER_VALUE)?.div_ceil(BLOCK_BITS).max(1);
    let blocks = wanted.min(budget / (BLOCK_WORDS * 8));
    // No block at all is a budget under one cache line, which is nowhere to put a bit rather than
    // a filter that keeps everything, so it is refused even where there are no values to hold.
    if blocks == 0 || blocks.checked_mul(BLOCK_BITS)? < values.checked_mul(LEAN_BITS_PER_VALUE)? {
        return None;
    }
    Some(Blocked { words: vec![0; blocks.checked_mul(BLOCK_WORDS)?] })
}

/// How many distinct values a column could hold, from the form it arrived in.
///
/// A dictionary and a run encoding both name their values once, so a column of two thousand rows
/// over a dictionary of nine is nine values and not two thousand, and sizing a filter for the rows
/// would spend two hundred times the bytes it needs. Anything else answers with the rows, which is
/// the bound every column has.
fn distinct_bound(vector: &Vector) -> usize {
    if let Some((_, values)) = vector.dictionary_parts() {
        return values.len();
    }
    match vector.run_parts() {
        Some((_, values)) => values.len(),
        None => vector.len(),
    }
}

/// Puts every value of `vector` into `sieve`, answering whether it could read all of them.
///
/// Three readers in falling order of what they cost. [`Vector::signed_at`] covers the signed widths
/// in every form and never builds anything. [`Vector::bytes_at`] covers the strings and the blobs
/// the same way. What is left is the unsigned widths and the booleans, which have no reader of their
/// own, and those go through a value.
fn fill(vector: &Vector, sieve: &mut Sieve) -> bool {
    let nullable = vector.validity().has_nulls(vector.len());
    // row at a time: the third reader has no vectorised form, and a row it cannot read is a sieve
    // that has to be abandoned rather than a row that can be left out.
    for row in 0..vector.len() {
        if nullable && vector.is_null_at(row) {
            continue;
        }
        if let Some(number) = vector.signed_at(row) {
            if !sieve.add_int(number) {
                return false;
            }
            continue;
        }
        if let Some(bytes) = vector.bytes_at(row) {
            if !sieve.add_bytes(bytes) {
                return false;
            }
            continue;
        }
        match Bound::of_value(&vector.value_at(row)) {
            Some(Bound::Int(number)) => {
                if !sieve.add_int(number) {
                    return false;
                }
            }
            Some(Bound::Bytes(bytes)) => {
                if !sieve.add_bytes(&bytes) {
                    return false;
                }
            }
            Some(Bound::Real(_)) | None => return false,
        }
    }
    true
}

/// The hash of one integer, fixed because a file keeps what it produced.
fn hash_int(value: i128) -> u64 {
    let low = value as u64;
    let high = (value >> 64) as u64;
    finish(mix(mix(SEED, low), high))
}

/// The hash of one string or blob, fixed for the same reason.
fn hash_bytes(value: &[u8]) -> u64 {
    let mut state = SEED;
    let mut rest = value;
    while let Some(head) = rest.get(..8) {
        state = mix(state, u64::from_le_bytes(head.try_into().unwrap_or([0; 8])));
        rest = &rest[8..];
    }
    let mut tail = [0_u8; 8];
    tail[..rest.len()].copy_from_slice(rest);
    finish(mix(mix(state, u64::from_le_bytes(tail)), value.len() as u64))
}

/// Folds one word into a running hash.
#[inline]
fn mix(state: u64, word: u64) -> u64 {
    (state.rotate_left(27) ^ word).wrapping_mul(SALTS[0])
}

/// Moves the entropy the multiplies left in the high bits back across the whole word.
///
/// The block is chosen from the top thirty two bits and the lanes from multiplies of the whole, so a
/// hash whose entropy sits in one half sends every value of a column into the same block.
#[inline]
fn finish(state: u64) -> u64 {
    let mut spread = state;
    spread ^= spread >> 33;
    spread = spread.wrapping_mul(SALTS[1]);
    spread ^= spread >> 29;
    spread
}

/// Appends words to a byte buffer, little endian.
fn words(out: &mut Vec<u8>, held: &[u64]) {
    for word in held {
        out.extend_from_slice(&word.to_le_bytes());
    }
}

/// Reads a whole slice of bytes back as words, or `None` when it is not a whole number of them.
fn read_words(bytes: &[u8]) -> Option<Vec<u64>> {
    if bytes.len() % 8 != 0 {
        return None;
    }
    let mut held = Vec::with_capacity(bytes.len() / 8);
    for word in bytes.chunks_exact(8) {
        held.push(u64::from_le_bytes(word.try_into().ok()?));
    }
    Some(held)
}

#[cfg(test)]
mod tests {
    use rudb_common::bounds::Bound;
    use rudb_common::{LogicalType, Value};
    use rudb_vector::Vector;

    use rudb_vector::Chunk;

    use super::{BLOCK_WORDS, Sieve};
    use crate::zone::Zone;

    /// The sieve of a one column chunk holding `values`, with a generous budget.
    fn sieve_of(ty: LogicalType, values: &[Value]) -> Option<Sieve> {
        let vector = Vector::from_values(ty, values).expect("a column");
        let chunk = Chunk::new(vec![vector.clone()]).expect("a chunk");
        let zone = Zone::of(&chunk);
        Sieve::of(&vector, zone.column(0).expect("one column"), 1 << 20)
    }

    /// The bound a whole number stands for.
    fn int(number: i128) -> Bound {
        Bound::Int(number)
    }

    #[test]
    fn a_small_integer_range_becomes_an_exact_bitmap_with_no_false_positives() {
        let held: Vec<Value> = [3_i32, 9, 12, 3].iter().map(|n| Value::Integer(*n)).collect();
        let sieve = sieve_of(LogicalType::Integer, &held).expect("a sieve");
        assert!(matches!(sieve, Sieve::Dense(_)));
        for number in 3..=12 {
            let present = [3, 9, 12].contains(&number);
            assert_eq!(sieve.excludes(&int(i128::from(number))), !present, "value {number}");
        }
    }

    #[test]
    fn a_value_outside_the_range_is_excluded_by_the_bitmap() {
        let held: Vec<Value> = [3_i32, 9].iter().map(|n| Value::Integer(*n)).collect();
        let sieve = sieve_of(LogicalType::Integer, &held).expect("a sieve");
        assert!(sieve.excludes(&int(2)));
        assert!(sieve.excludes(&int(10)));
    }

    #[test]
    fn a_wide_integer_range_becomes_a_filter_that_keeps_what_it_was_given() {
        let held: Vec<Value> =
            (0..500_i64).map(|n| Value::BigInt(n.wrapping_mul(982_451_653))).collect();
        let sieve = sieve_of(LogicalType::BigInt, &held).expect("a sieve");
        assert!(matches!(sieve, Sieve::Blocked(_)));
        for value in &held {
            let Value::BigInt(number) = value else { panic!("a big integer") };
            assert!(!sieve.excludes(&int(i128::from(*number))), "value {number} was given");
        }
    }

    #[test]
    fn a_filter_rules_out_nearly_everything_it_was_not_given() {
        let held: Vec<Value> =
            (0..1024_i64).map(|n| Value::BigInt(n.wrapping_mul(982_451_653))).collect();
        let sieve = sieve_of(LogicalType::BigInt, &held).expect("a sieve");
        let absent = (0..1024_i64).map(|n| i128::from(n.wrapping_mul(982_451_653)) + 1);
        let kept = absent.filter(|number| !sieve.excludes(&int(*number))).count();
        assert!(kept * 20 < 1024, "a filter kept {kept} of 1024 values it never saw");
    }

    #[test]
    fn a_string_column_is_filtered_by_its_bytes() {
        let held: Vec<Value> =
            ["alpha", "beta", "gamma"].iter().map(|text| Value::Varchar((*text).into())).collect();
        let sieve = sieve_of(LogicalType::Varchar, &held).expect("a sieve");
        assert!(matches!(sieve, Sieve::Blocked(_)));
        assert!(!sieve.excludes(&Bound::Bytes(b"alpha".to_vec())));
        assert!(!sieve.excludes(&Bound::Bytes(b"gamma".to_vec())));
        assert!(sieve.excludes(&Bound::Bytes(b"delta".to_vec())));
    }

    #[test]
    fn a_null_is_not_a_value_and_no_sieve_holds_one() {
        let held = vec![Value::Integer(4), Value::Null, Value::Integer(9)];
        let sieve = sieve_of(LogicalType::Integer, &held).expect("a sieve");
        assert!(!sieve.excludes(&int(4)));
        assert!(!sieve.excludes(&int(9)));
        for absent in 5..9 {
            assert!(sieve.excludes(&int(absent)), "value {absent}");
        }
    }

    #[test]
    fn a_column_whose_range_holds_nearly_every_value_it_has_gets_no_sieve() {
        let held: Vec<Value> = (0..40_i32).map(Value::Integer).collect();
        assert!(sieve_of(LogicalType::Integer, &held).is_none());
        let sparse: Vec<Value> = (0..40_i32).map(|n| Value::Integer(n * 4)).collect();
        assert!(sieve_of(LogicalType::Integer, &sparse).is_some());
    }

    #[test]
    fn a_column_of_nothing_but_nulls_has_no_sieve() {
        let held = vec![Value::Null, Value::Null];
        assert!(sieve_of(LogicalType::Integer, &held).is_none());
    }

    #[test]
    fn a_float_column_has_no_sieve_because_its_bits_are_not_its_equality() {
        let held: Vec<Value> = [1.5_f64, 2.5].iter().map(|n| Value::Double(*n)).collect();
        assert!(sieve_of(LogicalType::Double, &held).is_none());
    }

    #[test]
    fn a_budget_too_small_to_say_anything_gives_no_sieve() {
        let held: Vec<Value> =
            (0..1024_i64).map(|n| Value::BigInt(n.wrapping_mul(982_451_653))).collect();
        let vector = Vector::from_values(LogicalType::BigInt, &held).expect("a column");
        let chunk = Chunk::new(vec![vector.clone()]).expect("a chunk");
        let zone = Zone::of(&chunk);
        let range = zone.column(0).expect("one column");
        assert!(Sieve::of(&vector, range, 64).is_none());
        assert!(Sieve::of(&vector, range, 1 << 20).is_some());
    }

    #[test]
    fn a_dictionary_is_sized_by_its_values_and_not_by_its_rows() {
        let values = Vector::from_values(
            LogicalType::BigInt,
            &[Value::BigInt(1 << 40), Value::BigInt(1 << 41)],
        )
        .expect("the values");
        let codes: Vec<u32> = (0..1024).map(|row| row % 2).collect();
        let vector = Vector::dictionary(codes, values).expect("a dictionary column");
        let chunk = Chunk::new(vec![vector.clone()]).expect("a chunk");
        let zone = Zone::of(&chunk);
        let sieve =
            Sieve::of(&vector, zone.column(0).expect("one column"), 1 << 20).expect("a sieve");
        let Sieve::Blocked(blocked) = &sieve else { panic!("a wide range is a filter") };
        assert_eq!(blocked.words.len(), BLOCK_WORDS, "two values fit in one block");
        assert!(!sieve.excludes(&int(1 << 40)));
        assert!(!sieve.excludes(&int(1 << 41)));
    }

    #[test]
    fn a_sieve_survives_a_trip_through_its_bytes() {
        let held: Vec<Value> =
            (0..300_i64).map(|n| Value::BigInt(n.wrapping_mul(982_451_653))).collect();
        for values in [held, (0..40_i64).map(|n| Value::BigInt(n * 4)).collect()] {
            let sieve = sieve_of(LogicalType::BigInt, &values).expect("a sieve");
            let bytes = sieve.to_bytes();
            assert_eq!(bytes.len(), sieve.len());
            assert_eq!(Sieve::from_bytes(&bytes).expect("a sieve back"), sieve);
        }
    }

    #[test]
    fn bytes_that_are_not_a_sieve_are_refused_rather_than_guessed_at() {
        assert!(Sieve::from_bytes(&[]).is_none());
        assert!(Sieve::from_bytes(&[9, 0, 0, 0, 0]).is_none());
        assert!(Sieve::from_bytes(&[1, 1, 0, 0, 0]).is_none());
        assert!(Sieve::from_bytes(&[0, 0, 0, 0, 0]).is_none());
    }
}

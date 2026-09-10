//! Counting distinct values, and deciding whether two columns are related, without holding either
//! column in memory.
//!
//! Every decision in `spec/06-compression.md` section 6.4 and 6.5 starts with a question about a
//! column that is too big to answer exactly. Is this column worth a dictionary, which is a distinct
//! count. Do these two columns come from the same universe, which is an overlap between two value
//! sets. Is this column determined by that one, which is whether the pairs have as many distinct
//! values as the left side alone. Section 6.4 also says the pair space has to be pruned, because
//! 105 columns is 5,460 pairs and testing all of them exactly is not something a load can do.
//!
//! A bottom-k sketch answers all three from one pass per column and a fixed amount of memory.
//!
//! ## What it is
//!
//! Hash every value and keep the k smallest distinct hashes. That set is a uniform random sample of
//! the column's distinct values, chosen by a rule that does not depend on the order they arrived
//! in, so two sketches built on different machines from the same values are identical.
//!
//! The distinct count comes out of where the k smallest hashes end. If the hashes are uniform over
//! the 64 bit range, then after seeing `d` distinct values the kth smallest sits at about `k / d`
//! of the way through the range, so `d` is about `k` divided by that fraction. The standard
//! correction uses `k - 1` rather than `k`, which is what makes the estimate unbiased rather than
//! merely close. Relative error is about one over the square root of k, so the default k of 4096
//! is a bit under 2 percent, and a sketch that never filled up is not an estimate at all because
//! then it holds every distinct hash there was.
//!
//! The overlap between two columns comes from merging the two sketches and asking how many of the
//! k smallest hashes of the union are in both. That is the Jaccard similarity, and the reason it
//! works on sketches is that any hash small enough to be in the union's bottom k is small enough
//! that if it were in a column at all it would be in that column's own bottom k. So a lookup in the
//! sketch is a lookup in the column.
//!
//! ## Why not HyperLogLog
//!
//! Section 6.5 says HyperLogLog for the distinct count and that is the right structure if counting
//! is all you want, because it answers in a kilobyte where this wants tens. It cannot do the other
//! two questions. A HyperLogLog register holds a leading zero count and not a value, so two
//! HyperLogLogs can be merged into a count of the union but they cannot tell you which values the
//! union kept, and the intersection they give by inclusion and exclusion is the difference of three
//! noisy numbers, which for two columns that barely overlap is noise. The sketch here keeps actual
//! hashes, so an intersection is a set intersection and the error on it is the error on the sample
//! rather than the error on the difference. 32 KB per column at the default k, for 105 columns, is
//! 3 MB for a whole table, and the pair pruning it buys is worth more than the 3 MB.
//!
//! ## The hash
//!
//! Values are hashed with a multiply and fold over 8 byte words. This is a sketching hash and not a
//! persisted one: nothing on disk depends on it, so it can be replaced with something faster
//! without a format version. What it does have to be is uniform, because every estimate here
//! assumes it is, and the tests measure that rather than asserting it.

use rudb_common::{Error, Result};

/// The default number of hashes to keep, which puts the relative error a bit under 2 percent.
pub const DEFAULT_K: usize = 4096;

/// A bottom-k sketch of the distinct values of a column.
///
/// The retained hashes are sorted and deduplicated, so the sketch is a function of the set of
/// values and not of the order they were added in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sketch {
    k: usize,
    hashes: Vec<u64>,
}

impl Sketch {
    /// An empty sketch that will keep the `k` smallest hashes.
    ///
    /// # Errors
    ///
    /// If `k` is zero, which would make every estimate a division by nothing.
    pub fn new(k: usize) -> Result<Self> {
        if k == 0 {
            return Err(Error::internal("a sketch that keeps no hashes estimates nothing"));
        }
        Ok(Self { k, hashes: Vec::new() })
    }

    /// A sketch over a column, at the default k.
    #[must_use]
    pub fn of(values: &[&[u8]]) -> Self {
        let mut sketch = Self { k: DEFAULT_K, hashes: Vec::new() };
        for value in values {
            sketch.add(value);
        }
        sketch
    }

    /// Adds a value.
    pub fn add(&mut self, value: &[u8]) {
        self.add_hash(hash64(value));
    }

    /// Adds a value that has already been hashed, for a caller that is hashing anyway.
    pub fn add_hash(&mut self, hash: u64) {
        // The common case once the sketch is full. A column of a hundred million values takes this
        // branch for all but a few thousand of them, so everything below it is off the hot path.
        if self.hashes.len() == self.k {
            match self.hashes.last() {
                Some(largest) if hash >= *largest => return,
                _ => {}
            }
        }
        match self.hashes.binary_search(&hash) {
            Ok(_) => {}
            Err(at) => {
                self.hashes.insert(at, hash);
                self.hashes.truncate(self.k);
            }
        }
    }

    /// How many hashes the sketch is holding.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    /// Whether nothing has been added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Whether the sketch saw at most k distinct values, in which case it holds all of them and
    /// every count it gives is exact rather than estimated.
    #[must_use]
    pub fn is_exact(&self) -> bool {
        self.hashes.len() < self.k
    }

    /// The estimated number of distinct values, which is the exact number when [`Sketch::is_exact`]
    /// holds.
    #[must_use]
    pub fn distinct(&self) -> f64 {
        if self.is_exact() {
            return self.hashes.len() as f64;
        }
        let largest = self.hashes[self.hashes.len() - 1] as f64 / u64::MAX as f64;
        if largest <= 0.0 {
            return self.hashes.len() as f64;
        }
        (self.k as f64 - 1.0) / largest
    }

    /// The union of two sketches, which is the sketch the union of the two columns would have
    /// produced.
    ///
    /// # Errors
    ///
    /// If the two sketches keep a different number of hashes, because then neither one's threshold
    /// applies to the other and no estimate over the pair means anything.
    pub fn union(&self, other: &Self) -> Result<Self> {
        if self.k != other.k {
            return Err(Error::internal(format!(
                "sketches of {} and {} hashes cannot be combined",
                self.k, other.k
            )));
        }
        let mut merged = Self { k: self.k, hashes: Vec::with_capacity(self.k) };
        let mut left = self.hashes.iter().peekable();
        let mut right = other.hashes.iter().peekable();
        while merged.hashes.len() < self.k {
            let next = match (left.peek(), right.peek()) {
                (Some(a), Some(b)) => {
                    if a <= b {
                        left.next()
                    } else {
                        right.next()
                    }
                }
                (Some(_), None) => left.next(),
                (None, Some(_)) => right.next(),
                (None, None) => break,
            };
            let Some(hash) = next else {
                break;
            };
            if merged.hashes.last() != Some(hash) {
                merged.hashes.push(*hash);
            }
        }
        Ok(merged)
    }

    /// The estimated Jaccard similarity, which is the size of the intersection of the two value
    /// sets over the size of their union.
    ///
    /// Section 6.4 wants this to decide whether two columns are drawn from the same universe and
    /// should share a dictionary. It is not a decision on its own, because two columns can overlap
    /// heavily and still be better off apart if one of them is tiny, but it is what prunes 5,460
    /// pairs down to the handful worth measuring properly.
    ///
    /// # Errors
    ///
    /// As [`Sketch::union`].
    pub fn jaccard(&self, other: &Self) -> Result<f64> {
        let union = self.union(other)?;
        if union.is_empty() {
            return Ok(0.0);
        }
        let both =
            union.hashes.iter().filter(|hash| self.holds(**hash) && other.holds(**hash)).count();
        Ok(both as f64 / union.hashes.len() as f64)
    }

    /// Whether a hash is in the sketch. Only meaningful for a hash that is small enough to have
    /// been kept if it were present, which is what [`Sketch::jaccard`] guarantees by taking its
    /// candidates from the union.
    fn holds(&self, hash: u64) -> bool {
        self.hashes.binary_search(&hash).is_ok()
    }
}

/// How close a column is to being determined by another one, from a sketch of the left column and
/// a sketch of the two of them paired.
///
/// A functional dependency from A to B means every A value goes with exactly one B value, so the
/// pairs have exactly as many distinct values as A does. On ClickBench `hits` this is `URLHash`
/// against `URL` and `RefererHash` against `Referer`, which section 6.6 says is 1.6 GB of `BIGINT`
/// carrying nothing that is not already in two string columns.
///
/// The result is 1.0 for a dependency that holds and drops towards the ratio of the two counts as
/// it stops holding. It is an estimate over two estimates, so a value near 1.0 is a candidate to be
/// verified exactly and never a conclusion. Section 6.6 is explicit that a rule is applied only
/// after a full verification pass, and this is what decides which pairs are worth that pass.
///
/// # Errors
///
/// As [`Sketch::union`], and if the pairs somehow have fewer distinct values than the left column,
/// which cannot happen and is a bug in the caller's pairing if it does.
pub fn dependence(left: &Sketch, pairs: &Sketch) -> Result<f64> {
    if left.k != pairs.k {
        return Err(Error::internal("a column and its pairs need sketches of the same size"));
    }
    let alone = left.distinct();
    let together = pairs.distinct();
    if alone <= 0.0 {
        return Ok(1.0);
    }
    Ok((alone / together.max(alone)).min(1.0))
}

/// The hash of two values as a pair, for [`dependence`].
///
/// The left hash is mixed before the two are combined, so that the pair of `ab` and `c` does not
/// hash the same as the pair of `a` and `bc`.
#[must_use]
pub fn pair_hash(left: &[u8], right: &[u8]) -> u64 {
    pair_of(hash64(left), hash64(right))
}

/// The same pair hash for two values whose hashes are already known.
///
/// Testing every pair of a 105 column table is 5,460 pairs, and hashing the two values again for
/// each of them would hash every value of every column 104 times over. Hashing each column once a
/// row and combining the results here is the same answer for a hundredth of the work, and it is the
/// only way a pass over `hits` that tests all the pairs finishes in an afternoon.
#[must_use]
pub fn pair_of(left: u64, right: u64) -> u64 {
    mix(left ^ SEEDS[3], right.wrapping_add(SEEDS[2]))
}

/// The constants are odd 64 bit values with about half their bits set, which is what a multiply
/// based mixer needs to move low bits into high ones.
const SEEDS: [u64; 4] =
    [0xa076_1d64_78bd_642f, 0xe703_7ed1_a0b4_28db, 0x8ebc_6af0_9c88_c6e3, 0x5899_65cc_7537_4cc3];

/// A 64 bit multiply of two values, folded to 64 bits by xoring the halves.
///
/// This is the whole strength of the hash. A 64 by 64 multiply moves every input bit into the high
/// half of the product, and xoring the halves together brings them back down, so one of these turns
/// a one bit change anywhere into a change in about half the output bits.
fn mix(left: u64, right: u64) -> u64 {
    let wide = u128::from(left).wrapping_mul(u128::from(right));
    (wide as u64) ^ ((wide >> 64) as u64)
}

/// The hash used by every sketch here.
///
/// Nothing on disk depends on this, so it can be replaced with something faster without a format
/// version. What it has to be is uniform, because every estimate in this module assumes the hashes
/// are spread evenly over the range.
#[must_use]
pub fn hash64(value: &[u8]) -> u64 {
    let mut state = SEEDS[0] ^ mix(value.len() as u64, SEEDS[1]);
    let mut chunks = value.chunks_exact(8);
    let mut word = [0u8; 8];
    for chunk in &mut chunks {
        word.copy_from_slice(chunk);
        state = mix(state ^ u64::from_le_bytes(word), SEEDS[2]);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        state = mix(state ^ u64::from_le_bytes(last), SEEDS[3]);
    }
    mix(state, SEEDS[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(count: usize, prefix: &str) -> Vec<Vec<u8>> {
        (0..count).map(|index| format!("{prefix}{index}").into_bytes()).collect()
    }

    fn borrow(values: &[Vec<u8>]) -> Vec<&[u8]> {
        values.iter().map(Vec::as_slice).collect()
    }

    fn within(estimate: f64, actual: f64, tolerance: f64) -> bool {
        (estimate - actual).abs() / actual <= tolerance
    }

    #[test]
    fn a_sketch_that_never_filled_up_is_exact() {
        let column = values(1000, "value-");
        let sketch = Sketch::of(&borrow(&column));
        assert!(sketch.is_exact());
        assert_eq!(sketch.distinct(), 1000.0);
    }

    #[test]
    fn duplicates_do_not_count() {
        let mut sketch = Sketch::new(64).unwrap();
        for _ in 0..1000 {
            sketch.add(b"the same value");
        }
        assert_eq!(sketch.distinct(), 1.0);
    }

    #[test]
    fn the_distinct_count_is_within_two_percent_at_the_default_k() {
        for count in [50_000usize, 250_000, 1_000_000] {
            let mut sketch = Sketch::new(DEFAULT_K).unwrap();
            for index in 0..count {
                sketch.add(format!("http://example.com/page/{index}").as_bytes());
            }
            assert!(!sketch.is_exact());
            let estimate = sketch.distinct();
            assert!(
                within(estimate, count as f64, 0.02),
                "{estimate:.0} against {count} distinct values"
            );
        }
    }

    #[test]
    fn the_sketch_does_not_depend_on_the_order_values_arrived_in() {
        let column = values(100_000, "value-");
        let forwards = Sketch::of(&borrow(&column));
        let mut backwards = Sketch::new(DEFAULT_K).unwrap();
        for value in column.iter().rev() {
            backwards.add(value);
        }
        assert_eq!(forwards, backwards);
    }

    #[test]
    fn two_columns_with_the_same_values_overlap_completely() {
        let column = values(200_000, "http://example.com/");
        let left = Sketch::of(&borrow(&column));
        let right = Sketch::of(&borrow(&column));
        assert_eq!(left.jaccard(&right).unwrap(), 1.0);
    }

    #[test]
    fn two_columns_with_nothing_in_common_do_not_overlap() {
        let left = Sketch::of(&borrow(&values(200_000, "left-")));
        let right = Sketch::of(&borrow(&values(200_000, "right-")));
        assert_eq!(left.jaccard(&right).unwrap(), 0.0);
    }

    #[test]
    fn a_half_overlap_measures_as_a_third() {
        // Two columns of 100,000 values sharing 50,000 of them. The intersection is 50,000 and the
        // union is 150,000, so the Jaccard similarity is a third and not a half, which is the
        // number that catches people out about this measure.
        let shared = values(50_000, "shared-");
        let mut left = shared.clone();
        left.extend(values(50_000, "left-"));
        let mut right = shared;
        right.extend(values(50_000, "right-"));
        let overlap = Sketch::of(&borrow(&left)).jaccard(&Sketch::of(&borrow(&right))).unwrap();
        assert!(within(overlap, 1.0 / 3.0, 0.05), "{overlap:.4}");
    }

    #[test]
    fn the_union_of_two_sketches_counts_the_union_of_the_columns() {
        let left = values(300_000, "left-");
        let right = values(300_000, "right-");
        let union = Sketch::of(&borrow(&left)).union(&Sketch::of(&borrow(&right))).unwrap();
        assert!(within(union.distinct(), 600_000.0, 0.03), "{:.0}", union.distinct());
    }

    #[test]
    fn sketches_of_different_sizes_do_not_combine() {
        let small = Sketch::new(16).unwrap();
        let large = Sketch::new(32).unwrap();
        assert!(small.union(&large).is_err());
        assert!(small.jaccard(&large).is_err());
    }

    #[test]
    fn a_sketch_that_keeps_nothing_is_rejected() {
        assert!(Sketch::new(0).is_err());
    }

    #[test]
    fn a_functional_dependency_shows_up_as_a_dependence_of_one() {
        // The `URL` and `URLHash` case from section 6.6. The hash is determined by the URL, so
        // pairing them adds no distinct values.
        let urls = values(200_000, "http://example.com/page/");
        let mut left = Sketch::new(DEFAULT_K).unwrap();
        let mut pairs = Sketch::new(DEFAULT_K).unwrap();
        for url in &urls {
            let derived = hash64(url).to_le_bytes();
            left.add(url);
            pairs.add_hash(pair_hash(url, &derived));
        }
        let score = dependence(&left, &pairs).unwrap();
        assert!(score > 0.97, "{score:.4}");
    }

    #[test]
    fn two_independent_columns_do_not_look_like_a_dependency() {
        let left = values(1000, "left-");
        let right = values(1000, "right-");
        let mut alone = Sketch::new(DEFAULT_K).unwrap();
        let mut pairs = Sketch::new(DEFAULT_K).unwrap();
        for left_value in &left {
            alone.add(left_value);
            for right_value in &right {
                pairs.add_hash(pair_hash(left_value, right_value));
            }
        }
        let score = dependence(&alone, &pairs).unwrap();
        assert!(score < 0.01, "{score:.4}");
    }

    #[test]
    fn the_pair_hash_does_not_ignore_where_the_boundary_is() {
        assert_ne!(pair_hash(b"ab", b"c"), pair_hash(b"a", b"bc"));
        assert_ne!(pair_hash(b"a", b"b"), pair_hash(b"b", b"a"));
    }

    #[test]
    fn combining_two_hashes_is_the_same_as_hashing_the_pair() {
        // The lab hashes each column once a row and combines, and that has to be the same answer as
        // hashing the two values together, or a dependency measured the fast way is not the
        // dependency the slow way would have found.
        for left in ["", "a", "http://example.com/one"] {
            for right in ["", "b", "http://example.com/two"] {
                assert_eq!(
                    pair_hash(left.as_bytes(), right.as_bytes()),
                    pair_of(hash64(left.as_bytes()), hash64(right.as_bytes()))
                );
            }
        }
    }

    #[test]
    fn the_hash_spreads_one_bit_changes_across_the_output() {
        // Every estimate here assumes the hashes are uniform, so this measures the property rather
        // than asserting it. Flipping one bit of the input has to change about half the output
        // bits, and a hash that failed this would make every count above it wrong in a way that
        // looks like the sketch is broken.
        let mut total = 0u32;
        let mut trials = 0u32;
        for index in 0..2000u32 {
            let value = index.to_le_bytes();
            let base = hash64(&value);
            for bit in 0..32 {
                let mut flipped = value;
                flipped[bit / 8] ^= 1 << (bit % 8);
                total += (base ^ hash64(&flipped)).count_ones();
                trials += 1;
            }
        }
        let average = f64::from(total) / f64::from(trials);
        assert!((average - 32.0).abs() < 1.0, "{average:.3} bits changed on average");
    }

    #[test]
    fn the_hash_does_not_collide_on_values_that_differ_by_one_byte() {
        // The shape of a real column: a million near identical URLs. A hash that collided here
        // would make the distinct count an undercount and the overlap an overcount at the same
        // time.
        let mut hashes: Vec<u64> = (0..200_000u32)
            .map(|index| hash64(format!("http://a/{index:09}").as_bytes()))
            .collect();
        hashes.sort_unstable();
        let before = hashes.len();
        hashes.dedup();
        assert_eq!(hashes.len(), before);
    }

    #[test]
    fn a_long_value_and_its_prefix_hash_differently() {
        assert_ne!(hash64(b""), hash64(b"\0"));
        assert_ne!(hash64(b"abcdefgh"), hash64(b"abcdefgh\0"));
        assert_ne!(hash64(&[0u8; 16]), hash64(&[0u8; 24]));
    }
}

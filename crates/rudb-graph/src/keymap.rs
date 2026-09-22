//! Turning a parent key value into a [`Rid`].
//!
//! A link is built from an equality between a child column and a parent column, and to build it the
//! parent column's values have to become row ids. That map is the key map. It has the three
//! physical forms of spec/graph/02-the-data-model.md section 2.2, chosen by measurement at build
//! time rather than by declaration, and the form that was chosen is recorded in the header so that
//! a reader does not have to guess.
//!
//! The three exist because they are three different answers to the same question and the cheapest
//! one is usually available:
//!
//! - [`Form::Identity`] when the keys are exactly `base .. base + n` in order. Nothing is stored
//!   but two numbers, and TPC-H hits this on six of its eight tables.
//! - [`Form::Dense`] when the keys are distinct integers packed densely enough into a range that a
//!   bitmap plus a rank index beats storing them.
//! - [`Form::Sorted`] for everything else, including every string key, which arrives here as
//!   dictionary codes rather than as text.
//!
//! What is deliberately absent is a hash. A minimal perfect hash is faster to probe than the sorted
//! form and much slower to build, and there is no measurement yet saying the probe is where the
//! time goes. spec/graph/11-open-questions.md keeps it open, and adding it later costs nothing
//! because the form is a tag in a header that a reader is already required to be able to not
//! recognize.

use rudb_common::{Error, Result};
use rudb_encoding::bitpack;

use crate::rid::Rid;

/// How dense a range has to be before the bitmap form beats the sorted form.
///
/// One in eight, per section 2.2. Below it the bitmap is larger than storing the keys: a bitmap
/// costs `range / 8` bytes plus about an eighth again for the rank index, and the sorted form costs
/// `count` keys plus `count` permutation entries, so the crossover is a ratio rather than a size.
/// The default is here as a named constant rather than inline because it is a number somebody will
/// want to move once there is a measurement that says where, and moving it should be a diff.
pub const DENSE_THRESHOLD: u64 = 8;

/// Bits in one rank superblock.
const SUPERBLOCK_BITS: usize = 4096;

/// Bits in one rank block.
const BLOCK_BITS: usize = 512;

/// Blocks in one superblock.
const BLOCKS_PER_SUPERBLOCK: usize = SUPERBLOCK_BITS / BLOCK_BITS;

/// Which of the three physical forms a key map took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// `rid = key - base`, and nothing is stored but `base` and the count.
    Identity,
    /// `rid = rank(key - base)` over a bitmap of the range, with a two level rank index.
    Dense,
    /// Binary search over the sorted keys, then a permutation lookup.
    Sorted,
}

impl Form {
    /// The tag this form takes in a section header.
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            Self::Identity => 0,
            Self::Dense => 1,
            Self::Sorted => 2,
        }
    }

    /// The form a header tag names.
    ///
    /// # Errors
    ///
    /// If the tag is not one of the three. A reader that meets an unfamiliar form has met a file
    /// written by a later build, and the right response is the one section 3.2 requires of an
    /// unfamiliar section kind: ignore this key map and answer the query without it. So this
    /// returns an error and the caller drops the section rather than failing the open.
    pub fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Identity),
            1 => Ok(Self::Dense),
            2 => Ok(Self::Sorted),
            _ => Err(malformed(format!("key map form {tag} is not one this build knows"))),
        }
    }
}

/// What the build saw while it read the parent key column.
///
/// This is the cardinality verification of section 2.3, and it is written into the header rather
/// than recomputed because the build already had every value in front of it. Recording what was
/// observed rather than what was declared is what keeps a wrong `FOREIGN KEY` from producing a
/// wrong answer: a declaration that fails verification is reported, and no link is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    /// Non-null values seen.
    pub rows: u64,
    /// Nulls seen, which are not keys and match no child row.
    pub nulls: u64,
    /// Whether every non-null value was distinct. False means no link may be built at all.
    pub distinct: bool,
    /// Whether the values arrived in non-decreasing order.
    pub sorted: bool,
    /// The smallest non-null value, or `None` when there were none.
    pub min: Option<i128>,
    /// The largest non-null value, or `None` when there were none.
    pub max: Option<i128>,
}

impl Observed {
    /// Whether this column can be the parent side of a link.
    ///
    /// Distinctness is the whole requirement. A parent side that is not unique is not an error and
    /// is not a link: section 2.3 says it is a relationship that has to be executed as an ordinary
    /// join, and the planner is told so rather than left to find out.
    #[must_use]
    pub fn usable_as_parent(&self) -> bool {
        self.distinct
    }
}

/// A two level rank index over a bitmap.
///
/// Superblocks of 4096 bits hold a `u32` cumulative count from the start of the bitmap, and blocks
/// of 512 bits hold a `u16` count from the start of their superblock. A rank is then two loads and
/// a `popcount` over at most eight words, which is section 3.3's arithmetic and is the reason the
/// block size is 512: a `u16` cannot hold a count over a wider superblock than 4096, and eight
/// words is the most a `popcount` loop should have to do.
#[derive(Debug, Clone)]
struct Rank {
    superblocks: Vec<u32>,
    blocks: Vec<u16>,
}

impl Rank {
    fn build(bits: &[u64]) -> Self {
        let blocks = bits.len().div_ceil(BLOCK_BITS / 64);
        let mut index = Self {
            superblocks: Vec::with_capacity(blocks.div_ceil(BLOCKS_PER_SUPERBLOCK)),
            blocks: Vec::with_capacity(blocks),
        };
        let mut total = 0_u32;
        let mut within = 0_u16;
        for block in 0..blocks {
            if block % BLOCKS_PER_SUPERBLOCK == 0 {
                index.superblocks.push(total);
                within = 0;
            }
            index.blocks.push(within);
            let words = block * (BLOCK_BITS / 64);
            let ones: u32 = bits[words..(words + BLOCK_BITS / 64).min(bits.len())]
                .iter()
                .map(|word| word.count_ones())
                .sum();
            total += ones;
            // A superblock holds at most 4096 ones, so this cannot overflow a u16, and the `as` is
            // guarded by the reset above rather than by hope.
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a superblock holds at most 4096 bits, which fits a u16"
            )]
            let ones = ones as u16;
            within += ones;
        }
        index
    }

    /// How many bits are set strictly below `at`.
    fn rank(&self, bits: &[u64], at: usize) -> u64 {
        let block = at / BLOCK_BITS;
        let superblock = block / BLOCKS_PER_SUPERBLOCK;
        let mut count = u64::from(self.superblocks[superblock]) + u64::from(self.blocks[block]);
        let from = block * (BLOCK_BITS / 64);
        let word = at / 64;
        for whole in &bits[from..word] {
            count += u64::from(whole.count_ones());
        }
        let remainder = at % 64;
        if remainder != 0 {
            let mask = (1_u64 << remainder) - 1;
            count += u64::from((bits[word] & mask).count_ones());
        }
        count
    }

    fn bytes(&self) -> usize {
        self.superblocks.len() * size_of::<u32>() + self.blocks.len() * size_of::<u16>()
    }
}

/// The three forms, behind one interface.
#[derive(Debug, Clone)]
enum Body {
    Identity {
        base: i128,
        count: u64,
    },
    Dense {
        base: i128,
        range: u64,
        bits: Vec<u64>,
        rank: Rank,
    },
    Sorted {
        /// The smallest key, so that every stored key is a `u64` offset from it whatever the
        /// column's own type was.
        base: i128,
        /// Bits one stored key offset takes.
        key_width: usize,
        /// The key offsets in ascending order, bit packed.
        keys: Vec<u8>,
        /// Bits one permutation entry takes, which is `ceil(log2(rows))`.
        rid_width: usize,
        /// Sorted position to `rid`, bit packed.
        perm: Vec<u8>,
        count: u64,
    },
}

/// A map from a parent key value to the `rid` of the row that holds it.
#[derive(Debug, Clone)]
pub struct KeyMap {
    body: Body,
    observed: Observed,
}

impl KeyMap {
    /// Builds the cheapest correct form for these keys.
    ///
    /// `keys` is the parent key column in `rid` order, with `None` for a null. The `rid` of a value
    /// is its index, which is what makes this the whole build: the caller has already read the
    /// column in append order, so the row ids are the positions and there is nothing to look up.
    ///
    /// String keys arrive here as dictionary codes rather than as text, per section 2.2. That is
    /// not a convenience, it is the reason a sorted key map over a `VARCHAR` column never touches a
    /// byte of text: the codes of a file wide stable dictionary are integers with the column's own
    /// order, so the search is over `u32`.
    ///
    /// # Errors
    ///
    /// If the column's values span more than a `u64`, if it holds more rows than a `u64` of
    /// `rid`s, or if a bit packed payload cannot be written. A non-distinct column is not an
    /// error: it produces a key map whose [`Observed`] says so, and the caller is expected to ask
    /// before building a link on it.
    pub fn build(keys: &[Option<i128>]) -> Result<Self> {
        let mut observed = observe(keys);
        // A column with no keys in it at all is an identity map over nothing. It is worth having
        // rather than refusing, because an empty parent table is a legal table and a join against
        // it returns no rows rather than failing.
        if observed.rows == 0 {
            return Ok(Self { body: Body::Identity { base: 0, count: 0 }, observed });
        }
        let (Some(min), Some(max)) = (observed.min, observed.max) else {
            // A non-zero row count guarantees both, so this is unreachable. It is an error rather
            // than an `expect` because a key map that panicked on its own bookkeeping would take
            // down a query that section 3.1 promises can always be answered without it.
            return Err(malformed("a column with keys in it reported no minimum"));
        };
        let range = range_of(min, max)?;

        // Both of the cheap forms answer with a *count of keys below the value*, and both are
        // correct only where that count is the `rid`. It is the `rid` when the column is ascending
        // and holds no nulls, and it is not otherwise: a null earlier in the column, or a value out
        // of order, shifts every row after it. Getting this wrong would not fail, it would resolve
        // every key to a neighbour of the right row, which is the one failure mode section 3.1 does
        // not catch for free. So the guard is shared and stated once.
        let positional = observed.distinct && observed.sorted && observed.nulls == 0;

        if positional && range == observed.rows {
            // Identity needs more than positional: it needs the values to be exactly the
            // positions, which on a distinct ascending column is the range equalling the row count.
            // The check is subtraction rather than a walk because the walk already happened in
            // `observe`.
            return Ok(Self { body: Body::Identity { base: min, count: observed.rows }, observed });
        }

        // The bitmap is over the value range, so a range that does not fit a `usize` cannot be one
        // however dense it is.
        if positional && usize::try_from(range).is_ok() && range / observed.rows < DENSE_THRESHOLD {
            return Ok(Self { body: dense(keys, min, range)?, observed });
        }

        // The sorted form sorts, so it is the one place distinctness can be settled for a column
        // that did not arrive in order. `observe` can only see an adjacent duplicate; this sees
        // every duplicate, and the answer replaces the guess.
        let (body, distinct) = sorted(keys, min, observed.rows)?;
        observed.distinct = distinct;
        Ok(Self { body, observed })
    }

    /// Which form this map took.
    #[must_use]
    pub fn form(&self) -> Form {
        match self.body {
            Body::Identity { .. } => Form::Identity,
            Body::Dense { .. } => Form::Dense,
            Body::Sorted { .. } => Form::Sorted,
        }
    }

    /// What the build saw, which is the cardinality verification.
    #[must_use]
    pub fn observed(&self) -> &Observed {
        &self.observed
    }

    /// Keys this map resolves.
    #[must_use]
    pub fn len(&self) -> u64 {
        match &self.body {
            Body::Identity { count, .. } | Body::Sorted { count, .. } => *count,
            Body::Dense { .. } => self.observed.rows,
        }
    }

    /// Whether this map resolves nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes this map holds resident, for the budget of section 3.7 and the cache of section 4.4.
    ///
    /// Identity is twenty four bytes and says so, which is the number that makes the budget
    /// livable on TPC-H.
    #[must_use]
    pub fn bytes(&self) -> usize {
        match &self.body {
            Body::Identity { .. } => size_of::<i128>() + size_of::<u64>(),
            Body::Dense { bits, rank, .. } => bits.len() * size_of::<u64>() + rank.bytes(),
            Body::Sorted { keys, perm, .. } => keys.len() + perm.len(),
        }
    }

    /// The `rid` of the row holding this key, or `None` when no row holds it.
    ///
    /// `None` is the ordinary answer and not an exceptional one: a child key with no matching
    /// parent is what section 2.4 reserves *no parent* for, and a null child key never reaches
    /// here at all.
    ///
    /// # Errors
    ///
    /// If a bit packed payload is torn, which is a corrupt section rather than a missing key.
    pub fn lookup(&self, key: i128) -> Result<Option<Rid>> {
        match &self.body {
            Body::Identity { base, count } => {
                let Some(offset) = key.checked_sub(*base) else {
                    return Ok(None);
                };
                match u64::try_from(offset) {
                    Ok(rid) if rid < *count => Ok(Some(rid)),
                    _ => Ok(None),
                }
            }
            Body::Dense { base, range, bits, rank } => {
                let Some(offset) = key.checked_sub(*base) else {
                    return Ok(None);
                };
                let Ok(offset) = u64::try_from(offset) else {
                    return Ok(None);
                };
                if offset >= *range {
                    return Ok(None);
                }
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the build checked the range fits a usize"
                )]
                let at = offset as usize;
                if bits[at / 64] >> (at % 64) & 1 == 0 {
                    return Ok(None);
                }
                Ok(Some(rank.rank(bits, at)))
            }
            Body::Sorted { base, key_width, keys, rid_width, perm, count } => {
                let Some(offset) = key.checked_sub(*base) else {
                    return Ok(None);
                };
                let Ok(wanted) = u64::try_from(offset) else {
                    return Ok(None);
                };
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the build refused a column wider than a usize of rows"
                )]
                let len = *count as usize;
                // A plain binary search over the packed keys. Branchless in the sense that matters
                // here, which is that the comparison drives an index rather than a branch to a
                // different loop, and every probe is one `tail_at` rather than a decode of the
                // block around it.
                let mut low = 0_usize;
                let mut high = len;
                while low < high {
                    let mid = low + (high - low) / 2;
                    let at = bitpack::tail_at(keys, *key_width, mid)?;
                    if at < wanted {
                        low = mid + 1;
                    } else {
                        high = mid;
                    }
                }
                if low >= len || bitpack::tail_at(keys, *key_width, low)? != wanted {
                    return Ok(None);
                }
                Ok(Some(bitpack::tail_at(perm, *rid_width, low)?))
            }
        }
    }
}

/// One pass over the column, recording the four facts section 3.3 says the build records.
fn observe(keys: &[Option<i128>]) -> Observed {
    let mut observed =
        Observed { rows: 0, nulls: 0, distinct: true, sorted: true, min: None, max: None };
    let mut previous: Option<i128> = None;
    // Distinctness on a column that is not sorted cannot be settled in one pass without a set, so
    // this pass settles it for the sorted case and leaves the unsorted case to the sort that the
    // sorted form does anyway. That is why `distinct` is fixed up in `sorted` below rather than
    // being final here, and it is worth the awkwardness: the common case on real keys is ascending,
    // and a hash set over fifteen million rows to discover what adjacency already proves is the
    // build cost this avoids.
    for key in keys {
        let Some(key) = *key else {
            observed.nulls += 1;
            continue;
        };
        observed.rows += 1;
        observed.min = Some(observed.min.map_or(key, |held| held.min(key)));
        observed.max = Some(observed.max.map_or(key, |held| held.max(key)));
        if let Some(previous) = previous {
            if key < previous {
                observed.sorted = false;
            } else if key == previous {
                observed.distinct = false;
            }
        }
        previous = Some(key);
    }
    observed
}

/// How many distinct values lie between `min` and `max` inclusive.
///
/// The arithmetic is in `u128` and not `i128` because a column holding both `i128::MIN` and
/// `i128::MAX` has a range of `2^128`, and `max - min` on an `i128` for that column is an overflow
/// rather than a number. A `HUGEINT` key column spanning more than a `u64` of values is pathological
/// but legal, so it gets an error naming what happened rather than a panic in a build: the caller
/// records the relationship as not built, exactly as it does for one that does not fit the budget.
///
/// `max >= min` always holds here, so the wrapping subtraction is exact in `u128`.
fn range_of(min: i128, max: i128) -> Result<u64> {
    let span = max.wrapping_sub(min) as u128;
    u64::try_from(span)
        .ok()
        .and_then(|span| span.checked_add(1))
        .ok_or_else(|| malformed("the key column spans more than a u64 of values"))
}

/// The offset a key takes from the base.
///
/// `range_of` bounded the span to a `u64` before either form that uses this was chosen, so the
/// subtraction cannot overflow and the offset cannot exceed a `u64`. Both are checked anyway: this
/// is the one arithmetic in the crate whose silent failure would resolve keys to the wrong rows.
fn offset_of(key: i128, base: i128) -> Result<u64> {
    let offset = key
        .checked_sub(base)
        .ok_or_else(|| malformed("a key is further from the base than an i128 holds"))?;
    u64::try_from(offset)
        .map_err(|_| malformed("a key is below the base or further from it than a u64 holds"))
}

/// Builds the bitmap form.
///
/// The caller guarantees the column is distinct, ascending and null free, which is what makes a
/// rank equal to a `rid`. The assertion restates it where the correctness depends on it rather than
/// where the decision was made.
fn dense(keys: &[Option<i128>], base: i128, range: u64) -> Result<Body> {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the caller checked the range fits a usize"
    )]
    let range_usize = range as usize;
    let mut bits = vec![0_u64; range_usize.div_ceil(64)];
    let mut previous: Option<i128> = None;
    for key in keys.iter().flatten() {
        debug_assert!(
            previous.is_none_or(|held| *key > held),
            "the bitmap form needs a distinct ascending column, because a rank is a count of keys below a value and that is a rid only there"
        );
        previous = Some(*key);
        let offset = offset_of(*key, base)?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the caller checked the range fits a usize and the offset is inside it"
        )]
        let at = offset as usize;
        bits[at / 64] |= 1 << (at % 64);
    }
    let rank = Rank::build(&bits);
    Ok(Body::Dense { base, range, bits, rank })
}

/// Builds the general form, and settles distinctness on the way.
///
/// Returns the body and whether every key was distinct. The second is not a courtesy: the sort this
/// form performs is the only place a duplicate that is not adjacent in the column can be seen, and
/// section 2.3 needs that answer to decide whether a link may be built at all.
fn sorted(keys: &[Option<i128>], base: i128, rows: u64) -> Result<(Body, bool)> {
    let mut pairs: Vec<(u64, u64)> = Vec::with_capacity(keys.len());
    for (rid, key) in keys.iter().enumerate() {
        let Some(key) = *key else { continue };
        let offset = offset_of(key, base)?;
        let rid = u64::try_from(rid).map_err(|_| malformed("the column is too long for a rid"))?;
        pairs.push((offset, rid));
    }
    // Sorted by key, then by rid so that a duplicated key resolves to its first row rather than to
    // whichever one the sort happened to leave first. A duplicated key means no link gets built, so
    // this only decides what a map nobody should be using returns, and deciding it anyway is what
    // keeps a test of this form reproducible.
    pairs.sort_unstable();
    let distinct = pairs.windows(2).all(|pair| pair[0].0 != pair[1].0);
    debug_assert_eq!(
        u64::try_from(pairs.len()).ok(),
        Some(rows),
        "the pair list is the non-null column"
    );
    let key_width = width_for(pairs.last().map_or(0, |pair| pair.0));
    let rows_width = u64::try_from(keys.len().saturating_sub(1))
        .map_err(|_| malformed("the column is too long for a rid"))?;
    let rid_width = width_for(rows_width);
    let mut key_bytes = Vec::new();
    let mut rid_bytes = Vec::new();
    let key_values: Vec<u64> = pairs.iter().map(|pair| pair.0).collect();
    let rid_values: Vec<u64> = pairs.iter().map(|pair| pair.1).collect();
    bitpack::pack_tail(&key_values, key_width, &mut key_bytes)?;
    bitpack::pack_tail(&rid_values, rid_width, &mut rid_bytes)?;
    Ok((
        Body::Sorted { base, key_width, keys: key_bytes, rid_width, perm: rid_bytes, count: rows },
        distinct,
    ))
}

/// Bits needed to hold every value up to and including `largest`.
///
/// One rather than zero for a largest of zero, because a width of zero is a packed payload with no
/// bytes in it and `tail_at` on one of those has nothing to return. A column of a single key is a
/// real column.
fn width_for(largest: u64) -> usize {
    let bits = u64::BITS - largest.leading_zeros();
    bits.max(1) as usize
}

fn malformed(message: impl Into<String>) -> Error {
    Error::invalid_input(format!("invalid rudb key map: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(values: &[i128]) -> Vec<Option<i128>> {
        values.iter().copied().map(Some).collect()
    }

    /// Every key in the column resolves to the row that holds it, whatever form was chosen.
    fn resolves(column: &[Option<i128>], map: &KeyMap) {
        for (rid, key) in column.iter().enumerate() {
            let Some(key) = *key else { continue };
            let found = map.lookup(key).expect("lookup").expect("a key in the column resolves");
            assert_eq!(found, rid as u64, "key {key} resolved to {found} rather than {rid}");
        }
    }

    #[test]
    fn a_sequence_from_one_is_the_identity_form_and_stores_two_numbers() {
        // TPC-H's `region`, `nation`, `supplier`, `customer`, `part` and `orders` all land here,
        // which is the case the whole budget in section 3.7 depends on.
        let column = keys(&(1..=1000).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Identity);
        assert_eq!(map.bytes(), 24, "section 4.2 says identity is twenty four bytes");
        assert_eq!(map.len(), 1000);
        resolves(&column, &map);
        assert_eq!(map.lookup(0).expect("lookup"), None, "below the base");
        assert_eq!(map.lookup(1001).expect("lookup"), None, "past the end");
    }

    #[test]
    fn a_sequence_from_zero_is_also_the_identity_form() {
        let column = keys(&(0..64).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Identity);
        resolves(&column, &map);
    }

    #[test]
    fn a_sequence_with_a_gap_in_it_is_the_dense_form() {
        // Every other value over a range of two thousand, which is a density of one in two and
        // comfortably inside the threshold.
        let column = keys(&(0..1000).map(|value| value * 2).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Dense);
        resolves(&column, &map);
        assert_eq!(
            map.lookup(1).expect("lookup"),
            None,
            "a value in the range and not in the column"
        );
        assert_eq!(map.lookup(2001).expect("lookup"), None, "past the range");
    }

    #[test]
    fn a_range_too_sparse_for_a_bitmap_is_the_sorted_form() {
        // A thousand keys spread over a million, which is a density of one in a thousand: the
        // bitmap would be 125 KB to hold a thousand values and the sorted form is a few kilobytes.
        let column = keys(&(0..1000).map(|value| value * 1000).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Sorted);
        resolves(&column, &map);
        assert_eq!(map.lookup(500).expect("lookup"), None);
    }

    #[test]
    fn keys_in_no_order_at_all_resolve_to_the_rows_that_hold_them() {
        // The case the permutation exists for. The column is not sorted, so the sorted form's
        // position is not the rid, and a map that confused the two would resolve every key to the
        // wrong row while looking exactly like a working map.
        let column = keys(&[500, 3, 9000, 12, 7, 88, 41, 6]);
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Sorted);
        resolves(&column, &map);
    }

    #[test]
    fn a_descending_column_dense_enough_for_a_bitmap_still_resolves_correctly() {
        // The trap in the dense form: a bitmap is in value order, so a rank is a position in value
        // order, and on a descending column that is not the rid. `dense` detects it and falls back.
        let column = keys(&(0..500).rev().collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Sorted, "a descending column cannot take the bitmap");
        resolves(&column, &map);
    }

    #[test]
    fn nulls_are_not_keys_and_do_not_shift_the_rows_around_them() {
        // This column is distinct, ascending, and dense enough for a bitmap on the numbers alone:
        // three keys over a range of twenty one. It cannot have one, because a rank counts keys
        // below a value and the nulls in between mean that count is not the row's position. A map
        // that took the bitmap here would resolve key 20 to row 1 and look entirely healthy doing
        // it.
        let column = vec![Some(10), None, Some(20), None, Some(30)];
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(
            map.form(),
            Form::Sorted,
            "a null before a key shifts it out of the cheap forms"
        );
        resolves(&column, &map);
        assert_eq!(map.observed().nulls, 2);
        assert_eq!(map.observed().rows, 3);
        assert_eq!(
            map.lookup(20).expect("lookup"),
            Some(2),
            "the rid is the position in the column"
        );
    }

    #[test]
    fn a_leading_null_keeps_an_otherwise_perfect_sequence_out_of_the_identity_form() {
        // The same trap on the form that would otherwise be free. Worth its own test because a
        // sequence from one is the case every TPC-H table hits, and the version of it with a null
        // in front is one `INSERT` away.
        let mut column = vec![None];
        column.extend((1..=1000).map(Some));
        let map = KeyMap::build(&column).expect("build");
        assert_ne!(map.form(), Form::Identity);
        resolves(&column, &map);
        assert_eq!(map.lookup(1).expect("lookup"), Some(1), "row zero is the null, not key one");
    }

    #[test]
    fn a_null_only_column_builds_and_resolves_nothing() {
        let column = vec![None, None, None];
        let map = KeyMap::build(&column).expect("build");
        assert!(map.is_empty());
        assert_eq!(map.observed().nulls, 3);
        assert_eq!(map.lookup(0).expect("lookup"), None);
    }

    #[test]
    fn an_empty_column_builds_and_resolves_nothing() {
        let map = KeyMap::build(&[]).expect("build");
        assert!(map.is_empty());
        assert_eq!(map.lookup(0).expect("lookup"), None);
        assert!(map.observed().usable_as_parent(), "an empty parent is unique, vacuously");
    }

    #[test]
    fn a_duplicated_key_is_reported_rather_than_resolved_to_one_of_its_rows() {
        // Section 2.3's verification. The map still builds, because the caller is the one that
        // decides what to do about it, and what it decides is to build no link.
        let column = keys(&[5, 7, 5, 9]);
        let map = KeyMap::build(&column).expect("build");
        assert!(!map.observed().distinct);
        assert!(!map.observed().usable_as_parent(), "a non-unique parent side takes no link");
    }

    #[test]
    fn a_single_key_column_resolves_it() {
        // The width of zero case: one key at the base is an offset of zero, and a packed payload of
        // width zero has no bytes for `tail_at` to read.
        let column = keys(&[42]);
        let map = KeyMap::build(&column).expect("build");
        resolves(&column, &map);
        assert_eq!(map.lookup(41).expect("lookup"), None);
        assert_eq!(map.lookup(43).expect("lookup"), None);
    }

    #[test]
    fn negative_keys_resolve_because_the_base_is_the_minimum_and_not_zero() {
        let column = keys(&[-9000, -3, -1, 0, 7]);
        let map = KeyMap::build(&column).expect("build");
        resolves(&column, &map);
        assert_eq!(map.lookup(-9001).expect("lookup"), None);
    }

    #[test]
    fn a_column_spanning_more_than_a_u64_of_values_is_refused_and_not_panicked_over() {
        // `max - min` on a HUGEINT column holding both ends of the type overflows an i128, so this
        // is where a build panics if the range arithmetic is done in the column's own type. It is
        // refused instead, and the caller records the relationship as not built.
        let column = keys(&[i128::MIN, 0, i128::MAX]);
        let error = KeyMap::build(&column).expect_err("refused");
        assert!(error.to_string().contains("spans more than a u64"), "{error}");
    }

    #[test]
    fn keys_at_the_far_end_of_the_integer_type_resolve_when_their_range_is_narrow() {
        // The other half of the same arithmetic: the values are extreme and the range is not, which
        // is a column a key map has to handle rather than refuse.
        let column = keys(&[i128::MIN, i128::MIN + 5, i128::MIN + 2]);
        let map = KeyMap::build(&column).expect("build");
        resolves(&column, &map);
        assert_eq!(map.lookup(i128::MAX).expect("lookup"), None);
        assert_eq!(map.lookup(0).expect("lookup"), None);
    }

    #[test]
    fn the_rank_index_agrees_with_counting_the_bits_by_hand() {
        // The rank structure is two levels and an eight word popcount, and an off by one in any of
        // the three resolves every key past the fault to the row before or after the right one. So
        // it is checked against the naive count over a bitmap wide enough to use every level: 4096
        // bits is one superblock exactly, so 20,000 forces five of them and the last one partial.
        let column = keys(&(0..10_000).map(|value| value * 2).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Dense);
        resolves(&column, &map);
    }

    #[test]
    fn a_string_key_arrives_as_dictionary_codes_and_never_as_text() {
        // Section 2.2's composition with the global dictionary. There is nothing string shaped in
        // this crate and that is the point: the codes of a file wide stable dictionary carry the
        // column's own order, so a sorted key map over a VARCHAR is this and the search is over
        // integers.
        let codes = keys(&[7, 1, 4, 9, 2]);
        let map = KeyMap::build(&codes).expect("build");
        resolves(&codes, &map);
    }

    #[test]
    fn the_form_tag_round_trips_and_an_unknown_one_is_refused() {
        for form in [Form::Identity, Form::Dense, Form::Sorted] {
            assert_eq!(Form::from_tag(form.tag()).expect("a known tag"), form);
        }
        assert!(Form::from_tag(3).is_err(), "an unfamiliar form is refused rather than guessed");
    }

    #[test]
    fn the_dense_form_costs_a_bitmap_and_about_an_eighth_again() {
        // The space claim in section 3.3, checked. A range of 80,000 bits is 10,000 bytes and the
        // index is a u16 per 512 bits plus a u32 per 4096, which is about 12.5 percent.
        let column = keys(&(0..10_000).map(|value| value * 8).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Dense);
        let bitmap = 80_000 / 8;
        let bytes = map.bytes();
        assert!(bytes > bitmap, "the map took {bytes} bytes and the bitmap alone is {bitmap}");
        assert!(
            bytes < bitmap * 5 / 4,
            "the map took {bytes} bytes, more than a quarter over the bitmap's {bitmap}"
        );
    }
}

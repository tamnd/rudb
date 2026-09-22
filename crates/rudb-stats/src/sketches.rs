//! `RUDBSK1`, the sketches of `spec/stats/03-the-file-format.md` section 3.4.
//!
//! One merged KMV sketch for the column, and a smaller one per stripe. A sketch is the k smallest
//! hashes of the column's distinct values, so writing one down is writing that list down and
//! nothing else, which is why this module is mostly a header and a length prefix.
//!
//! # Why a KMV sketch and not a HyperLogLog
//!
//! `rudb_encoding::sketch` argues this at length for the encoding chooser and the argument carries
//! over unchanged: a HyperLogLog register holds a leading zero count rather than a value, so two of
//! them merge into a count of the union and cannot say which values the union kept. Intersection
//! and Jaccard are both needed here, the first for join selectivity and the second for deciding
//! whether two columns come from the same universe, and inclusion and exclusion over three noisy
//! counts is noise for exactly the pair of columns worth asking about. A KMV sketch is also exact
//! below k, which a HyperLogLog never is, and that exactness is what lets a `COUNT(DISTINCT c)` be
//! answered out of metadata for most dimension keys and most grouping columns anybody writes.
//!
//! # Why the per stripe sketches are kept as well as the merged one
//!
//! Because a planner asking about a range of stripes is what a selective range predicate on a
//! clustered column actually is, and the merged sketch cannot answer that at all. They also let an
//! append add a stripe without recomputing anything.
//!
//! What they are not is a cheaper way of storing the merged sketch. Section 3.4 says they union to
//! it, and they would at the same k, but they are kept at [`STRIPE_K`] of 256 against a column's
//! 4096, so eight of them hold 2,048 hashes between them and a union of those is a sketch that never
//! filled and therefore reports its own size as an exact distinct count. Both are built in one pass
//! over the column instead, each fed every value. [`Sketches::new`] says more about why that
//! distinction is the difference between a worse estimate and a confident wrong answer.
//!
//! # What section 3.8's arithmetic actually says
//!
//! Not that [`STRIPE_K`] makes the per stripe sketches fit. It says they do not fit even at 256:
//! 2 KB per stripe per column over ten thousand stripes of an SF100 `lineitem` is about 320 MB
//! across sixteen columns, against a budget of two percent of the column bytes for everything in
//! the statistics document. The merged sketches are the negligible half at 32 KB a column.
//!
//! The resolution is the rule rather than a smaller k pulled out of the air: per stripe structures
//! are written only for the columns that get read, which is the ones the encoding chooser already
//! sketched plus anything in a declared key or relationship, and document 06's observation log is
//! what promotes a column into that set at the next checkpoint. So [`Sketches::stripes`] being empty
//! is the common case and not a degraded one, and [`STRIPE_K`] is chosen for what a stripe sketch is
//! asked rather than for the budget: six percent relative error, which is a poor distinct count and
//! a perfectly good answer to whether two stripes hold the same values.
//!
//! # The hash is in the header
//!
//! A sketch built by one hash and merged with a sketch built by another is silently wrong, not
//! merely worse. So every stored sketch carries [`rudb_encoding::sketch::HASH_IDENTITY`] and a
//! reader that meets a different one declines the section, which is the ordinary missing statistic
//! case. This is a file format that will outlive a hash choice, and the cost of being wrong about
//! that is a confident wrong answer.

use rudb_common::{Error, Result};
use rudb_encoding::sketch::{DEFAULT_K, HASH_IDENTITY, Sketch};

/// What layout the bytes are in.
const LAYOUT: u8 = 1;

/// How many hashes a per stripe sketch keeps.
///
/// Two hundred and fifty six, which is section 3.8's number and which puts the per stripe relative
/// error around six percent. That is a poor answer to how many distinct values a stripe has and a
/// perfectly good answer to whether two stripes hold the same values, which is the question the per
/// stripe sketches exist for. The merged sketch is where the accurate count lives.
pub const STRIPE_K: usize = 256;

/// The most stripe sketches one column may carry.
///
/// An SF100 `lineitem` is about ten thousand stripes, so this is comfortably past anything a file
/// has. The bound is here so that a torn header naming four billion of them is refused at decode
/// rather than turned into an allocation, which is the same reason the extent count has one.
const MAX_STRIPES: u32 = 1 << 20;

/// The bytes at the front of the payload that are this kind's own header.
///
/// Reported to the section table as `header_bytes`, which is what lets a reader decide whether to
/// read the rest without reading the rest: the hash identity and the two k values are all it needs
/// to know the sketches are ones it can use.
pub const HEADER_BYTES: u32 = 21;

/// A column's sketches: one merged and one per stripe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sketches {
    /// The sketch of the whole column, at the column's k.
    pub merged: Sketch,
    /// One per stripe in stripe order, at [`STRIPE_K`], or empty for a column the per stripe rule
    /// of section 3.8 did not choose.
    ///
    /// Empty is not a degraded state. It is what section 3.8 says a column with no predicate ever
    /// pushed to it gets, and the merged sketch above answers every question that does not name a
    /// range of stripes.
    pub stripes: Vec<Sketch>,
}

impl Sketches {
    /// The sketches of a column that has only ever been looked at as a whole.
    #[must_use]
    pub fn merged(merged: Sketch) -> Self {
        Self { merged, stripes: Vec::new() }
    }

    /// The sketches of a column whose stripes were sketched as well as the whole.
    ///
    /// The merged sketch is taken rather than derived, and that is worth being plain about because
    /// the obvious alternative is wrong. Section 3.4 says the per stripe sketches union to the
    /// merged one, and they do, at the same k. They are not at the same k: [`STRIPE_K`] is 256 and a
    /// column's k defaults to 4096, so eight stripes hold at most 2,048 hashes between them and a
    /// sketch of the column's k built by pouring those in never fills up. It would then report 2,048
    /// distinct values and report them as *exact*, for a column of eighty thousand. Not a worse
    /// estimate, a confident wrong answer, which is the failure this whole crate is arranged to
    /// avoid.
    ///
    /// So both are built in the same pass over the column, each fed every value, and the per stripe
    /// sketches are a second thing the pass produces rather than the source of the first. Unioning
    /// them is still meaningful and still cheap and gives a lower bound on the column, which is what
    /// [`Sketches::floor`] is for.
    ///
    /// # Errors
    ///
    /// If a stripe sketch is not at [`STRIPE_K`], or if there are more stripes than a file can have.
    /// Both are writer bugs and both stop at the write.
    pub fn new(merged: Sketch, stripes: Vec<Sketch>) -> Result<Self> {
        if stripes.len() > MAX_STRIPES as usize {
            return Err(Error::internal(format!(
                "{} stripe sketches are more than the {MAX_STRIPES} a column may carry",
                stripes.len()
            )));
        }
        for stripe in &stripes {
            if stripe.k() != STRIPE_K {
                return Err(Error::internal(format!(
                    "a stripe sketch of {} hashes is not the {STRIPE_K} a stripe sketch keeps",
                    stripe.k()
                )));
            }
        }
        Ok(Self { merged, stripes })
    }

    /// A lower bound on the distinct values of a run of stripes, from their sketches alone.
    ///
    /// This is the question the per stripe sketches exist to answer and the merged sketch cannot:
    /// how much of a column a predicate that keeps stripes `from..to` is going to see. It is a lower
    /// bound rather than an estimate because each stripe sketch holds at most [`STRIPE_K`] hashes,
    /// so a stripe with more distinct values than that contributes the ones it kept and no more.
    ///
    /// `None` when the column has no per stripe sketches, which is the ordinary case under section
    /// 3.8's rule, or when the range names stripes the column does not have.
    #[must_use]
    pub fn floor(&self, from: usize, to: usize) -> Option<f64> {
        let run = self.stripes.get(from..to)?;
        if run.is_empty() {
            return None;
        }
        // Into a sketch of STRIPE_K rather than of the column's k, so that the union is a bottom-k
        // set of the same k as its inputs and the estimate off it means what the estimator says it
        // means. A sketch of a larger k poured into from smaller ones is the mistake `new` above
        // describes.
        let mut union = Sketch::new(STRIPE_K).ok()?;
        for stripe in run {
            for hash in stripe.hashes() {
                union.add_hash(hash);
            }
        }
        Some(union.distinct())
    }

    /// How many hashes the merged sketch keeps.
    #[must_use]
    pub fn k(&self) -> usize {
        self.merged.k()
    }

    /// How many bytes this takes on disk, without encoding it.
    ///
    /// The budget of section 3.8 is checked before the sketches are built, so the thing that checks
    /// it needs the size without the bytes.
    #[must_use]
    pub fn bytes(&self) -> usize {
        let run = |sketch: &Sketch| 4 + sketch.len() * 8;
        HEADER_BYTES as usize + run(&self.merged) + self.stripes.iter().map(run).sum::<usize>()
    }

    /// Appends the header and then every sketch.
    ///
    /// # Errors
    ///
    /// If a k or a sketch length is larger than a `u32` can count, which [`Sketch::new`]'s own bound
    /// already rules out and which is checked here so that the bytes cannot be written truncated.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let before = out.len();
        out.push(LAYOUT);
        out.extend_from_slice(&HASH_IDENTITY.to_le_bytes());
        out.extend_from_slice(&count(self.merged.k())?.to_le_bytes());
        out.extend_from_slice(&count(STRIPE_K)?.to_le_bytes());
        out.extend_from_slice(&count(self.stripes.len())?.to_le_bytes());
        debug_assert_eq!(
            out.len() - before,
            HEADER_BYTES as usize,
            "the header is what HEADER_BYTES says it is"
        );
        put(out, &self.merged)?;
        for stripe in &self.stripes {
            put(out, stripe)?;
        }
        Ok(())
    }

    /// Reads sketches written by [`Sketches::encode`].
    ///
    /// # Errors
    ///
    /// If the layout number is one this build does not write, if the hash identity is not this
    /// build's, if the bytes run out, or if the header names more stripes than a column may carry.
    /// Every one of those is answered by not having the sketches, which section 3.1 says is a
    /// correct state, so the caller drops them and runs the query the way it ran before.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut at = 0;
        let layout = take(bytes, &mut at, 1)?[0];
        if layout != LAYOUT {
            return Err(torn(format!("layout {layout} and this build writes {LAYOUT}")));
        }
        let hash = eight(bytes, &mut at)?;
        if hash != HASH_IDENTITY {
            return Err(torn(format!(
                "been built with hash {hash:#x} and this build hashes {HASH_IDENTITY:#x}"
            )));
        }
        let k = four(bytes, &mut at)?;
        let stripe_k = four(bytes, &mut at)?;
        let stripes = four(bytes, &mut at)?;
        if stripes > MAX_STRIPES {
            return Err(torn(format!("{stripes} stripe sketches, past the bound")));
        }
        let merged = get(bytes, &mut at, k as usize)?;
        let mut held = Vec::with_capacity(stripes as usize);
        for _ in 0..stripes {
            held.push(get(bytes, &mut at, stripe_k as usize)?);
        }
        Ok(Self { merged, stripes: held })
    }
}

/// The sketches of a column nobody has sketched, at the default k.
///
/// An empty sketch rather than an absent one, so that a caller folding stripes into a column has
/// something to fold onto.
impl Default for Sketches {
    fn default() -> Self {
        Self {
            merged: Sketch::new(DEFAULT_K).expect("the default k is one a sketch takes"),
            stripes: Vec::new(),
        }
    }
}

fn put(out: &mut Vec<u8>, sketch: &Sketch) -> Result<()> {
    let hashes = sketch.hashes();
    out.extend_from_slice(&count(hashes.len())?.to_le_bytes());
    for hash in hashes {
        out.extend_from_slice(&hash.to_le_bytes());
    }
    Ok(())
}

fn get(bytes: &[u8], at: &mut usize, k: usize) -> Result<Sketch> {
    let held = four(bytes, at)? as usize;
    if held > k {
        return Err(torn(format!("{held} hashes in a sketch that keeps {k}")));
    }
    let mut hashes = Vec::with_capacity(held);
    for _ in 0..held {
        hashes.push(eight(bytes, at)?);
    }
    Sketch::from_hashes(k, &hashes)
}

fn eight(bytes: &[u8], at: &mut usize) -> Result<u64> {
    let held: [u8; 8] = take(bytes, at, 8)?.try_into().map_err(|_| torn("a short field"))?;
    Ok(u64::from_le_bytes(held))
}

fn four(bytes: &[u8], at: &mut usize) -> Result<u32> {
    let held: [u8; 4] = take(bytes, at, 4)?.try_into().map_err(|_| torn("a short field"))?;
    Ok(u32::from_le_bytes(held))
}

fn count(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::internal("a sketch longer than a u32 can count"))
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = at.checked_add(len).ok_or_else(|| torn("a length that overflows"))?;
    let taken = bytes.get(*at..end).ok_or_else(|| torn("fewer bytes than it names"))?;
    *at = end;
    Ok(taken)
}

fn torn(what: impl Into<String>) -> Error {
    Error::invalid_input(format!("a stored sketch has {}", what.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sketch(k: usize, from: u64, count: u64) -> Sketch {
        let mut sketch = Sketch::new(k).expect("a sketch");
        for value in from..from + count {
            sketch.add(&value.to_le_bytes());
        }
        sketch
    }

    #[test]
    fn a_column_with_no_stripe_sketches_round_trips() {
        let one = Sketches::merged(sketch(DEFAULT_K, 0, 100_000));
        let mut bytes = Vec::new();
        one.encode(&mut bytes).expect("encode");
        assert_eq!(bytes.len(), one.bytes(), "the size is the size without encoding it");
        assert_eq!(Sketches::decode(&bytes).expect("decode"), one);
    }

    #[test]
    fn a_column_with_stripe_sketches_round_trips() {
        let stripes: Vec<Sketch> = (0..8).map(|at| sketch(STRIPE_K, at * 10_000, 10_000)).collect();
        let one = Sketches::new(sketch(DEFAULT_K, 0, 80_000), stripes).expect("sketches");
        let mut bytes = Vec::new();
        one.encode(&mut bytes).expect("encode");
        assert_eq!(bytes.len(), one.bytes());
        let back = Sketches::decode(&bytes).expect("decode");
        assert_eq!(back, one);
        assert_eq!(back.stripes.len(), 8);
        assert_eq!(back.k(), DEFAULT_K);
    }

    #[test]
    fn an_empty_column_round_trips() {
        let one = Sketches::default();
        let mut bytes = Vec::new();
        one.encode(&mut bytes).expect("encode");
        assert_eq!(Sketches::decode(&bytes).expect("decode"), one);
        assert!(one.merged.is_empty());
    }

    #[test]
    fn the_merged_sketch_counts_the_column_and_no_arithmetic_over_the_stripes_can() {
        // Eight stripes of ten thousand values each, none shared with another. The merged sketch
        // saw all eighty thousand and says so. The stripe sketches kept STRIPE_K hashes apiece and
        // threw the rest away, so nothing built out of them gets back to eighty thousand.
        //
        // This is a test rather than a comment because the version of `new` that derived the merged
        // sketch from the stripes answered 2048 here, and answered it as exact.
        let stripes: Vec<Sketch> = (0..8).map(|at| sketch(STRIPE_K, at * 10_000, 10_000)).collect();
        let one = Sketches::new(sketch(DEFAULT_K, 0, 80_000), stripes).expect("sketches");

        let merged = one.merged.distinct();
        assert!(merged > 60_000.0 && merged < 100_000.0, "{merged}");
        assert!(!one.merged.is_exact(), "eighty thousand values past k is not an exact sketch");

        let floor = one.floor(0, 8).expect("eight stripes");
        assert!(floor < merged, "{floor} against {merged}, a floor is not the count");
    }

    #[test]
    fn a_run_of_stripes_gets_a_floor_and_a_column_without_stripes_gets_nothing() {
        let stripes: Vec<Sketch> = (0..8).map(|at| sketch(STRIPE_K, at * 10_000, 10_000)).collect();
        let one = Sketches::new(sketch(DEFAULT_K, 0, 80_000), stripes).expect("sketches");

        let two = one.floor(0, 2).expect("two stripes");
        let all = one.floor(0, 8).expect("eight stripes");
        assert!(all > two, "{all} against {two}, more stripes hold more between them");
        assert_eq!(one.floor(0, 9), None, "a range naming stripes the column has not got");
        assert_eq!(one.floor(3, 3), None, "an empty range");
        assert_eq!(Sketches::merged(sketch(DEFAULT_K, 0, 10)).floor(0, 1), None);
    }

    #[test]
    fn a_stripe_sketch_at_the_wrong_k_is_refused_at_the_write() {
        let wrong = vec![sketch(DEFAULT_K, 0, 100)];
        assert!(Sketches::new(sketch(DEFAULT_K, 0, 100), wrong).is_err());
    }

    #[test]
    fn a_sketch_built_by_another_hash_is_declined_rather_than_merged() {
        // The one failure this format exists to catch. Two bottom-k sets drawn by two different
        // hashes are two samples of two different orderings of the same values, and a union of them
        // answers confidently and wrongly.
        let one = Sketches::merged(sketch(DEFAULT_K, 0, 1000));
        let mut bytes = Vec::new();
        one.encode(&mut bytes).expect("encode");
        bytes[1..9].copy_from_slice(&HASH_IDENTITY.wrapping_add(1).to_le_bytes());
        let refused = Sketches::decode(&bytes).expect_err("a foreign hash is declined");
        assert!(refused.to_string().contains("hash"), "{refused}");
    }

    #[test]
    fn a_truncated_payload_is_refused_at_every_length() {
        let stripes: Vec<Sketch> = (0..3).map(|at| sketch(STRIPE_K, at * 500, 500)).collect();
        let one = Sketches::new(sketch(DEFAULT_K, 0, 1500), stripes).expect("sketches");
        let mut bytes = Vec::new();
        one.encode(&mut bytes).expect("encode");
        for short in 0..bytes.len() {
            assert!(Sketches::decode(&bytes[..short]).is_err(), "{short} bytes");
        }
    }

    #[test]
    fn a_header_naming_more_stripes_than_a_file_has_is_refused_before_it_allocates() {
        let one = Sketches::merged(sketch(DEFAULT_K, 0, 10));
        let mut bytes = Vec::new();
        one.encode(&mut bytes).expect("encode");
        bytes[17..21].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Sketches::decode(&bytes).is_err());
    }

    #[test]
    fn a_sketch_claiming_more_hashes_than_its_k_is_refused() {
        let one = Sketches::merged(sketch(DEFAULT_K, 0, 10));
        let mut bytes = Vec::new();
        one.encode(&mut bytes).expect("encode");
        let at = HEADER_BYTES as usize;
        bytes[at..at + 4].copy_from_slice(&(DEFAULT_K as u32 + 1).to_le_bytes());
        assert!(Sketches::decode(&bytes).is_err());
    }

    #[test]
    fn the_per_stripe_arithmetic_of_section_three_eight_holds() {
        // Section 3.8's numbers, restated where the constant they are about lives, because the easy
        // misreading is that STRIPE_K is what makes them fit. It is not. An SF100 lineitem, sixteen
        // columns, about ten thousand stripes.
        let (stripes, columns) = (10_000, 16);

        let merged_all = (4 + DEFAULT_K * 8) * columns;
        assert!(merged_all < 1024 * 1024, "{merged_all} bytes, the negligible half");

        let per_stripe_all = (4 + STRIPE_K * 8) * stripes * columns;
        assert!(per_stripe_all > 300 * 1024 * 1024, "{per_stripe_all} bytes");
        // The point. The smaller k already bought a factor of sixteen and the per stripe sketches
        // are still three hundred times the merged ones and still in the hundreds of megabytes, so
        // what brings the total under two percent is the rule that most columns get none of them.
        assert!(per_stripe_all > 300 * merged_all, "{per_stripe_all} against {merged_all}");

        let read = 2;
        let chosen = (4 + STRIPE_K * 8) * stripes * read;
        assert!(chosen < 50 * 1024 * 1024, "{chosen} bytes for the columns that are read");
    }
}

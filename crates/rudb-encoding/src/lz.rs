//! A match finder, which is the one thing this crate did not have.
//!
//! # Why
//!
//! Every other encoding here is a local transform. Front coding looks at exactly one value back.
//! FSST builds 255 symbols of at most eight bytes. Run length encoding looks at the previous value.
//! None of them can say "this forty character path segment appeared six hundred values ago", and on
//! the data this engine is measured against that is where most of the redundancy is.
//!
//! Measured on one chunk of 122,880 sorted URLs out of ClickBench `hits`, in tiles of about 190 KB
//! so a lookup decompresses a tile rather than a chunk. The cascade without this got 3.16 times.
//! Front coding followed by this, with the three streams below handed to the encoders that already
//! existed, got 5.15 times. deflate at the same tile size got 5.00 and zstd at level three got
//! 5.63. Issue #575 has the full table.
//!
//! # Why there is no entropy coder
//!
//! Because it is not where the win is, which was worth measuring rather than assuming. The same
//! matcher with byte aligned tokens gets 3.57 times, barely above the 3.16 the cascade already
//! managed. Sending the same tokens through [`crate::integer`] instead gets 4.40. So what makes
//! matching pay here is the bitpacking and frame of reference this crate already has, applied to
//! the length and offset streams, and not a Huffman coder anybody has to write.
//!
//! # The three streams
//!
//! A token is a run of literal bytes followed by a copy from earlier in the output. The literal runs
//! go back through the string cascade, so FSST still gets a go at the bytes no match covered, and
//! the lengths and offsets go through the integer cascade. Splitting them matters: interleaved
//! tokens are three distributions in one stream and none of the integer encodings can see any of
//! them.
//!
//! # The segment
//!
//! Matching restarts every [`SEGMENT`] bytes. That bounds the hash chain's memory, which would
//! otherwise be one entry per byte of a 23 MB chunk, and it bounds the worst case search. Offsets
//! are still written relative to the whole output, so a decoder copies from `out.len() - offset`
//! and never has to know where a segment began.

use rudb_common::{Error, Result};

/// How many bytes are matched against before the hash chain is thrown away and started again.
///
/// 256 KiB because the measurement in #575 was taken in tiles of 1024 sorted URLs, which is about
/// 190 KB, and because the chain costs four bytes a byte of segment. Larger segments compress
/// slightly better and cost proportionally more memory. Smaller ones are what a format with finer
/// random access would want, and the number is here rather than spread through the code so that
/// trade can be made in one place.
const SEGMENT: usize = 256 * 1024;

/// The shortest copy worth emitting, in bytes.
///
/// Below four the token costs more than the bytes it saves, which is the same number deflate and
/// lz4 landed on for the same reason.
const MIN_MATCH: usize = 4;

/// The shortest copy worth emitting, as the value the hash is taken over.
const HASH_BITS: u32 = 16;

/// How far back along one hash chain the search goes before it settles for what it has.
///
/// A greedy matcher with a bounded chain, which is what deflate calls a compression level. Raising
/// this buys tenths of a ratio point for a proportional amount of time.
const MAX_TRIES: usize = 32;

/// What a match finder produced, as three streams rather than one.
pub(crate) struct Tokens<'a> {
    /// The bytes no copy covered, one run a token, empty where a copy followed a copy.
    pub literals: Vec<&'a [u8]>,
    /// How many bytes each token copies, and zero for the last token when it is literals only.
    pub lengths: Vec<i64>,
    /// How far back each token copies from, counted from the end of the output so far.
    pub offsets: Vec<i64>,
}

/// What one segment's matching produced, before the literal ranges become slices.
#[derive(Default)]
struct Raw {
    /// Where each token's literal run sits in the input, as a half open range.
    runs: Vec<(usize, usize)>,
    lengths: Vec<i64>,
    offsets: Vec<i64>,
}

/// Splits `input` into literal runs and back references.
pub(crate) fn tokens_of(input: &[u8]) -> Tokens<'_> {
    let mut raw = Raw::default();
    let mut head = vec![u32::MAX; 1 << HASH_BITS];
    let span = SEGMENT.min(input.len()).max(1);
    let mut prev = vec![u32::MAX; span];

    let mut start = 0;
    while start < input.len() {
        let end = (start + SEGMENT).min(input.len());
        head.fill(u32::MAX);
        matches_in(input, start, end, &mut head, &mut prev, &mut raw);
        start = end;
    }
    Tokens {
        literals: raw.runs.iter().map(|(from, to)| &input[*from..*to]).collect(),
        lengths: raw.lengths,
        offsets: raw.offsets,
    }
}

/// One segment's worth of matching, appending to `tokens`.
fn matches_in(
    input: &[u8],
    start: usize,
    end: usize,
    head: &mut [u32],
    prev: &mut [u32],
    raw: &mut Raw,
) {
    let mut literal_start = start;
    let mut at = start;
    while at < end {
        if at + MIN_MATCH > end {
            break;
        }
        let key = hash(&input[at..at + MIN_MATCH]);
        let found = longest(input, at, end, head[key], prev, start);
        insert(input, at, end, head, prev, start);
        match found {
            Some((length, offset)) => {
                push(raw, (literal_start, at), length, offset);
                for step in 1..length {
                    insert(input, at + step, end, head, prev, start);
                }
                at += length;
                literal_start = at;
            }
            None => at += 1,
        }
    }
    if literal_start < end {
        push(raw, (literal_start, end), 0, 0);
    }
}

/// Walks one hash chain and keeps the longest copy it finds.
fn longest(
    input: &[u8],
    at: usize,
    end: usize,
    mut candidate: u32,
    prev: &[u32],
    start: usize,
) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    let mut tries = 0;
    while candidate != u32::MAX && tries < MAX_TRIES {
        let position = start + candidate as usize;
        if position >= at {
            break;
        }
        let length = shared(&input[position..end], &input[at..end]);
        if length >= MIN_MATCH && best.is_none_or(|(had, _)| length > had) {
            best = Some((length, at - position));
        }
        candidate = prev[candidate as usize];
        tries += 1;
    }
    best
}

/// Puts `at` at the head of its chain, so later positions can match against it.
fn insert(input: &[u8], at: usize, end: usize, head: &mut [u32], prev: &mut [u32], start: usize) {
    if at + MIN_MATCH > end {
        return;
    }
    let key = hash(&input[at..at + MIN_MATCH]);
    let slot = at - start;
    prev[slot] = head[key];
    head[key] = slot as u32;
}

fn push(raw: &mut Raw, run: (usize, usize), length: usize, offset: usize) {
    raw.runs.push(run);
    raw.lengths.push(length as i64);
    raw.offsets.push(offset as i64);
}

/// Rebuilds the bytes [`tokens_of`] took apart.
///
/// # Errors
///
/// If the three streams disagree on how many tokens there are, or if a copy reaches back further
/// than the output goes, which is what a corrupt or hand written chunk looks like from here.
pub(crate) fn rebuild(
    literals: &[Vec<u8>],
    lengths: &[i64],
    offsets: &[i64],
    total: usize,
) -> Result<Vec<u8>> {
    if literals.len() != lengths.len() || lengths.len() != offsets.len() {
        return Err(Error::internal(format!(
            "a matched chunk has {} literal runs, {} lengths and {} offsets",
            literals.len(),
            lengths.len(),
            offsets.len()
        )));
    }
    let mut out = Vec::with_capacity(total);
    for (index, run) in literals.iter().enumerate() {
        out.extend_from_slice(run);
        let length = usize::try_from(lengths[index])
            .map_err(|_| Error::internal("a negative copy length"))?;
        if length == 0 {
            continue;
        }
        let offset = usize::try_from(offsets[index])
            .map_err(|_| Error::internal("a negative copy offset"))?;
        if offset == 0 || offset > out.len() {
            return Err(Error::internal(format!(
                "a copy reaches {offset} bytes back into {} bytes of output",
                out.len()
            )));
        }
        // Byte at a time because a copy is allowed to overlap itself, which is how a run of one
        // repeated byte is written as a single token.
        let from = out.len() - offset;
        for step in 0..length {
            let byte = out[from + step];
            out.push(byte);
        }
    }
    Ok(out)
}

fn hash(bytes: &[u8]) -> usize {
    let word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    (word.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

fn shared(a: &[u8], b: &[u8]) -> usize {
    let cap = a.len().min(b.len());
    let mut n = 0;
    while n < cap && a[n] == b[n] {
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(input: &[u8]) {
        let tokens = tokens_of(input);
        let owned: Vec<Vec<u8>> = tokens.literals.iter().map(|run| run.to_vec()).collect();
        let back = rebuild(&owned, &tokens.lengths, &tokens.offsets, input.len()).unwrap();
        assert_eq!(back, input, "{} tokens", tokens.lengths.len());
    }

    #[test]
    fn nothing_round_trips() {
        round_trip(b"");
    }

    #[test]
    fn something_with_no_repeats_round_trips() {
        round_trip(b"abcdefghijklmnop");
    }

    #[test]
    fn a_repeat_becomes_a_copy() {
        let input = b"the same sentence twice, the same sentence twice";
        let tokens = tokens_of(input);
        assert!(tokens.lengths.iter().any(|length| *length >= MIN_MATCH as i64), "no copy emitted");
        round_trip(input);
    }

    #[test]
    fn a_run_of_one_byte_is_one_overlapping_copy() {
        // The copy reads bytes this same token is still writing, which is the case the byte at a
        // time loop in rebuild exists for.
        let input = vec![b'x'; 4096];
        round_trip(&input);
        let tokens = tokens_of(&input);
        assert!(tokens.lengths.len() < 8, "{} tokens for one repeated byte", tokens.lengths.len());
    }

    #[test]
    fn something_longer_than_a_segment_round_trips() {
        let mut input = Vec::new();
        while input.len() < SEGMENT * 2 + 1234 {
            input.extend_from_slice(b"http://example.com/some/path?query=value&more=stuff ");
        }
        round_trip(&input);
    }

    #[test]
    fn urls_compress() {
        let mut input = Vec::new();
        for n in 0..4000 {
            input.extend_from_slice(format!("http://example.com/page/{n}?ref=search\n").as_bytes());
        }
        let tokens = tokens_of(&input);
        let literal_bytes: usize = tokens.literals.iter().map(|run| run.len()).sum();
        assert!(literal_bytes * 4 < input.len(), "{literal_bytes} literal of {}", input.len());
        round_trip(&input);
    }

    #[test]
    fn a_copy_that_reaches_too_far_is_refused() {
        let literals = vec![b"ab".to_vec()];
        assert!(rebuild(&literals, &[4], &[99], 6).is_err());
    }

    #[test]
    fn streams_of_different_lengths_are_refused() {
        let literals = vec![b"ab".to_vec()];
        assert!(rebuild(&literals, &[0, 0], &[0, 0], 2).is_err());
    }
}

//! Blocks produced by a real Snappy compressor, decompressed and checked byte for byte.
//!
//! The unit tests in `snappy.rs` build their blocks by hand, which proves the decoder agrees with
//! my reading of the format description. That is worth having and it is not the same as proving
//! the decoder agrees with the compressors whose output it will actually meet, because the two
//! only differ where I misread something, and where I misread something the hand built block is
//! wrong in the same direction as the decoder.
//!
//! So these blocks came out of a real compressor. `tests/data/*.snappy` was produced by Arrow's
//! Snappy codec, which is the same `RawCompress` entry point the Parquet writers call, and the
//! bytes are committed rather than generated at test time because a test that needs a Python
//! environment is a test that does not run.
//!
//! Each case reconstructs its plaintext here rather than committing a second copy of it. The
//! generators are short enough to read and they are transcriptions of the ones that fed the
//! compressor, so a divergence between them fails the test loudly, which is the point.
//!
//! `spec/16-testing.md` calls this the other-writers idea, and at 2d it arrives for the codec
//! before it arrives for the container. The Parquet corpus from DuckDB, pyarrow, parquet-java and
//! parquet-cpp is the same argument one layer up and it lands with the reader.

use rudb_compress::{Codec, snappy};

/// A block from `tests/data`, by name.
macro_rules! block {
    ($name:literal) => {
        include_bytes!(concat!("data/", $name, ".snappy")).as_slice()
    };
}

/// The SplitMix64 stream, one byte per draw, seeded the way the fixture was.
fn random(n: usize) -> Vec<u8> {
    let mut state = 0x5eed_5eed_5eed_5eedu64;
    (0..n)
        .map(|_| {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) as u8
        })
        .collect()
}

/// Checks a real block against the bytes it should hold, through both entry points.
fn check(block: &[u8], expected: &[u8]) {
    let out = snappy::decompress(block).expect("a real Snappy block did not decompress");
    assert_eq!(out.len(), expected.len(), "wrong length");
    assert_eq!(out, expected, "wrong bytes");
    // And through `Codec`, because that is the path a page reader takes and the length check it
    // adds is the one that must not reject a block that is right.
    assert_eq!(Codec::Snappy.decompress(block, expected.len()).unwrap(), expected);
    // The header alone should agree with the whole block, since a page reader may want to size a
    // buffer before it decompresses into it.
    assert_eq!(snappy::decompressed_len(block).unwrap(), expected.len());
}

#[test]
fn an_empty_block_from_a_real_compressor() {
    check(block!("empty"), b"");
}

#[test]
fn a_line_of_text_too_short_to_compress() {
    // 43 bytes in, 45 out. A real compressor emits a block larger than its input when there is
    // nothing to find, and a decoder that assumed otherwise would be wrong on most small pages.
    check(block!("text"), b"the quick brown fox jumps over the lazy dog");
}

#[test]
fn four_kilobytes_of_zeroes() {
    // The overlapping copy at its most extreme. A real compressor writes one zero and then copies
    // at an offset well under the length over and over, so this is the case a `memcpy` based copy
    // gets wrong while still producing four kilobytes of something.
    check(block!("zeros"), &vec![0u8; 4096]);
}

#[test]
fn a_column_of_repeated_urls() {
    // The shape ClickBench's `hits` is full of and the reason Snappy is worth anything on it:
    // 11400 bytes down to 594. Long copies at large offsets, which is the two byte offset form.
    let one = b"https://example.com/path/to/a/page?utm_source=clickbench ";
    let expected: Vec<u8> = one.iter().copied().cycle().take(one.len() * 200).collect();
    check(block!("url"), &expected);
}

#[test]
fn five_thousand_bytes_a_compressor_could_not_shrink() {
    // 5000 in, 5005 out, so this is almost entirely literals and it is the only case here that
    // reaches the multi byte literal length form. Without it the long literal path would be
    // tested only by a block I wrote myself.
    check(block!("random"), &random(5000));
}

#[test]
fn runs_and_literals_alternating() {
    // Long runs of one byte next to a stretch with no repeats in it at all, so the compressor
    // switches between forms repeatedly inside one block.
    let mut expected = Vec::new();
    for _ in 0..20 {
        expected.extend(std::iter::repeat_n(b'A', 100));
        expected.extend((0..=255u8).collect::<Vec<_>>());
        expected.extend(std::iter::repeat_n(b'A', 100));
    }
    check(block!("mixed"), &expected);
}

#[test]
fn thirty_runs_of_three_hundred_bytes_each() {
    // Every run is longer than the 64 byte cap on a single copy, so each one is several copies
    // chained, which is where an off by one in the copy length shows up as a shifted boundary
    // rather than as a wrong length.
    let mut expected = Vec::new();
    for i in 0..30u8 {
        expected.extend(std::iter::repeat_n(i % 7, 300));
    }
    check(block!("runs"), &expected);
}

#[test]
fn a_real_block_with_a_byte_changed_is_caught_rather_than_decoded() {
    // Not every corruption is detectable, and Snappy has no checksum of its own, which is why
    // Parquet carries one. What this asserts is the weaker and still useful thing: the decoder
    // does not read outside its input or hang when the bytes stop making sense. Anything that
    // decodes to the right length is a block whose corruption Snappy genuinely cannot see.
    let original = block!("url");
    for at in [1usize, 5, 50, 200, 400, 593] {
        let mut damaged = original.to_vec();
        damaged[at] ^= 0xff;
        match snappy::decompress(&damaged) {
            Ok(out) => {
                assert_eq!(out.len(), 11400, "a block that decoded should be the right size")
            }
            Err(error) => assert!(!error.message().is_empty()),
        }
    }
}

#[test]
fn a_real_block_truncated_anywhere_is_an_error_and_never_a_hang() {
    let original = block!("runs");
    for cut in 1..original.len() {
        let result = snappy::decompress(&original[..cut]);
        // A prefix can only ever produce fewer bytes than the header promised, so every one of
        // these must be refused. The one that would be silently wrong is a short page handed to a
        // reader as if it were whole.
        assert!(result.is_err(), "a block truncated at {cut} decompressed anyway");
    }
}

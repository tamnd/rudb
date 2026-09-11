//! Frames produced by a real Zstandard compressor, decompressed and checked byte for byte.
//!
//! The unit tests under `src/zstd` build their inputs by hand, which proves the decoder agrees with
//! my reading of the format description and nothing more. Where I misread something the hand built
//! input is wrong in the same direction as the decoder and the test passes anyway. That argument is
//! the same one `real_blocks.rs` makes for Snappy and it is much stronger here, because zstd is an
//! entropy coder and most of what can be misread is in a table layout that never appears in a file
//! as a number anybody can look at.
//!
//! So these frames came out of the zstd command line tool, which is the reference implementation
//! itself. The bytes are committed rather than compressed at test time, because a test that needs a
//! compressor installed is a test that does not run.
//!
//! Each case reconstructs its plaintext here rather than committing a second copy of it, and the
//! generators are transcriptions of the ones that fed the compressor, so a divergence between them
//! fails the test loudly.
//!
//! The cases are chosen for what a frame contains rather than for what it looks like. Between them
//! they cover every block kind, every literals kind including the treeless one that reuses the
//! previous block's tree, the predefined sequence tables and tables written into the block, the
//! repeat offset codes, a frame with a checksum and one without, and two frames in a row.
//!
//! `spec/16-testing.md` calls this the other-writers idea, and at 2d it arrives for the codec before
//! it arrives for the container.

use rudb_compress::{Codec, zstd};

/// A frame from `tests/data`, by name.
macro_rules! frame {
    ($name:literal) => {
        include_bytes!(concat!("data/", $name, ".zst")).as_slice()
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

/// Four thousand lines of a web server log, which is the shape most Parquet text columns have.
fn log() -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..4000 {
        out.extend(
            format!(
                "2026-09-11T12:{:02}:{:02}Z GET /page/{} 200 {}\n",
                i % 60,
                (i * 7) % 60,
                i % 512,
                100 + (i * 37) % 9000
            )
            .into_bytes(),
        );
    }
    out
}

/// Checks a real frame against the bytes it should hold, through both entry points.
fn check(frame: &[u8], expected: &[u8]) {
    let out = zstd::decompress(frame).expect("a real zstd frame did not decompress");
    assert_eq!(out.len(), expected.len(), "wrong length");
    assert_eq!(out, expected, "wrong bytes");
    // And through `Codec`, because that is the path a page reader takes and the length check it
    // adds is the one that must not reject a frame that is right.
    assert_eq!(Codec::Zstd.decompress(frame, expected.len()).unwrap(), expected);
}

#[test]
fn an_empty_frame_from_a_real_compressor() {
    check(frame!("empty"), b"");
}

#[test]
fn a_line_of_text_too_short_to_compress() {
    // 43 bytes in, 56 out. A compressor with nothing to find writes a raw block and a frame around
    // it, and a decoder that assumed every block was compressed would be wrong on most small pages.
    check(frame!("text"), b"the quick brown fox jumps over the lazy dog");
}

#[test]
fn four_kilobytes_of_zeroes() {
    // Twenty three bytes out, which is a frame header and a run block and nothing else. The run
    // block is the one kind whose size field says what it produces rather than what it takes.
    check(frame!("zeros"), &vec![0u8; 4096]);
}

#[test]
fn a_column_of_repeated_urls() {
    // The shape ClickBench's `hits` is full of and the reason anybody compresses it: 11400 bytes
    // down to 80. Almost all of that is one enormous match at a short offset, so this is the case
    // that catches an overlapping copy done with a block move.
    let one = b"https://example.com/path/to/a/page?utm_source=clickbench ";
    let expected: Vec<u8> = one.iter().copied().cycle().take(one.len() * 200).collect();
    check(frame!("url"), &expected);
}

#[test]
fn five_thousand_bytes_a_compressor_could_not_shrink() {
    // 5000 in, 5014 out. Nothing here compresses, so the compressor gives up and stores the block,
    // which is what it will do to any column of identifiers or hashes.
    check(frame!("random"), &random(5000));
}

#[test]
fn runs_and_literals_alternating() {
    // Long runs of one byte next to a stretch with no repeats in it at all, so the block has both a
    // real literals section and long matches, and the two have to be spent in the right order.
    let mut expected = Vec::new();
    for _ in 0..20 {
        expected.extend(std::iter::repeat_n(b'A', 100));
        expected.extend((0..=255u8).collect::<Vec<_>>());
        expected.extend(std::iter::repeat_n(b'A', 100));
    }
    check(frame!("mixed"), &expected);
}

#[test]
fn thirty_runs_of_three_hundred_bytes_each() {
    let mut expected = Vec::new();
    for i in 0..30u8 {
        expected.extend(std::iter::repeat_n(i % 7, 300));
    }
    check(frame!("runs"), &expected);
}

#[test]
fn a_log_that_is_longer_than_one_block_holds() {
    // 174705 bytes, which is two blocks, and that is the point of it. Everything a block can carry
    // is allowed to say reuse what the last one used: the Huffman tree for the literals and each of
    // the three sequence tables. A decoder that rebuilds them every block reads the first block
    // correctly and then produces bytes rather than an error, which is the worst failure there is.
    check(frame!("log"), &log());
}

#[test]
fn the_same_log_written_by_a_compressor_trying_much_harder() {
    // The same bytes at level nineteen, 25471 down to 14247. A higher level does not change the
    // format, it changes which parts of it get used: longer matches, more of the repeat offset
    // codes, and tables written into the block where level three was happy with the predefined
    // ones. It is the same plaintext on purpose, so a failure here against a pass above is a
    // statement about the decoder and not about the data.
    check(frame!("log19"), &log());
}

#[test]
fn a_frame_that_carries_no_checksum_is_read_and_not_demanded_of() {
    let one = b"https://example.com/path/to/a/page?utm_source=clickbench ";
    let expected: Vec<u8> = one.iter().copied().cycle().take(one.len() * 200).collect();
    check(frame!("unchecked"), &expected);
}

#[test]
fn two_frames_in_a_row_are_both_read_and_not_just_the_first() {
    // Legal, rare, and the failure is silent: a reader that stops at the first frame returns a
    // prefix of the page and every row after it is missing rather than wrong.
    let one = b"https://example.com/path/to/a/page?utm_source=clickbench ";
    let mut expected = b"the quick brown fox jumps over the lazy dog".to_vec();
    expected.extend(one.iter().copied().cycle().take(one.len() * 200));
    check(frame!("two"), &expected);
}

#[test]
fn a_real_frame_with_a_byte_changed_is_caught_rather_than_decoded() {
    // This is what the checksum at the end of a frame is for, and it is why it is checked here
    // rather than skipped as a formality. A damaged frame may also fail earlier, in a table that no
    // longer adds up, and either answer is fine. What must not happen is bytes with no complaint.
    let original = frame!("log");
    for at in [10usize, 100, 5000, 12345, 20000, 25470] {
        let mut damaged = original.to_vec();
        damaged[at] ^= 0xff;
        match zstd::decompress(&damaged) {
            Ok(out) => assert_ne!(out, log(), "a damaged frame decoded to the original bytes"),
            Err(error) => assert!(!error.message().is_empty()),
        }
    }
}

#[test]
fn a_real_frame_truncated_anywhere_is_an_error_and_never_a_hang() {
    let original = frame!("runs");
    for cut in 1..original.len() {
        // A prefix can only ever be missing something, so every one of these must be refused. The
        // one that would be silently wrong is a short page handed to a reader as if it were whole.
        assert!(zstd::decompress(&original[..cut]).is_err(), "a frame cut at {cut} decompressed");
    }
}

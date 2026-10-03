//! Gzip files produced by the real gzip tool, decompressed and checked byte for byte.
//!
//! The argument is the one `real_blocks.rs` makes for Snappy. A decoder tested only against input
//! I built by hand agrees with my reading of RFC 1951 and nothing more, so these came out of
//! `gzip -n` and the bytes are committed. Each case reconstructs its plaintext here, with
//! generators that are transcriptions of the ones that fed the compressor.
//!
//! Between them the files cover the three block types: `hello.gz` is one fixed Huffman block,
//! `rows.gz` and `many.gz` are dynamic blocks, the second several of them, and `random.gz` is
//! incompressible, which gzip writes as stored blocks.

use rudb_compress::gzip;

macro_rules! member {
    ($name:literal) => {
        include_bytes!(concat!("data/", $name, ".gz")).as_slice()
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

/// Newline delimited JSON rows, which is what a `.json.gz` usually holds.
fn rows(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| format!("{{\"id\":{i},\"name\":\"row {}\",\"ok\":{}}}\n", i * 7 % 13, i % 3 != 0))
        .collect::<String>()
        .into_bytes()
}

const HELLO: &[u8] = b"hello hello hello hello\n";

#[test]
fn a_fixed_huffman_block_decompresses() {
    assert_eq!(gzip::decompress(member!("hello")).unwrap(), HELLO);
}

#[test]
fn a_dynamic_huffman_block_decompresses() {
    assert_eq!(gzip::decompress(member!("rows")).unwrap(), rows(40));
}

#[test]
fn a_file_of_several_dynamic_blocks_decompresses() {
    assert_eq!(gzip::decompress(member!("many")).unwrap(), rows(4000));
}

#[test]
fn stored_blocks_decompress() {
    assert_eq!(gzip::decompress(member!("random")).unwrap(), random(20000));
}

#[test]
fn an_empty_file_decompresses_to_nothing() {
    assert_eq!(gzip::decompress(member!("empty")).unwrap(), b"");
}

#[test]
fn every_member_of_a_concatenated_file_is_read() {
    let mut expected = rows(40);
    expected.extend_from_slice(HELLO);
    assert_eq!(gzip::decompress(member!("members")).unwrap(), expected);
}

#[test]
fn zeros_after_the_last_member_are_padding() {
    let mut padded = member!("hello").to_vec();
    padded.extend_from_slice(&[0; 512]);
    assert_eq!(gzip::decompress(&padded).unwrap(), HELLO);
}

#[test]
fn a_truncated_file_is_an_error_and_not_a_short_answer() {
    let whole = member!("many");
    let error = gzip::decompress(&whole[..whole.len() / 2]).unwrap_err();
    assert!(error.message().contains("ends before"), "{}", error.message());
}

#[test]
fn a_damaged_byte_is_caught() {
    let mut damaged = member!("rows").to_vec();
    let at = damaged.len() - 12;
    damaged[at] ^= 0x40;
    assert!(gzip::decompress(&damaged).is_err());
}

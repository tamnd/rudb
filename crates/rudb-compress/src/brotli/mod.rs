//! Brotli, RFC 7932, both ways.
//!
//! A brotli stream is a window size and then meta-blocks. A meta-block is metadata to skip, bytes
//! stored as they are, or a compressed run of commands, each some literals and then a copy from
//! earlier in the output or from a static dictionary of English and other words. The literals are
//! Huffman coded with a code picked by the last two bytes, so a meta-block can carry up to 256
//! literal codes, and the codes for literals, commands and distances can each change part way
//! through it.
//!
//! [`decompress`] reads all of that, since the pin's Parquet writer and everybody else's write
//! brotli at quality 11, which uses every part of the format. [`compress`] writes the plainest
//! streams the format allows, with matches from the same hash chains [`crate::zstd`] uses. Those
//! come out larger than libbrotli's at quality 11, which spends far longer on a page than a
//! Parquet writer can afford, and any reader reads them.
//!
//! The pin writes `BrotliEncoderCompress` at quality 11 with a window of 22 bits and ignores
//! `COMPRESSION_LEVEL`, which this writer ignores as well.

mod context;
mod decode;
mod dictionary;
mod encode;

use rudb_common::Result;

/// Compresses `input` into one brotli stream.
#[must_use]
pub fn compress(input: &[u8]) -> Vec<u8> {
    encode::compress(input)
}

/// Decompresses the brotli stream `input` starts with.
///
/// # Errors
///
/// If the stream is malformed, refers to something it does not hold, or ends before its last
/// meta-block does.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    decode::decompress_onto(input, &mut out)?;
    Ok(out)
}

/// Decompresses the brotli stream `input` starts with onto the end of `out`.
///
/// # Errors
///
/// As for [`decompress`].
pub fn decompress_onto(input: &[u8], out: &mut Vec<u8>) -> Result<()> {
    decode::decompress_onto(input, out)
}

#[cfg(test)]
mod tests {
    use super::{compress, decompress, decompress_onto};

    /// Bytes that repeat at a few distances, with some that do not.
    fn sample(len: usize, seed: u32) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|at| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                if (state >> 16).is_multiple_of(5) {
                    (state >> 8) as u8
                } else {
                    (at % 251 / 3) as u8
                }
            })
            .collect()
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex"))
            .collect()
    }

    #[test]
    fn what_is_compressed_comes_back() {
        for len in [0, 1, 3, 4, 5, 17, 100, 1000, 1008, 1009, 65_536, 262_144, 262_145, 700_000] {
            let input = sample(len, len as u32);
            let stream = compress(&input);
            assert_eq!(decompress(&stream).expect("decompresses"), input, "{len}");
        }
        let text =
            b"it was the best of times, it was the worst of times, it was the age of ".repeat(400);
        let stream = compress(&text);
        assert!(stream.len() < text.len() / 20, "{}", stream.len());
        assert_eq!(decompress(&stream).expect("decompresses"), text);
    }

    #[test]
    fn bytes_that_do_not_compress_are_stored() {
        let mut state = 9_u64;
        let noise: Vec<u8> = (0..300_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let stream = compress(&noise);
        assert!(stream.len() < noise.len() + 64, "{}", stream.len());
        assert_eq!(decompress(&stream).expect("decompresses"), noise);
    }

    #[test]
    fn many_small_inputs_come_back() {
        for round in 0..300_u32 {
            let len = (round as usize * 37) % 3000;
            let mut input = sample(len, round);
            // Some with few distinct bytes, so the codes are simple ones.
            if round % 3 == 0 {
                input.iter_mut().for_each(|byte| *byte %= (round % 5 + 1) as u8);
            }
            let stream = compress(&input);
            let mut out = vec![7];
            decompress_onto(&stream, &mut out).expect("decompresses");
            assert_eq!(out[1..], input[..], "{round}");
        }
    }

    #[test]
    fn libbrotli_streams_read_back() {
        // What libbrotli makes of these at quality 11, through pyarrow, which codes the text with
        // words from the dictionary.
        let text =
            b"The quick brown fox jumps over the lazy dog. The five boxing wizards jump quickly.";
        let stream = hex(LIBBROTLI_TEXT);
        assert_eq!(decompress(&stream).expect("decompresses"), text);
        let rows: Vec<u8> =
            (0..60).flat_map(|n| format!("{n},{},name{}\n", n * n, n % 13).into_bytes()).collect();
        let stream = hex(LIBBROTLI_ROWS);
        assert_eq!(decompress(&stream).expect("decompresses"), rows);
    }

    #[test]
    fn a_broken_stream_is_refused() {
        let stream = compress(&sample(5000, 3));
        for cut in [0, 1, stream.len() / 2, stream.len() - 1] {
            assert!(decompress(&stream[..cut]).is_err(), "{cut}");
        }
        // The large window extension.
        assert!(decompress(&[0x11, 0x03]).is_err());
        let error = decompress(&[]).unwrap_err();
        assert!(error.message().contains("Brotli Decompression failure"), "{}", error.message());
    }

    const LIBBROTLI_TEXT: &str = concat!(
        "1b5100a88d94abed6841a5dbbcde1d97e76f482a44833a32c121070e2d49833c203c6cc02906c3b8",
        "4edf780b29809f2c08ef82c3a5fd6174418c8c78b0723b5eb61e2b9c361001",
    );
    const LIBBROTLI_ROWS: &str = concat!(
        "1b1b03a08cd456371dd944ad001323d31d6c9e869240909da473bcb52dd8e1664f0a22278f4d9ad7",
        "c7e283e3d022d1f1a76f280cfc0ac14e123ca5f1af66302a36d09411744a2cfc651de2721ec7bfc7",
        "02abb299f1108d0edb75e49f42131f9c03694862a26ddcf33cb46c7c849750c15516c374f4395f5d",
        "d418fd2263368b06d13405d9342d56df74d873d3e3ccd685af6a4d7cf86e81254b46c7bde5e4c499",
        "94c68fc46733dbb3f3e64edbee73c4a7b8c854e1601c897263adf3fcccf91d53062a8d4892b13655",
        "efdcdde5dfab1f72719d82a66a1d4229f611b4e53e0d5d7d1f43cff4197469f68464876dca6f5c76",
        "2cfef611",
    );
}

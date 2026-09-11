//! Block compression codecs, written rather than depended on.
//!
//! Rank 1 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! Rank 1 and not higher because a codec turns bytes into bytes. It knows nothing about a vector, a
//! page or a column, and the thing that wants a decompressed Parquet page is at rank 5 with several
//! layers in between that are free to not know this crate exists.
//!
//! # Why this is written and not a dependency
//!
//! `spec/18-package-layout.md` is unambiguous that the published workspace has zero external
//! dependencies. That rule is not decoration and it is not about compile times. An embedded
//! database is a thing other people's binaries contain, and every crate in the tree is a crate
//! their security team has to account for, their licence audit has to clear and their supply chain
//! has to trust. Snappy is a few hundred lines. Those lines are cheaper than the conversation.
//!
//! # What is here
//!
//! Snappy in [`snappy`] and Zstandard in [`zstd`], decompression only. Snappy is what a Parquet
//! writer emits when nobody chose, and zstd is what somebody chose, which between them is most of
//! the Parquet anybody has.
//!
//! The two are not the same size of problem. Snappy is a few hundred lines of copy instructions and
//! zstd is a few thousand, because zstd carries a Huffman coder and an arithmetic coder and most of
//! the work is in them rather than in the format around them.
//!
//! Decompression only because reading comes before writing in this project. The write path is M2m
//! and it is where a compressor belongs, since a compressor that nothing writes with is a
//! compressor nothing tests.
//!
//! # What is not here yet
//!
//! gzip, LZ4 and brotli. `spec/engine/05-scan.md` defers them by name at 2d. A file in one of them
//! should say so clearly rather than be read slowly by an untested decoder.

#![deny(unsafe_code)]

pub mod snappy;
pub mod zstd;

use rudb_common::Result;

/// A block compression codec, named the way Parquet names them.
///
/// The numbers are Parquet's `CompressionCodec` enum, so a codec read out of a file's metadata is
/// this type and not an integer that has to be remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Codec {
    /// No compression. The page is its own decompressed form.
    #[default]
    Uncompressed,
    /// Snappy, which is what almost every writer emits.
    Snappy,
    /// Gzip. Not implemented.
    Gzip,
    /// LZO. Not implemented, and effectively extinct in files written this decade.
    Lzo,
    /// Brotli. Not implemented.
    Brotli,
    /// LZ4, the pre-2.9.0 framing that turned out to be ambiguous between writers. Not
    /// implemented, and the reason `Lz4Raw` exists.
    Lz4,
    /// Zstd, which is what a writer emits when somebody chose.
    Zstd,
    /// LZ4 raw blocks, the framing that replaced `Lz4`. Not implemented.
    Lz4Raw,
}

impl Codec {
    /// The codec Parquet's metadata means by `code`.
    ///
    /// # Errors
    ///
    /// If the number is not one Parquet defines. A file claiming a codec that does not exist is a
    /// corrupt file, and guessing at it produces a wrong answer rather than an error.
    pub fn from_parquet(code: i32) -> Result<Self> {
        match code {
            0 => Ok(Self::Uncompressed),
            1 => Ok(Self::Snappy),
            2 => Ok(Self::Gzip),
            3 => Ok(Self::Lzo),
            4 => Ok(Self::Brotli),
            5 => Ok(Self::Lz4),
            6 => Ok(Self::Zstd),
            7 => Ok(Self::Lz4Raw),
            other => Err(rudb_common::Error::invalid_input(format!(
                "compression codec {other} is not one Parquet defines"
            ))),
        }
    }

    /// What this codec is called, in the spelling the Parquet specification uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Uncompressed => "UNCOMPRESSED",
            Self::Snappy => "SNAPPY",
            Self::Gzip => "GZIP",
            Self::Lzo => "LZO",
            Self::Brotli => "BROTLI",
            Self::Lz4 => "LZ4",
            Self::Zstd => "ZSTD",
            Self::Lz4Raw => "LZ4_RAW",
        }
    }

    /// Whether this build can decompress it.
    #[must_use]
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Uncompressed | Self::Snappy | Self::Zstd)
    }

    /// Decompresses `input`, which is expected to produce exactly `expected` bytes.
    ///
    /// The expected length is not a hint. Every caller this has, Parquet included, records the
    /// uncompressed page length in metadata next to the compressed one, and a decompressed page
    /// that does not match it means the page and the metadata disagree. Carrying on from there is
    /// how a reader produces a wrong answer instead of an error.
    ///
    /// # Errors
    ///
    /// If the codec is not implemented, if the input is malformed, or if the result is not
    /// `expected` bytes long.
    pub fn decompress(self, input: &[u8], expected: usize) -> Result<Vec<u8>> {
        let out = match self {
            Self::Uncompressed => input.to_vec(),
            Self::Snappy => snappy::decompress(input)?,
            Self::Zstd => zstd::decompress(input)?,
            other => {
                return Err(rudb_common::Error::not_implemented(format!(
                    "the {} codec is not implemented, only UNCOMPRESSED, SNAPPY and ZSTD are",
                    other.name()
                )));
            }
        };
        if out.len() == expected {
            Ok(out)
        } else {
            Err(rudb_common::Error::io(format!(
                "the metadata says this {} block is {expected} bytes and it decompressed to {}",
                self.name(),
                out.len()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Codec;

    #[test]
    fn the_codec_numbers_are_the_ones_parquet_writes() {
        assert_eq!(Codec::from_parquet(0).unwrap(), Codec::Uncompressed);
        assert_eq!(Codec::from_parquet(1).unwrap(), Codec::Snappy);
        assert_eq!(Codec::from_parquet(6).unwrap(), Codec::Zstd);
        assert_eq!(Codec::from_parquet(7).unwrap(), Codec::Lz4Raw);
    }

    #[test]
    fn a_codec_number_that_does_not_exist_is_an_error_and_not_a_guess() {
        let error = Codec::from_parquet(9).unwrap_err();
        assert!(error.message().contains("not one Parquet defines"), "{}", error.message());
    }

    #[test]
    fn a_codec_we_cannot_read_says_so_by_name() {
        let error = Codec::Gzip.decompress(&[0], 1).unwrap_err();
        assert!(error.message().contains("GZIP"), "{}", error.message());
    }

    #[test]
    fn an_uncompressed_block_comes_back_as_itself() {
        assert_eq!(Codec::Uncompressed.decompress(b"hello", 5).unwrap(), b"hello");
    }

    #[test]
    fn a_block_that_is_not_the_length_the_metadata_claims_is_an_error() {
        // The case this catches is the one that matters: the bytes decompressed fine, so nothing
        // downstream would have noticed, and the answer would have been quietly short.
        let error = Codec::Uncompressed.decompress(b"hello", 6).unwrap_err();
        assert!(error.message().contains("6 bytes"), "{}", error.message());
        assert!(error.message().contains("decompressed to 5"), "{}", error.message());
    }

    #[test]
    fn what_this_build_can_read_is_answerable_without_trying_it() {
        assert!(Codec::Snappy.is_supported());
        assert!(Codec::Uncompressed.is_supported());
        assert!(Codec::Zstd.is_supported());
        assert!(!Codec::Gzip.is_supported());
    }
}

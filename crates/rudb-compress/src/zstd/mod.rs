//! Zstandard, decompression only.
//!
//! The first codec here after Snappy, and the one that changes what files this project can read.
//! Snappy is what a Parquet writer emits when nobody chose, and zstd is what somebody chose: every
//! writer worth the name defaults to it now, and a Parquet file written in the last few years is
//! more likely to be zstd than anything else.
//!
//! # What a frame is
//!
//! A header, a run of blocks, and optionally four bytes of checksum. A block is raw bytes, one byte
//! repeated, or a compressed block, and a compressed block is a pile of literals with an entropy
//! coded description of how to spend them. The literals are Huffman coded and everything else is
//! coded with finite state entropy, which is an arithmetic coder in the shape of a lookup table.
//! Those two live in the `huffman` and `fse` modules, the block is in the `block` module, and this
//! file is the frame around them.
//!
//! # Why this is written and not a dependency
//!
//! The same reason as Snappy, which `lib.rs` sets out, and it costs more here. Snappy is a few
//! hundred lines and this is a few thousand, because zstd is a real entropy coder and the entropy
//! coder is most of it. That is still the right trade for an embedded database: a crate in this
//! tree is a crate somebody else's security team has to account for, and the one dependency
//! everybody reaches for here is a C library with a decade of CVEs behind it, wrapped.
//!
//! What is given up is speed. The reference decoder decodes four Huffman streams and two FSE states
//! in parallel with hand scheduled loads and no bounds checks, and this decodes them in the order
//! the format describes. The format's own parallelism is kept, because it is structural: four
//! literal streams stay four streams and the interleaved states stay interleaved. What is not here
//! is the assembly level work, and `spec/engine/05-scan.md` is the place that will say when the
//! difference starts showing up in a scan rather than in a microbenchmark.
//!
//! # What is not here
//!
//! Dictionaries, because Parquet does not use them and a dictionary is a second format. A frame
//! that names one is refused by name rather than decoded without it, since decoding without it
//! produces bytes rather than an error.

pub(crate) mod bits;
pub(crate) mod block;
pub(crate) mod checksum;
pub(crate) mod fse;
pub(crate) mod huffman;

use rudb_common::{Error, Result};

/// The four bytes at the front of every zstd frame.
const MAGIC: u32 = 0xFD2F_B528;

/// The range of magic numbers a frame nothing has to understand uses.
const SKIPPABLE: u32 = 0x184D_2A50;

/// The most a single frame is allowed to produce, which is a sanity limit rather than a format one.
///
/// A Parquet page is at most a few megabytes and a frame that says it holds a terabyte is either
/// corrupt or hostile. Refusing it is the difference between an error and a machine that is out of
/// memory while somebody watches a query.
const CEILING: u64 = 1 << 34;

/// Decompresses every frame in `input`, concatenated.
///
/// More than one frame is legal and rare. Parquet writes one frame per page, but nothing says a
/// page is one frame, and a reader that stops at the first one would read a fraction of a page and
/// call it the page.
///
/// # Errors
///
/// If the input is not zstd, if it uses a dictionary, if a frame is malformed, or if a frame's
/// checksum does not match what came out of it.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        rest = frame(rest, &mut out)?;
    }
    Ok(out)
}

/// Decompresses one frame onto the end of `out` and gives back whatever follows it.
fn frame<'a>(input: &'a [u8], out: &mut Vec<u8>) -> Result<&'a [u8]> {
    let magic = word(input, 0, 4)? as u32;
    if magic & 0xFFFF_FFF0 == SKIPPABLE {
        // A frame carrying something that is not compressed data, for a program that knows what it
        // put there. Nothing here does, and skipping it is what the format asks of everybody else.
        let size = word(input, 4, 4)? as usize;
        return input
            .get(8 + size..)
            .ok_or_else(|| Error::io("a zstd skippable frame longer than the bytes it is in"));
    }
    if magic != MAGIC {
        return Err(Error::io(format!(
            "these bytes start with {magic:#010x} where a zstd frame starts with {MAGIC:#010x}"
        )));
    }
    let head = Header::read(&input[4..])?;
    if head.dictionary != 0 {
        return Err(Error::not_implemented(format!(
            "this zstd frame was written against dictionary {}, and dictionaries are a second \
             format that Parquet does not use",
            head.dictionary
        )));
    }
    if head.content.is_some_and(|size| size > CEILING) {
        return Err(Error::io("a zstd frame claiming to hold more than this will decompress"));
    }
    let from = out.len();
    if let Some(size) = head.content {
        // Capped, because the size in the header is the frame's claim about itself and a claim is
        // not a reason to ask the allocator for sixteen gigabytes before reading a byte.
        out.reserve(size.min(1 << 26) as usize);
    }

    let mut rest = &input[4 + head.width..];
    let mut carried = block::Carried::default();
    loop {
        let header = word(rest, 0, 3)? as u32;
        let last = header & 1 == 1;
        let kind = (header >> 1) & 3;
        let size = (header >> 3) as usize;
        rest = &rest[3..];
        // A run block is the one kind whose size field is what it produces rather than what it
        // takes, because what it takes is always the one byte it repeats.
        let stored = if kind == 1 { 1 } else { size };
        let body = rest
            .get(..stored)
            .ok_or_else(|| Error::io("a zstd block longer than the frame it is in"))?;
        match kind {
            0 => out.extend_from_slice(body),
            1 => out.resize(out.len() + size, body[0]),
            2 => block::decompress(body, from, out, &mut carried)?,
            _ => return Err(Error::io("a zstd block of the reserved kind")),
        }
        rest = &rest[stored..];
        if last {
            break;
        }
        if (out.len() - from) as u64 > CEILING {
            return Err(Error::io("a zstd frame that has decompressed further than this allows"));
        }
    }

    if let Some(size) = head.content {
        if (out.len() - from) as u64 != size {
            return Err(Error::io(format!(
                "a zstd frame that says it holds {size} bytes and produced {}",
                out.len() - from
            )));
        }
    }
    if head.checked {
        let written = word(rest, 0, 4)? as u32;
        let found = checksum::xxh64(&out[from..]) as u32;
        if written != found {
            return Err(Error::io(format!(
                "a zstd frame whose checksum is {written:#010x} and whose contents hash to \
                 {found:#010x}"
            )));
        }
        rest = &rest[4..];
    }
    Ok(rest)
}

/// What a frame header says about itself.
#[derive(Debug)]
struct Header {
    /// How many bytes the header took, the descriptor included.
    width: usize,
    /// The dictionary it was written against, or zero for none.
    dictionary: u32,
    /// How many bytes it holds, when it says.
    content: Option<u64>,
    /// Whether four bytes of checksum follow the last block.
    checked: bool,
}

impl Header {
    /// Reads a frame header, which starts after the magic number.
    fn read(input: &[u8]) -> Result<Self> {
        let &descriptor = input
            .first()
            .ok_or_else(|| Error::io("a zstd frame with no header after its magic number"))?;
        if descriptor & 0x08 != 0 {
            return Err(Error::io("a zstd frame header using a reserved bit"));
        }
        let content_width = match descriptor >> 6 {
            0 => usize::from(descriptor & 0x20 != 0),
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let dictionary_width = match descriptor & 0x03 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        // The window descriptor is absent when the frame is one segment, because then the window is
        // the whole content and the content size is written instead.
        let window_width = usize::from(descriptor & 0x20 == 0);
        let mut at = 1 + window_width;
        let dictionary = word(input, at, dictionary_width)? as u32;
        at += dictionary_width;
        let content = if content_width == 0 {
            None
        } else {
            // Two bytes mean the size less 256, because a frame that small would not have bothered
            // writing a size at all.
            let size = word(input, at, content_width)?;
            Some(if content_width == 2 { size + 256 } else { size })
        };
        at += content_width;
        Ok(Self { width: at, dictionary, content, checked: descriptor & 0x04 != 0 })
    }
}

/// `n` bytes at `at`, little endian, refusing rather than padding when they are not there.
fn word(input: &[u8], at: usize, n: usize) -> Result<u64> {
    if n == 0 {
        return Ok(0);
    }
    let bytes = input
        .get(at..at + n)
        .ok_or_else(|| Error::io("a zstd frame that ends in the middle of a header"))?;
    let mut word = 0;
    for (step, &byte) in bytes.iter().enumerate() {
        word |= u64::from(byte) << (8 * step);
    }
    Ok(word)
}

#[cfg(test)]
mod tests {
    use super::{Header, decompress};

    /// A frame with one raw block in it, built by hand so the frame layer is tested without the
    /// entropy coders underneath it.
    fn raw(payload: &[u8], checked: bool) -> Vec<u8> {
        let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD];
        // Single segment, so no window descriptor, and a one byte content size.
        frame.push(0x20 | if checked { 0x04 } else { 0 });
        frame.push(payload.len() as u8);
        let header = (payload.len() as u32) << 3 | 1;
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(payload);
        if checked {
            let sum = super::checksum::xxh64(payload) as u32;
            frame.extend_from_slice(&sum.to_le_bytes());
        }
        frame
    }

    #[test]
    fn a_frame_of_one_raw_block_comes_back_as_itself() {
        assert_eq!(decompress(&raw(b"hello", false)).unwrap(), b"hello");
    }

    #[test]
    fn a_frame_that_says_what_it_hashes_to_is_checked_against_it() {
        assert_eq!(decompress(&raw(b"hello", true)).unwrap(), b"hello");
        let mut broken = raw(b"hello", true);
        let last = broken.len() - 1;
        broken[last] ^= 0xFF;
        let error = decompress(&broken).unwrap_err();
        assert!(error.message().contains("whose checksum is"), "{}", error.message());
    }

    #[test]
    fn two_frames_one_after_the_other_are_both_read() {
        let mut both = raw(b"one", false);
        both.extend_from_slice(&raw(b"two", false));
        assert_eq!(decompress(&both).unwrap(), b"onetwo");
    }

    #[test]
    fn a_frame_that_is_not_zstd_says_so_with_the_bytes_it_found() {
        let error = decompress(b"not a frame at all").unwrap_err();
        assert!(error.message().contains("where a zstd frame starts"), "{}", error.message());
    }

    #[test]
    fn a_frame_written_against_a_dictionary_is_refused_and_not_decoded_without_one() {
        // Descriptor: single segment, one byte content size, a one byte dictionary id.
        let frame = [0x28, 0xB5, 0x2F, 0xFD, 0x21, 0x07, 0x05];
        let error = decompress(&frame).unwrap_err();
        assert!(error.message().contains("dictionary 7"), "{}", error.message());
    }

    #[test]
    fn a_skippable_frame_is_stepped_over_rather_than_read() {
        let mut both = vec![0x50, 0x2A, 0x4D, 0x18, 0x03, 0x00, 0x00, 0x00, 1, 2, 3];
        both.extend_from_slice(&raw(b"after", false));
        assert_eq!(decompress(&both).unwrap(), b"after");
    }

    #[test]
    fn a_two_byte_content_size_is_the_number_plus_the_two_hundred_and_fifty_six_it_saves() {
        let head = Header::read(&[0x40, 0x00, 0x00, 0x00]).unwrap();
        assert_eq!(head.content, Some(256));
        assert_eq!(head.width, 4, "the descriptor, the window byte and the two size bytes");
    }

    #[test]
    fn a_frame_that_produces_a_different_length_from_the_one_it_promised_is_an_error() {
        let mut frame = raw(b"hello", false);
        frame[5] = 6;
        let error = decompress(&frame).unwrap_err();
        assert!(error.message().contains("says it holds 6 bytes"), "{}", error.message());
    }
}

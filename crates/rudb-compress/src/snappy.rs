//! Snappy, the raw block format, decompression only.
//!
//! This is the format Parquet means by `SNAPPY`: a bare block, not the framed stream format with
//! the `sNaPpY` magic and the per chunk CRCs. A Parquet page is a block, its length is in the
//! metadata, and its checksum is Parquet's own, so the framing would be a second copy of things
//! the container already does.
//!
//! # The format, since the decoder is short enough that the format is the documentation
//!
//! A block is a varint holding the uncompressed length, then a sequence of elements until the
//! input runs out. Each element starts with a tag byte whose low two bits say which of four kinds
//! it is.
//!
//! A literal, tag `00`, is bytes copied straight through. The tag's upper six bits are the length
//! minus one when that fits in six bits. Values 60 through 63 mean the length minus one is instead
//! in the next one, two, three or four bytes, little-endian.
//!
//! The other three are copies, which say go back `offset` bytes in what has been produced so far
//! and take `length` bytes from there. Tag `01` packs an eleven bit offset and a length of four to
//! eleven into the tag plus one byte. Tag `10` and tag `11` take the length from the tag's upper
//! six bits and the offset from the next two or four bytes.
//!
//! # The copy that overlaps itself is the whole trick
//!
//! A copy may reach back fewer bytes than it produces. A run of two hundred zeroes is one zero
//! literal and a copy of length 199 at offset 1, and it works because the bytes being read are
//! bytes this same copy just wrote. So a copy is not a `memcpy` and cannot be written as one, it is
//! a byte at a time or, as here, in chunks no longer than the offset so that no chunk overlaps its
//! own destination. Getting this wrong produces plausible output rather than a crash, which is why
//! there is a test for it that spells out the expected bytes.
//!
//! # Why the output is sized once
//!
//! The uncompressed length is the first thing in the block, so the output is sized at its final
//! size and written into by position. That removes every growth check from the inner loop, and
//! more usefully it turns a corrupt length into an error at the first element that would run past
//! the end rather than into a `Vec` that quietly grows to whatever a corrupt file asked for.
//!
//! Sized, not allocated. [`decompress_into`] takes a buffer the caller keeps across blocks, and on
//! a page of any real size that is worth more than anything in the inner loop, for the reason
//! written out there.

use rudb_common::{Error, Result};

/// Decompresses a Snappy block that is known to hold `limit` bytes at the outside.
///
/// The limit is the guard against a corrupt length, and it is a parameter rather than a constant
/// because the caller has a better number than this module can guess. Every container Snappy
/// arrives in records the uncompressed size next to the compressed one, so a Parquet page passes
/// the size its own header claims and a block that asks for more than that is refused before
/// anything is allocated for it. It was a constant of sixty four megabytes until a ClickBench
/// sample of ten million rows turned out to hold a page of seventy five, and a file other readers
/// read is not a file this one gets to call implausible.
///
/// # Errors
///
/// If the block is truncated, if a tag is malformed, if a copy reaches back further than the
/// output produced so far, if the length is above `limit`, or if the elements produce a different
/// number of bytes than the header said they would. Every one of those is a corrupt block, and
/// every one of them is a case where carrying on produces bytes that look like data.
pub fn decompress(input: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let len = decompress_into(input, limit, &mut out)?;
    out.truncate(len);
    Ok(out)
}

/// Decompresses a block into a buffer the caller keeps, and says how many bytes of it were filled.
///
/// The answer is `out[..n]`, and `out` is allowed to come back longer than `n`. That is the point
/// of it. A reader walking a column chunk decompresses a few thousand pages, and a page that
/// allocates its own output asks the allocator for a fresh region every time, which on any
/// interesting page size means fresh pages from the kernel and a fault on the first write to each
/// one. Measured with `cargo xtask compress` on `gamingpc-wsl`, sixteen megabytes through the
/// decoder either way:
///
/// ```text
/// payload                block    fresh buffer    buffer kept    (MiB/s out)
/// incompressible            4K           37594          69764
/// incompressible            1M           30349          43937
/// a hits.parquet page      15K            2280           2323
/// ```
///
/// The incompressible rows are a `memcpy` and nothing else, so they are the page fault cost on its
/// own with no decoding in front of it to hide it, and it is worth 45 percent at a four kilobyte
/// page. A real page does work per byte as well, so the same saving is a smaller share of it, and
/// end to end on a ten million row ClickBench scan keeping the buffer took decompression from
/// 1344.7 MB/s to 1434.7 MB/s.
///
/// The buffer is only ever grown, never shrunk and never re-zeroed, because zeroing the tail again
/// on every page would be most of what keeping it saved. So the length is the high water mark of
/// every block that has been through it and the length of this one is the return value.
///
/// # Errors
///
/// The same set as [`decompress`], which is the same function with the buffer supplied.
pub fn decompress_into(input: &[u8], limit: usize, out: &mut Vec<u8>) -> Result<usize> {
    let (expected, header) = read_varint(input)?;
    if expected > limit {
        return Err(Error::io(format!(
            "this block claims to hold {expected} bytes, and what it came from says {limit}"
        )));
    }
    if out.is_empty() {
        // A buffer that has never held anything comes from the allocator already zeroed, and on
        // any size worth talking about that means pages the kernel has not had to write to yet.
        // Growing an empty `Vec` by hand instead would write those zeroes a second time.
        *out = vec![0u8; expected];
    } else if out.len() < expected {
        out.resize(expected, 0);
    }
    decompress_within(input, expected, header, out)?;
    Ok(expected)
}

/// How many bytes a short literal is copied as, regardless of how many it holds.
///
/// Most literals in a real block are small. A page of integers that barely vary compresses to a
/// long run of short literals, and at that size the copy is not the cost, the length dependent
/// call in front of it is. So a short literal writes a fixed sixteen bytes and lets the next
/// element overwrite whatever it wrote past its own end.
///
/// That is only sound because the output buffer is allocated at its final size up front. The
/// extra bytes land inside it, every byte before `expected` is written by some later element
/// before the block finishes, and the guard on each use is that `pos + WIDE` still fits. Where it
/// does not, which is the last element or two of a block, the exact width path runs instead.
///
/// Sixteen rather than eight because it is one SSE or NEON register, and one register move is one
/// instruction whichever length the literal actually had.
///
/// # Copies too, which took two goes to get right
///
/// The same trick applies to a short copy whose offset is at least sixteen. It was written that
/// way first, measured against the synthetic payloads `cargo xtask compress` used to have, found
/// to lose 19 percent on the run heavy row, and taken out with a comment saying so.
///
/// That measurement was on payloads that do not have a real page's element mix. Over 391 pages of
/// the first two row groups of a ClickBench `hits.parquet`, 72.6 percent of elements are copies,
/// 72.3 percent of those copies are four to seven bytes, 89.9 percent are at most sixteen, only 4
/// percent have an offset under sixteen and only 1.9 percent overlap. A generator that emits long
/// runs at an offset of one produces the opposite of that, and it was the generator being
/// measured rather than the decoder.
///
/// On the real file the wide copy is worth six percent: decompression on a ten million row scan
/// goes from 1434.7 MB/s to 1538.5 MB/s. `crates/rudb-compress/tests/data/hits-page.snappy` is a
/// page out of that file and it is now the first row of the table, so the next person to reach
/// this paragraph has a payload that behaves like the engine's.
const WIDE: usize = 16;

/// How long `input` says it decompresses to, without decompressing it.
///
/// This reads a varint and allocates nothing, so there is no limit on it. A caller that is about
/// to hand the block to [`decompress`] is the one that decides whether the length is one it will
/// pay for.
///
/// # Errors
///
/// If the block does not begin with a well formed varint.
pub fn decompressed_len(input: &[u8]) -> Result<usize> {
    let (len, _) = read_varint(input)?;
    Ok(len)
}

/// The elements of a block whose length has already been read and accepted.
///
/// `out` is at least `expected` bytes long and its contents are whatever the last block through it
/// left behind. That is sound because every byte below `expected` is written by some element
/// before this returns, which is what the count at the end checks, and a block that fails that
/// check returns an error rather than the buffer.
fn decompress_within(input: &[u8], expected: usize, header: usize, out: &mut [u8]) -> Result<()> {
    let out = &mut out[..expected];
    let mut src = header;
    let mut pos = 0usize;

    while src < input.len() {
        let tag = input[src];
        src += 1;
        if tag & 0b11 == 0 {
            // A literal. The length lives in the tag unless the tag says it lives after it.
            let count = usize::from(tag >> 2);
            let len = if count < 60 {
                count + 1
            } else {
                let extra = count - 59;
                let bytes = input
                    .get(src..src + extra)
                    .ok_or_else(|| truncated("a literal length", src, extra, input.len()))?;
                src += extra;
                let mut value = 0usize;
                for (i, &byte) in bytes.iter().enumerate() {
                    value |= usize::from(byte) << (8 * i);
                }
                // Plus one can overflow only if the file claimed a four byte length of all ones,
                // which is a corrupt file rather than a block.
                value.checked_add(1).ok_or_else(|| {
                    Error::io("this block claims a literal longer than memory".to_string())
                })?
            };
            let bytes = input
                .get(src..src + len)
                .ok_or_else(|| truncated("a literal", src, len, input.len()))?;
            if pos + len > expected {
                return Err(overruns("a literal", len, pos, expected));
            }
            if len <= WIDE && pos + WIDE <= expected && src + WIDE <= input.len() {
                // Overruns on purpose. See [`WIDE`].
                out[pos..pos + WIDE].copy_from_slice(&input[src..src + WIDE]);
            } else {
                out[pos..pos + len].copy_from_slice(bytes);
            }
            src += len;
            pos += len;
        } else {
            let (len, offset) = read_copy(input, &mut src, tag)?;
            if offset == 0 {
                return Err(Error::io("this block has a copy with an offset of zero".to_string()));
            }
            if offset > pos {
                return Err(Error::io(format!(
                    "this block has a copy reaching back {offset} bytes with only {pos} produced"
                )));
            }
            if pos + len > expected {
                return Err(overruns("a copy", len, pos, expected));
            }
            let from = pos - offset;
            if len <= WIDE && offset >= WIDE && pos + WIDE <= expected {
                // Overruns on purpose. See [`WIDE`]. The offset is what makes it sound as well as
                // fast: reaching back at least sixteen means the sixteen bytes read end at or
                // before `pos`, so this is a copy of two registers and not an overlapping one.
                //
                // Through a register sized array rather than through `copy_within`, because that
                // one is the overlapping move and the compiler leaves it as a call into libc even
                // where the length is the constant sixteen. On a ClickBench page nine copies in ten
                // land here, so that call was being paid per element: seven million of them for one
                // column of a million rows, against a pair of loads and a pair of stores.
                let mut wide = [0u8; WIDE];
                wide.copy_from_slice(&out[from..from + WIDE]);
                out[pos..pos + WIDE].copy_from_slice(&wide);
            } else if offset >= len {
                // Nothing overlaps, so the two halves can be split apart and copied the non
                // overlapping way, which is the one the compiler inlines. What lands here is the
                // long copy and the one too near the end of the block to write wide.
                //
                // Reaching back at least `len` is what makes the split sound: `from + len` is at
                // most `pos`, so the bytes being read are all on the left of it.
                let (before, after) = out.split_at_mut(pos);
                after[..len].copy_from_slice(&before[from..from + len]);
            } else {
                // The pattern repeats. Write it once, then keep doubling what has been written,
                // because every byte already at `pos` is a byte this copy may read. Doubling
                // rather than stepping by the offset is worth having: a run of 64 identical bytes
                // is offset one, and stepping does 64 one byte moves where doubling does seven.
                out.copy_within(from..pos, pos);
                let mut done = offset;
                while done < len {
                    let chunk = done.min(len - done);
                    out.copy_within(pos..pos + chunk, pos + done);
                    done += chunk;
                }
            }
            pos += len;
        }
    }

    if pos == expected {
        Ok(())
    } else {
        Err(Error::io(format!(
            "this block says it holds {expected} bytes and its elements produced {pos}"
        )))
    }
}

/// Reads a copy element's length and offset, advancing `src` past the bytes it used.
fn read_copy(input: &[u8], src: &mut usize, tag: u8) -> Result<(usize, usize)> {
    match tag & 0b11 {
        // One byte of offset, and three bits of it ride along in the tag. Lengths four to eleven.
        1 => {
            let low = *input
                .get(*src)
                .ok_or_else(|| truncated("a one byte copy offset", *src, 1, input.len()))?;
            *src += 1;
            let len = 4 + usize::from((tag >> 2) & 0b111);
            let offset = (usize::from(tag >> 5) << 8) | usize::from(low);
            Ok((len, offset))
        }
        // Two or four bytes of offset, and the length is the whole top of the tag.
        kind => {
            let width = if kind == 2 { 2 } else { 4 };
            let bytes = input
                .get(*src..*src + width)
                .ok_or_else(|| truncated("a copy offset", *src, width, input.len()))?;
            *src += width;
            let mut offset = 0usize;
            for (i, &byte) in bytes.iter().enumerate() {
                offset |= usize::from(byte) << (8 * i);
            }
            Ok((1 + usize::from(tag >> 2), offset))
        }
    }
}

/// Reads the block's leading varint, returning the length and where the elements start.
///
/// Five bytes at most, because the value is a `u32`. A sixth continuation byte is a corrupt block
/// rather than a large number, and saying so is the difference between an error and a loop that
/// reads until the input ends.
fn read_varint(input: &[u8]) -> Result<(usize, usize)> {
    let mut value = 0u64;
    for (i, &byte) in input.iter().take(5).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            let len = usize::try_from(value)
                .map_err(|_| Error::io("this block's length does not fit in memory".to_string()))?;
            return Ok((len, i + 1));
        }
    }
    Err(Error::io(
        "this block does not begin with a Snappy length, so it is not a Snappy block".to_string(),
    ))
}

fn truncated(what: &str, at: usize, wanted: usize, have: usize) -> Error {
    Error::io(format!(
        "this block ends in the middle of {what}: {wanted} bytes wanted at {at} and it is {have} \
         bytes long"
    ))
}

fn overruns(what: &str, len: usize, pos: usize, expected: usize) -> Error {
    Error::io(format!(
        "{what} of {len} bytes at {pos} runs past the {expected} bytes this block says it holds"
    ))
}

#[cfg(test)]
mod tests {
    use super::decompressed_len;
    use rudb_common::Result;

    /// What the tests here say a block holds, which is more than any of them builds.
    ///
    /// A test that wants to check the limit itself passes its own, and everything else is checking
    /// the elements rather than the guard in front of them.
    const ROOM: usize = 64 << 20;

    fn decompress(input: &[u8]) -> Result<Vec<u8>> {
        super::decompress(input, ROOM)
    }

    /// A literal element holding all of `bytes`, for blocks built by hand.
    fn literal(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(bytes.len() <= 60, "the short form only reaches 60");
        // Tag 00 is a literal, and the low two bits of `len - 1 << 2` are already zero.
        out.push((bytes.len() as u8 - 1) << 2);
        out.extend_from_slice(bytes);
        out
    }

    /// A two byte offset copy, which is the form that can express anything.
    fn copy2(len: usize, offset: usize) -> Vec<u8> {
        assert!((1..=64).contains(&len));
        vec![(((len - 1) as u8) << 2) | 0b10, offset as u8, (offset >> 8) as u8]
    }

    /// A block with `body` in it and a header saying it produces `len` bytes.
    fn block(len: usize, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut value = len;
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn an_empty_block_decompresses_to_nothing() {
        assert_eq!(decompress(&block(0, &[])).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn a_block_that_is_one_literal_is_that_literal() {
        let body = literal(b"hello snappy");
        assert_eq!(decompress(&block(12, &body)).unwrap(), b"hello snappy");
    }

    #[test]
    fn a_copy_repeats_what_came_before_it() {
        // "abcd" then a copy of 4 bytes from 4 back, which is "abcd" again.
        let mut body = literal(b"abcd");
        body.extend(copy2(4, 4));
        assert_eq!(decompress(&block(8, &body)).unwrap(), b"abcdabcd");
    }

    #[test]
    fn a_copy_shorter_than_its_own_length_repeats_the_pattern_it_overlaps() {
        // The case a `memcpy` gets wrong. One 'x', then a copy of 9 bytes from 1 back. Each byte
        // read is a byte this copy wrote, so the answer is ten 'x' and not one 'x' and nine of
        // whatever happened to be in the buffer.
        let mut body = literal(b"x");
        body.extend(copy2(9, 1));
        assert_eq!(decompress(&block(10, &body)).unwrap(), b"xxxxxxxxxx");
    }

    #[test]
    fn an_overlapping_copy_with_a_multi_byte_pattern_repeats_the_whole_pattern() {
        // Offset 3, length 7, so "abc" repeats and the answer is truncated mid pattern. This is
        // the one that catches a chunked copy whose chunking is off by the pattern length.
        let mut body = literal(b"abc");
        body.extend(copy2(7, 3));
        assert_eq!(decompress(&block(10, &body)).unwrap(), b"abcabcabca");
    }

    #[test]
    fn the_one_byte_offset_copy_form_works() {
        // Tag 01, length 4 to 11 in bits 2 to 4, offset's top three bits in bits 5 to 7.
        let mut body = literal(b"abcd");
        // Length 4 is 0 in the length field, offset 4 fits in the one byte entirely.
        body.extend_from_slice(&[0b0000_0001, 4]);
        assert_eq!(decompress(&block(8, &body)).unwrap(), b"abcdabcd");
    }

    #[test]
    fn a_one_byte_offset_copy_uses_the_three_offset_bits_in_its_tag() {
        // Offset 300, which needs the tag's top bits: 300 is 0b1_0010_1100, so the low byte is
        // 0b0010_1100 and the high bit 1 goes in bit 5 of the tag.
        let filler = vec![b'z'; 60];
        let mut body = literal(&filler);
        body.extend(literal(b"abc"));
        // 63 bytes produced, copy 4 from 63 back is the whole thing's start.
        body.extend_from_slice(&[0b0000_0001 | ((63 >> 8) << 5) as u8, 63u8]);
        let out = decompress(&block(67, &body)).unwrap();
        assert_eq!(&out[..60], &filler[..]);
        assert_eq!(&out[60..63], b"abc");
        assert_eq!(&out[63..], b"zzzz");
    }

    #[test]
    fn a_long_literal_takes_its_length_from_the_bytes_after_the_tag() {
        // Tag 60 in the length field means one extra byte holding the length minus one.
        let payload = vec![b'q'; 200];
        let mut body = vec![60 << 2, 199];
        body.extend_from_slice(&payload);
        assert_eq!(decompress(&block(200, &body)).unwrap(), payload);
    }

    #[test]
    fn the_length_can_be_read_without_decompressing() {
        let body = literal(b"hello snappy");
        assert_eq!(decompressed_len(&block(12, &body)).unwrap(), 12);
        // And a multi byte varint, since one byte only reaches 127.
        assert_eq!(decompressed_len(&block(300, &[])).unwrap(), 300);
    }

    #[test]
    fn a_block_whose_elements_produce_less_than_the_header_says_is_an_error() {
        // Not a short answer. This is the shape a truncated page takes and the reader must not be
        // handed four bytes where it asked for eight.
        let body = literal(b"abcd");
        let error = decompress(&block(8, &body)).unwrap_err();
        assert!(error.message().contains("produced 4"), "{}", error.message());
    }

    #[test]
    fn a_block_whose_elements_produce_more_than_the_header_says_is_an_error() {
        let mut body = literal(b"abcd");
        body.extend(literal(b"efgh"));
        let error = decompress(&block(4, &body)).unwrap_err();
        assert!(error.message().contains("runs past"), "{}", error.message());
    }

    #[test]
    fn a_copy_reaching_back_further_than_anything_produced_is_an_error() {
        let mut body = literal(b"abcd");
        body.extend(copy2(4, 99));
        let error = decompress(&block(8, &body)).unwrap_err();
        assert!(error.message().contains("reaching back 99"), "{}", error.message());
    }

    #[test]
    fn a_copy_with_an_offset_of_zero_is_an_error_and_not_an_infinite_pattern() {
        let mut body = literal(b"abcd");
        body.extend(copy2(4, 0));
        let error = decompress(&block(8, &body)).unwrap_err();
        assert!(error.message().contains("offset of zero"), "{}", error.message());
    }

    #[test]
    fn a_literal_that_runs_past_the_end_of_the_block_is_an_error() {
        // The tag says twenty bytes follow and four do.
        let body = vec![19 << 2, b'a', b'b', b'c', b'd'];
        let error = decompress(&block(20, &body)).unwrap_err();
        assert!(error.message().contains("ends in the middle"), "{}", error.message());
    }

    #[test]
    fn a_varint_that_never_terminates_is_not_a_snappy_block() {
        let error = decompress(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80]).unwrap_err();
        assert!(error.message().contains("not a Snappy block"), "{}", error.message());
    }

    #[test]
    fn an_empty_input_is_not_a_snappy_block_either() {
        assert!(decompress(&[]).is_err());
    }

    #[test]
    fn a_corrupt_length_is_refused_rather_than_allocated() {
        // Five 0xff bytes with the top bits set is a varint of about four billion. Without the
        // limit this allocates four gigabytes before discovering the block is nine bytes long.
        let error = decompress(&[0xff, 0xff, 0xff, 0xff, 0x0f]).unwrap_err();
        assert!(error.message().contains("what it came from says"), "{}", error.message());
    }

    #[test]
    fn a_block_larger_than_the_old_constant_is_read_when_its_container_says_so() {
        // A page of seventy five megabytes is what a ten million row ClickBench sample holds, and
        // refusing it was refusing a file that every other reader reads. The block here is one
        // literal of that size, so this is the length check rather than the elements.
        let big = 79_310_925;
        let mut body = vec![63 << 2, 0, 0, 0, 0];
        let len = u32::try_from(big - 1).unwrap();
        body[1..5].copy_from_slice(&len.to_le_bytes());
        body.resize(body.len() + big, b'x');
        assert_eq!(super::decompress(&block(big, &body), big).unwrap().len(), big);
        let error = super::decompress(&block(big, &body), 64 << 20).unwrap_err();
        assert!(error.message().contains("what it came from says"), "{}", error.message());
    }

    #[test]
    fn a_short_element_that_writes_wide_has_its_extra_bytes_overwritten() {
        // The wide write's whole premise. Two four byte literals, the first of which writes
        // sixteen bytes and so scribbles over where the second one goes. If the order or the
        // guard were wrong this would come back as "aaaa" followed by twelve bytes of the first
        // literal's overrun, which is a plausible looking answer and not the right one.
        let mut body = literal(b"aaaa");
        body.extend(literal(b"bbbb"));
        assert_eq!(decompress(&block(8, &body)).unwrap(), b"aaaabbbb");
    }

    #[test]
    fn a_short_copy_that_writes_wide_has_its_extra_bytes_overwritten() {
        // The literal test's premise on the copy path. A four byte copy at an offset of 32 writes
        // sixteen bytes, so it scribbles over the twelve bytes the following literal owns, and the
        // literal has to win. Sixteen bytes of tail rather than four so that `pos + WIDE` still
        // fits at the copy and the wide path is the one actually under test.
        let head: Vec<u8> = (0..32u8).collect();
        let tail = vec![b'z'; 16];
        let mut body = literal(&head);
        body.extend(copy2(4, 32));
        body.extend(literal(&tail));
        let out = decompress(&block(52, &body)).unwrap();
        assert_eq!(&out[..32], &head[..]);
        assert_eq!(&out[32..36], &head[..4]);
        assert_eq!(&out[36..], &tail[..]);
    }

    #[test]
    fn a_short_element_at_the_very_end_of_a_block_takes_the_exact_width_path() {
        // Every length from one to twenty as the final element, which walks the block end across
        // the point where `pos + WIDE` stops fitting. Anything that wrote wide here would be
        // writing past the output buffer, so if the guard is wrong this panics rather than
        // returning something subtly wrong, which is the failure mode to prefer.
        for tail in 1..=20usize {
            let head = vec![b'h'; 40];
            let last = vec![b'z'; tail];
            let mut body = literal(&head);
            body.extend(literal(&last));
            let out = decompress(&block(40 + tail, &body)).unwrap();
            assert_eq!(out.len(), 40 + tail, "tail of {tail}");
            assert_eq!(&out[..40], &head[..], "tail of {tail}");
            assert_eq!(&out[40..], &last[..], "tail of {tail}");
        }
    }

    #[test]
    fn a_short_copy_near_the_end_of_a_block_lands_exactly_where_it_should() {
        // The copy path writes wide at an offset of sixteen or more and exactly below it, so this
        // sweep walks both sides of that line and both sides of the block end guard at once. The
        // answer has to be the same whichever path it took.
        for offset in [1usize, 4, 15, 16, 32] {
            for len in [4usize, 8, 16] {
                let head: Vec<u8> = (0..32u8).collect();
                let mut body = literal(&head);
                body.extend(copy2(len, offset));
                let out = decompress(&block(32 + len, &body)).unwrap();
                assert_eq!(out.len(), 32 + len, "offset {offset} len {len}");
                let mut expected = head.clone();
                for i in 0..len {
                    let byte = expected[32 + i - offset];
                    expected.push(byte);
                }
                assert_eq!(out, expected, "offset {offset} len {len}");
            }
        }
    }

    #[test]
    fn a_buffer_that_already_holds_a_longer_block_does_not_leak_it_into_a_shorter_one() {
        // The whole risk in keeping the buffer. Decompress something long, then something short
        // through the same buffer, and the short answer must be the short answer. If the tail were
        // ever read rather than written this comes back with the first block's bytes on the end,
        // which is a plausible looking answer and the worst kind of wrong.
        let mut buffer = Vec::new();
        let long = vec![b'L'; 300];
        let mut body = vec![60 << 2, 255];
        body.extend_from_slice(&long[..256]);
        body.extend(literal(&long[256..]));
        let len = super::decompress_into(&block(300, &body), ROOM, &mut buffer).unwrap();
        assert_eq!(&buffer[..len], &long[..]);

        let short = literal(b"short");
        let len = super::decompress_into(&block(5, &short), ROOM, &mut buffer).unwrap();
        assert_eq!(&buffer[..len], b"short");
        assert_eq!(len, 5, "the length is the block's and not the buffer's");
        assert!(buffer.len() >= 300, "the buffer kept the room it had");
    }

    #[test]
    fn a_block_refused_for_its_length_leaves_the_buffer_alone() {
        // The limit check runs before anything is sized, so a caller looping over pages with one
        // buffer does not lose it to a page it refused to read.
        let mut buffer = vec![7u8; 64];
        let error =
            super::decompress_into(&[0xff, 0xff, 0xff, 0xff, 0x0f], 16, &mut buffer).unwrap_err();
        assert!(error.message().contains("what it came from says"), "{}", error.message());
        assert_eq!(buffer, vec![7u8; 64]);
    }

    #[test]
    fn a_run_of_a_thousand_bytes_round_trips_through_the_overlapping_copy_path() {
        // Long enough that the chunked copy runs many iterations, which is where an off by one in
        // the chunk arithmetic would show up rather than in the ten byte case above.
        let mut body = literal(b"ab");
        // Copies are at most 64 bytes each, so build the run out of several.
        let mut produced = 2usize;
        while produced < 1000 {
            let len = (1000 - produced).min(64);
            body.extend(copy2(len, 2));
            produced += len;
        }
        let out = decompress(&block(1000, &body)).unwrap();
        assert_eq!(out.len(), 1000);
        assert!(out.chunks(2).all(|pair| pair == b"ab"), "the pattern did not hold");
    }
}

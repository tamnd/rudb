//! A compressed block, which is a pile of literals and a list of instructions for spending them.
//!
//! Every compressed block is the same two sections. The literals are the bytes that could not be
//! expressed as a copy of something earlier in the frame, stored together and entropy coded on
//! their own, and the sequences are triples of how many literals to emit, how far back to look and
//! how much to copy from there. Nothing else. The whole of LZ77 and the whole of zstd's entropy
//! coding meet in one loop at the bottom of this file.
//!
//! Keeping the literals together rather than interleaving them with the copies is what lets them be
//! Huffman coded as one distribution, and it is the main structural difference between this format
//! and the ones that came before it.

use rudb_common::{Error, Result};

use super::bits::Backward;
use super::fse::Table;
use super::huffman::Huffman;

/// What a decoder carries from one block to the next inside a frame.
///
/// All four tables can be reused by a later block rather than described again, which is most of why
/// zstd stays small on data that arrives in many small blocks. It also means a block is not
/// independently decodable, which is the trade: a frame is the unit you can start from, not a
/// block.
#[derive(Debug, Default)]
pub(crate) struct Carried {
    literals: Option<Huffman>,
    lengths: Option<Table>,
    offsets: Option<Table>,
    matches: Option<Table>,
    /// The three most recent offsets, which a later block may name rather than repeat.
    recent: Recent,
}

/// The three offsets a sequence can name instead of writing one out.
///
/// They start at one, four and eight at the top of a frame and they are not reset at a block
/// boundary, which is easy to get wrong and produces bytes rather than an error when you do: the
/// first block decodes perfectly and the second one starts copying from the wrong place.
#[derive(Debug)]
struct Recent([u64; 3]);

impl Default for Recent {
    fn default() -> Self {
        Self([1, 4, 8])
    }
}

/// The literal length codes, as a base and a number of extra bits.
const LENGTH_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
const LENGTH_BITS: [u32; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];

/// The match length codes, which start at three because a shorter copy is not worth coding.
const MATCH_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];
const MATCH_BITS: [u32; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// The distribution a block uses when it says it has nothing better.
///
/// Worth having rather than always describing a table, because a small block's own table costs more
/// than it saves, and these three are close enough to what real data looks like that a compressor
/// reaches for them often.
const LENGTH_DEFAULT: [i32; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const MATCH_DEFAULT: [i32; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
const OFFSET_DEFAULT: [i32; 29] =
    [1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1];

/// Decompresses one compressed block onto the end of the frame's output.
///
/// `from` is where this frame's output starts, because a copy may reach back into an earlier block
/// of this frame and may not reach back into an earlier frame.
///
/// # Errors
///
/// On anything the block says that is not true of itself, which for a copy that reaches before the
/// start of the frame is the difference between an error and reading whatever was there.
pub(crate) fn decompress(
    input: &[u8],
    from: usize,
    out: &mut Vec<u8>,
    carried: &mut Carried,
) -> Result<()> {
    let (literals, used) = read_literals(input, carried)?;
    let rest = &input[used..];
    let (count, mut at) = read_count(rest)?;
    if count == 0 {
        out.extend_from_slice(&literals);
        return Ok(());
    }
    let modes = *rest
        .get(at)
        .ok_or_else(|| Error::io("a zstd block that ends before it says how it is coded"))?;
    at += 1;
    if modes & 0x03 != 0 {
        return Err(Error::io("a zstd block using the reserved bits of its mode byte"));
    }
    let lengths = read_table(rest, &mut at, modes >> 6, Kind::LENGTH, &mut carried.lengths)?;
    let offsets = read_table(rest, &mut at, modes >> 4, Kind::OFFSET, &mut carried.offsets)?;
    let matches = read_table(rest, &mut at, modes >> 2, Kind::MATCH, &mut carried.matches)?;
    let stream = rest.get(at..).unwrap_or_default();

    let mut bits = Backward::new(stream)?;
    let mut length_at = lengths.start(&mut bits);
    let mut offset_at = offsets.start(&mut bits);
    let mut match_at = matches.start(&mut bits);
    let mut spent = 0usize;

    for left in (0..count).rev() {
        let offset_code = u32::from(offsets.symbol(offset_at));
        let match_code = matches.symbol(match_at) as usize;
        let length_code = lengths.symbol(length_at) as usize;
        if offset_code > 31 || match_code >= MATCH_BASE.len() || length_code >= LENGTH_BASE.len() {
            return Err(Error::io("a zstd sequence with a code its table should not produce"));
        }
        // The three extra bit fields are read offset first, which is the order the encoder wrote
        // them in, and the states move on afterwards. The last sequence never moves them, because
        // the encoder folded their final values into the three it wrote at the front of the stream.
        let coded = (1u64 << offset_code) + bits.take(offset_code);
        let matched = u64::from(MATCH_BASE[match_code]) + bits.take(MATCH_BITS[match_code]);
        let literal = u64::from(LENGTH_BASE[length_code]) + bits.take(LENGTH_BITS[length_code]);
        if left > 0 {
            lengths.step(&mut length_at, &mut bits);
            matches.step(&mut match_at, &mut bits);
            offsets.step(&mut offset_at, &mut bits);
        }
        let offset = resolve(coded, literal == 0, &mut carried.recent.0)?;
        emit(&literals, &mut spent, literal, offset, matched, from, out)?;
    }
    if !bits.done() {
        return Err(Error::io("a zstd block whose sequences do not use up its bitstream"));
    }
    out.extend_from_slice(&literals[spent..]);
    Ok(())
}

/// Turns a coded offset into a distance, keeping the three most recent ones up to date.
///
/// Codes one to three are not distances but references to the last three distances used, which is
/// what makes a run of copies from the same place cost almost nothing. The shift when there are no
/// literals is the part that looks arbitrary and is not: a sequence with no literals cannot be
/// repeating the most recent distance, because the encoder would have merged it into the previous
/// copy, so that code is free to mean something else and it means the one before.
fn resolve(coded: u64, no_literals: bool, recent: &mut [u64; 3]) -> Result<u64> {
    if coded > 3 {
        let offset = coded - 3;
        recent[2] = recent[1];
        recent[1] = recent[0];
        recent[0] = offset;
        return Ok(offset);
    }
    let which = coded as usize - 1 + usize::from(no_literals);
    if which == 0 {
        return Ok(recent[0]);
    }
    let offset = if which == 3 {
        recent[0]
            .checked_sub(1)
            .ok_or_else(|| Error::io("a zstd sequence repeating an offset of zero"))?
    } else {
        recent[which]
    };
    if which >= 2 {
        recent[2] = recent[1];
    }
    recent[1] = recent[0];
    recent[0] = offset;
    Ok(offset)
}

/// Copies out one sequence's literals and then its match.
fn emit(
    literals: &[u8],
    spent: &mut usize,
    literal: u64,
    offset: u64,
    matched: u64,
    from: usize,
    out: &mut Vec<u8>,
) -> Result<()> {
    let literal = literal as usize;
    if *spent + literal > literals.len() {
        return Err(Error::io("a zstd block asking for more literals than it stored"));
    }
    out.extend_from_slice(&literals[*spent..*spent + literal]);
    *spent += literal;
    let offset = offset as usize;
    if offset == 0 || offset > out.len() - from {
        return Err(Error::io(format!(
            "a zstd copy reaching {offset} bytes back into the {} this frame has produced",
            out.len() - from
        )));
    }
    let start = out.len() - offset;
    let matched = matched as usize;
    if offset >= matched {
        out.extend_from_within(start..start + matched);
    } else {
        // An overlapping copy, which is how a run is expressed: an offset of one and a length of a
        // thousand is a thousand of the same byte. It has to go one byte at a time because each one
        // is the source of a later one.
        for step in 0..matched {
            let byte = out[start + step];
            out.push(byte);
        }
    }
    Ok(())
}

/// Reads the number of sequences, which is one, two or three bytes depending on how many.
fn read_count(input: &[u8]) -> Result<(usize, usize)> {
    let short = "a zstd block that ends inside its sequence count";
    let &first = input.first().ok_or_else(|| Error::io(short))?;
    if first < 128 {
        Ok((first as usize, 1))
    } else if first < 255 {
        let &second = input.get(1).ok_or_else(|| Error::io(short))?;
        Ok((((first as usize - 128) << 8) + second as usize, 2))
    } else {
        let pair = input.get(1..3).ok_or_else(|| Error::io(short))?;
        Ok((u16::from_le_bytes([pair[0], pair[1]]) as usize + 0x7F00, 3))
    }
}

/// What one of the three sequence sections is allowed to say about itself.
struct Kind {
    default: &'static [i32],
    default_log: u32,
    top_symbol: usize,
    top_log: u32,
}

impl Kind {
    const LENGTH: Self =
        Self { default: &LENGTH_DEFAULT, default_log: 6, top_symbol: 35, top_log: 9 };
    const OFFSET: Self =
        Self { default: &OFFSET_DEFAULT, default_log: 5, top_symbol: 31, top_log: 8 };
    const MATCH: Self =
        Self { default: &MATCH_DEFAULT, default_log: 6, top_symbol: 52, top_log: 9 };
}

/// Reads whichever of the four ways a section names its table, and remembers it for the next block.
fn read_table(
    input: &[u8],
    at: &mut usize,
    modes: u8,
    kind: Kind,
    slot: &mut Option<Table>,
) -> Result<Table> {
    let table = match modes & 0x03 {
        0 => Table::build(kind.default, kind.default_log)?,
        1 => {
            let &symbol = input
                .get(*at)
                .ok_or_else(|| Error::io("a zstd block that ends inside its RLE table"))?;
            *at += 1;
            if symbol as usize > kind.top_symbol {
                return Err(Error::io("a zstd RLE table naming a code its section does not have"));
            }
            Table::rle(symbol)
        }
        2 => {
            let start = input
                .get(*at..)
                .ok_or_else(|| Error::io("a zstd block that ends before its table description"))?;
            let (table, used) = Table::read(start, kind.top_symbol, kind.top_log)?;
            *at += used;
            table
        }
        _ => slot
            .clone()
            .ok_or_else(|| Error::io("a zstd block reusing a table no earlier block described"))?,
    };
    *slot = Some(table.clone());
    Ok(table)
}

/// Reads the literals section and gives back the bytes it regenerates.
fn read_literals(input: &[u8], carried: &mut Carried) -> Result<(Vec<u8>, usize)> {
    let short = "a zstd block that ends inside its literals";
    let &first = input.first().ok_or_else(|| Error::io(short))?;
    let kind = first & 0x03;
    let shape = (first >> 2) & 0x03;
    if kind < 2 {
        let (size, header) = match shape {
            1 => (word(input, 2)? >> 4 & 0x0FFF, 2),
            3 => (word(input, 3)? >> 4 & 0x000F_FFFF, 3),
            _ => (u64::from(first) >> 3, 1),
        };
        let size = size as usize;
        return if kind == 0 {
            let bytes = input.get(header..header + size).ok_or_else(|| Error::io(short))?;
            Ok((bytes.to_vec(), header + size))
        } else {
            let &byte = input.get(header).ok_or_else(|| Error::io(short))?;
            Ok((vec![byte; size], header + 1))
        };
    }

    let (size, stored, header) = match shape {
        2 => (word(input, 4)? >> 4 & 0x3FFF, word(input, 4)? >> 18 & 0x3FFF, 4),
        3 => (word(input, 5)? >> 4 & 0x0003_FFFF, word(input, 5)? >> 22 & 0x0003_FFFF, 5),
        _ => (word(input, 3)? >> 4 & 0x03FF, word(input, 3)? >> 14 & 0x03FF, 3),
    };
    let (size, stored) = (size as usize, stored as usize);
    let body = input.get(header..header + stored).ok_or_else(|| Error::io(short))?;
    // A treeless block is one whose literals are coded with the table an earlier block described,
    // which is most of the win on data that arrives in many small blocks.
    let (table, body) = if kind == 2 {
        let (table, used) = Huffman::read(body)?;
        carried.literals = Some(table);
        (carried.literals.as_ref().expect("just stored"), &body[used..])
    } else {
        let table = carried
            .literals
            .as_ref()
            .ok_or_else(|| Error::io("a zstd block reusing a Huffman code nothing described"))?;
        (table, body)
    };

    let mut out = Vec::with_capacity(size);
    if shape == 0 {
        table.stream(body, size, &mut out)?;
        return Ok((out, header + stored));
    }
    // Four streams, so four independent chains of dependent lookups, which is the only reason the
    // format splits them. The jump table at the front gives the compressed size of the first three
    // and the fourth is whatever is left, and the regenerated bytes divide the same way.
    let jump = body.get(..6).ok_or_else(|| Error::io(short))?;
    let mut sizes = [0usize; 4];
    for (which, pair) in jump.chunks_exact(2).enumerate() {
        sizes[which] = u16::from_le_bytes([pair[0], pair[1]]) as usize;
    }
    sizes[3] = (body.len() - 6).checked_sub(sizes[0] + sizes[1] + sizes[2]).ok_or_else(|| {
        Error::io("a zstd literals jump table longer than the streams it indexes")
    })?;
    let each = size.div_ceil(4);
    let last = size.checked_sub(3 * each).ok_or_else(|| {
        Error::io("a zstd literals block with too few bytes in it to split four ways")
    })?;
    let mut cut = 6;
    for (which, &length) in sizes.iter().enumerate() {
        let piece = body.get(cut..cut + length).ok_or_else(|| Error::io(short))?;
        table.stream(piece, if which == 3 { last } else { each }, &mut out)?;
        cut += length;
    }
    Ok((out, header + stored))
}

/// The first `n` bytes of a header as a little endian number.
fn word(input: &[u8], n: usize) -> Result<u64> {
    let bytes = input
        .get(..n)
        .ok_or_else(|| Error::io("a zstd literals header shorter than its own shape"))?;
    let mut word = 0;
    for (at, &byte) in bytes.iter().enumerate() {
        word |= u64::from(byte) << (8 * at);
    }
    Ok(word)
}

#[cfg(test)]
mod tests {
    use super::{
        LENGTH_BASE, LENGTH_BITS, LENGTH_DEFAULT, MATCH_BASE, MATCH_BITS, MATCH_DEFAULT,
        OFFSET_DEFAULT, Table, read_count, resolve,
    };

    #[test]
    fn the_three_default_distributions_add_up_to_the_tables_they_claim() {
        // A distribution that does not fill its table is the sort of transcription error that
        // shows up as a wrong answer on one file in a thousand rather than as a failure.
        assert!(Table::build(&LENGTH_DEFAULT, 6).is_ok());
        assert!(Table::build(&OFFSET_DEFAULT, 5).is_ok());
        assert!(Table::build(&MATCH_DEFAULT, 6).is_ok());
    }

    #[test]
    fn the_code_tables_are_as_long_as_the_distributions_that_index_them() {
        assert_eq!(LENGTH_BASE.len(), LENGTH_DEFAULT.len());
        assert_eq!(LENGTH_BITS.len(), LENGTH_DEFAULT.len());
        assert_eq!(MATCH_BASE.len(), MATCH_DEFAULT.len());
        assert_eq!(MATCH_BITS.len(), MATCH_DEFAULT.len());
    }

    #[test]
    fn a_code_and_the_next_one_are_a_field_of_extra_bits_apart() {
        // Which is what makes the tables a coding of the whole range rather than a list of sizes
        // somebody typed. It catches a digit in the wrong place, which is the failure to fear here.
        for code in 0..LENGTH_BASE.len() - 1 {
            assert_eq!(
                LENGTH_BASE[code] + (1 << LENGTH_BITS[code]),
                LENGTH_BASE[code + 1],
                "literal length code {code}"
            );
        }
        for code in 0..MATCH_BASE.len() - 1 {
            assert_eq!(
                MATCH_BASE[code] + (1 << MATCH_BITS[code]),
                MATCH_BASE[code + 1],
                "match length code {code}"
            );
        }
    }

    #[test]
    fn a_sequence_count_grows_its_own_header_as_it_gets_bigger() {
        assert_eq!(read_count(&[0]).unwrap(), (0, 1));
        assert_eq!(read_count(&[127]).unwrap(), (127, 1));
        assert_eq!(read_count(&[128, 5]).unwrap(), (5, 2));
        assert_eq!(read_count(&[254, 0]).unwrap(), (126 * 256, 2));
        assert_eq!(read_count(&[255, 0, 0]).unwrap(), (0x7F00, 3));
    }

    #[test]
    fn a_plain_offset_pushes_the_three_recent_ones_along() {
        let mut recent = [1, 4, 8];
        assert_eq!(resolve(10, false, &mut recent).unwrap(), 7);
        assert_eq!(recent, [7, 1, 4]);
    }

    #[test]
    fn the_first_repeat_code_reuses_the_last_offset_and_changes_nothing() {
        let mut recent = [7, 1, 4];
        assert_eq!(resolve(1, false, &mut recent).unwrap(), 7);
        assert_eq!(recent, [7, 1, 4], "a repeat of the most recent leaves the order alone");
    }

    #[test]
    fn a_repeat_code_in_a_sequence_with_no_literals_means_the_one_before() {
        // Because a sequence with no literals cannot be repeating the most recent offset: the
        // encoder would have written one longer copy instead of two.
        let mut recent = [7, 1, 4];
        assert_eq!(resolve(1, true, &mut recent).unwrap(), 1);
        assert_eq!(recent, [1, 7, 4]);
    }

    #[test]
    fn the_third_repeat_code_with_no_literals_is_the_last_offset_less_one() {
        let mut recent = [7, 1, 4];
        assert_eq!(resolve(3, true, &mut recent).unwrap(), 6);
        assert_eq!(recent, [6, 7, 1]);
    }
}

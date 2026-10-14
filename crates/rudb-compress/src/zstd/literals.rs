//! The literals section of a compressed block, written whichever way is shortest.
//!
//! Stored as they are, one byte repeated, or Huffman coded. The Huffman code is the one
//! [`crate::deflate`] builds, limited to the eleven bits zstd allows, and it is described by
//! weights the way [`super::huffman`] reads them: a weight of `width + 1 - length` for each symbol
//! below the last one that occurs, with the last one left for the decoder to work out. The weights
//! are FSE coded when that is shorter and when there is more than one of them to code, and written
//! a nibble each otherwise, which the format only allows for up to 128 of them.

use super::encode::{Bits, Encoder, describe_table, normalize};

/// The longest code zstd allows a literal.
const WIDEST: u8 = 11;
/// The most bits a table for the weights may have.
const WEIGHT_LOG: u32 = 6;

/// Appends the literals section for `literals`.
pub(super) fn write(literals: &[u8], out: &mut Vec<u8>) {
    let mut counts = [0_u32; 256];
    for &byte in literals {
        counts[usize::from(byte)] += 1;
    }
    let used = counts.iter().filter(|&&count| count > 0).count();
    if used == 1 && literals.len() > 1 {
        header(1, literals.len(), out);
        out.push(literals[0]);
        return;
    }
    if used > 1
        && literals.len() >= 32
        && let Some(coded) = huffman(literals, &counts)
        && coded.len() < literals.len()
    {
        out.extend_from_slice(&coded);
        return;
    }
    header(0, literals.len(), out);
    out.extend_from_slice(literals);
}

/// The header of a section that is stored or repeated, which is its kind and its size.
fn header(kind: usize, size: usize, out: &mut Vec<u8>) {
    if size < 32 {
        out.push((kind | size << 3) as u8);
    } else if size < 4096 {
        out.extend_from_slice(&[(kind | 0x04 | (size & 15) << 4) as u8, (size >> 4) as u8]);
    } else {
        out.extend_from_slice(&[
            (kind | 0x0C | (size & 15) << 4) as u8,
            (size >> 4) as u8,
            (size >> 12) as u8,
        ]);
    }
}

/// The whole section Huffman coded, or `None` when the code cannot be described.
fn huffman(literals: &[u8], counts: &[u32; 256]) -> Option<Vec<u8>> {
    let lengths = crate::deflate::lengths(counts, WIDEST);
    let width = lengths.iter().copied().max().unwrap_or(0);
    let last = lengths.iter().rposition(|&length| length > 0)?;
    let weight = |length: u8| if length == 0 { 0 } else { width + 1 - length };
    let weights: Vec<u8> = lengths[..=last].iter().map(|&length| weight(length)).collect();
    let mut body = describe(&weights[..last])?;

    // The codes the decoder's table gives, which lays the longest codes down first and the
    // symbols of one length in order.
    let mut counted = [0_u32; WIDEST as usize + 2];
    for &weight in &weights {
        counted[usize::from(weight)] += 1;
    }
    let mut start = [0_u32; WIDEST as usize + 2];
    let mut next = 0;
    for weight in 1..=usize::from(width) {
        start[weight] = next;
        next += counted[weight] << (weight - 1);
    }
    let mut codes = [(0_u32, 0_u32); 256];
    for (symbol, &weight) in weights.iter().enumerate() {
        if weight > 0 {
            let weight = usize::from(weight);
            codes[symbol] = (start[weight] >> (weight - 1), u32::from(width) + 1 - weight as u32);
            start[weight] += 1 << (weight - 1);
        }
    }
    let stream = |bytes: &[u8], out: &mut Vec<u8>| {
        let mut bits = Bits::default();
        for &byte in bytes.iter().rev() {
            let (code, length) = codes[usize::from(byte)];
            bits.add(code, length);
        }
        bits.finish(out);
    };

    let size = literals.len();
    let single = size < 256;
    if single {
        stream(literals, &mut body);
    } else {
        let each = size.div_ceil(4);
        let table = body.len();
        body.extend_from_slice(&[0; 6]);
        for (which, piece) in literals.chunks(each).enumerate() {
            let before = body.len();
            stream(piece, &mut body);
            if which < 3 {
                let length = u16::try_from(body.len() - before).ok()?;
                body[table + 2 * which..table + 2 * which + 2]
                    .copy_from_slice(&length.to_le_bytes());
            }
        }
    }
    let stored = body.len();
    let (shape, bits) = match (single, size.max(stored)) {
        (true, ..1024) => (0, 10),
        (false, ..1024) => (1, 10),
        (false, ..16_384) => (2, 14),
        (false, ..262_144) => (3, 18),
        _ => return None,
    };
    let word = 2 | shape << 2 | (size as u64) << 4 | (stored as u64) << (4 + bits);
    let mut out = word.to_le_bytes()[..(4 + 2 * bits as usize).div_ceil(8)].to_vec();
    out.extend_from_slice(&body);
    Some(out)
}

/// The tree description for the weights of every symbol but the last, shortest way first.
fn describe(weights: &[u8]) -> Option<Vec<u8>> {
    let coded = fse_weights(weights);
    let direct = (weights.len() <= 128).then(|| {
        let mut out = vec![127 + weights.len() as u8];
        for pair in weights.chunks(2) {
            out.push(pair[0] << 4 | pair.get(1).copied().unwrap_or(0));
        }
        out
    });
    match (coded, direct) {
        (Some(coded), Some(direct)) => {
            Some(if coded.len() < direct.len() { coded } else { direct })
        }
        (coded, direct) => coded.or(direct),
    }
}

/// The weights FSE coded behind a byte of their length, or `None` when that does not work.
///
/// One weight value alone cannot be coded, since a table of one symbol moves without spending a
/// bit and the decoder finds the end of the weights by running out of bits.
fn fse_weights(weights: &[u8]) -> Option<Vec<u8>> {
    let mut counts = [0_u32; WIDEST as usize + 1];
    for &weight in weights {
        counts[usize::from(weight)] += 1;
    }
    if counts.iter().filter(|&&count| count > 0).count() < 2 {
        return None;
    }
    let top = counts.iter().rposition(|&count| count > 0)?;
    let log = if weights.len() > 64 { WEIGHT_LOG } else { 5 };
    let normal = normalize(&counts[..=top], log);
    let mut bits = Bits::default();
    describe_table(&normal, log, &mut bits);
    let mut out = vec![0];
    bits.pad(&mut out);

    let table = Encoder::new(&normal, log);
    let symbol = |at: usize| usize::from(weights[at]);
    let mut bits = Bits::default();
    let mut at = weights.len();
    let (mut first, mut second);
    if at % 2 == 1 {
        first = table.start(symbol(at - 1));
        second = table.start(symbol(at - 2));
        at -= 3;
        table.encode(&mut first, symbol(at), &mut bits);
    } else {
        second = table.start(symbol(at - 1));
        first = table.start(symbol(at - 2));
        at -= 2;
    }
    while at > 0 {
        table.encode(&mut second, symbol(at - 1), &mut bits);
        table.encode(&mut first, symbol(at - 2), &mut bits);
        at -= 2;
    }
    bits.add(second, log);
    bits.add(first, log);
    bits.finish(&mut out);
    out[0] = u8::try_from(out.len() - 1).ok().filter(|&size| size < 128)?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::write;
    use crate::zstd::block::Carried;
    use crate::zstd::block::read_literals;

    fn back(section: &[u8]) -> Vec<u8> {
        let (literals, used) = read_literals(section, &mut Carried::default()).expect("reads");
        assert_eq!(used, section.len());
        literals
    }

    #[test]
    fn text_is_huffman_coded_and_reads_back() {
        let text =
            b"it was the best of times, it was the worst of times, it was the age of ".repeat(20);
        for size in [1, 31, 32, 100, 255, 256, 1000, text.len()] {
            let mut section = Vec::new();
            write(&text[..size], &mut section);
            if size >= 256 {
                assert_eq!(section[0] & 3, 2, "{size}");
                assert!(section.len() < size * 3 / 4, "{size} {}", section.len());
            }
            assert_eq!(back(&section), &text[..size], "{size}");
        }
    }

    #[test]
    fn bytes_over_the_whole_range_read_back() {
        let mut state = 5_u64;
        for round in 0..400 {
            let spread = [3, 17, 129, 200, 256][round % 5];
            let size = 32 + round * 37 % 5000;
            let bytes: Vec<u8> = (0..size)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    // Skewed, so the code has a spread of lengths in it.
                    let draw = (state % 1000) as u32;
                    ((draw * draw / 1000) * spread / 1000) as u8 ^ (round as u8 & 0x80)
                })
                .collect();
            let mut section = Vec::new();
            write(&bytes, &mut section);
            assert_eq!(back(&section), bytes, "{round}");
        }
    }

    #[test]
    fn one_byte_repeated_is_one_byte() {
        let mut section = Vec::new();
        write(&[9; 5000], &mut section);
        assert_eq!(section, [0x8D, 0x38, 0x01, 9]);
        assert_eq!(back(&section), vec![9; 5000]);
    }
}

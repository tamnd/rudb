//! The Huffman code zstd uses for literals, and nothing else.
//!
//! Literals are the bytes a block could not express as a copy of something earlier, and they are
//! the one part of a block where a plain prefix code still wins, because there are at most 256
//! symbols and the distribution is usually lopsided enough that whole bits are not wasted.
//!
//! The code is described by weights rather than by lengths. A symbol with weight `w` is coded in
//! `max + 1 - w` bits, where `max` is the largest weight present, so the weights are small numbers
//! that compress well, and the last symbol's weight is not written at all: the lengths of a
//! complete prefix code have to sum to one, so whatever is missing from the total is exactly that
//! symbol's share. A file where it does not come out to a power of two is a file that was not
//! written by a Huffman coder, and this refuses it rather than decoding most of it.
//!
//! Decoding is a single flat table of `1 << max` entries. Look up the next `max` bits, read a
//! symbol and a length out of the entry, and consume that many bits. That costs more memory than
//! walking a tree and it is one load instead of a chain of dependent branches, which is the whole
//! reason anybody decodes Huffman this way.

use rudb_common::{Error, Result};

use super::bits::Backward;
use super::fse;

/// The most bits zstd allows a literal to be coded in.
const WIDEST: u32 = 11;

/// One entry of the flat lookup table.
#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    symbol: u8,
    bits: u8,
}

/// A built Huffman decoding table.
#[derive(Debug, Clone)]
pub(crate) struct Huffman {
    /// The width of a lookup, which is the longest code in the table.
    width: u32,
    entries: Vec<Entry>,
}

impl Huffman {
    /// Reads a tree description and builds the table, returning how many bytes it took.
    ///
    /// Two spellings. A first byte of 128 or more means the weights follow as plain nibbles, which
    /// is what a compressor falls back to when there are too few of them for entropy coding to pay
    /// for its own table. Anything less is the byte length of an FSE stream holding the weights.
    ///
    /// # Errors
    ///
    /// If the description runs past its bytes, or if the weights do not describe a complete code.
    pub(crate) fn read(input: &[u8]) -> Result<(Self, usize)> {
        let &header = input
            .first()
            .ok_or_else(|| Error::io("a zstd literals block with no Huffman description in it"))?;
        if header >= 128 {
            let count = header as usize - 127;
            let bytes = count.div_ceil(2);
            if input.len() < 1 + bytes {
                return Err(Error::io("a zstd Huffman description shorter than it claims to be"));
            }
            let weights = (0..count)
                .map(|at| {
                    let byte = input[1 + at / 2];
                    if at % 2 == 0 { byte >> 4 } else { byte & 0x0F }
                })
                .collect::<Vec<_>>();
            Ok((Self::from_weights(&weights)?, 1 + bytes))
        } else {
            let size = header as usize;
            if input.len() < 1 + size {
                return Err(Error::io("a zstd Huffman description shorter than it claims to be"));
            }
            let body = &input[1..1 + size];
            let (table, used) = fse::Table::read(body, 255, 6)?;
            Ok((Self::from_weights(&weights_from_fse(&table, &body[used..])?)?, 1 + size))
        }
    }

    /// Builds the table from the weights, including the one that was not written.
    fn from_weights(written: &[u8]) -> Result<Self> {
        if written.is_empty() || written.len() > 255 {
            return Err(Error::io(format!(
                "a zstd Huffman code over {} symbols, which is not one that can exist",
                written.len()
            )));
        }
        let mut total = 0u32;
        for &weight in written {
            if u32::from(weight) > WIDEST {
                return Err(Error::io(format!(
                    "a zstd Huffman weight of {weight}, which is too big"
                )));
            }
            if weight > 0 {
                total += 1 << (weight - 1);
            }
        }
        if total == 0 {
            return Err(Error::io("a zstd Huffman code in which no symbol occurs"));
        }
        let width = 32 - total.leading_zeros();
        if width > WIDEST {
            return Err(Error::io(format!(
                "a zstd Huffman code {width} bits wide, over the limit"
            )));
        }
        let rest = (1 << width) - total;
        if !rest.is_power_of_two() {
            return Err(Error::io("a zstd Huffman code whose weights do not complete it"));
        }
        let mut weights = written.to_vec();
        weights.push((rest.trailing_zeros() + 1) as u8);

        let size = 1usize << width;
        let mut counted = [0u32; WIDEST as usize + 1];
        for &weight in &weights {
            if weight > 0 {
                counted[weight as usize] += 1;
            }
        }
        // Long codes first, which is what makes the table canonical: a decoder and an encoder that
        // both walk the weights in this order agree on which code is which without saying so.
        let mut start = [0u32; WIDEST as usize + 1];
        let mut next = 0;
        for weight in 1..=width as usize {
            start[weight] = next;
            next += counted[weight] << (weight - 1);
        }
        if next != size as u32 {
            return Err(Error::io("a zstd Huffman code that does not fill its decoding table"));
        }
        let mut entries = vec![Entry::default(); size];
        for (symbol, &weight) in weights.iter().enumerate() {
            if weight == 0 {
                continue;
            }
            let run = 1usize << (weight - 1);
            let at = start[weight as usize] as usize;
            let bits = (width + 1 - u32::from(weight)) as u8;
            for slot in &mut entries[at..at + run] {
                *slot = Entry { symbol: symbol as u8, bits };
            }
            start[weight as usize] += run as u32;
        }
        Ok(Self { width, entries })
    }

    /// Decodes exactly `count` literals out of one stream, appending them.
    ///
    /// # Errors
    ///
    /// If the stream does not end exactly where the last literal does, which means the stream and
    /// the regenerated size the header promised disagree about how many literals there are.
    pub(crate) fn stream(&self, data: &[u8], count: usize, out: &mut Vec<u8>) -> Result<()> {
        let mut bits = Backward::new(data)?;
        for _ in 0..count {
            let entry = self.entries[bits.peek(self.width) as usize];
            bits.skip(u32::from(entry.bits));
            out.push(entry.symbol);
        }
        if bits.done() {
            Ok(())
        } else {
            Err(Error::io("a zstd literals stream that does not end where its literals do"))
        }
    }
}

/// Decodes the Huffman weights from the FSE stream that carries them.
///
/// Two states sharing one table and one bitstream, taking alternate weights. It is not for speed,
/// since nothing about a table description is hot. It is because a single state has to wait for its
/// own previous transition before it can look up the next symbol, and two states interleaved give
/// the processor two independent chains to work on, which is the same trick the four literal
/// streams use one level up.
///
/// How many weights there are is not written anywhere. The stream ends when it ends, and the last
/// two weights are the two the encoder folded into the initial states rather than coding, so the
/// decoder reads one past the end and then takes the other state's symbol without moving it.
fn weights_from_fse(table: &fse::Table, stream: &[u8]) -> Result<Vec<u8>> {
    let mut bits = Backward::new(stream)?;
    let mut first = table.start(&mut bits);
    let mut second = table.start(&mut bits);
    let mut weights = Vec::new();
    loop {
        weights.push(table.decode(&mut first, &mut bits));
        if bits.over() {
            weights.push(table.symbol(second));
            break;
        }
        weights.push(table.decode(&mut second, &mut bits));
        if bits.over() {
            weights.push(table.symbol(first));
            break;
        }
        if weights.len() > 255 {
            return Err(Error::io("a zstd Huffman weight stream that does not end"));
        }
    }
    Ok(weights)
}

#[cfg(test)]
mod tests {
    use super::Huffman;

    #[test]
    fn the_weight_that_is_not_written_is_the_one_that_completes_the_code() {
        // Three symbols, weights 2, 1 and the one that is missing. Two plus one leaves one, so the
        // last symbol is also weight 1 and every code is two bits except the first, which is one.
        let table = Huffman::from_weights(&[2, 1]).unwrap();
        assert_eq!(table.width, 2);
        assert_eq!(table.entries.len(), 4);
        let symbols: Vec<u8> = table.entries.iter().map(|e| e.symbol).collect();
        assert_eq!(symbols, vec![1, 2, 0, 0]);
        let lengths: Vec<u8> = table.entries.iter().map(|e| e.bits).collect();
        assert_eq!(lengths, vec![2, 2, 1, 1]);
    }

    #[test]
    fn weights_that_do_not_complete_a_code_are_refused_rather_than_rounded() {
        let error = Huffman::from_weights(&[3, 1]).unwrap_err();
        assert!(error.message().contains("do not complete it"), "{}", error.message());
    }

    #[test]
    fn a_code_wider_than_zstd_allows_is_refused() {
        let error = Huffman::from_weights(&[12]).unwrap_err();
        assert!(error.message().contains("too big"), "{}", error.message());
    }

    #[test]
    fn a_direct_description_reads_its_weights_a_nibble_at_a_time_high_half_first() {
        // A first byte of 129 says two weights follow, which is one byte of two nibbles.
        let (table, used) = Huffman::read(&[129, 0x21]).unwrap();
        assert_eq!(used, 2);
        assert_eq!(table.width, 2);
    }

    #[test]
    fn decoding_a_stream_gives_back_what_the_code_says_it_should() {
        let table = Huffman::from_weights(&[2, 1]).unwrap();
        // Symbol 0 is 1 bit, symbols 1 and 2 are 2 bits. The table lays the long codes down first,
        // so 00 is symbol 1, 01 is symbol 2, and 1 is symbol 0.
        let mut out = Vec::new();
        // Bits, most significant first: 1 00 01, then the marker.
        table.stream(&[0b0011_0001], 3, &mut out).unwrap();
        assert_eq!(out, vec![0, 1, 2]);
    }
}

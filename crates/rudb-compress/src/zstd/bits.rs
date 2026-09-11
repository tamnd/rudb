//! The two bit orders zstd reads in.
//!
//! A format with one bit order is easier to read and zstd has two, for a reason that is worth
//! knowing rather than working around. The FSE table descriptions are written as the encoder
//! discovers them, front to back, so they are read front to back with the low bit of each byte
//! first. Everything an FSE or Huffman state machine consumes is written by an encoder walking its
//! input backwards, because that is the only way an arithmetic coder can be decoded forwards, so
//! those streams are read from the last byte towards the first.
//!
//! Both readers pad with zeros past their end rather than failing, and the backward one remembers
//! that it did. That is not leniency. The zstd decoding loops are written to read a fixed width and
//! then find out how much of it was real, which is what makes them branch free in the reference
//! implementation, and a reader that refuses to answer near the end turns those loops into
//! something that has to know the answer before it asks the question.

use rudb_common::{Error, Result};

/// A bitstream read from its last byte towards its first, most significant bit of each field first.
///
/// The end marker is the highest set bit of the last byte. Everything above it is padding the
/// encoder added to reach a byte boundary, and the marker itself is not data, so a last byte of
/// zero is a stream with no marker in it and that is corruption rather than an empty stream.
#[derive(Debug)]
pub(crate) struct Backward<'a> {
    data: &'a [u8],
    /// Bits not read yet, counted from the front of the stream.
    left: u64,
    /// Whether a read asked for more bits than were there.
    over: bool,
}

impl<'a> Backward<'a> {
    /// Positions a reader at the end marker.
    ///
    /// # Errors
    ///
    /// If there are no bytes, or if the last byte has no marker bit in it.
    pub(crate) fn new(data: &'a [u8]) -> Result<Self> {
        let last = *data
            .last()
            .ok_or_else(|| Error::io("a zstd bitstream with no bytes in it to read backwards"))?;
        if last == 0 {
            return Err(Error::io("a zstd bitstream whose last byte carries no end marker"));
        }
        let marker = u64::from(7 - last.leading_zeros());
        Ok(Self { data, left: (data.len() as u64 - 1) * 8 + marker, over: false })
    }

    /// Whether a read has asked for bits past the front of the stream.
    pub(crate) fn over(&self) -> bool {
        self.over
    }

    /// Whether every bit has been read, which is where a well formed stream ends.
    pub(crate) fn done(&self) -> bool {
        self.left == 0 && !self.over
    }

    /// The next `n` bits without consuming them, zero padded when fewer than `n` are left.
    pub(crate) fn peek(&self, n: u32) -> u64 {
        if n == 0 {
            return 0;
        }
        if self.left >= u64::from(n) {
            let low = self.left - u64::from(n);
            (self.word(low / 8) >> (low % 8)) & mask(n)
        } else {
            let short = n - self.left as u32;
            (self.word(0) & mask(self.left as u32)) << short
        }
    }

    /// Drops the next `n` bits, remembering it if there were not that many.
    pub(crate) fn skip(&mut self, n: u32) {
        if u64::from(n) > self.left {
            self.over = true;
            self.left = 0;
        } else {
            self.left -= u64::from(n);
        }
    }

    /// The next `n` bits, consumed.
    pub(crate) fn take(&mut self, n: u32) -> u64 {
        let value = self.peek(n);
        self.skip(n);
        value
    }

    /// Eight bytes little endian from a byte offset, zero padded past the end.
    fn word(&self, at: u64) -> u64 {
        let at = at as usize;
        let mut word = 0;
        for step in 0..8 {
            if let Some(byte) = self.data.get(at + step) {
                word |= u64::from(*byte) << (8 * step);
            }
        }
        word
    }
}

/// A bitstream read from its first byte towards its last, least significant bit of each byte first.
///
/// This is how the FSE table descriptions are written, and it is the only part of zstd in this
/// order. It runs off the end into zeros rather than failing, and [`Forward::past`] is how a caller
/// finds out that it did, for the same reason the backward reader works that way.
#[derive(Debug)]
pub(crate) struct Forward<'a> {
    data: &'a [u8],
    /// Bits read so far.
    at: u64,
}

impl<'a> Forward<'a> {
    /// Positions a reader at the first bit.
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    /// The next `n` bits without consuming them.
    pub(crate) fn peek(&self, n: u32) -> u64 {
        let at = self.at as usize;
        let mut word = 0;
        for step in 0..8 {
            if let Some(byte) = self.data.get(at / 8 + step) {
                word |= u64::from(*byte) << (8 * step);
            }
        }
        (word >> (at % 8)) & mask(n)
    }

    /// Drops the next `n` bits.
    pub(crate) fn skip(&mut self, n: u32) {
        self.at += u64::from(n);
    }

    /// The next `n` bits, consumed.
    pub(crate) fn take(&mut self, n: u32) -> u64 {
        let value = self.peek(n);
        self.skip(n);
        value
    }

    /// Whether the reader has run off the end of its bytes.
    pub(crate) fn past(&self) -> bool {
        self.at > self.data.len() as u64 * 8
    }

    /// How many bytes the bits read so far occupy, which is where whatever follows starts.
    pub(crate) fn used(&self) -> usize {
        self.at.div_ceil(8) as usize
    }
}

/// The low `n` bits set.
fn mask(n: u32) -> u64 {
    if n >= 64 { u64::MAX } else { (1 << n) - 1 }
}

#[cfg(test)]
mod tests {
    use super::{Backward, Forward};

    #[test]
    fn a_backward_reader_starts_below_the_marker_bit_and_not_at_the_top_of_the_byte() {
        // 0b0000_1101: the marker is bit 3, so the stream is the three bits below it.
        let mut bits = Backward::new(&[0b0000_1101]).unwrap();
        assert_eq!(bits.take(3), 0b101);
        assert!(bits.done());
    }

    #[test]
    fn a_backward_reader_crosses_a_byte_boundary_the_way_the_encoder_wrote_it() {
        // Nine bits of stream: the one below the marker in the last byte, then the whole first one.
        let mut bits = Backward::new(&[0xCD, 0b0000_0011]).unwrap();
        assert_eq!(bits.take(3), 0b111, "one bit out of the last byte and two out of the first");
        assert_eq!(bits.take(6), 0b00_1101);
        assert!(bits.done());
    }

    #[test]
    fn a_backward_reader_pads_with_zeros_and_says_that_it_did() {
        let mut bits = Backward::new(&[0b0000_1101]).unwrap();
        assert_eq!(bits.peek(5), 0b10100, "the two missing bits are zeros at the bottom");
        assert!(!bits.over());
        bits.skip(5);
        assert!(bits.over());
        assert!(!bits.done());
    }

    #[test]
    fn a_last_byte_with_no_marker_is_corruption_rather_than_an_empty_stream() {
        assert!(Backward::new(&[0x00]).is_err());
        assert!(Backward::new(&[]).is_err());
    }

    #[test]
    fn a_forward_reader_takes_the_low_bits_of_the_first_byte_first() {
        let mut bits = Forward::new(&[0b1010_0110, 0xFF]);
        assert_eq!(bits.take(4), 0b0110);
        assert_eq!(bits.take(4), 0b1010);
        assert_eq!(bits.used(), 1);
        assert!(!bits.past());
    }

    #[test]
    fn a_forward_reader_that_runs_out_reads_zeros_and_admits_it() {
        let mut bits = Forward::new(&[0xFF]);
        assert_eq!(bits.take(12), 0x0FF);
        assert!(bits.past());
        assert_eq!(bits.used(), 2, "the used count is what was asked for, not what was there");
    }
}

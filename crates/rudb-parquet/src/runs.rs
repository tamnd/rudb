//! The writing half of the RLE and bit-packing hybrid, which picks its runs the way the pin's
//! encoder does so a page of levels or of dictionary indices comes out the same bytes.
//!
//! A value starts a run and the values that repeat it make it longer. Once a run is four long, a
//! different value closes it and starts the next one. A different value after a shorter run turns
//! the run into a bit-packed block instead, and the block then takes every value that comes until
//! it holds 256 of them and is written whole. A block the end of the stream cuts short is still
//! written as 256 values, and the slots past its end hold what the last full block left there,
//! which is what the pin writes too.

/// How many values a bit-packed block holds.
const BLOCK: usize = 256;

/// How long a run has to be before a different value closes it rather than packing it.
const SHORTEST_RUN: u64 = 4;

/// An encoder of one stream of values of one bit width.
#[derive(Debug)]
pub(crate) struct Runs {
    width: u32,
    out: Vec<u8>,
    value: u32,
    count: u64,
    block: Box<[u32; BLOCK]>,
    filled: usize,
}

impl Runs {
    /// An encoder for values of `width` bits, from one to thirty two.
    pub(crate) fn new(width: u8) -> Self {
        Self {
            width: u32::from(width),
            out: Vec::new(),
            value: 0,
            count: 0,
            block: Box::new([0; BLOCK]),
            filled: 0,
        }
    }

    /// Takes the next value.
    pub(crate) fn push(&mut self, value: u32) {
        if self.filled != 0 {
            self.block[self.filled] = value;
            self.filled += 1;
            if self.filled == BLOCK {
                self.flush();
            }
            return;
        }
        if self.count == 0 {
            self.value = value;
            self.count = 1;
            return;
        }
        if self.value == value {
            self.count += 1;
            return;
        }
        if self.count >= SHORTEST_RUN {
            self.flush();
            self.value = value;
            self.count = 1;
            return;
        }
        for _ in 0..self.count {
            self.block[self.filled] = self.value;
            self.filled += 1;
        }
        self.block[self.filled] = value;
        self.filled += 1;
        self.count = 0;
    }

    /// Writes what is left and hands back the stream.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.flush();
        self.out
    }

    /// Writes the run or the block that is open.
    fn flush(&mut self) {
        if self.count != 0 {
            varint(self.count << 1, &mut self.out);
            let bytes = self.width.div_ceil(8) as usize;
            self.out.extend_from_slice(&self.value.to_le_bytes()[..bytes]);
            self.count = 0;
        } else if self.filled != 0 {
            varint(((BLOCK as u64 / 8) << 1) | 1, &mut self.out);
            let mut bits = 0_u64;
            let mut held = 0_u32;
            for &value in self.block.iter() {
                bits |= u64::from(value) << held;
                held += self.width;
                while held >= 8 {
                    self.out.push(bits as u8);
                    bits >>= 8;
                    held -= 8;
                }
            }
            self.filled = 0;
        }
    }
}

/// Appends an unsigned varint.
pub(crate) fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// The width of a dictionary index for a dictionary of `count` values, which is the pin's: one
/// bit for a dictionary of one value, and otherwise the bits the largest index needs.
pub(crate) fn index_width(count: usize) -> u8 {
    match count {
        0 => 0,
        1 => 1,
        count => (usize::BITS - (count - 1).leading_zeros()) as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::{Runs, index_width};

    fn encode(width: u8, values: &[u32]) -> Vec<u8> {
        let mut runs = Runs::new(width);
        for &value in values {
            runs.push(value);
        }
        runs.finish()
    }

    #[test]
    fn a_repeated_value_is_one_run() {
        assert_eq!(encode(1, &[1; 3]), vec![6, 1]);
        assert_eq!(encode(9, &[300; 2]), vec![4, 0x2C, 0x01]);
    }

    #[test]
    fn a_short_run_is_packed_into_a_whole_block() {
        let bytes = encode(1, &[1, 0, 1]);
        assert_eq!(bytes.len(), 1 + 32);
        assert_eq!(bytes[..2], [65, 0b101]);
        assert!(bytes[2..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn a_long_run_closes_and_the_next_one_starts() {
        assert_eq!(encode(2, &[3, 3, 3, 3, 1]), vec![8, 3, 2, 1]);
    }

    #[test]
    fn a_short_block_keeps_what_the_last_full_one_left() {
        let mut values = vec![0, 1];
        values.extend(std::iter::repeat_n(1, 254));
        values.extend([0, 1]);
        let bytes = encode(1, &values);
        assert_eq!(bytes.len(), 2 * 33);
        assert_eq!(bytes[33..35], [65, 0b1111_1110]);
        assert!(bytes[35..].iter().all(|byte| *byte == 0xFF));
    }

    #[test]
    fn index_widths_are_the_pins() {
        let widths: Vec<_> = [0, 1, 2, 3, 4, 5, 256, 257].map(index_width).to_vec();
        assert_eq!(widths, vec![0, 1, 1, 2, 2, 3, 8, 9]);
    }
}

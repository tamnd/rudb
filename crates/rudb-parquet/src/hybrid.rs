//! The run length and bit packed hybrid, which is how Parquet writes levels and dictionary indices.
//!
//! One stream, two kinds of run, and a varint in front of each run saying which it is. The low bit
//! of that varint picks the kind and the rest is a count, so a run header is one byte until the
//! counts get large. A run length run is a count of repeats and then one value, packed into as few
//! bytes as the bit width needs. A bit packed run is a count of groups of eight values, each group
//! packed at the bit width with the first value in the low bits of the first byte.
//!
//! The bit order is the part that is easy to get wrong and it is the opposite of the one in
//! `rudb-encoding`. Parquet packs least significant bit first and lets a value straddle a byte
//! boundary, so a width of three puts value zero in bits 0 to 2 of byte zero, value one in bits 3
//! to 5, and value two in bits 6 and 7 of byte zero and bit 0 of byte one. `rudb-encoding` packs
//! for the FastLanes layout, which interleaves lanes so that unpacking is branch free under SIMD,
//! and the two are not the same bits in a different order, they are different layouts. So this is
//! written here rather than reached for there.
//!
//! A width of zero is legal and means every value is zero, which is what a writer emits for the
//! definition levels of a column with no nulls in it. It consumes no bytes, so a run of it has to
//! be recognised rather than divided by.

use rudb_common::{Error, Result};

/// A cursor over one hybrid stream.
///
/// The stream is read into a caller's buffer rather than returned, because every caller here knows
/// how many values it wants and a `Vec` per page is a `Vec` per page.
#[derive(Debug)]
pub(crate) struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
    width: u8,
}

impl<'a> Decoder<'a> {
    /// A decoder over `bytes` at `width` bits a value.
    pub(crate) fn new(bytes: &'a [u8], width: u8) -> Self {
        Self { bytes, at: 0, width }
    }

    /// Reads exactly `count` values into `out`.
    ///
    /// The stream is allowed to hold more than `count`, and usually does: a bit packed run rounds up
    /// to a whole group of eight, so a page of a hundred values ends with four values that are
    /// present in the bytes and are not values. They are read and dropped rather than left in the
    /// buffer, which is why this takes a count at all.
    ///
    /// # Errors
    ///
    /// If the stream ends before `count` values have been read, or a run header does not parse.
    pub(crate) fn read(&mut self, out: &mut Vec<u32>, count: usize) -> Result<()> {
        out.clear();
        out.reserve(count);
        while out.len() < count {
            let header = self.varint()?;
            let run = usize::try_from(header >> 1)
                .map_err(|_| Error::io("a parquet run longer than memory"))?;
            if header & 1 == 0 {
                self.repeated(out, run, count)?;
            } else {
                self.packed(out, run, count)?;
            }
        }
        Ok(())
    }

    /// Reads a run of `run` copies of one value.
    fn repeated(&mut self, out: &mut Vec<u32>, run: usize, count: usize) -> Result<()> {
        if run == 0 {
            return Err(Error::io("a parquet run of length zero, which never ends"));
        }
        let bytes = usize::from(self.width).div_ceil(8);
        let value = self.value(bytes)?;
        // Never past `count`, because the last run of a page is allowed to be longer than what is
        // left of the page and reading its tail into the caller's buffer would be reading values
        // that are not values.
        let take = run.min(count - out.len());
        out.resize(out.len() + take, value);
        Ok(())
    }

    /// Reads `run` groups of eight bit packed values.
    fn packed(&mut self, out: &mut Vec<u32>, run: usize, count: usize) -> Result<()> {
        if run == 0 {
            return Err(Error::io("a parquet packed run of no groups, which never ends"));
        }
        let width = usize::from(self.width);
        if width == 0 {
            // Legal and consumes nothing, so it has to be handled before the byte arithmetic below
            // divides by it. Eight zeroes a group is what it means.
            let take = (run * 8).min(count - out.len());
            out.resize(out.len() + take, 0);
            return Ok(());
        }
        let len = run * width;
        let bytes = self.take(len)?;
        let wanted = (run * 8).min(count - out.len());
        let mut bit = 0_usize;
        for _ in 0..wanted {
            let mut value = 0_u32;
            for offset in 0..width {
                let at = bit + offset;
                let byte = bytes[at / 8];
                value |= u32::from((byte >> (at % 8)) & 1) << offset;
            }
            out.push(value);
            bit += width;
        }
        Ok(())
    }

    /// Reads the repeated value of a run length run, which is little endian in `bytes` bytes.
    fn value(&mut self, bytes: usize) -> Result<u32> {
        if bytes > 4 {
            return Err(Error::io(format!(
                "a parquet bit width of {}, which is too wide",
                self.width
            )));
        }
        let run = self.take(bytes)?;
        let mut value = 0_u32;
        for (at, &byte) in run.iter().enumerate() {
            value |= u32::from(byte) << (at * 8);
        }
        Ok(value)
    }

    /// An unsigned varint, seven bits a byte, low group first.
    fn varint(&mut self) -> Result<u64> {
        let mut value = 0_u64;
        let mut shift = 0;
        loop {
            let byte = *self
                .bytes
                .get(self.at)
                .ok_or_else(|| Error::io("a parquet page that ends inside a run header"))?;
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                return Err(Error::io("a parquet varint longer than ten bytes"));
            }
        }
    }

    /// A run of bytes, or an error if the page ends first.
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end =
            self.at.checked_add(len).ok_or_else(|| Error::io("a parquet length that wraps"))?;
        let bytes = self.bytes.get(self.at..end).ok_or_else(|| {
            Error::io(format!("a parquet run of {len} bytes past the end of a page"))
        })?;
        self.at = end;
        Ok(bytes)
    }
}

/// How many bits it takes to write every value from zero to `max`.
///
/// This is how a reader knows the bit width of a definition level stream, because the width is not
/// written down anywhere: it follows from the schema's maximum level, which for a flat schema is one
/// when the column is optional and zero when it is required.
pub(crate) const fn bit_width(max: u32) -> u8 {
    if max == 0 {
        return 0;
    }
    (u32::BITS - max.leading_zeros()) as u8
}

#[cfg(test)]
mod tests {
    use super::{Decoder, bit_width};

    fn decode(bytes: &[u8], width: u8, count: usize) -> Vec<u32> {
        let mut out = Vec::new();
        Decoder::new(bytes, width).read(&mut out, count).expect("decodes");
        out
    }

    #[test]
    fn a_run_length_run_is_a_count_and_a_value() {
        // Header 8 is a run of four, and the value is one byte because the width is 3.
        assert_eq!(decode(&[0x08, 0x05], 3, 4), vec![5, 5, 5, 5]);
    }

    #[test]
    fn a_bit_packed_run_puts_the_first_value_in_the_low_bits() {
        // Header 3 is one packed group, so eight values at one bit each in one byte. 0b1010_1100
        // reads as 0, 0, 1, 1, 0, 1, 0, 1 because the first value is the low bit.
        assert_eq!(decode(&[0x03, 0b1010_1100], 1, 8), vec![0, 0, 1, 1, 0, 1, 0, 1]);
    }

    #[test]
    fn a_value_is_allowed_to_straddle_a_byte_boundary() {
        // Three bits a value, eight values, so three bytes. 0, 1, 2, 3, 4, 5, 6, 7 packs to
        // 0b10001000, 0b11000110, 0b11111010, and value two ends in the first bit of byte one.
        assert_eq!(
            decode(&[0x03, 0b1000_1000, 0b1100_0110, 0b1111_1010], 3, 8),
            vec![0, 1, 2, 3, 4, 5, 6, 7]
        );
    }

    #[test]
    fn a_width_of_zero_produces_zeroes_and_reads_no_bytes() {
        // What a writer emits for the definition levels of a column with no nulls.
        assert_eq!(decode(&[0x08], 0, 4), vec![0, 0, 0, 0]);
        assert_eq!(decode(&[0x03], 0, 8), vec![0; 8]);
    }

    #[test]
    fn the_tail_of_the_last_group_is_dropped_rather_than_returned() {
        // One packed group is eight values and the caller wants five, so three are read and thrown
        // away. A reader that kept them would have three extra rows in the page.
        assert_eq!(decode(&[0x03, 0b1010_1100], 1, 5), vec![0, 0, 1, 1, 0]);
    }

    #[test]
    fn runs_of_both_kinds_follow_each_other_in_one_stream() {
        let bytes = [0x04, 0x01, 0x03, 0b1010_1100];
        assert_eq!(decode(&bytes, 1, 10), vec![1, 1, 0, 0, 1, 1, 0, 1, 0, 1]);
    }

    #[test]
    fn a_stream_that_ends_early_is_an_error_rather_than_a_short_read() {
        let mut out = Vec::new();
        let error = Decoder::new(&[0x03], 1).read(&mut out, 8).unwrap_err();
        assert!(error.to_string().contains("past the end of a page"), "{error}");
    }

    #[test]
    fn a_run_of_length_zero_is_an_error_rather_than_a_loop_that_never_ends() {
        let mut out = Vec::new();
        let error = Decoder::new(&[0x00, 0x00], 1).read(&mut out, 4).unwrap_err();
        assert!(error.to_string().contains("never ends"), "{error}");
    }

    #[test]
    fn the_width_of_a_level_follows_from_the_largest_value_it_has_to_hold() {
        assert_eq!(bit_width(0), 0);
        assert_eq!(bit_width(1), 1);
        assert_eq!(bit_width(2), 2);
        assert_eq!(bit_width(3), 2);
        assert_eq!(bit_width(255), 8);
        assert_eq!(bit_width(256), 9);
    }
}

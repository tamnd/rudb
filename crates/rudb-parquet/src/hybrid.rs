//! The RLE and bit-packing hybrid, which is how Parquet writes every small integer it has.
//!
//! Definition levels, repetition levels and dictionary indices are all written in this one
//! encoding, so this module is on the path of every column of every row group. A column of
//! `hits` that is dictionary encoded reads its indices through here, and a column that can hold a
//! null reads its definition levels through here as well, which means the loop below runs twice
//! per page for most of the file.
//!
//! # The two forms and why there are two
//!
//! The stream is a sequence of runs, each introduced by a varint header whose low bit says which
//! form follows. A run of one repeated value is a header and the value, which is what a column of
//! definition levels looks like when nothing is null: two bytes for a page of twenty thousand
//! values. A bit-packed run is the header and then the values at a fixed width, in groups of
//! eight, which is what a column of dictionary indices looks like.
//!
//! Nothing chooses between them here. The writer chose, and a reader that assumed either one is a
//! reader that is wrong on half the files.
//!
//! # The width is not in the stream
//!
//! A bit-packed value's width comes from the caller and not from the bytes. For a level it is the
//! number of bits the maximum level needs, which for a flat optional column is one and for a
//! required column is zero. For a dictionary index it is a single byte written in front of the
//! stream by the page. A width of zero is legal and it means every value is zero, which is the
//! case a reader that computed `1 << width` without thinking about it gets wrong.
//!
//! # The bit order
//!
//! Within a group of eight, values are packed from the least significant bit of the first byte
//! upward, and a value that crosses a byte boundary continues in the low bits of the next one.
//! That is the opposite of what a reader who has done this before in another format expects, and
//! it is the bug that produces a stream of plausible looking wrong indices rather than an error.

use rudb_common::{Error, Result};

/// A reader over one RLE and bit-packing hybrid stream.
///
/// Borrows its bytes. A page holds its levels and its values in one buffer, and copying the level
/// section out of it to decode it would be a copy per page per column.
#[derive(Debug)]
pub(crate) struct Hybrid<'a> {
    bytes: &'a [u8],
    /// Where the next run header starts.
    at: usize,
    /// How wide a bit-packed value is, which the stream does not say.
    width: u8,
    /// What is left of the run being read, and which form it is.
    run: Run,
}

/// What the reader is in the middle of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// Between runs.
    Done,
    /// A repeated value, and how many of it are left.
    Repeat { value: u32, left: usize },
    /// A bit-packed run: where its body starts, how many values it holds, and how many of them
    /// have been handed out.
    ///
    /// The position rather than the count, because a caller reads a page's worth at a time and a
    /// run does not end where a read does. A value that is not a multiple of eight long leaves
    /// the next one starting part way into a byte, so what has to survive between calls is a bit
    /// offset and not a number of bytes consumed.
    Packed { start: usize, total: usize, done: usize },
}

impl<'a> Hybrid<'a> {
    /// A reader over `bytes` whose bit-packed values are `width` bits each.
    ///
    /// # Errors
    ///
    /// If the width is wider than a `u32`, which is not a width any Parquet writer emits and is a
    /// sign the caller computed it from a corrupt maximum level.
    pub(crate) fn new(bytes: &'a [u8], width: u8) -> Result<Self> {
        if width > 32 {
            return Err(Error::io(format!(
                "a hybrid run of {width} bit values, and nothing in parquet is wider than 32"
            )));
        }
        Ok(Self { bytes, at: 0, width, run: Run::Done })
    }

    /// Reads exactly `count` values onto the end of `out`.
    ///
    /// # Errors
    ///
    /// If the stream runs out before `count` values have been produced, which means the page
    /// header and the page body disagree about how many values the page holds.
    pub(crate) fn read(&mut self, out: &mut Vec<u32>, count: usize) -> Result<()> {
        out.reserve(count);
        let mut left = count;
        while left > 0 {
            if self.run == Run::Done {
                self.next_run()?;
            }
            match self.run {
                Run::Done => {
                    return Err(Error::io(format!(
                        "this page's levels ran out {left} values before the {count} it promised"
                    )));
                }
                Run::Repeat { value, left: have } => {
                    let take = have.min(left);
                    out.resize(out.len() + take, value);
                    left -= take;
                    self.run = if have == take {
                        Run::Done
                    } else {
                        Run::Repeat { value, left: have - take }
                    };
                }
                Run::Packed { start, total, done } => {
                    let take = (total - done).min(left);
                    self.unpack(out, start, done, take);
                    left -= take;
                    if done + take == total {
                        self.at = start + self.packed_bytes(total);
                        self.run = Run::Done;
                    } else {
                        self.run = Run::Packed { start, total, done: done + take };
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads the next run header and sets up the run it introduces.
    fn next_run(&mut self) -> Result<()> {
        if self.at >= self.bytes.len() {
            return Ok(());
        }
        let header = self.varint()?;
        if header & 1 == 1 {
            // Bit-packed. The header counts groups of eight, not values, which is why a run can
            // never be a number of values that is not a multiple of eight and why a page whose
            // value count is not one pads with values nobody reads.
            let groups = usize::try_from(header >> 1).map_err(|_| {
                Error::io("a bit-packed run longer than this machine can address".to_string())
            })?;
            let total = groups.checked_mul(8).ok_or_else(|| {
                Error::io("a bit-packed run longer than this machine can address".to_string())
            })?;
            // Checked once here rather than per group, so `unpack` can index rather than probe
            // and so a run whose body is short fails before any of it is handed to a caller.
            let bytes = self.packed_bytes(total);
            if self.at + bytes > self.bytes.len() {
                return Err(Error::io(format!(
                    "a bit-packed run wants {bytes} bytes and the page has {} left",
                    self.bytes.len() - self.at
                )));
            }
            self.run = Run::Packed { start: self.at, total, done: 0 };
        } else {
            let times = usize::try_from(header >> 1).map_err(|_| {
                Error::io("a repeated run longer than this machine can address".to_string())
            })?;
            let value = self.repeated()?;
            if times == 0 {
                // A run of nothing says nothing, so take the next header rather than hand back a
                // run the caller then has to treat as empty. Its value bytes are read and thrown
                // away above rather than skipped, because the header says a value follows and
                // whether one was written is not something the count changes.
                return self.next_run();
            }
            self.run = Run::Repeat { value, left: times };
        }
        Ok(())
    }

    /// The value of a repeated run, which is written in as many whole bytes as the width needs.
    fn repeated(&mut self) -> Result<u32> {
        let bytes = usize::from(self.width).div_ceil(8);
        let end = self.at + bytes;
        let slice = self.bytes.get(self.at..end).ok_or_else(|| {
            Error::io(format!(
                "a repeated run wants {bytes} bytes for its value and the page has {} left",
                self.bytes.len().saturating_sub(self.at)
            ))
        })?;
        self.at = end;
        let mut value = 0u32;
        for (i, &byte) in slice.iter().enumerate() {
            value |= u32::from(byte) << (8 * i);
        }
        Ok(value)
    }

    /// How many bytes a bit-packed run of `total` values occupies.
    ///
    /// Exact rather than rounded up, because `total` is always a multiple of eight and eight
    /// values of any width are a whole number of bytes.
    fn packed_bytes(&self, total: usize) -> usize {
        total * usize::from(self.width) / 8
    }

    /// Unpacks `count` values from the run at `start`, skipping the `done` already handed out.
    fn unpack(&mut self, out: &mut Vec<u32>, start: usize, done: usize, count: usize) {
        if self.width == 0 {
            // Legal, and it means the maximum value is zero, so the run occupies no bytes at all.
            // A column that is required has a maximum definition level of zero and every page of
            // it takes this branch.
            out.resize(out.len() + count, 0);
            return;
        }
        let width = usize::from(self.width);
        let mask = if width == 32 { u32::MAX } else { (1u32 << width) - 1 };
        let mut bit = done * width;
        for _ in 0..count {
            let byte = start + bit / 8;
            let shift = bit % 8;
            // Up to five bytes, because a 32 bit value starting at bit seven of a byte ends in
            // the fifth one. Reading them as a u64 and shifting is one branch instead of a loop
            // with a carry in it. The tail of the last group can ask for a byte past the run, and
            // those bits are masked off, so a missing byte reads as zero rather than as an error.
            let mut window = 0u64;
            for i in 0..5 {
                if let Some(&b) = self.bytes.get(byte + i) {
                    window |= u64::from(b) << (8 * i);
                }
            }
            out.push(((window >> shift) as u32) & mask);
            bit += width;
        }
    }

    /// A little-endian base 128 varint, which is what a run header is.
    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self.bytes.get(self.at).ok_or_else(|| {
                Error::io("a hybrid run header ran off the end of the page".to_string())
            })?;
            self.at += 1;
            if shift >= 64 {
                return Err(Error::io("a hybrid run header longer than 64 bits".to_string()));
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }
}

/// How many bits a level of at most `max` needs.
///
/// Zero for a required column, which is the case worth getting right: the width is not one, the
/// run occupies no bytes, and a reader that rounded it up to one reads the next column's bytes.
pub(crate) fn width_for(max: u32) -> u8 {
    if max == 0 { 0 } else { (32 - max.leading_zeros()) as u8 }
}

#[cfg(test)]
mod tests {
    use super::{Hybrid, width_for};

    /// A run of `times` copies of `value`, at `width` bits.
    fn repeat(times: u64, value: u32, width: u8) -> Vec<u8> {
        let mut out = varint(times << 1);
        for i in 0..usize::from(width).div_ceil(8) {
            out.push((value >> (8 * i)) as u8);
        }
        out
    }

    /// A bit-packed run holding `values`, padded to a whole group of eight.
    fn packed(values: &[u32], width: u8) -> Vec<u8> {
        let groups = values.len().div_ceil(8);
        let mut out = varint(((groups as u64) << 1) | 1);
        let mut bit = 0usize;
        let mut body = vec![0u8; groups * 8 * usize::from(width).div_ceil(1) / 8 + 8];
        for &value in values {
            for i in 0..usize::from(width) {
                if value >> i & 1 == 1 {
                    body[(bit + i) / 8] |= 1 << ((bit + i) % 8);
                }
            }
            bit += usize::from(width);
        }
        body.truncate((groups * 8 * usize::from(width)).div_ceil(8));
        out.extend_from_slice(&body);
        out
    }

    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    fn read(bytes: &[u8], width: u8, count: usize) -> Vec<u32> {
        let mut out = Vec::new();
        Hybrid::new(bytes, width)
            .expect("a width this narrow is allowed")
            .read(&mut out, count)
            .expect("the stream holds this many values");
        out
    }

    #[test]
    fn a_page_with_no_nulls_is_two_bytes_of_definition_levels() {
        // The common case by a mile and the reason the repeated form exists. Twenty thousand
        // levels, all one, in a header and a value.
        let bytes = repeat(20_000, 1, 1);
        assert_eq!(bytes.len(), 4, "a header of three bytes and one byte of value");
        assert_eq!(read(&bytes, 1, 20_000), vec![1u32; 20_000]);
    }

    #[test]
    fn a_bit_packed_group_reads_from_the_low_bits_upward() {
        // The bit order is the thing to pin down, because getting it backwards produces indices
        // that are all in range and all wrong. Eight three bit values, 0 through 7, pack into
        // three bytes, and those three bytes are a constant anybody can check against the format
        // description rather than against this encoder.
        let bytes = packed(&[0, 1, 2, 3, 4, 5, 6, 7], 3);
        assert_eq!(&bytes[1..], &[0b1000_1000, 0b1100_0110, 0b1111_1010]);
        assert_eq!(read(&bytes, 3, 8), vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn a_value_that_crosses_a_byte_boundary_comes_back_whole() {
        let values: Vec<u32> = (0..24).map(|i| i * 1009 % 4096).collect();
        let bytes = packed(&values, 12);
        assert_eq!(read(&bytes, 12, values.len()), values);
    }

    #[test]
    fn a_width_of_thirty_two_is_read_without_overflowing_its_mask() {
        // The mask is `(1 << width) - 1` and at 32 that shift is undefined, so this is the case a
        // reader gets wrong once and only once.
        let values = vec![0, 1, u32::MAX, u32::MAX - 1, 0x8000_0000, 7, 0x7fff_ffff, 12];
        let bytes = packed(&values, 32);
        assert_eq!(read(&bytes, 32, values.len()), values);
    }

    #[test]
    fn a_width_of_zero_occupies_no_bytes_and_reads_as_zeroes() {
        // A required column's definition levels. The run header is there and the values are not,
        // which is what `width_for(0)` being zero rather than one is for.
        assert_eq!(width_for(0), 0);
        let bytes = packed(&[0; 8], 0);
        assert_eq!(bytes.len(), 1, "the header and nothing else");
        assert_eq!(read(&bytes, 0, 8), vec![0; 8]);
    }

    #[test]
    fn a_repeated_run_of_width_zero_is_a_header_and_a_zero_byte() {
        let bytes = repeat(100, 0, 0);
        assert_eq!(read(&bytes, 0, 100), vec![0; 100]);
    }

    #[test]
    fn the_two_forms_alternate_inside_one_stream() {
        // Which is what a page looks like when most rows are present and a few are not.
        let mut bytes = repeat(500, 1, 1);
        bytes.extend(packed(&[1, 0, 1, 1, 0, 0, 1, 1], 1));
        bytes.extend(repeat(300, 1, 1));
        let mut want = vec![1u32; 500];
        want.extend([1, 0, 1, 1, 0, 0, 1, 1]);
        want.extend(vec![1u32; 300]);
        assert_eq!(read(&bytes, 1, want.len()), want);
    }

    #[test]
    fn a_read_can_stop_in_the_middle_of_a_run_and_be_continued() {
        // The caller reads a page's worth at a time and a run does not end where a page does.
        let bytes = repeat(1000, 3, 2);
        let mut reader = Hybrid::new(&bytes, 2).expect("a width of two is allowed");
        let mut out = Vec::new();
        reader.read(&mut out, 400).expect("the first 400");
        reader.read(&mut out, 600).expect("the rest");
        assert_eq!(out, vec![3u32; 1000]);
    }

    #[test]
    fn a_read_can_stop_in_the_middle_of_a_packed_group_and_be_continued() {
        let values: Vec<u32> = (0..32).map(|i| i % 16).collect();
        let bytes = packed(&values, 4);
        let mut reader = Hybrid::new(&bytes, 4).expect("a width of four is allowed");
        let mut out = Vec::new();
        for _ in 0..5 {
            reader.read(&mut out, 5).expect("five at a time");
        }
        reader.read(&mut out, 7).expect("and the rest");
        assert_eq!(out, values);
    }

    #[test]
    fn a_run_of_no_values_is_skipped_rather_than_returned_as_one() {
        let mut bytes = repeat(0, 9, 4);
        bytes.extend(repeat(6, 9, 4));
        assert_eq!(read(&bytes, 4, 6), vec![9; 6]);
    }

    #[test]
    fn asking_for_more_than_the_stream_holds_is_an_error_and_not_a_short_answer() {
        // The case that matters. A short read here is a column with fewer rows than its
        // neighbours, which is a wrong answer rather than a failure if nobody checks.
        let bytes = repeat(10, 1, 1);
        let mut out = Vec::new();
        let error = Hybrid::new(&bytes, 1).expect("allowed").read(&mut out, 11).unwrap_err();
        assert!(error.message().contains("ran out"), "{}", error.message());
    }

    #[test]
    fn a_packed_run_whose_bytes_are_missing_is_an_error() {
        let mut bytes = packed(&[1, 2, 3, 4, 5, 6, 7, 8], 4);
        bytes.truncate(bytes.len() - 2);
        let mut out = Vec::new();
        let error = Hybrid::new(&bytes, 4).expect("allowed").read(&mut out, 8).unwrap_err();
        assert!(error.message().contains("bit-packed run wants"), "{}", error.message());
    }

    #[test]
    fn a_repeated_run_whose_value_is_missing_is_an_error() {
        let bytes = varint(20 << 1);
        let mut out = Vec::new();
        let error = Hybrid::new(&bytes, 8).expect("allowed").read(&mut out, 20).unwrap_err();
        assert!(error.message().contains("repeated run wants"), "{}", error.message());
    }

    #[test]
    fn a_width_wider_than_a_u32_is_refused_before_anything_is_read() {
        let error = Hybrid::new(&[], 33).unwrap_err();
        assert!(error.message().contains("nothing in parquet is wider"), "{}", error.message());
    }

    #[test]
    fn every_prefix_of_a_stream_is_an_error_rather_than_a_panic() {
        let mut bytes = repeat(300, 1, 1);
        bytes.extend(packed(&[1, 0, 1, 1, 0, 0, 1, 1], 1));
        bytes.extend(packed(&(0..64).map(|i| i % 2).collect::<Vec<_>>(), 1));
        for cut in 0..bytes.len() {
            let mut out = Vec::new();
            let result = Hybrid::new(&bytes[..cut], 1).expect("allowed").read(&mut out, 372);
            assert!(result.is_err(), "a stream cut at {cut} produced 372 values anyway");
        }
    }

    #[test]
    fn the_width_a_level_needs_is_the_bits_its_maximum_occupies() {
        assert_eq!(width_for(0), 0, "a required column");
        assert_eq!(width_for(1), 1, "a flat optional column, which is most of hits");
        assert_eq!(width_for(2), 2);
        assert_eq!(width_for(3), 2);
        assert_eq!(width_for(4), 3);
        assert_eq!(width_for(255), 8);
        assert_eq!(width_for(256), 9);
    }
}

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

    /// Whether the next `count` values are one repeat of `value`, consuming them if they are.
    ///
    /// This is the question a page of definition levels is asked before anything else, because a
    /// page of an optional column where nothing is null is written as a single repeated run and the
    /// run header says so in two bytes. Answering from the header rather than from the values means
    /// there is no vector of levels to allocate, none to count and none to walk again when the
    /// values are placed, which for a wide file with few nulls is most of what reading a level
    /// stream costs.
    ///
    /// A false answer leaves the reader on the run it just looked at rather than rewinding, so the
    /// caller can go straight on to [`Hybrid::read`] and the header is not parsed twice.
    ///
    /// # Errors
    ///
    /// If the run header or the value behind it runs off the end of the stream.
    pub(crate) fn whole_run_of(&mut self, value: u32, count: usize) -> Result<bool> {
        if self.run == Run::Done {
            self.next_run()?;
        }
        match self.run {
            Run::Repeat { value: found, left } if found == value && left >= count => {
                self.run = if left == count {
                    Run::Done
                } else {
                    Run::Repeat { value, left: left - count }
                };
                Ok(true)
            }
            _ => Ok(false),
        }
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
        // Narrowing back to 32 bits loses nothing: `Hybrid::new` refuses a width past 32, so
        // every value the unpacker produces here already fits.
        unpack(&self.bytes[start..], self.width, done, count, |value| out.push(value as u32));
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

/// Reads `count` bit packed values of `width` bits each, starting `skip` values into `bytes`.
///
/// Little endian at the bit level: the first value is in the low bits of the first byte and a value
/// that does not fit carries into the low bits of the next one. Parquet uses this same packing in
/// two places that look unrelated, the hybrid encoding's packed runs and the miniblocks of the
/// delta encodings, which is why it is a free function rather than a method on either.
///
/// The values go to a closure rather than into a vector because the two callers want them in
/// different widths. Levels and dictionary indices are never wider than 32 bits and are held that
/// way, and a delta of two 64 bit integers needs all 64.
///
/// Bits past the end of `bytes` read as zero rather than as an error. The last group of a run is
/// padded out to eight values whether or not the file has the bytes for them, and the values the
/// caller did not ask for are the ones the padding covers.
///
/// # It reads the stream once, not once per value
///
/// The obvious way to write this is to work out which byte each value starts in and read a window
/// around it, and that is what this was. It costs nine bounds checked byte loads and nine shifts a
/// value, and for a ten million row dictionary column that is ninety million loads over bytes that
/// were already in a register from the value before.
///
/// So the bits go through a 128 bit accumulator instead. Eight bytes come in at a time, values come
/// off the low end, and a byte is read once no matter how many values it holds part of. At sixteen
/// bits, which is what a `hits` dictionary index is, that is one load per four values rather than
/// nine per value.
pub(crate) fn unpack(
    bytes: &[u8],
    width: u8,
    skip: usize,
    count: usize,
    mut push: impl FnMut(u64),
) {
    if width == 0 {
        // Legal, and it means the maximum value is zero, so the run occupies no bytes at all. A
        // column that is required has a maximum definition level of zero and every page of it takes
        // this branch. A delta miniblock whose values are all the block minimum takes it too.
        for _ in 0..count {
            push(0);
        }
        return;
    }
    let width = u32::from(width);
    let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
    let bit = skip * width as usize;
    let mut at = bit / 8;
    // The bits already read and not yet handed out, low end first, and how many of them are real.
    // Never more than 128 and never refilled above 64, so a whole word always has room.
    let mut held = 0u128;
    let mut bits = 0u32;

    // Written as a macro rather than a closure because the accumulator is three locals and a
    // closure over them borrows all three for as long as `push` is alive.
    macro_rules! fill {
        () => {
            while bits <= 64 {
                if let Some(word) = bytes.get(at..at + 8) {
                    let word = u64::from_le_bytes(word.try_into().expect("eight bytes"));
                    held |= u128::from(word) << bits;
                    at += 8;
                    bits += 64;
                } else if let Some(&byte) = bytes.get(at) {
                    held |= u128::from(byte) << bits;
                    at += 1;
                    bits += 8;
                } else {
                    // Past the end, where the format says a reader finds zeros. The accumulator is
                    // already zero up there, so claiming the bits is the whole of it, and claiming
                    // all of them stops this loop running again per value at the end of a run.
                    bits = 128;
                    break;
                }
            }
        };
    }

    fill!();
    // The first value does not have to start on a byte boundary, so drop what belongs to the values
    // being skipped over. At most seven bits, and the fill above left at least fifty seven.
    let ahead = (bit % 8) as u32;
    held >>= ahead;
    bits -= ahead;

    for _ in 0..count {
        if bits < width {
            fill!();
        }
        push((held as u64) & mask);
        held >>= width;
        bits -= width;
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
    use super::{Hybrid, unpack, width_for};

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
    fn a_whole_page_of_ones_is_answered_from_the_run_header() {
        let bytes = repeat(20_000, 1, 1);
        let mut stream = Hybrid::new(&bytes, 1).expect("a width this narrow is allowed");
        assert!(stream.whole_run_of(1, 20_000).expect("the header reads"));
    }

    #[test]
    fn a_run_that_is_short_of_the_page_is_not_the_whole_page() {
        // The run says ten thousand and the page holds twenty, so something else follows and the
        // levels have to be decoded after all. Saying yes here would put ten thousand nulls in the
        // wrong place and never report a thing.
        let mut bytes = repeat(10_000, 1, 1);
        bytes.extend(repeat(10_000, 0, 1));
        let mut stream = Hybrid::new(&bytes, 1).expect("a width this narrow is allowed");
        assert!(!stream.whole_run_of(1, 20_000).expect("the header reads"));
    }

    #[test]
    fn a_no_that_looked_at_a_header_does_not_lose_the_run_it_looked_at() {
        // The reader is left standing on the run rather than rewound, so a caller that asked and
        // was told no goes straight on to reading and gets every value, including the ones in the
        // run whose header was already eaten.
        let mut bytes = repeat(16, 1, 1);
        bytes.extend(repeat(8, 0, 1));
        let mut stream = Hybrid::new(&bytes, 1).expect("a width this narrow is allowed");
        assert!(!stream.whole_run_of(1, 24).expect("the header reads"));
        let mut out = Vec::new();
        stream.read(&mut out, 24).expect("the stream holds this many values");
        assert_eq!(out, [vec![1u32; 16], vec![0u32; 8]].concat());
    }

    #[test]
    fn a_run_longer_than_the_page_leaves_the_rest_where_it_was() {
        // One run can cover more than the page asks for, and what is left of it belongs to
        // whatever is read next rather than being thrown away with the run.
        let bytes = repeat(20, 1, 1);
        let mut stream = Hybrid::new(&bytes, 1).expect("a width this narrow is allowed");
        assert!(stream.whole_run_of(1, 12).expect("the header reads"));
        let mut out = Vec::new();
        stream.read(&mut out, 8).expect("the stream holds this many values");
        assert_eq!(out, vec![1u32; 8]);
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

    /// The obvious unpacker, one window per value, kept as the thing the real one has to match.
    ///
    /// This is what `unpack` used to be, and it is short enough to read and slow enough that nobody
    /// would ship it. Leaving it here is cheaper than arguing about whether the accumulator in the
    /// real one is right: the two are compared over every width, every starting offset and every
    /// length of buffer below.
    fn window(bytes: &[u8], width: u8, skip: usize, count: usize) -> Vec<u64> {
        if width == 0 {
            return vec![0; count];
        }
        let width = usize::from(width);
        let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
        let mut bit = skip * width;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let mut value = 0u128;
            for i in 0..9 {
                if let Some(&byte) = bytes.get(bit / 8 + i) {
                    value |= u128::from(byte) << (8 * i);
                }
            }
            out.push(((value >> (bit % 8)) as u64) & mask);
            bit += width;
        }
        out
    }

    #[test]
    fn the_accumulator_reads_the_same_values_a_window_per_value_would() {
        // Every width the format allows, started at every offset inside the first two groups, over
        // bytes that mean nothing in particular. The widths past 32 are the delta encodings, which
        // call this directly rather than through `Hybrid`.
        let bytes: Vec<u8> = (0..256u32).map(|i| (i.wrapping_mul(167) >> 1) as u8).collect();
        for width in 1..=64u8 {
            for skip in 0..17 {
                let wanted = window(&bytes, width, skip, 40);
                let mut got = Vec::new();
                unpack(&bytes, width, skip, 40, |value| got.push(value));
                assert_eq!(got, wanted, "width {width} from {skip}");
            }
        }
    }

    #[test]
    fn a_read_that_runs_off_the_end_finds_zeroes_rather_than_stopping() {
        // The last group of a packed run is padded to eight values whether or not the file has the
        // bytes, so a reader that asked for the padding gets zeros. This is the case the
        // accumulator has to get right in two places at once, since it claims the bits past the end
        // in one go rather than one value at a time.
        let bytes: Vec<u8> = (0..24u8).collect();
        for width in [1u8, 3, 7, 8, 12, 16, 17, 32, 64] {
            for cut in 0..=bytes.len() {
                let short = &bytes[..cut];
                let wanted = window(short, width, 0, 30);
                let mut got = Vec::new();
                unpack(short, width, 0, 30, |value| got.push(value));
                assert_eq!(got, wanted, "width {width} over {cut} bytes");
            }
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

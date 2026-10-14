//! The encodings a version two file falls back to for a column chunk with no dictionary, written
//! byte for byte the way the pin writes them.
//!
//! An integer of 32 or 64 bits is `DELTA_BINARY_PACKED`, a string or a blob is
//! `DELTA_LENGTH_BYTE_ARRAY`, and a float or a double is `BYTE_STREAM_SPLIT`. The pin's delta
//! blocks hold 2048 values in eight miniblocks of 256, and each data page starts a new stream, so
//! a page always begins with the header, even one that holds no value at all.

use crate::runs::varint;

/// How many values a block of the delta encoding holds, and how many miniblocks it is cut into.
const BLOCK: usize = 2048;
const MINIBLOCKS: usize = 8;
const PER_MINIBLOCK: usize = BLOCK / MINIBLOCKS;

/// Appends `values` delta binary packed.
pub(crate) fn binary_packed(values: &[i64], out: &mut Vec<u8>) {
    varint(BLOCK as u64, out);
    varint(MINIBLOCKS as u64, out);
    varint(values.len() as u64, out);
    varint(zigzag(values.first().copied().unwrap_or_default()), out);
    let mut deltas = Vec::with_capacity(BLOCK);
    for pair in values.windows(2) {
        deltas.push(pair[1].wrapping_sub(pair[0]));
        if deltas.len() == BLOCK {
            block(&deltas, out);
            deltas.clear();
        }
    }
    if !deltas.is_empty() {
        block(&deltas, out);
    }
}

/// Appends one block of differences: the smallest of them, the bit width of each miniblock, and
/// then each miniblock that holds a difference, packed. The last miniblock is padded with zeroes,
/// and the widths of the miniblocks past the last difference are zero.
fn block(deltas: &[i64], out: &mut Vec<u8>) {
    let min = deltas.iter().copied().min().unwrap_or_default();
    let above = deltas.iter().map(|delta| delta.wrapping_sub(min) as u64).collect::<Vec<_>>();
    let used = above.len().div_ceil(PER_MINIBLOCK);
    let mut widths = [0_u8; MINIBLOCKS];
    for (width, miniblock) in widths.iter_mut().zip(above.chunks(PER_MINIBLOCK)) {
        let most = miniblock.iter().copied().max().unwrap_or_default();
        *width = (u64::BITS - most.leading_zeros()) as u8;
    }
    varint(zigzag(min), out);
    out.extend_from_slice(&widths);
    for (width, miniblock) in widths.iter().zip(above.chunks(PER_MINIBLOCK)).take(used) {
        pack(miniblock, *width, out);
    }
}

/// Appends a miniblock's values packed at `width` bits each, the lowest bits first, padded out
/// to a full miniblock.
fn pack(values: &[u64], width: u8, out: &mut Vec<u8>) {
    let width = u32::from(width);
    let start = out.len();
    out.resize(start + PER_MINIBLOCK * width as usize / 8, 0);
    let bytes = &mut out[start..];
    for (at, value) in values.iter().enumerate() {
        let mut bit = at * width as usize;
        let mut left = width;
        let mut value = *value;
        while left > 0 {
            let taken = (8 - (bit % 8) as u32).min(left);
            bytes[bit / 8] |= ((value & ((1 << taken) - 1)) << (bit % 8)) as u8;
            value = value.checked_shr(taken).unwrap_or_default();
            bit += taken as usize;
            left -= taken;
        }
    }
}

/// Appends `values` delta length byte array: their lengths delta binary packed, and then their
/// bytes one after another.
pub(crate) fn length_byte_array(values: &[&[u8]], out: &mut Vec<u8>) {
    let lengths = values.iter().map(|value| value.len() as i64).collect::<Vec<_>>();
    binary_packed(&lengths, out);
    for value in values {
        out.extend_from_slice(value);
    }
}

/// Appends `plain`, the plain encoding of values of `width` bytes each, as a byte stream split:
/// the first byte of every value, then the second byte of every value, and so on.
pub(crate) fn stream_split(plain: &[u8], width: usize, out: &mut Vec<u8>) {
    for stream in 0..width {
        out.extend(plain.chunks_exact(width).map(|value| value[stream]));
    }
}

/// Makes a small negative number a small unsigned one.
fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

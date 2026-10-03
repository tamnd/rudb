//! Gzip, which is DEFLATE in a thin wrapper. Decompression only.
//!
//! DEFLATE is RFC 1951 and the wrapper is RFC 1952. A gzip file is one or more members end to end,
//! each a header, a DEFLATE stream and a trailer with the CRC-32 and the length of what the stream
//! decompressed to. `cat a.gz b.gz` is a legal gzip file, so a reader that stops at the first
//! member reads a fraction of it.
//!
//! The Huffman decode is a table indexed by the next [`FAST`] bits of the stream, which answers
//! almost every symbol in one look, with a walk of the canonical code one bit at a time for the
//! few codes longer than that. Every literal and length code in a typical stream is shorter than
//! ten bits, so the walk is the rare path and it is kept simple rather than fast.

use rudb_common::{Error, Result};

/// The two bytes every gzip member starts with.
pub const MAGIC: [u8; 2] = [0x1f, 0x8b];

/// How many bits the first look of a Huffman decode takes.
const FAST: u32 = 10;

/// The longest code DEFLATE allows.
const LONGEST: usize = 15;

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The order a dynamic block writes the lengths of the code length code in.
const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// The CRC-32 table, for the polynomial gzip and zip and PNG all use.
const CRC: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

/// Whether `input` starts the way a gzip member does.
#[must_use]
pub fn is_gzip(input: &[u8]) -> bool {
    input.starts_with(&MAGIC)
}

/// Decompresses every member of a gzip file.
///
/// # Errors
///
/// If the input is not gzip, if a member is truncated or malformed, or if a member's trailer does
/// not match what came out of it.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len().saturating_mul(4));
    decompress_onto(input, &mut out)?;
    Ok(out)
}

/// [`decompress`] onto the end of a buffer the caller keeps.
///
/// # Errors
///
/// As [`decompress`]. Some of the output may have been appended when it fails.
pub fn decompress_onto(input: &[u8], out: &mut Vec<u8>) -> Result<()> {
    if !is_gzip(input) {
        return Err(Error::io("Input is not a GZIP stream"));
    }
    let mut rest = input;
    // Whatever follows the last member and is not another one is padding, which gzip itself warns
    // about and otherwise ignores. A tape block of zeros after the file is the usual case.
    while is_gzip(rest) {
        rest = member(rest, out)?;
    }
    Ok(())
}

/// Decompresses one member onto the end of `out` and gives back whatever follows it.
fn member<'a>(input: &'a [u8], out: &mut Vec<u8>) -> Result<&'a [u8]> {
    if input.len() < 18 {
        return Err(truncated());
    }
    if input[2] != 8 {
        return Err(Error::io(format!(
            "a gzip member compressed with method {}, not DEFLATE",
            input[2]
        )));
    }
    let flags = input[3];
    let mut at = 10;
    if flags & 0x04 != 0 {
        let extra = usize::from(u16::from_le_bytes([input[at], input[at + 1]]));
        at += 2 + extra;
    }
    for flag in [0x08, 0x10] {
        if flags & flag != 0 {
            let end = input.get(at..).and_then(|tail| tail.iter().position(|&byte| byte == 0));
            at += end.ok_or_else(truncated)? + 1;
        }
    }
    if flags & 0x02 != 0 {
        at += 2;
    }
    let body = input.get(at..).ok_or_else(truncated)?;
    let start = out.len();
    let mut bits = Bits::new(body);
    inflate(&mut bits, out, start)?;
    let end = at + bits.consumed();
    let trailer = input.get(end..end + 8).ok_or_else(truncated)?;
    let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    if crc != crc32(&out[start..]) {
        return Err(Error::io("a gzip member whose CRC-32 does not match what it decompressed to"));
    }
    if size != (out.len() - start) as u32 {
        return Err(Error::io("a gzip member whose length does not match what it decompressed to"));
    }
    Ok(&input[end + 8..])
}

/// The CRC-32 of `bytes`.
fn crc32(bytes: &[u8]) -> u32 {
    let mut c = !0u32;
    for &byte in bytes {
        c = CRC[((c ^ u32::from(byte)) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

fn truncated() -> Error {
    Error::io("a gzip stream that ends before it says it does")
}

fn corrupt(what: &str) -> Error {
    Error::io(format!("a malformed DEFLATE stream: {what}"))
}

/// The stream as bits, least significant first, which is the order DEFLATE packs them in.
struct Bits<'a> {
    input: &'a [u8],
    at: usize,
    buffer: u64,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, at: 0, buffer: 0, count: 0 }
    }

    fn refill(&mut self) {
        while self.count <= 56 {
            let Some(&byte) = self.input.get(self.at) else { break };
            self.buffer |= u64::from(byte) << self.count;
            self.count += 8;
            self.at += 1;
        }
    }

    fn take(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        if self.count < n {
            self.refill();
            if self.count < n {
                return Err(truncated());
            }
        }
        let value = (self.buffer & ((1u64 << n) - 1)) as u32;
        self.buffer >>= n;
        self.count -= n;
        Ok(value)
    }

    /// Drops what is left of the current byte, which a stored block and the end of a stream do.
    fn align(&mut self) {
        let partial = self.count % 8;
        self.buffer >>= partial;
        self.count -= partial;
    }

    /// How many whole bytes have been read, counting the ones still in the buffer as unread.
    fn consumed(&self) -> usize {
        self.at - (self.count / 8) as usize
    }

    /// Moves to a byte position, emptying the buffer.
    fn seek(&mut self, at: usize) {
        self.at = at;
        self.buffer = 0;
        self.count = 0;
    }
}

/// A canonical Huffman code, as the counts of each length and the symbols in code order, with a
/// table for the codes short enough to look up.
struct Huffman {
    count: [u16; LONGEST + 1],
    symbol: Vec<u16>,
    fast: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self> {
        let mut count = [0u16; LONGEST + 1];
        for &length in lengths {
            count[usize::from(length)] += 1;
        }
        count[0] = 0;
        let mut left: i32 = 1;
        for &n in &count[1..] {
            left = (left << 1) - i32::from(n);
            if left < 0 {
                return Err(corrupt("an over-subscribed Huffman code"));
            }
        }
        let mut offset = [0usize; LONGEST + 2];
        for length in 1..=LONGEST {
            offset[length + 1] = offset[length] + usize::from(count[length]);
        }
        let mut symbol = vec![0u16; offset[LONGEST + 1]];
        for (s, &length) in lengths.iter().enumerate() {
            if length != 0 {
                symbol[offset[usize::from(length)]] = s as u16;
                offset[usize::from(length)] += 1;
            }
        }
        let mut next = [0u32; LONGEST + 1];
        let mut code = 0u32;
        for length in 1..=LONGEST {
            code = (code + u32::from(count[length - 1])) << 1;
            next[length] = code;
        }
        let mut fast = vec![0u16; 1 << FAST];
        for (s, &length) in lengths.iter().enumerate() {
            let length = u32::from(length);
            if length == 0 {
                continue;
            }
            let code = next[length as usize];
            next[length as usize] += 1;
            if length > FAST {
                continue;
            }
            let reversed = (code.reverse_bits() >> (32 - length)) as usize;
            let entry = ((s as u16) << 4) | length as u16;
            let mut at = reversed;
            while at < fast.len() {
                fast[at] = entry;
                at += 1 << length;
            }
        }
        Ok(Self { count, symbol, fast })
    }

    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16> {
        if bits.count < LONGEST as u32 {
            bits.refill();
        }
        let entry = self.fast[(bits.buffer & ((1 << FAST) - 1)) as usize];
        let length = u32::from(entry & 15);
        if entry != 0 && length <= bits.count {
            bits.buffer >>= length;
            bits.count -= length;
            return Ok(entry >> 4);
        }
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for &n in &self.count[1..] {
            code |= bits.take(1)? as i32;
            let n = i32::from(n);
            if code - n < first {
                return Ok(self.symbol[(index + code - first) as usize]);
            }
            index += n;
            first = (first + n) << 1;
            code <<= 1;
        }
        Err(corrupt("a code that is in no Huffman table"))
    }
}

/// Decompresses one DEFLATE stream onto `out`, whose bytes from `start` on are this stream's and
/// the only ones a back reference may reach.
fn inflate(bits: &mut Bits<'_>, out: &mut Vec<u8>, start: usize) -> Result<()> {
    loop {
        let last = bits.take(1)?;
        match bits.take(2)? {
            0 => stored(bits, out)?,
            1 => {
                let mut lengths = [0u8; 288];
                lengths[..144].fill(8);
                lengths[144..256].fill(9);
                lengths[256..280].fill(7);
                lengths[280..].fill(8);
                let literal = Huffman::new(&lengths)?;
                let distance = Huffman::new(&[5u8; 30])?;
                codes(bits, out, start, &literal, &distance)?;
            }
            2 => {
                let (literal, distance) = dynamic(bits)?;
                codes(bits, out, start, &literal, &distance)?;
            }
            _ => return Err(corrupt("a block of type 3")),
        }
        if last == 1 {
            bits.align();
            return Ok(());
        }
    }
}

fn stored(bits: &mut Bits<'_>, out: &mut Vec<u8>) -> Result<()> {
    bits.align();
    let at = bits.consumed();
    bits.seek(at);
    let header = bits.input.get(at..at + 4).ok_or_else(truncated)?;
    let length = u16::from_le_bytes([header[0], header[1]]);
    let check = u16::from_le_bytes([header[2], header[3]]);
    if length != !check {
        return Err(corrupt("a stored block whose length does not match its complement"));
    }
    let body = bits.input.get(at + 4..at + 4 + usize::from(length)).ok_or_else(truncated)?;
    out.extend_from_slice(body);
    bits.seek(at + 4 + usize::from(length));
    Ok(())
}

fn dynamic(bits: &mut Bits<'_>) -> Result<(Huffman, Huffman)> {
    let literals = bits.take(5)? as usize + 257;
    let distances = bits.take(5)? as usize + 1;
    let code_lengths = bits.take(4)? as usize + 4;
    if literals > 286 || distances > 30 {
        return Err(corrupt("more codes than DEFLATE has"));
    }
    let mut lengths = [0u8; 19];
    for &at in &ORDER[..code_lengths] {
        lengths[at] = bits.take(3)? as u8;
    }
    let lengths_code = Huffman::new(&lengths)?;
    let mut lengths = vec![0u8; literals + distances];
    let mut at = 0;
    while at < lengths.len() {
        let symbol = lengths_code.decode(bits)?;
        let (value, repeat) = match symbol {
            0..=15 => (symbol as u8, 1),
            16 => {
                let previous =
                    *lengths[..at].last().ok_or_else(|| corrupt("a repeat of nothing"))?;
                (previous, 3 + bits.take(2)? as usize)
            }
            17 => (0, 3 + bits.take(3)? as usize),
            _ => (0, 11 + bits.take(7)? as usize),
        };
        let end = at + repeat;
        if end > lengths.len() {
            return Err(corrupt("code lengths that run past the codes"));
        }
        lengths[at..end].fill(value);
        at = end;
    }
    if lengths[256] == 0 {
        return Err(corrupt("a block with no end of block code"));
    }
    Ok((Huffman::new(&lengths[..literals])?, Huffman::new(&lengths[literals..])?))
}

fn codes(
    bits: &mut Bits<'_>,
    out: &mut Vec<u8>,
    start: usize,
    literal: &Huffman,
    distance: &Huffman,
) -> Result<()> {
    loop {
        let symbol = literal.decode(bits)?;
        if symbol < 256 {
            out.push(symbol as u8);
            continue;
        }
        if symbol == 256 {
            return Ok(());
        }
        let symbol = usize::from(symbol - 257);
        if symbol >= LENGTH_BASE.len() {
            return Err(corrupt("a length code past the last one"));
        }
        let length =
            usize::from(LENGTH_BASE[symbol]) + bits.take(u32::from(LENGTH_EXTRA[symbol]))? as usize;
        let symbol = usize::from(distance.decode(bits)?);
        if symbol >= DISTANCE_BASE.len() {
            return Err(corrupt("a distance code past the last one"));
        }
        let back = usize::from(DISTANCE_BASE[symbol])
            + bits.take(u32::from(DISTANCE_EXTRA[symbol]))? as usize;
        if back > out.len() - start {
            return Err(corrupt("a distance that reaches back before the stream"));
        }
        let from = out.len() - back;
        if back >= length {
            out.extend_from_within(from..from + length);
        } else {
            for at in from..from + length {
                out.push(out[at]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{crc32, decompress};

    #[test]
    fn the_crc_is_the_one_everybody_uses() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn something_that_is_not_gzip_says_so() {
        let error = decompress(b"{\"a\":1}").unwrap_err();
        assert!(error.message().contains("not a GZIP stream"), "{}", error.message());
    }
}

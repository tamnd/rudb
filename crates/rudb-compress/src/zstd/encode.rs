//! Zstandard compression, writing frames the decoder next to it reads.
//!
//! A frame here is one segment with its content size in the header and no checksum, which is what
//! the pin's Parquet writer asks its zstd for. The input is cut into blocks of 128 KiB, and each
//! block is whichever is shortest of its bytes as they are, one byte repeated, or a compressed
//! block.
//!
//! A compressed block is found the way [`crate::deflate`] finds one: a hash of the next four bytes
//! leads to a chain of the earlier places they occurred, and the longest match on the chain wins,
//! with a look one byte further on at the levels that can afford it. A match may reach back into
//! an earlier block of the frame. The literals are Huffman coded when that is shorter, which the
//! `literals` module decides. Each of the three codes of the sequences is coded with the table the
//! format predefines, with one code repeated, or with a table the block describes, whichever comes
//! to the fewest bits, and a match at one of the three most recent offsets is coded as that.
//!
//! `COMPRESSION_LEVEL` decides how far down a chain the search goes and how much of the frame
//! behind it a match may reach, so a higher level is slower and smaller, as it is in the pin, but
//! the bytes are not the pin's.

use super::MAGIC;
use super::block::{
    LENGTH_BASE, LENGTH_BITS, LENGTH_DEFAULT, MATCH_BASE, MATCH_BITS, MATCH_DEFAULT, OFFSET_DEFAULT,
};

/// The level the pin compresses at when `COMPRESSION_LEVEL` is not given.
pub const DEFAULT_LEVEL: i64 = 3;

/// The most a block may hold, before or after compression.
const BLOCK: usize = 1 << 17;
/// The shortest match the search looks for, which is the width of what it hashes.
const MIN_MATCH: usize = 4;
/// The bits of a hash, so the number of chains.
const HASH_BITS: u32 = 16;
/// Where a chain ends.
const NONE: u32 = u32::MAX;

/// Compresses `input` into one frame, searching as hard as `level` says.
#[must_use]
pub fn compress(input: &[u8], level: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 2 + 16);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    let size = input.len() as u64;
    let (flag, width) = match size {
        0..256 => (0, 1),
        256..65_792 => (1, 2),
        65_792..=0xFFFF_FFFF => (2, 4),
        _ => (3, 8),
    };
    out.push((flag << 6) | 0x20);
    let written = if width == 2 { size - 256 } else { size };
    out.extend_from_slice(&written.to_le_bytes()[..width]);
    if input.is_empty() {
        // One empty block of bytes as they are, which is the least a frame can hold.
        out.extend_from_slice(&[1, 0, 0]);
        return out;
    }
    let mut finder = Finder::new(input, Search::at(level));
    let mut start = 0;
    while start < input.len() {
        let end = (start + BLOCK).min(input.len());
        block(&mut finder, start, end, &mut out);
        start = end;
    }
    out
}

/// Appends the block of `input[start..end]`, the last one when it ends the input.
fn block(finder: &mut Finder<'_>, start: usize, end: usize, out: &mut Vec<u8>) {
    let bytes = &finder.input[start..end];
    let last = u32::from(end == finder.input.len());
    let header = |kind: u32, size: usize, out: &mut Vec<u8>| {
        let word = last | (kind << 1) | ((size as u32) << 3);
        out.extend_from_slice(&word.to_le_bytes()[..3]);
    };
    if bytes.len() > 1 && bytes.iter().all(|&byte| byte == bytes[0]) {
        header(1, bytes.len(), out);
        out.push(bytes[0]);
        return;
    }
    let recent = finder.recent;
    let (literals, sequences) = finder.parse(start, end);
    if !sequences.is_empty() {
        let mut body = Vec::with_capacity(bytes.len());
        super::literals::write(&literals, &mut body);
        code_sequences(&sequences, &mut body);
        if body.len() < bytes.len() {
            header(2, body.len(), out);
            out.extend_from_slice(&body);
            return;
        }
    }
    // The decoder never sees these sequences, so it never moves its recent offsets either.
    finder.recent = recent;
    header(0, bytes.len(), out);
    out.extend_from_slice(bytes);
}

/// How hard a level searches.
#[derive(Debug, Clone, Copy)]
struct Search {
    /// How many earlier places on a chain are tried.
    chain: u32,
    /// Whether a match is held back when the next byte starts a longer one.
    lazy: bool,
    /// A match this long is taken without looking for a longer one.
    nice: usize,
    /// The bits of how far back a match may reach.
    window: u32,
}

impl Search {
    fn at(level: i64) -> Self {
        let (chain, lazy, nice, window) = match level {
            ..=0 => (1, false, 32, 17),
            1..=2 => (4, false, 32, 18),
            3..=5 => (16, true, 64, 20),
            6..=12 => (64, true, 128, 21),
            _ => (512, true, 1024, 22),
        };
        Self { chain, lazy, nice, window }
    }
}

/// One copy, after the literals that come before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sequence {
    literals: u32,
    /// The offset as the block codes it: one of the recent offsets, or three more than the
    /// distance.
    offset: u32,
    length: u32,
}

/// The hash chains over the whole input, which outlive a block so a match can reach behind it.
struct Finder<'a> {
    input: &'a [u8],
    search: Search,
    head: Vec<u32>,
    /// The place before each one with the same hash, kept for a window's worth of places.
    previous: Vec<u32>,
    mask: usize,
    /// The first place not in the chains yet.
    indexed: usize,
    /// The three most recent offsets, as the decoder will have them at the place being parsed.
    recent: [u64; 3],
}

impl<'a> Finder<'a> {
    fn new(input: &'a [u8], search: Search) -> Self {
        let window = (1_usize << search.window).min(input.len().next_power_of_two());
        Self {
            input,
            search,
            head: vec![NONE; 1 << HASH_BITS],
            previous: vec![NONE; window],
            mask: window - 1,
            indexed: 0,
            recent: [1, 4, 8],
        }
    }

    fn hash(&self, at: usize) -> usize {
        let word = u32::from_le_bytes(self.input[at..at + 4].try_into().unwrap_or_default());
        (word.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
    }

    /// Puts every place before `upto` on its chain.
    fn index_to(&mut self, upto: usize) {
        let upto = upto.min(self.input.len().saturating_sub(MIN_MATCH - 1));
        while self.indexed < upto {
            let at = self.indexed;
            let h = self.hash(at);
            self.previous[at & self.mask] = self.head[h];
            self.head[h] = at as u32;
            self.indexed += 1;
        }
    }

    /// The longest match for the bytes at `at` that ends by `end`, as its length and distance.
    fn best(&mut self, at: usize, end: usize) -> (usize, usize) {
        let limit = end - at;
        if limit < MIN_MATCH {
            return (0, 0);
        }
        self.index_to(at);
        let input = self.input;
        let mut best = (0, 0);
        let mut candidate = self.head[self.hash(at)];
        let mut tries = self.search.chain;
        while candidate != NONE && tries > 0 {
            let from = candidate as usize;
            let distance = at - from;
            if distance > self.mask {
                break;
            }
            if input[from + best.0] == input[at + best.0] {
                let length =
                    input[from..].iter().zip(&input[at..end]).take_while(|(a, b)| a == b).count();
                if length > best.0 {
                    best = (length, distance);
                    if length == limit || length >= self.search.nice {
                        break;
                    }
                }
            }
            let next = self.previous[from & self.mask];
            if next == NONE || next >= candidate {
                break;
            }
            candidate = next;
            tries -= 1;
        }
        // A recent offset costs a few bits where a new one costs a few bytes, so one that comes
        // within a byte of the longest match wins.
        for distance in self.recent {
            let distance = distance as usize;
            if distance == 0 || distance > at || distance == best.1 {
                continue;
            }
            let from = at - distance;
            let length =
                input[from..].iter().zip(&input[at..end]).take_while(|(a, b)| a == b).count();
            if length >= MIN_MATCH && length + 1 >= best.0 {
                best = (length, distance);
            }
        }
        if best.0 < MIN_MATCH { (0, 0) } else { best }
    }

    /// Splits `input[start..end]` into the literals it stores and the copies that spend them.
    fn parse(&mut self, start: usize, end: usize) -> (Vec<u8>, Vec<Sequence>) {
        let input = self.input;
        let mut literals = Vec::new();
        let mut sequences = Vec::new();
        let (mut at, mut anchor) = (start, start);
        while at + MIN_MATCH <= end {
            let (mut length, mut distance) = self.best(at, end);
            if length == 0 {
                at += 1;
                continue;
            }
            while self.search.lazy && length < self.search.nice {
                let (longer, further) = self.best(at + 1, end);
                if longer <= length {
                    break;
                }
                at += 1;
                (length, distance) = (longer, further);
            }
            literals.extend_from_slice(&input[anchor..at]);
            let offset = self.offset(distance as u64, at > anchor);
            sequences.push(Sequence {
                literals: (at - anchor) as u32,
                offset,
                length: length as u32,
            });
            at += length;
            anchor = at;
        }
        literals.extend_from_slice(&input[anchor..end]);
        (literals, sequences)
    }

    /// How a sequence codes `distance`, moving the recent offsets on the way the decoder will.
    fn offset(&mut self, distance: u64, literals: bool) -> u32 {
        let [first, second, third] = self.recent;
        let coded = match distance {
            _ if literals && distance == first => 1,
            _ if distance == second => 1 + u64::from(literals),
            _ if distance == third => 2 + u64::from(literals),
            _ if !literals && distance + 1 == first => 3,
            _ => distance + 3,
        };
        let resolved = super::block::resolve(coded, !literals, &mut self.recent);
        debug_assert_eq!(resolved.ok(), Some(distance));
        coded as u32
    }
}

/// Appends a sequences section, with the cheapest table for each of its three codes.
///
/// The bitstream is written from the last sequence to the first, so a decoder reading it from its
/// end meets the first sequence first. Each sequence's three codes move the three states on and
/// its extra bits follow, in the order libzstd writes them, which is the reverse of the order
/// [`super::block`] reads them.
fn code_sequences(sequences: &[Sequence], out: &mut Vec<u8>) {
    let count = sequences.len();
    if count < 128 {
        out.push(count as u8);
    } else if count < 0x7F00 {
        out.extend_from_slice(&[((count >> 8) + 128) as u8, count as u8]);
    } else {
        let rest = (count - 0x7F00) as u16;
        out.push(255);
        out.extend_from_slice(&rest.to_le_bytes());
    }
    let codes: Vec<[Code; 3]> = sequences
        .iter()
        .map(|sequence| {
            let literal = Code::of(&LENGTH_BASE, &LENGTH_BITS, sequence.literals);
            let matched = Code::of(&MATCH_BASE, &MATCH_BITS, sequence.length);
            let value = sequence.offset;
            let bits = 31 - value.leading_zeros();
            let offset = Code { symbol: bits as usize, extra: value - (1 << bits), bits };
            [literal, matched, offset]
        })
        .collect();
    let symbols =
        |which: usize| -> Vec<usize> { codes.iter().map(|code| code[which].symbol).collect() };
    let lengths = Section::pick(&symbols(0), &LENGTH_DEFAULT, 6, 9);
    let matches = Section::pick(&symbols(1), &MATCH_DEFAULT, 6, 9);
    let offsets = Section::pick(&symbols(2), &OFFSET_DEFAULT, 5, 8);
    out.push(lengths.mode << 6 | offsets.mode << 4 | matches.mode << 2);
    for section in [&lengths, &offsets, &matches] {
        out.extend_from_slice(&section.description);
    }
    let (lengths, matches, offsets) = (lengths.table, matches.table, offsets.table);
    let mut bits = Bits::default();
    let [literal, matched, offset] = codes[count - 1];
    let mut length_state = lengths.start(literal.symbol);
    let mut match_state = matches.start(matched.symbol);
    let mut offset_state = offsets.start(offset.symbol);
    bits.extra(literal);
    bits.extra(matched);
    bits.extra(offset);
    for &[literal, matched, offset] in codes[..count - 1].iter().rev() {
        offsets.encode(&mut offset_state, offset.symbol, &mut bits);
        matches.encode(&mut match_state, matched.symbol, &mut bits);
        lengths.encode(&mut length_state, literal.symbol, &mut bits);
        bits.extra(literal);
        bits.extra(matched);
        bits.extra(offset);
    }
    bits.add(match_state, matches.log);
    bits.add(offset_state, offsets.log);
    bits.add(length_state, lengths.log);
    bits.finish(out);
}

/// How a block tables one of the three codes of its sequences.
struct Section {
    /// Two bits of the mode byte: predefined, one code repeated, or described.
    mode: u8,
    /// What goes in the block ahead of the bitstream to name the table.
    description: Vec<u8>,
    table: Encoder,
}

impl Section {
    /// The table that costs the fewest bits for `symbols`, which are never empty.
    ///
    /// A table the block describes gets the accuracy libzstd's `FSE_optimalTableLog` gives it, and
    /// is only chosen when its description and its bits come to less than the predefined table's.
    fn pick(symbols: &[usize], default: &[i32], default_log: u32, top_log: u32) -> Self {
        let mut counts = vec![0_u32; symbols.iter().max().map_or(1, |&top| top + 1)];
        for &symbol in symbols {
            counts[symbol] += 1;
        }
        let top = counts.len() - 1;
        if counts.iter().filter(|&&count| count > 0).count() == 1 {
            return Self { mode: 1, description: vec![top as u8], table: Encoder::rle(top) };
        }
        let high = |value: usize| usize::BITS - 1 - value.leading_zeros();
        let size = symbols.len();
        let fewest = (high(size - 1) + 1).min(high(top) + 2);
        let log = top_log.min(high(size - 1).saturating_sub(2)).max(fewest).clamp(5, top_log);
        let normal = normalize(&counts, log);
        let mut bits = Bits::default();
        describe_table(&normal, log, &mut bits);
        let mut description = Vec::new();
        bits.pad(&mut description);
        let described = cost(&counts, &normal, log) + 8.0 * description.len() as f64;
        if top < default.len() && cost(&counts, default, default_log) <= described {
            return Self {
                mode: 0,
                description: Vec::new(),
                table: Encoder::new(default, default_log),
            };
        }
        Self { mode: 2, description, table: Encoder::new(&normal, log) }
    }
}

/// About how many bits coding `counts` with the table for `normal` takes.
fn cost(counts: &[u32], normal: &[i32], log: u32) -> f64 {
    counts
        .iter()
        .zip(normal)
        .map(|(&count, &share)| {
            f64::from(count) * (f64::from(log) - f64::from(share.max(1)).log2())
        })
        .sum()
}

/// A length or an offset as its code and the extra bits that pick it out of the code's range.
#[derive(Debug, Clone, Copy)]
struct Code {
    symbol: usize,
    extra: u32,
    bits: u32,
}

impl Code {
    fn of(base: &[u32], bits: &[u32], value: u32) -> Self {
        let symbol = base.partition_point(|&start| start <= value) - 1;
        Self { symbol, extra: value - base[symbol], bits: bits[symbol] }
    }
}

/// An FSE coding table, the encoder's side of [`super::fse::Table`].
#[derive(Debug)]
pub(super) struct Encoder {
    log: u32,
    /// The state to move to, by symbol and by what is left of the state after its bits go out.
    states: Vec<u32>,
    /// For each symbol, what gives the number of bits a state sheds and where its states start.
    symbols: Vec<(u32, i32)>,
}

impl Encoder {
    /// Builds the table for a normalized distribution, laid out the way the decoder lays it out.
    pub(super) fn new(counts: &[i32], log: u32) -> Self {
        let size = 1_usize << log;
        let mut spread = vec![0_usize; size];
        let mut starts = vec![0_usize; counts.len()];
        let mut top = size;
        let mut total = 0;
        for (symbol, &count) in counts.iter().enumerate() {
            starts[symbol] = total;
            if count == -1 {
                top -= 1;
                spread[top] = symbol;
                total += 1;
            } else {
                total += count.max(0) as usize;
            }
        }
        let (mask, stride) = (size - 1, (size >> 1) + (size >> 3) + 3);
        let mut at = 0;
        for (symbol, &count) in counts.iter().enumerate() {
            for _ in 0..count.max(0) {
                spread[at] = symbol;
                at = (at + stride) & mask;
                while at >= top {
                    at = (at + stride) & mask;
                }
            }
        }
        let mut states = vec![0; size];
        let mut next = starts.clone();
        for (slot, &symbol) in spread.iter().enumerate() {
            states[next[symbol]] = (size + slot) as u32;
            next[symbol] += 1;
        }
        let symbols = counts
            .iter()
            .zip(&starts)
            .map(|(&count, &start)| match count {
                0 => (0, 0),
                -1 | 1 => ((log << 16) - size as u32, start as i32 - 1),
                count => {
                    let count = count as u32;
                    let most = log - (31 - (count - 1).leading_zeros());
                    ((most << 16) - (count << most), start as i32 - count as i32)
                }
            })
            .collect();
        Self { log, states, symbols }
    }

    /// The table of a section that repeats `symbol`, which spends no bits at all.
    fn rle(symbol: usize) -> Self {
        Self { log: 0, states: vec![0], symbols: vec![(0, 0); symbol + 1] }
    }

    /// The state that leaves the decoder on `symbol` without spending a bit.
    pub(super) fn start(&self, symbol: usize) -> u32 {
        let (shed, base) = self.symbols[symbol];
        let bits = (shed + (1 << 15)) >> 16;
        let value = (bits << 16).wrapping_sub(shed);
        self.states[((value >> bits) as i32 + base) as usize]
    }

    /// Writes the bits that get from `state` to `symbol`, and moves on to the state before it.
    pub(super) fn encode(&self, state: &mut u32, symbol: usize, bits: &mut Bits) {
        let (shed, base) = self.symbols[symbol];
        let count = (*state + shed) >> 16;
        bits.add(*state, count);
        *state = self.states[((*state >> count) as i32 + base) as usize];
    }
}

/// Scales `counts` to sum to `1 << log`, keeping every symbol that occurs at one or more.
pub(super) fn normalize(counts: &[u32], log: u32) -> Vec<i32> {
    let size = 1_i64 << log;
    let total: i64 = counts.iter().map(|&count| i64::from(count)).sum();
    let mut normal: Vec<i32> = counts
        .iter()
        .map(|&count| match count {
            0 => 0,
            count => (i64::from(count) * size / total).max(1) as i32,
        })
        .collect();
    let mut sum: i64 = normal.iter().map(|&count| i64::from(count)).sum();
    while sum > size {
        let (most, _) =
            normal.iter().enumerate().max_by_key(|&(_, &count)| count).unwrap_or((0, &0));
        normal[most] -= 1;
        sum -= 1;
    }
    if sum < size {
        let (most, _) =
            counts.iter().enumerate().max_by_key(|&(_, &count)| count).unwrap_or((0, &0));
        normal[most] += (size - sum) as i32;
    }
    normal
}

/// Writes the description of a table the way [`super::fse::Table::read`] reads one.
///
/// The accuracy less five in four bits, then a count for each symbol plus one, each in as many
/// bits as the counts not accounted for yet could need, with one bit saved on the smaller values.
/// A zero count is followed by how many more zeros come after it, in pairs of bits.
pub(super) fn describe_table(normal: &[i32], log: u32, bits: &mut Bits) {
    bits.add(log - 5, 4);
    let size = 1_i32 << log;
    let (mut left, mut threshold, mut width) = (size + 1, size, log + 1);
    let mut symbol = 0;
    let mut after_zero = false;
    while symbol < normal.len() && left > 1 {
        if after_zero {
            let from = symbol;
            while normal[symbol] == 0 {
                symbol += 1;
            }
            let mut run = symbol - from;
            while run >= 3 {
                bits.add(3, 2);
                run -= 3;
            }
            bits.add(run as u32, 2);
        }
        let count = normal[symbol];
        symbol += 1;
        let ceiling = 2 * threshold - 1 - left;
        left -= count.abs();
        let mut value = count + 1;
        if value >= threshold {
            value += ceiling;
        }
        bits.add(value as u32, if value < ceiling { width - 1 } else { width });
        after_zero = value == 1;
        while left < threshold {
            width -= 1;
            threshold >>= 1;
        }
    }
}

/// A bitstream written low bit first, for a decoder that reads it from the end.
#[derive(Debug, Default)]
pub(super) struct Bits {
    out: Vec<u8>,
    pending: u64,
    count: u32,
}

impl Bits {
    /// Appends the low `count` bits of `value`.
    pub(super) fn add(&mut self, value: u32, count: u32) {
        if count == 0 {
            return;
        }
        self.pending |= (u64::from(value) & ((1 << count) - 1)) << self.count;
        self.count += count;
        while self.count >= 8 {
            self.out.push(self.pending as u8);
            self.pending >>= 8;
            self.count -= 8;
        }
    }

    fn extra(&mut self, code: Code) {
        self.add(code.extra, code.bits);
    }

    /// Appends the end marker, a one bit above the last one written, and the stream to `out`.
    pub(super) fn finish(mut self, out: &mut Vec<u8>) {
        self.add(1, 1);
        self.pad(out);
    }

    /// Appends the stream to `out`, with its last byte filled out with zeros.
    pub(super) fn pad(mut self, out: &mut Vec<u8>) {
        if self.count > 0 {
            self.out.push(self.pending as u8);
        }
        out.extend_from_slice(&self.out);
    }
}

#[cfg(test)]
mod tests {
    use super::{BLOCK, compress, normalize};
    use crate::zstd::decompress;

    /// Bytes from a small alphabet, so some of them repeat and some do not.
    fn sample(len: usize, alphabet: u32, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % u64::from(alphabet)) as u8
            })
            .collect()
    }

    #[test]
    fn what_is_compressed_decompresses_back() {
        let text = b"the quick brown fox jumps over the lazy dog, the quick brown fox again. "
            .repeat(2000);
        for level in [-5, 1, 3, 9, 22] {
            let frame = compress(&text, level);
            assert!(frame.len() < text.len() / 20, "{level} {}", frame.len());
            assert_eq!(decompress(&frame).expect("decompresses"), text, "{level}");
        }
    }

    #[test]
    fn many_small_inputs_decompress_back() {
        let mut seed = 1_u64;
        for round in 0..2000 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let alphabet = [2, 4, 16, 256][round % 4];
            let input = sample(round % 700, alphabet, seed);
            let level = [1, 3, 19][round % 3];
            let frame = compress(&input, level);
            assert_eq!(decompress(&frame).expect("decompresses"), input, "{round}");
        }
    }

    #[test]
    fn a_frame_of_many_blocks_reaches_back_across_them() {
        let piece = sample(50_000, 256, 9);
        let mut input = Vec::new();
        for _ in 0..8 {
            input.extend_from_slice(&piece);
        }
        input.extend(sample(3 * BLOCK, 7, 11));
        input.extend(vec![b'x'; BLOCK + 5]);
        for level in [1, 3] {
            let frame = compress(&input, level);
            assert!(frame.len() < input.len() / 2, "{}", frame.len());
            assert_eq!(decompress(&frame).expect("decompresses"), input);
        }
    }

    #[test]
    fn bytes_that_do_not_compress_are_stored_and_a_run_is_one_byte() {
        let noise = sample(10_000, 256, 5);
        let frame = compress(&noise, 3);
        // The magic, a descriptor, two bytes of size and a block header.
        assert_eq!(frame.len(), noise.len() + 4 + 1 + 2 + 3);
        assert_eq!(decompress(&frame).expect("decompresses"), noise);
        let run = vec![7; 1000];
        let frame = compress(&run, 3);
        assert_eq!(frame.len(), 4 + 1 + 2 + 3 + 1);
        assert_eq!(decompress(&frame).expect("decompresses"), run);
    }

    #[test]
    fn counting_integers_reuse_their_offsets() {
        // Each value differs from the one eight bytes back in its low byte only, so after the
        // first copy every one repeats the offset before it.
        let input: Vec<u8> = (0..100_000_u64).flat_map(u64::to_le_bytes).collect();
        for level in [1, 3, 9] {
            let frame = compress(&input, level);
            assert!(frame.len() < input.len() / 4, "{level} {}", frame.len());
            assert_eq!(decompress(&frame).expect("decompresses"), input, "{level}");
        }
    }

    #[test]
    fn normalized_counts_fill_the_table_and_keep_rare_symbols() {
        let normal = normalize(&[1000, 1, 0, 3, 1], 5);
        assert_eq!(normal.iter().sum::<i32>(), 32);
        assert!(normal[1] >= 1 && normal[2] == 0 && normal[4] >= 1);
    }

    #[test]
    fn nothing_is_a_frame_with_one_empty_block() {
        let frame = compress(&[], 3);
        assert_eq!(frame, [0x28, 0xB5, 0x2F, 0xFD, 0x20, 0, 1, 0, 0]);
        assert_eq!(decompress(&frame).expect("decompresses"), b"");
    }
}

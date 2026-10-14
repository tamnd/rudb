//! DEFLATE the other way, which is what the pin's `MiniZStream` writes inside a gzip member for a
//! Parquet page in `GZIP`.
//!
//! The matches come from a hash of the next three bytes and a chain of the earlier positions with
//! the same hash, searched a bounded number of steps back, with a match given up for a longer one
//! that starts a byte later. Each block of symbols is then written whichever way is shortest:
//! stored, with the fixed code, or with a code built for the block.
//!
//! This does not try to be the bytes miniz writes, only a stream any inflater reads back. The pin
//! reads the pages of another writer just as well, so nothing that reads the file can tell.

use crate::gzip::{DISTANCE_BASE, DISTANCE_EXTRA, LENGTH_BASE, LENGTH_EXTRA, ORDER};

/// How far back a match may point.
const WINDOW: usize = 32 * 1024;
/// The shortest match DEFLATE has a code for.
const MIN_MATCH: usize = 3;
/// The longest.
const MAX_MATCH: usize = 258;
/// The bits of the hash of three bytes.
const HASH_BITS: u32 = 15;
/// How many earlier positions with the same hash a search tries.
const CHAIN: usize = 64;
/// How many symbols a block holds before the next one starts.
const BLOCK: usize = 32 * 1024;
/// The end of block symbol.
const END: usize = 256;

/// One symbol of the stream: a literal byte when `distance` is zero, and otherwise a match of
/// `length` bytes `distance` back.
#[derive(Clone, Copy)]
struct Symbol {
    length: u16,
    distance: u16,
}

/// Compresses `input` as one DEFLATE stream onto the end of `out`.
pub fn compress_onto(input: &[u8], out: &mut Vec<u8>) {
    let symbols = parse(input);
    let mut bits = Writer { out, buffer: 0, count: 0 };
    let mut start = 0;
    let mut chunks = symbols.chunks(BLOCK).peekable();
    if chunks.peek().is_none() {
        // Nothing at all is one fixed block holding only its end.
        bits.put(1, 1);
        bits.put(1, 2);
        bits.put(0, 7);
    }
    while let Some(block) = chunks.next() {
        let covered: usize = block.iter().map(|symbol| usize::from(symbol.length)).sum();
        let last = chunks.peek().is_none();
        write_block(&mut bits, block, &input[start..start + covered], last);
        start += covered;
    }
    bits.flush();
}

/// Compresses `input` as one DEFLATE stream.
#[must_use]
pub fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 2 + 64);
    compress_onto(input, &mut out);
    out
}

/// The symbols `input` comes to, each literal with a length of one.
fn parse(input: &[u8]) -> Vec<Symbol> {
    let mut symbols = Vec::with_capacity(input.len() / 3 + 1);
    let mut chains =
        Chains { head: vec![u32::MAX; 1 << HASH_BITS], previous: vec![u32::MAX; WINDOW] };
    let mut at = 0;
    let mut pending = chains.longest(input, at);
    while at < input.len() {
        let (length, distance) = pending;
        if length < MIN_MATCH {
            chains.insert(input, at);
            symbols.push(Symbol { length: 1, distance: 0 });
            at += 1;
            pending = chains.longest(input, at);
            continue;
        }
        // A match a byte on that is longer is worth the literal it costs.
        chains.insert(input, at);
        let next = chains.longest(input, at + 1);
        if next.0 > length {
            symbols.push(Symbol { length: 1, distance: 0 });
            at += 1;
            pending = next;
            continue;
        }
        symbols.push(Symbol { length: length as u16, distance: distance as u16 });
        for skipped in at + 1..at + length {
            chains.insert(input, skipped);
        }
        at += length;
        pending = chains.longest(input, at);
    }
    symbols
}

/// The earlier positions of each hash, newest first.
struct Chains {
    /// The newest position of each hash.
    head: Vec<u32>,
    /// The position before each one with the same hash, by position modulo the window.
    previous: Vec<u32>,
}

impl Chains {
    fn hash(input: &[u8], at: usize) -> usize {
        let word =
            u32::from(input[at]) | u32::from(input[at + 1]) << 8 | u32::from(input[at + 2]) << 16;
        (word.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
    }

    fn insert(&mut self, input: &[u8], at: usize) {
        if at + MIN_MATCH > input.len() {
            return;
        }
        let hash = Self::hash(input, at);
        self.previous[at % WINDOW] = self.head[hash];
        self.head[hash] = at as u32;
    }

    /// The longest match for the bytes at `at`, as its length and distance, with a length of zero
    /// when there is none.
    fn longest(&self, input: &[u8], at: usize) -> (usize, usize) {
        if at + MIN_MATCH > input.len() {
            return (0, 0);
        }
        let most = (input.len() - at).min(MAX_MATCH);
        let mut best = (0, 0);
        let mut candidate = self.head[Self::hash(input, at)];
        for _ in 0..CHAIN {
            let from = candidate as usize;
            if candidate == u32::MAX || from >= at || at - from > WINDOW {
                break;
            }
            if input[from + best.0] == input[at + best.0] || best.0 == 0 {
                let length = input[from..from + most]
                    .iter()
                    .zip(&input[at..at + most])
                    .take_while(|(a, b)| a == b)
                    .count();
                if length > best.0 {
                    best = (length, at - from);
                    if length == most {
                        break;
                    }
                }
            }
            let next = self.previous[from % WINDOW];
            // A slot written over by a newer position ends the chain.
            if next != u32::MAX && next as usize >= from {
                break;
            }
            candidate = next;
        }
        best
    }
}

/// Writes one block of `symbols`, which stand for `bytes`, the shortest of the three ways.
fn write_block(bits: &mut Writer<'_>, symbols: &[Symbol], bytes: &[u8], last: bool) {
    let mut literal = [0_u32; 286];
    let mut distance = [0_u32; 30];
    let mut at = 0;
    for symbol in symbols {
        if symbol.distance == 0 {
            literal[usize::from(bytes[at])] += 1;
        } else {
            literal[257 + length_code(symbol.length)] += 1;
            distance[distance_code(symbol.distance)] += 1;
        }
        at += usize::from(symbol.length);
    }
    literal[END] = 1;
    let literal_lengths = lengths(&literal, 15);
    let distance_lengths = lengths(&distance, 15);
    let header = Header::new(&literal_lengths, &distance_lengths);
    let dynamic =
        3 + header.bits() + cost(&literal, &distance, &literal_lengths, &distance_lengths);
    let (fixed_literal, fixed_distance) = fixed();
    let fixed = 3 + cost(&literal, &distance, &fixed_literal, &fixed_distance);
    // A stored block starts on a byte and holds at most 65535 bytes.
    let stored = (bytes.len().div_ceil(65_535).max(1) * 5 + bytes.len()) * 8 + 7;
    if stored < fixed && stored < dynamic {
        let mut pieces = bytes.chunks(65_535).peekable();
        while let Some(piece) = pieces.next() {
            bits.put(u32::from(last && pieces.peek().is_none()), 1);
            bits.put(0, 2);
            bits.align();
            let length = piece.len() as u16;
            bits.bytes(&length.to_le_bytes());
            bits.bytes(&(!length).to_le_bytes());
            bits.bytes(piece);
        }
        return;
    }
    bits.put(u32::from(last), 1);
    let (literal_codes, distance_codes) = if fixed <= dynamic {
        bits.put(1, 2);
        (Code::new(&fixed_literal), Code::new(&fixed_distance))
    } else {
        bits.put(2, 2);
        header.write(bits);
        (Code::new(&literal_lengths), Code::new(&distance_lengths))
    };
    let mut at = 0;
    for symbol in symbols {
        if symbol.distance == 0 {
            literal_codes.put(bits, usize::from(bytes[at]));
        } else {
            let length = length_code(symbol.length);
            literal_codes.put(bits, 257 + length);
            bits.put(
                u32::from(symbol.length - LENGTH_BASE[length]),
                u32::from(LENGTH_EXTRA[length]),
            );
            let code = distance_code(symbol.distance);
            distance_codes.put(bits, code);
            bits.put(
                u32::from(symbol.distance - DISTANCE_BASE[code]),
                u32::from(DISTANCE_EXTRA[code]),
            );
        }
        at += usize::from(symbol.length);
    }
    literal_codes.put(bits, END);
}

/// Which of the 29 length codes a match length is, counted from 257.
fn length_code(length: u16) -> usize {
    LENGTH_BASE.partition_point(|&base| base <= length) - 1
}

/// Which of the 30 distance codes a distance is.
fn distance_code(distance: u16) -> usize {
    DISTANCE_BASE.partition_point(|&base| base <= distance) - 1
}

/// The bits the symbols of a block take with the given code lengths, extra bits included.
fn cost(
    literal: &[u32],
    distance: &[u32],
    literal_lengths: &[u8],
    distance_lengths: &[u8],
) -> usize {
    let mut bits = 0;
    for (symbol, &count) in literal.iter().enumerate() {
        let extra = if symbol > END { usize::from(LENGTH_EXTRA[symbol - 257]) } else { 0 };
        bits += count as usize * (usize::from(literal_lengths[symbol]) + extra);
    }
    for (symbol, &count) in distance.iter().enumerate() {
        bits += count as usize
            * (usize::from(distance_lengths[symbol]) + usize::from(DISTANCE_EXTRA[symbol]));
    }
    bits
}

/// The code lengths of the fixed code, for the literals and lengths and for the distances.
///
/// All 288 literal and length codes are there, although the last two are never written, because
/// they are numbered before the nine bit codes and leaving them out would move every one of those.
fn fixed() -> ([u8; 288], [u8; 30]) {
    let mut literal = [8_u8; 288];
    literal[144..256].fill(9);
    literal[256..280].fill(7);
    (literal, [5; 30])
}

/// Code lengths of at most `limit` bits for symbols that occur `counts` times, with none for a
/// symbol that does not occur.
///
/// The lengths are Huffman's, and when some come out longer than `limit` the longest are cut down
/// and others lengthened until the code is complete again, which is what miniz does. A code with
/// fewer than two symbols gets two of length one, since an inflater wants a complete code.
pub(crate) fn lengths(counts: &[u32], limit: u8) -> Vec<u8> {
    let mut lengths = vec![0_u8; counts.len()];
    let mut used: Vec<usize> = (0..counts.len()).filter(|&symbol| counts[symbol] > 0).collect();
    if used.len() < 2 {
        lengths[..2].fill(1);
        if let Some(&symbol) = used.first()
            && symbol >= 2
        {
            lengths[1] = 0;
            lengths[symbol] = 1;
        }
        return lengths;
    }
    // The tree, with the leaves first and each node's parent once it has one.
    let mut weight: Vec<u64> = used.iter().map(|&symbol| u64::from(counts[symbol])).collect();
    let mut parent = vec![usize::MAX; used.len()];
    let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, usize)>> = weight
        .iter()
        .enumerate()
        .map(|(node, &weight)| std::cmp::Reverse((weight, node)))
        .collect();
    while heap.len() > 1 {
        let std::cmp::Reverse((first, a)) = heap.pop().unwrap_or_default();
        let std::cmp::Reverse((second, b)) = heap.pop().unwrap_or_default();
        let node = weight.len();
        weight.push(first + second);
        parent.push(usize::MAX);
        parent[a] = node;
        parent[b] = node;
        heap.push(std::cmp::Reverse((first + second, node)));
    }
    let limit = usize::from(limit);
    let mut per_length = vec![0_u32; limit + 1];
    for leaf in 0..used.len() {
        let mut depth = 0;
        let mut node = leaf;
        while parent[node] != usize::MAX {
            node = parent[node];
            depth += 1;
        }
        per_length[depth.min(limit)] += 1;
    }
    let full = 1_u64 << limit;
    let mut total: u64 =
        (1..=limit).map(|length| u64::from(per_length[length]) << (limit - length)).sum();
    while total > full {
        per_length[limit] -= 1;
        if let Some(length) = (1..limit).rev().find(|&length| per_length[length] > 0) {
            per_length[length] -= 1;
            per_length[length + 1] += 2;
        }
        total -= 1;
    }
    // The most frequent symbols get the shortest codes.
    used.sort_by(|&a, &b| counts[b].cmp(&counts[a]).then(a.cmp(&b)));
    let mut next = used.into_iter();
    for (length, &count) in per_length.iter().enumerate().skip(1) {
        for symbol in next.by_ref().take(count as usize) {
            lengths[symbol] = length as u8;
        }
    }
    lengths
}

/// The canonical codes for a set of code lengths, bit reversed for writing least significant
/// first.
struct Code {
    codes: Vec<u16>,
    lengths: Vec<u8>,
}

impl Code {
    fn new(lengths: &[u8]) -> Self {
        let mut count = [0_u16; 16];
        for &length in lengths {
            count[usize::from(length)] += 1;
        }
        count[0] = 0;
        let mut next = [0_u16; 16];
        let mut code = 0_u16;
        for length in 1..16 {
            code = (code + count[length - 1]) << 1;
            next[length] = code;
        }
        let codes = lengths
            .iter()
            .map(|&length| {
                if length == 0 {
                    return 0;
                }
                let code = next[usize::from(length)];
                next[usize::from(length)] += 1;
                code.reverse_bits() >> (16 - length)
            })
            .collect();
        Self { codes, lengths: lengths.to_vec() }
    }

    fn put(&self, bits: &mut Writer<'_>, symbol: usize) {
        bits.put(u32::from(self.codes[symbol]), u32::from(self.lengths[symbol]));
    }
}

/// The header of a block with a code of its own: the code lengths, run length encoded and written
/// with a code of their own.
struct Header {
    literals: usize,
    distances: usize,
    /// The code length symbols, each with its extra bits.
    runs: Vec<(u8, u8)>,
    lengths: Vec<u8>,
    sent: usize,
}

impl Header {
    fn new(literal: &[u8], distance: &[u8]) -> Self {
        let literals =
            literal.iter().rposition(|&length| length != 0).map_or(257, |at| (at + 1).max(257));
        let distances = distance.iter().rposition(|&length| length != 0).map_or(1, |at| at + 1);
        let all: Vec<u8> =
            literal[..literals].iter().chain(&distance[..distances]).copied().collect();
        let mut runs = Vec::new();
        let mut at = 0;
        while at < all.len() {
            let length = all[at];
            let run = all[at..].iter().take_while(|&&next| next == length).count();
            if length == 0 && run >= 3 {
                let taken = run.min(138);
                if taken <= 10 {
                    runs.push((17, (taken - 3) as u8));
                } else {
                    runs.push((18, (taken - 11) as u8));
                }
                at += taken;
            } else if length != 0 && run >= 4 {
                runs.push((length, 0));
                let taken = (run - 1).min(6);
                runs.push((16, (taken - 3) as u8));
                at += 1 + taken;
            } else {
                runs.push((length, 0));
                at += 1;
            }
        }
        let mut counts = [0_u32; 19];
        for &(symbol, _) in &runs {
            counts[usize::from(symbol)] += 1;
        }
        let lengths = lengths(&counts, 7);
        let sent =
            ORDER.iter().rposition(|&symbol| lengths[symbol] != 0).map_or(4, |at| (at + 1).max(4));
        Self { literals, distances, runs, lengths, sent }
    }

    fn bits(&self) -> usize {
        let runs: usize = self
            .runs
            .iter()
            .map(|&(symbol, _)| {
                usize::from(self.lengths[usize::from(symbol)])
                    + match symbol {
                        16 => 2,
                        17 => 3,
                        18 => 7,
                        _ => 0,
                    }
            })
            .sum();
        14 + 3 * self.sent + runs
    }

    fn write(&self, bits: &mut Writer<'_>) {
        bits.put((self.literals - 257) as u32, 5);
        bits.put((self.distances - 1) as u32, 5);
        bits.put((self.sent - 4) as u32, 4);
        for &symbol in &ORDER[..self.sent] {
            bits.put(u32::from(self.lengths[symbol]), 3);
        }
        let code = Code::new(&self.lengths);
        for &(symbol, extra) in &self.runs {
            code.put(bits, usize::from(symbol));
            match symbol {
                16 => bits.put(u32::from(extra), 2),
                17 => bits.put(u32::from(extra), 3),
                18 => bits.put(u32::from(extra), 7),
                _ => {}
            }
        }
    }
}

/// Bits onto the end of a buffer, least significant first.
struct Writer<'a> {
    out: &'a mut Vec<u8>,
    buffer: u64,
    count: u32,
}

impl Writer<'_> {
    fn put(&mut self, value: u32, bits: u32) {
        self.buffer |= u64::from(value) << self.count;
        self.count += bits;
        while self.count >= 8 {
            self.out.push(self.buffer as u8);
            self.buffer >>= 8;
            self.count -= 8;
        }
    }

    /// Pads to the next byte with zeros.
    fn align(&mut self) {
        if self.count > 0 {
            self.put(0, 8 - self.count);
        }
    }

    /// Appends whole bytes, which must come on a byte.
    fn bytes(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    fn flush(&mut self) {
        self.align();
    }
}

#[cfg(test)]
mod tests {
    use super::compress;

    /// Inflates a stream by wrapping it in a gzip member and reading that back.
    fn inflate(stream: &[u8], original: &[u8]) -> Vec<u8> {
        let mut member = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
        member.extend_from_slice(stream);
        member.extend_from_slice(&crate::gzip::crc32(original).to_le_bytes());
        member.extend_from_slice(&(original.len() as u32).to_le_bytes());
        crate::gzip::decompress(&member).expect("the stream inflates")
    }

    fn sample(len: usize) -> Vec<u8> {
        let mut state = 11_u32;
        (0..len)
            .map(|at| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                if (state >> 16).is_multiple_of(7) {
                    (state >> 8) as u8
                } else {
                    (at % 199 / 4) as u8
                }
            })
            .collect()
    }

    #[test]
    fn what_is_compressed_inflates_back() {
        for len in [0, 1, 2, 3, 4, 100, 1000, 40_000, 70_000, 300_000] {
            let input = sample(len);
            assert_eq!(inflate(&compress(&input), &input), input, "{len}");
        }
        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(500);
        let stream = compress(&text);
        assert!(stream.len() < text.len() / 20, "{}", stream.len());
        assert_eq!(inflate(&stream, &text), text);
    }

    #[test]
    fn many_small_inputs_inflate_back() {
        let mut state = 3_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..3000 {
            let len = (next() % 600) as usize;
            let alphabet = 1 + next() % [2, 4, 16, 256][round % 4];
            let input: Vec<u8> = (0..len).map(|_| (next() % alphabet) as u8).collect();
            let stream = compress(&input);
            assert_eq!(inflate(&stream, &input), input, "round {round}, {input:?}");
        }
    }

    #[test]
    fn bytes_that_do_not_compress_are_stored() {
        let mut state = 5_u64;
        let noise: Vec<u8> = (0..200_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let stream = compress(&noise);
        assert!(stream.len() < noise.len() + noise.len() / 1000, "{}", stream.len());
        assert_eq!(inflate(&stream, &noise), noise);
    }

    #[test]
    fn nothing_is_the_shortest_fixed_block() {
        assert_eq!(compress(b""), [0x03, 0x00]);
        let zeroes = vec![0_u8; 100_000];
        let stream = compress(&zeroes);
        assert!(stream.len() < 300, "{}", stream.len());
        assert_eq!(inflate(&stream, &zeroes), zeroes);
    }
}

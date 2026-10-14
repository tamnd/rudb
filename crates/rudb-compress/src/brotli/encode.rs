//! Writing a brotli stream.
//!
//! The matches come from the hash chains [`crate::zstd`] searches with, at the depth its level 9
//! uses, and the meta-blocks are as plain as the format allows: one block type of each kind, one
//! literal code and one distance code, and no direct distance codes. Each meta-block holds up to
//! [`META`] bytes and is stored as it is when coding it would not make it smaller.

use super::decode::{CELLS, COPY, INSERT, ORDER};
use crate::zstd::encode::{Finder, Search};

/// The most bytes one meta-block holds.
const META: usize = 1 << 18;
/// The shortest match worth a command.
const MIN_MATCH: usize = 4;
/// The longest code the writer gives a symbol.
const LONGEST: u8 = 15;
/// The code the writer gives each length of a code length code, and how long that code is. These
/// are the codes the reader's peek at four bits expects.
const LENGTH_CODE: [u32; 6] = [0, 7, 3, 2, 1, 15];
const LENGTH_BITS: [u32; 6] = [2, 4, 3, 2, 2, 4];

/// One command: literals, then a copy, unless it is the last of its meta-block and has none.
struct Command {
    insert: usize,
    copy: usize,
    /// The distance code, which is 0 for the last distance again.
    code: usize,
    /// The extra bits of the distance code, as their value and how many.
    extra: (usize, u32),
}

pub(super) fn compress(input: &[u8]) -> Vec<u8> {
    let window = (10..=22).find(|&bits| (1_usize << bits) - 16 >= input.len()).unwrap_or(22);
    let mut bits = Writer::default();
    match window {
        16 => bits.put(0, 1),
        17 => bits.put(1, 7),
        18.. => bits.put(1 | (window - 17) << 1, 4),
        _ => bits.put(1 | (window - 8) << 4, 7),
    }
    let search = Search::at(9);
    let mut finder = Finder::new(input, search);
    let mut ring = [4, 11, 15, 16];
    let mut start = 0;
    while start < input.len() {
        let end = (start + META).min(input.len());
        let before = ring;
        let commands = parse(&mut finder, search.nice, start, end, &mut ring);
        let mut trial = bits.fork();
        meta_block(&mut trial, &input[start..end], &commands);
        if trial.out.len() < end - start {
            bits.join(trial);
        } else {
            ring = before;
            header(&mut bits, end - start);
            bits.put(1, 1);
            bits.align();
            bits.out.extend_from_slice(&input[start..end]);
        }
        start = end;
    }
    // The last meta-block, empty.
    bits.put(3, 2);
    bits.align();
    bits.out
}

/// Splits `input[start..end]` into commands, moving the last four distances on as the reader will.
fn parse(
    finder: &mut Finder<'_>,
    nice: usize,
    start: usize,
    end: usize,
    ring: &mut [usize; 4],
) -> Vec<Command> {
    let mut commands = Vec::new();
    let (mut at, mut anchor) = (start, start);
    while at + MIN_MATCH <= end {
        finder.recent = [ring[0], ring[1], ring[2]].map(|distance| distance as u64);
        let (mut length, mut distance) = finder.best(at, end);
        if length == 0 {
            at += 1;
            continue;
        }
        while length < nice {
            let (longer, further) = finder.best(at + 1, end);
            if longer <= length {
                break;
            }
            at += 1;
            (length, distance) = (longer, further);
        }
        let (code, extra) = distance_code(distance, ring);
        if code != 0 {
            *ring = [distance, ring[0], ring[1], ring[2]];
        }
        commands.push(Command { insert: at - anchor, copy: length, code, extra });
        at += length;
        anchor = at;
    }
    if anchor < end {
        commands.push(Command { insert: end - anchor, copy: 0, code: 0, extra: (0, 0) });
    }
    commands
}

/// How a command codes `distance` given the last four, as its code and the code's extra bits.
fn distance_code(distance: usize, ring: &[usize; 4]) -> (usize, (usize, u32)) {
    if let Some(back) = ring.iter().position(|&last| last == distance) {
        return (back, (0, 0));
    }
    for (base, last) in [(4, ring[0]), (10, ring[1])] {
        let near =
            [-1, 1, -2, 2, -3, 3].iter().position(|&delta| last as i64 + delta == distance as i64);
        if let Some(near) = near {
            return (base + near, (0, 0));
        }
    }
    // With no postfix bits and no direct codes, code 16 + x means the distance whose value three
    // more than it is 2 + (x & 1) shifted up by 1 + x / 2 bits, plus that many extra bits.
    let value = distance + 3;
    let bits = usize::BITS - value.leading_zeros() - 2;
    let high = (value >> bits) & 1;
    (16 + 2 * (bits as usize - 1) + high, (value - ((2 + high) << bits), bits))
}

/// The header of a meta-block that is not the last, up to the bit that says whether it is stored.
fn header(bits: &mut Writer, length: usize) {
    let nibbles = if length <= 1 << 16 {
        4
    } else if length <= 1 << 20 {
        5
    } else {
        6
    };
    bits.put(0, 1);
    bits.put(nibbles - 4, 2);
    bits.put(length - 1, 4 * nibbles as u32);
}

/// Writes the meta-block of `bytes` that `commands` code.
fn meta_block(bits: &mut Writer, bytes: &[u8], commands: &[Command]) {
    let mut literal_counts = [0_u32; 256];
    let mut command_counts = vec![0_u32; 704];
    let mut distance_counts = [0_u32; 64];
    let mut symbols = Vec::with_capacity(commands.len());
    let mut at = 0;
    for command in commands {
        for &byte in &bytes[at..at + command.insert] {
            literal_counts[usize::from(byte)] += 1;
        }
        let insert = INSERT.iter().rposition(|&(base, _)| base <= command.insert).unwrap_or(0);
        let copy = COPY.iter().rposition(|&(base, _)| base <= command.copy).unwrap_or(0);
        // The last distance again needs no distance symbol when the lengths are short enough for
        // the first two cells, and the last command reads none at all.
        let implicit = insert < 8 && copy < 16 && (command.code == 0 || command.copy == 0);
        let cell = if implicit {
            copy >> 3
        } else {
            CELLS.iter().skip(2).position(|&(i, c)| i == insert & !7 && c == copy & !7).unwrap_or(0)
                + 2
        };
        let symbol = cell << 6 | (insert & 7) << 3 | (copy & 7);
        command_counts[symbol] += 1;
        let distance = !implicit && command.copy > 0;
        if distance {
            distance_counts[command.code] += 1;
        }
        symbols.push((symbol, insert, copy, distance));
        at += command.insert + command.copy;
    }

    header(bits, bytes.len());
    // Not stored, one block type of each kind, no postfix bits or direct codes, the LSB6 context
    // mode for the one literal block type, and one literal tree and one distance tree.
    bits.put(0, 1 + 3 + 6 + 2 + 2);
    let literals = prefix_code(bits, &literal_counts);
    let codes = prefix_code(bits, &command_counts);
    let distances = prefix_code(bits, &distance_counts);

    let mut at = 0;
    for (command, &(symbol, insert, copy, distance)) in commands.iter().zip(&symbols) {
        codes.put(bits, symbol);
        let (base, extra) = INSERT[insert];
        bits.put(command.insert - base, extra);
        let (base, extra) = COPY[copy];
        bits.put(command.copy.saturating_sub(base), extra);
        for &byte in &bytes[at..at + command.insert] {
            literals.put(bits, usize::from(byte));
        }
        if distance {
            distances.put(bits, command.code);
            bits.put(command.extra.0, command.extra.1);
        }
        at += command.insert + command.copy;
    }
}

/// Writes a prefix code for symbols that occur `counts` times, and returns it.
///
/// Up to four symbols get a simple code, which lists them, and more get lengths of up to fifteen
/// bits run length coded the way libbrotli codes them, with a code of their own.
fn prefix_code(bits: &mut Writer, counts: &[u32]) -> Code {
    let width = usize::BITS - (counts.len() - 1).leading_zeros();
    let mut used: Vec<usize> = (0..counts.len()).filter(|&symbol| counts[symbol] > 0).collect();
    let mut lengths = vec![0_u8; counts.len()];
    if used.len() <= 4 {
        used.sort_by(|&a, &b| counts[b].cmp(&counts[a]).then(a.cmp(&b)));
        if used.is_empty() {
            used.push(0);
        }
        bits.put(1, 2);
        bits.put(used.len() - 1, 2);
        for &symbol in &used {
            bits.put(symbol, width);
        }
        let shape: &[u8] = match *used.as_slice() {
            [_] => &[0],
            [_, _] => &[1, 1],
            [_, _, _] => &[1, 2, 2],
            [a, b, c, d] => {
                let flat = 2 * (counts[a] + counts[b] + counts[c] + counts[d]);
                let skewed = counts[a] + 2 * counts[b] + 3 * (counts[c] + counts[d]);
                bits.put(usize::from(skewed < flat), 1);
                if skewed < flat { &[1, 2, 3, 3] } else { &[2, 2, 2, 2] }
            }
            _ => &[],
        };
        for (&symbol, &length) in used.iter().zip(shape) {
            lengths[symbol] = length;
        }
        return Code::new(&lengths);
    }

    let lengths = crate::deflate::lengths(counts, LONGEST);
    let used = lengths.iter().rposition(|&length| length > 0).map_or(0, |last| last + 1);
    let mut runs = Vec::new();
    let mut previous = 8;
    let mut at = 0;
    while at < used {
        let value = lengths[at];
        let reps = lengths[at..used].iter().take_while(|&&length| length == value).count();
        if value == 0 {
            zeros(reps, &mut runs);
        } else {
            repeats(previous, value, reps, &mut runs);
            previous = value;
        }
        at += reps;
    }
    let mut run_counts = [0_u32; 18];
    for &(symbol, _) in &runs {
        run_counts[usize::from(symbol)] += 1;
    }
    let run_lengths = crate::deflate::lengths(&run_counts, 5);
    let stored =
        ORDER.iter().rposition(|&symbol| run_lengths[symbol] > 0).map_or(0, |last| last + 1);
    let skip = match (run_lengths[ORDER[0]], run_lengths[ORDER[1]], run_lengths[ORDER[2]]) {
        (0, 0, 0) => 3,
        (0, 0, _) => 2,
        _ => 0,
    };
    bits.put(skip, 2);
    for &symbol in &ORDER[skip..stored] {
        let length = usize::from(run_lengths[symbol]);
        bits.put(LENGTH_CODE[length] as usize, LENGTH_BITS[length]);
    }
    let run_code = Code::new(&run_lengths);
    for &(symbol, extra) in &runs {
        run_code.put(bits, usize::from(symbol));
        match symbol {
            16 => bits.put(usize::from(extra), 2),
            17 => bits.put(usize::from(extra), 3),
            _ => {}
        }
    }
    Code::new(&lengths)
}

/// Codes `reps` lengths of `value` after a `previous` one that was not zero, as libbrotli's
/// `BrotliWriteHuffmanTreeRepetitions` does. Code 16 repeats the previous length, and a run of
/// them counts in base four with the most significant digit first.
fn repeats(previous: u8, value: u8, mut reps: usize, runs: &mut Vec<(u8, u8)>) {
    if previous != value {
        runs.push((value, 0));
        reps -= 1;
    }
    if reps == 7 {
        runs.push((value, 0));
        reps -= 1;
    }
    if reps < 3 {
        runs.extend(std::iter::repeat_n((value, 0), reps));
        return;
    }
    let start = runs.len();
    reps -= 3;
    loop {
        runs.push((16, (reps & 3) as u8));
        reps >>= 2;
        if reps == 0 {
            break;
        }
        reps -= 1;
    }
    runs[start..].reverse();
}

/// Codes `reps` zero lengths the same way, with code 17 and base eight.
fn zeros(mut reps: usize, runs: &mut Vec<(u8, u8)>) {
    if reps == 11 {
        runs.push((0, 0));
        reps -= 1;
    }
    if reps < 3 {
        runs.extend(std::iter::repeat_n((0, 0), reps));
        return;
    }
    let start = runs.len();
    reps -= 3;
    loop {
        runs.push((17, (reps & 7) as u8));
        reps >>= 3;
        if reps == 0 {
            break;
        }
        reps -= 1;
    }
    runs[start..].reverse();
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

    fn put(&self, bits: &mut Writer, symbol: usize) {
        bits.put(usize::from(self.codes[symbol]), u32::from(self.lengths[symbol]));
    }
}

/// Bits onto the end of a buffer, least significant first.
#[derive(Default)]
struct Writer {
    out: Vec<u8>,
    buffer: u64,
    count: u32,
}

impl Writer {
    /// Appends the low `bits` bits of `value`, for up to 24 bits.
    fn put(&mut self, value: usize, bits: u32) {
        self.buffer |= (value as u64 & ((1 << bits) - 1)) << self.count;
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

    /// A writer that carries on from where this one is, into a buffer of its own.
    fn fork(&self) -> Self {
        Self { out: Vec::new(), buffer: self.buffer, count: self.count }
    }

    /// Takes on what a fork of this writer wrote.
    fn join(&mut self, fork: Self) {
        self.out.extend_from_slice(&fork.out);
        self.buffer = fork.buffer;
        self.count = fork.count;
    }
}

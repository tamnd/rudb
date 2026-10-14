//! Reading a brotli stream, RFC 7932 section by section.
//!
//! Where the RFC leaves room the reader does what libbrotli does, since that is what wrote nearly
//! every stream there is and what the pin reads them with. The one place that shows is the ring of
//! the last four distances, which is kept the way libbrotli keeps it, as four slots and a place to
//! write the next one, because a copy at distance code 0 that turns out to be a dictionary word
//! leaves the ring turned back a slot there and nowhere in the RFC.

use rudb_common::{Error, Result};

use super::context::{SIGNED, UTF8_BEFORE, UTF8_LAST};
use super::dictionary;

/// How many bits the first look of a prefix code decode takes.
const FAST: u32 = 10;
/// The longest prefix code brotli allows.
const LONGEST: usize = 15;
/// The order the lengths of the code length code come in.
pub(super) const ORDER: [usize; 18] =
    [1, 2, 3, 4, 0, 5, 17, 6, 16, 7, 8, 9, 10, 11, 12, 13, 14, 15];
/// How long the code is for the length of a code length code that the next four bits start.
const LENGTH_BITS: [u32; 16] = [2, 2, 2, 3, 2, 2, 2, 4, 2, 2, 2, 3, 2, 2, 2, 4];
/// The length of a code length code that the next four bits start.
const LENGTH_VALUE: [u8; 16] = [0, 4, 3, 2, 0, 4, 3, 1, 0, 4, 3, 2, 0, 4, 3, 5];

/// The insert length codes, as the least length each means and how many extra bits follow it.
pub(super) const INSERT: [(usize, u32); 24] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 1),
    (8, 1),
    (10, 2),
    (14, 2),
    (18, 3),
    (26, 3),
    (34, 4),
    (50, 4),
    (66, 5),
    (98, 5),
    (130, 6),
    (194, 7),
    (322, 8),
    (578, 9),
    (1090, 10),
    (2114, 12),
    (6210, 14),
    (22594, 24),
];
/// The copy length codes, the same way.
pub(super) const COPY: [(usize, u32); 24] = [
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 1),
    (12, 1),
    (14, 2),
    (18, 2),
    (22, 3),
    (30, 3),
    (38, 4),
    (54, 4),
    (70, 5),
    (102, 5),
    (134, 6),
    (198, 7),
    (326, 8),
    (582, 9),
    (1094, 10),
    (2118, 24),
];
/// The block count codes, the same way.
const COUNT: [(usize, u32); 26] = [
    (1, 2),
    (5, 2),
    (9, 2),
    (13, 2),
    (17, 3),
    (25, 3),
    (33, 3),
    (41, 3),
    (49, 4),
    (65, 4),
    (81, 4),
    (97, 4),
    (113, 5),
    (145, 5),
    (177, 5),
    (209, 5),
    (241, 6),
    (305, 6),
    (369, 7),
    (497, 8),
    (753, 9),
    (1265, 10),
    (2289, 11),
    (4337, 12),
    (8433, 13),
    (16625, 24),
];
/// The first insert and copy length codes of each run of 64 command symbols. The first two runs
/// copy from the last distance without a distance symbol.
pub(super) const CELLS: [(usize, usize); 11] =
    [(0, 0), (0, 8), (0, 0), (0, 8), (8, 0), (8, 8), (0, 16), (16, 0), (8, 16), (16, 8), (16, 16)];

/// The error for every way a stream can be wrong, in the words the pin uses.
pub(super) fn broken() -> Error {
    Error::io("Brotli Decompression failure")
}

/// Decompresses the stream `input` starts with onto the end of `out`. Bytes after the stream are
/// ignored, as libbrotli ignores them.
pub(super) fn decompress_onto(input: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let start = out.len();
    let mut bits = Bits::new(input);
    let window = (1_usize << window_bits(&mut bits)?) - 16;
    let mut ring = Ring { slots: [16, 15, 11, 4], next: 0 };
    loop {
        let last = bits.take(1)? == 1;
        if last && bits.take(1)? == 1 {
            break;
        }
        let nibbles = bits.take(2)?;
        if nibbles == 3 {
            // Metadata, which is skipped. It cannot be the last meta-block.
            if last || bits.take(1)? != 0 {
                return Err(broken());
            }
            let count = bits.take(2)?;
            let mut skip = 0;
            for byte in 0..count {
                let value = bits.take(8)?;
                if byte + 1 == count && count > 1 && value == 0 {
                    return Err(broken());
                }
                skip |= value << (8 * byte);
            }
            if count > 0 {
                skip += 1;
            }
            bits.align()?;
            bits.bytes(skip)?;
            continue;
        }
        let nibbles = nibbles + 4;
        let mut length = 0;
        for nibble in 0..nibbles {
            let value = bits.take(4)?;
            if nibble + 1 == nibbles && nibbles > 4 && value == 0 {
                return Err(broken());
            }
            length |= value << (4 * nibble);
        }
        let length = length + 1;
        if !last && bits.take(1)? == 1 {
            bits.align()?;
            out.extend_from_slice(bits.bytes(length)?);
            continue;
        }
        let mut block = MetaBlock::read(&mut bits)?;
        block.run(&mut bits, Reach { start, window, end: out.len() + length }, &mut ring, out)?;
        if last {
            break;
        }
    }
    bits.align()
}

/// The window size's bits, from the start of the stream.
fn window_bits(bits: &mut Bits<'_>) -> Result<u32> {
    if bits.take(1)? == 0 {
        return Ok(16);
    }
    let n = bits.take(3)? as u32;
    if n != 0 {
        return Ok(17 + n);
    }
    match bits.take(3)? as u32 {
        // The large window extension, which is not RFC 7932 and which no Parquet writer uses.
        1 => Err(broken()),
        0 => Ok(17),
        m => Ok(8 + m),
    }
}

/// Where a meta-block's copies may reach: back to the stream's start in `out` and no further than
/// the window, and forward to its end.
#[derive(Clone, Copy)]
struct Reach {
    start: usize,
    window: usize,
    end: usize,
}

/// The last four distances, as libbrotli keeps them.
struct Ring {
    slots: [usize; 4],
    /// The slot the next distance goes in, so the last one is in the slot before it.
    next: usize,
}

impl Ring {
    /// The distance `back` places before the last one.
    fn get(&self, back: usize) -> usize {
        self.slots[self.next.wrapping_sub(1 + back) & 3]
    }

    /// The last distance, taken off the ring for the copy that uses it to put back. A copy that
    /// turns out to be a dictionary word does not, which is the turn back a slot.
    fn take_last(&mut self) -> usize {
        self.next = self.next.wrapping_sub(1);
        self.slots[self.next & 3]
    }

    fn push(&mut self, distance: usize) {
        self.slots[self.next & 3] = distance;
        self.next = self.next.wrapping_add(1);
    }
}

/// How the blocks of one kind of symbol change type through a meta-block.
struct Switch {
    types: usize,
    kind: usize,
    previous: usize,
    /// How many more symbols the current block has.
    left: usize,
    codes: Option<(Code, Code)>,
}

impl Switch {
    fn read(bits: &mut Bits<'_>) -> Result<Self> {
        let types = count_of(bits)?;
        let mut switch = Self { types, kind: 0, previous: 1, left: 1 << 24, codes: None };
        if types >= 2 {
            let kinds = Code::read(bits, types + 2)?;
            let counts = Code::read(bits, COUNT.len())?;
            switch.left = block_count(bits, &counts)?;
            switch.codes = Some((kinds, counts));
        }
        Ok(switch)
    }

    /// Counts one symbol off the current block, moving to the next block first when it is used up.
    fn next(&mut self, bits: &mut Bits<'_>) -> Result<()> {
        if self.left == 0 {
            let Some((kinds, counts)) = &self.codes else {
                // A meta-block holds fewer symbols than one block of one type does.
                return Err(broken());
            };
            let kind = match kinds.decode(bits)? {
                0 => self.previous,
                1 => (self.kind + 1) % self.types,
                symbol => symbol - 2,
            };
            self.previous = self.kind;
            self.kind = kind;
            self.left = block_count(bits, counts)?;
        }
        self.left -= 1;
        Ok(())
    }
}

/// A count of 1 to 256, the way block types and trees are counted.
fn count_of(bits: &mut Bits<'_>) -> Result<usize> {
    if bits.take(1)? == 0 {
        return Ok(1);
    }
    let n = bits.take(3)? as u32;
    if n == 0 {
        return Ok(2);
    }
    Ok(1 + (1 << n) + bits.take(n)?)
}

fn block_count(bits: &mut Bits<'_>, code: &Code) -> Result<usize> {
    let (base, extra) = COUNT[code.decode(bits)?];
    Ok(base + bits.take(extra)?)
}

/// The tree each context of each block type uses, for `size` contexts in all and `trees` trees.
fn context_map(bits: &mut Bits<'_>, size: usize, trees: usize) -> Result<Vec<u8>> {
    let mut map = vec![0_u8; size];
    if trees < 2 {
        return Ok(map);
    }
    let runs = if bits.take(1)? == 1 { bits.take(4)? + 1 } else { 0 };
    let code = Code::read(bits, trees + runs)?;
    let mut at = 0;
    while at < size {
        let symbol = code.decode(bits)?;
        if symbol == 0 {
            at += 1;
        } else if symbol <= runs {
            at += (1 << symbol) + bits.take(symbol as u32)?;
            if at > size {
                return Err(broken());
            }
        } else {
            map[at] = (symbol - runs) as u8;
            at += 1;
        }
    }
    if bits.take(1)? == 1 {
        // The values were move to front coded.
        let mut order: [u8; 256] = std::array::from_fn(|value| value as u8);
        for value in &mut map {
            let index = usize::from(*value);
            let front = order[index];
            order.copy_within(..index, 1);
            order[0] = front;
            *value = front;
        }
    }
    Ok(map)
}

/// A meta-block's header: its block types, its context maps and its prefix codes.
struct MetaBlock {
    literal: Switch,
    command: Switch,
    distance: Switch,
    postfix: u32,
    direct: usize,
    modes: Vec<u8>,
    literal_map: Vec<u8>,
    distance_map: Vec<u8>,
    literals: Vec<Code>,
    commands: Vec<Code>,
    distances: Vec<Code>,
}

impl MetaBlock {
    fn read(bits: &mut Bits<'_>) -> Result<Self> {
        let literal = Switch::read(bits)?;
        let command = Switch::read(bits)?;
        let distance = Switch::read(bits)?;
        let postfix = bits.take(2)? as u32;
        let direct = bits.take(4)? << postfix;
        let modes = (0..literal.types)
            .map(|_| bits.take(2).map(|mode| mode as u8))
            .collect::<Result<_>>()?;
        let literal_trees = count_of(bits)?;
        let literal_map = context_map(bits, 64 * literal.types, literal_trees)?;
        let distance_trees = count_of(bits)?;
        let distance_map = context_map(bits, 4 * distance.types, distance_trees)?;
        let literals = (0..literal_trees).map(|_| Code::read(bits, 256)).collect::<Result<_>>()?;
        let commands = (0..command.types).map(|_| Code::read(bits, 704)).collect::<Result<_>>()?;
        let alphabet = 16 + direct + (48 << postfix);
        let distances =
            (0..distance_trees).map(|_| Code::read(bits, alphabet)).collect::<Result<_>>()?;
        Ok(Self {
            literal,
            command,
            distance,
            postfix,
            direct,
            modes,
            literal_map,
            distance_map,
            literals,
            commands,
            distances,
        })
    }

    /// Decodes the meta-block's commands onto `out` until it reaches `reach.end`.
    fn run(
        &mut self,
        bits: &mut Bits<'_>,
        reach: Reach,
        ring: &mut Ring,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        while out.len() < reach.end {
            self.command.next(bits)?;
            let symbol = self.commands[self.command.kind].decode(bits)?;
            let cell = symbol >> 6;
            let (insert, copy) = CELLS[cell];
            let (base, extra) = INSERT[insert + ((symbol >> 3) & 7)];
            let insert = base + bits.take(extra)?;
            let (base, extra) = COPY[copy + (symbol & 7)];
            let copy = base + bits.take(extra)?;
            if out.len() + insert > reach.end {
                return Err(broken());
            }
            for _ in 0..insert {
                self.literal.next(bits)?;
                let made = out.len() - reach.start;
                let p1 = if made > 0 { out[out.len() - 1] } else { 0 };
                let p2 = if made > 1 { out[out.len() - 2] } else { 0 };
                let kind = self.literal.kind;
                let tree = self.literal_map[64 * kind + context(self.modes[kind], p1, p2)];
                out.push(self.literals[usize::from(tree)].decode(bits)? as u8);
            }
            if out.len() == reach.end {
                break;
            }
            let distance = if cell < 2 {
                ring.take_last()
            } else {
                self.distance.next(bits)?;
                let context = if copy > 4 { 3 } else { copy - 2 };
                let tree = self.distance_map[4 * self.distance.kind + context];
                let code = self.distances[usize::from(tree)].decode(bits)?;
                self.distance_of(code, bits, ring)?
            };
            let made = out.len() - reach.start;
            if distance > reach.window.min(made) {
                let id = distance - reach.window.min(made) - 1;
                dictionary::word(copy, id, out).ok_or_else(broken)?;
                if out.len() > reach.end {
                    return Err(broken());
                }
                continue;
            }
            ring.push(distance);
            if out.len() + copy > reach.end {
                return Err(broken());
            }
            let from = out.len() - distance;
            if distance >= copy {
                out.extend_from_within(from..from + copy);
            } else {
                for offset in 0..copy {
                    out.push(out[from + offset]);
                }
            }
        }
        Ok(())
    }

    /// The distance distance code `code` means.
    fn distance_of(&self, code: usize, bits: &mut Bits<'_>, ring: &mut Ring) -> Result<usize> {
        if code == 0 {
            return Ok(ring.take_last());
        }
        if code < 4 {
            return Ok(ring.get(code));
        }
        if code < 16 {
            let (back, delta) = if code < 10 { (0, code - 4) } else { (1, code - 10) };
            let delta = [-1, 1, -2, 2, -3, 3][delta];
            let distance = ring.get(back) as i64 + delta;
            // Nothing is this far back, so a distance that came out below one fails as a word.
            return Ok(usize::try_from(distance)
                .ok()
                .filter(|&d| d > 0)
                .unwrap_or(usize::MAX >> 1));
        }
        if code < 16 + self.direct {
            return Ok(code - 15);
        }
        let x = code - self.direct - 16;
        let bits_of = 1 + (x >> (self.postfix + 1)) as u32;
        let high = x >> self.postfix;
        let low = x & ((1 << self.postfix) - 1);
        let offset = ((2 + (high & 1)) << bits_of) - 4;
        let extra = bits.take(bits_of)?;
        Ok(((offset + extra) << self.postfix) + low + self.direct + 1)
    }
}

/// The context a literal is coded in after `p1` and the `p2` before it, in context mode `mode`.
fn context(mode: u8, p1: u8, p2: u8) -> usize {
    let (p1, p2) = (usize::from(p1), usize::from(p2));
    usize::from(match mode {
        0 => (p1 & 63) as u8,
        1 => (p1 >> 2) as u8,
        2 => UTF8_LAST[p1] | UTF8_BEFORE[p2],
        _ => (SIGNED[p1] << 3) | SIGNED[p2],
    })
}

/// The stream as bits, least significant first.
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

    /// The next `n` bits, for `n` up to 24.
    fn take(&mut self, n: u32) -> Result<usize> {
        if self.count < n {
            self.refill();
            if self.count < n {
                return Err(broken());
            }
        }
        let value = (self.buffer & ((1_u64 << n) - 1)) as usize;
        self.buffer >>= n;
        self.count -= n;
        Ok(value)
    }

    /// The next bits without taking them, with zeros past the end of the input.
    fn peek(&mut self) -> u64 {
        if self.count < LONGEST as u32 {
            self.refill();
        }
        self.buffer
    }

    fn skip(&mut self, n: u32) -> Result<()> {
        self.take(n).map(|_| ())
    }

    /// Moves to the next byte, which the bits it passes over must all be zero to do.
    fn align(&mut self) -> Result<()> {
        let partial = self.count % 8;
        if self.buffer & ((1 << partial) - 1) != 0 {
            return Err(broken());
        }
        self.buffer >>= partial;
        self.count -= partial;
        Ok(())
    }

    /// The next `n` whole bytes, which must start on a byte.
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let at = self.at - (self.count / 8) as usize;
        let bytes = self.input.get(at..at + n).ok_or_else(broken)?;
        self.at = at + n;
        self.buffer = 0;
        self.count = 0;
        Ok(bytes)
    }
}

/// A prefix code, as the counts of each length and the symbols in code order, with a table for the
/// codes short enough to look up. A code of one symbol spends no bits on it.
struct Code {
    single: Option<usize>,
    count: [u16; LONGEST + 1],
    symbol: Vec<u16>,
    fast: Vec<u16>,
}

impl Code {
    /// Reads a code for `alphabet` symbols, simple or complex.
    fn read(bits: &mut Bits<'_>, alphabet: usize) -> Result<Self> {
        let mut lengths = vec![0_u8; alphabet];
        let skip = bits.take(2)?;
        if skip == 1 {
            let width = usize::BITS - (alphabet - 1).leading_zeros();
            let n = bits.take(2)? + 1;
            let mut symbols = [0; 4];
            for at in 0..n {
                let symbol = bits.take(width)?;
                if symbol >= alphabet || symbols[..at].contains(&symbol) {
                    return Err(broken());
                }
                symbols[at] = symbol;
            }
            let shape: &[u8] = match n {
                1 => return Ok(Self::single(symbols[0])),
                2 => &[1, 1],
                3 => &[1, 2, 2],
                _ if bits.take(1)? == 1 => &[1, 2, 3, 3],
                _ => &[2, 2, 2, 2],
            };
            for (&symbol, &length) in symbols.iter().zip(shape) {
                lengths[symbol] = length;
            }
            return Self::new(&lengths);
        }

        let mut code_lengths = [0_u8; 18];
        let mut space = 32_i32;
        let mut used = 0;
        for &symbol in &ORDER[skip..] {
            let peek = (bits.peek() & 15) as usize;
            bits.skip(LENGTH_BITS[peek])?;
            let length = LENGTH_VALUE[peek];
            code_lengths[symbol] = length;
            if length != 0 {
                space -= 32 >> length;
                used += 1;
                if space <= 0 {
                    break;
                }
            }
        }
        if used != 1 && space != 0 {
            return Err(broken());
        }
        let lengths_code = Self::new(&code_lengths)?;

        let mut space = 32_768_i32;
        let mut previous = 8_u8;
        let (mut repeat, mut repeated) = (0_usize, 0_u8);
        let mut symbol = 0;
        while symbol < alphabet && space > 0 {
            let length = lengths_code.decode(bits)?;
            if length < 16 {
                repeat = 0;
                lengths[symbol] = length as u8;
                if length != 0 {
                    previous = length as u8;
                    space -= 32_768 >> length;
                }
                symbol += 1;
                continue;
            }
            let (extra, value) = if length == 16 { (2, previous) } else { (3, 0) };
            if repeated != value {
                repeat = 0;
                repeated = value;
            }
            let before = repeat;
            if repeat > 0 {
                repeat = (repeat - 2) << extra;
            }
            repeat += bits.take(extra)? + 3;
            let more = repeat - before;
            if symbol + more > alphabet {
                return Err(broken());
            }
            lengths[symbol..symbol + more].fill(value);
            symbol += more;
            if value != 0 {
                space -= (more << (15 - value)) as i32;
            }
        }
        if space != 0 {
            return Err(broken());
        }
        Self::new(&lengths)
    }

    fn single(symbol: usize) -> Self {
        Self { single: Some(symbol), count: [0; LONGEST + 1], symbol: Vec::new(), fast: Vec::new() }
    }

    fn new(lengths: &[u8]) -> Result<Self> {
        let mut used = lengths.iter().enumerate().filter(|&(_, &length)| length > 0);
        if let (Some((symbol, _)), None) = (used.next(), used.next()) {
            return Ok(Self::single(symbol));
        }
        let mut count = [0_u16; LONGEST + 1];
        for &length in lengths {
            count[usize::from(length)] += 1;
        }
        count[0] = 0;
        let mut left = 1_i32;
        for &n in &count[1..] {
            left = (left << 1) - i32::from(n);
            if left < 0 {
                return Err(broken());
            }
        }
        let mut offset = [0_usize; LONGEST + 2];
        for length in 1..=LONGEST {
            offset[length + 1] = offset[length] + usize::from(count[length]);
        }
        let mut symbol = vec![0_u16; offset[LONGEST + 1]];
        for (s, &length) in lengths.iter().enumerate() {
            if length != 0 {
                symbol[offset[usize::from(length)]] = s as u16;
                offset[usize::from(length)] += 1;
            }
        }
        let mut next = [0_u32; LONGEST + 1];
        let mut code = 0_u32;
        for length in 1..=LONGEST {
            code = (code + u32::from(count[length - 1])) << 1;
            next[length] = code;
        }
        let mut fast = vec![0_u16; 1 << FAST];
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
        Ok(Self { single: None, count, symbol, fast })
    }

    fn decode(&self, bits: &mut Bits<'_>) -> Result<usize> {
        if let Some(symbol) = self.single {
            return Ok(symbol);
        }
        let entry = self.fast[(bits.peek() & ((1 << FAST) - 1)) as usize];
        let length = u32::from(entry & 15);
        if entry != 0 && length <= bits.count {
            bits.buffer >>= length;
            bits.count -= length;
            return Ok(usize::from(entry >> 4));
        }
        let (mut code, mut first, mut index) = (0_i32, 0_i32, 0_i32);
        for &n in &self.count[1..] {
            code |= bits.take(1)? as i32;
            let n = i32::from(n);
            if code - n < first {
                return Ok(usize::from(self.symbol[(index + code - first) as usize]));
            }
            index += n;
            first = (first + n) << 1;
            code <<= 1;
        }
        Err(broken())
    }
}

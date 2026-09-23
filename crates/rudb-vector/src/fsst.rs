//! FSST, the string encoding.
//!
//! Fast Static Symbol Table, from the 2020 paper by Boncz, Neumann and Leis. A table of at most 255
//! symbols of one to eight bytes each, and compression is replacing the longest matching symbol at
//! each position with its one byte code. A byte that no symbol covers is escaped, which costs two
//! bytes, so the table has to be good or the output is larger than the input.
//!
//! ## Why this and not a general compressor
//!
//! `spec/06-compression.md` section 6.2 is blunt about it. FSST compresses text about 2x, which is
//! worse than what zstd does to the same bytes, and the ratio is not why it is here. Two other
//! properties are.
//!
//! The first is random access. Every string in a column is compressed independently against a
//! shared table, so reading row 4,000,000 does not mean decompressing the four million before it. A
//! block compressor gives up that property and gets it back by cutting the data into blocks, which
//! means reading one string decompresses a block.
//!
//! The second is that a substring search can run against the compressed bytes. Compress the needle
//! with the same symbol table and look for the compressed needle in the compressed haystack. That is
//! what turns `URL LIKE '%google%'` from a decompress and scan into a scan, and section 6.7 says it
//! is worth more on the ClickBench workload than any ratio improvement. It needs care, because the
//! greedy match that compresses a needle standing alone can segment it differently from the way the
//! same bytes were segmented inside a longer string, so a hit is a candidate and a miss is not a
//! proof. The scan that uses it is M3 work and lives with the rest of encoded execution.
//!
//! ## Training
//!
//! The table is built from a sample rather than from the whole column, and the algorithm is the
//! paper's: start with nothing, so every byte escapes, then repeat five times. Compress the sample
//! with the table you have, count how often each symbol is used and how often each pair of adjacent
//! symbols occurs, and build the next table from the best 255 of the symbols and the concatenations
//! by gain, where gain is how many bytes of input the symbol accounts for. Five generations is what
//! the paper found, and the shape of the thing is that the first generation learns single bytes, the
//! second learns pairs, and the fifth is finding eight byte symbols like `https://`.
//!
//! ## Matching
//!
//! Three lookups in a fixed order, longest first. A hash table on the first three bytes for symbols
//! of three bytes and up, a flat table indexed by the first two bytes, and a flat table indexed by
//! the first one. The hash table probes eight slots and keeps the longest symbol that matches rather
//! than the first, because several symbols share a three byte prefix and taking the first would make
//! the ratio depend on insertion order.

use std::cell::RefCell;

use rudb_common::{Error, Result};

/// The code that means the next byte is a literal. 255 rather than 0 so that the 255 real codes are
/// a contiguous range starting at zero and a code is its own index into the symbol table.
pub const ESCAPE: u8 = 255;

/// How many real symbols a table can hold.
pub const MAX_SYMBOLS: usize = 255;

/// The longest a symbol can be. Eight, so that a symbol is a `u64` and a match is a mask and a
/// compare rather than a loop over bytes.
pub const MAX_SYMBOL_LEN: usize = 8;

/// How many generations the trainer runs. The paper's number.
const GENERATIONS: usize = 5;

/// Slots in the prefix hash table. A power of two, and four times the largest number of symbols that
/// can be in it, which keeps the eight slot probe from filling up on a full table.
const HASH_SLOTS: usize = 1024;

/// How far a lookup probes before giving up. A miss here costs ratio and not correctness.
const PROBE: usize = 8;

/// One symbol. The bytes are in the low end of `value` in the order they appear, so that a match
/// against the next eight bytes of input is one mask and one compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Symbol {
    value: u64,
    len: u8,
}

impl Symbol {
    fn new(bytes: &[u8]) -> Self {
        let len = bytes.len().min(MAX_SYMBOL_LEN);
        let mut value = 0u64;
        for (index, byte) in bytes[..len].iter().enumerate() {
            value |= u64::from(*byte) << (8 * index);
        }
        Self { value, len: len as u8 }
    }

    fn single(byte: u8) -> Self {
        Self { value: u64::from(byte), len: 1 }
    }

    fn len(self) -> usize {
        self.len as usize
    }

    fn mask(self) -> u64 {
        mask_of(self.len())
    }

    fn bytes(self) -> Vec<u8> {
        (0..self.len()).map(|index| (self.value >> (8 * index)) as u8).collect()
    }

    /// The two symbols end to end, cut off at eight bytes.
    fn concat(self, other: Self) -> Self {
        if self.len() >= MAX_SYMBOL_LEN {
            return self;
        }
        let len = (self.len() + other.len()).min(MAX_SYMBOL_LEN);
        let value = self.value | (other.value << (8 * self.len()));
        Self { value: value & mask_of(len), len: len as u8 }
    }
}

fn mask_of(len: usize) -> u64 {
    if len >= 8 { u64::MAX } else { (1u64 << (8 * len)) - 1 }
}

/// A trained symbol table, and everything needed to compress and decompress against it.
pub struct SymbolTable {
    /// Code to symbol. At most [`MAX_SYMBOLS`] long.
    symbols: Vec<Symbol>,
    /// First byte to code, or [`ESCAPE`] when no one byte symbol covers it.
    single: Vec<u8>,
    /// First two bytes to code, or `u16::MAX` when there is no two byte symbol for them.
    pair: Vec<u16>,
    /// Open addressed, keyed on the first three bytes, holding every symbol of three bytes or more.
    hash: Vec<Option<(Symbol, u8)>>,
}

impl std::fmt::Debug for SymbolTable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The lookup tables are 64k entries and printing them is never what anybody wanted.
        formatter
            .debug_struct("SymbolTable")
            .field("symbols", &self.symbols.len())
            .field("bytes", &self.serialized_len())
            .finish()
    }
}

/// Two tables are equal when they hold the same symbols in the same order.
///
/// The three lookup tables are built from the symbols when a table is built and hold nothing the
/// symbols do not, so comparing them would be comparing the same information a second time over
/// sixty five thousand entries. A vector in FSST form carries a table, and a vector is compared for
/// equality all over the tests, so this is on a path that gets walked.
impl PartialEq for SymbolTable {
    fn eq(&self, other: &Self) -> bool {
        self.symbols == other.symbols
    }
}

impl Eq for SymbolTable {}

impl SymbolTable {
    /// How many bytes of memory this table is holding.
    ///
    /// Mostly the hash table, which is sixty five thousand slots however few symbols are in it. That
    /// is the number a vector in FSST form reports, and it is why the form is a decision about a
    /// page rather than about a chunk: one table over a hundred chunks is nothing per chunk and one
    /// table per chunk is a megabyte.
    #[must_use]
    pub fn footprint(&self) -> usize {
        size_of::<Self>()
            + self.symbols.capacity() * size_of::<Symbol>()
            + self.single.capacity()
            + self.pair.capacity() * size_of::<u16>()
            + self.hash.capacity() * size_of::<Option<(Symbol, u8)>>()
    }

    /// A table with no symbols, which escapes everything and doubles its input. The starting point
    /// of training, and what a column of nothing but unique bytes ends up with.
    #[must_use]
    pub fn empty() -> Self {
        Self::build(Vec::new())
    }

    /// Trains a table on a sample.
    ///
    /// The caller picks the sample. Section 6.3 says a systematic sample across the chunk rather
    /// than the first N rows, because column data is frequently clustered, and that decision belongs
    /// to whoever knows what the chunk is rather than to this function.
    #[must_use]
    pub fn train(samples: &[&[u8]]) -> Self {
        // The counts are a megabyte of pair slots, and a load trains a table for every block of
        // every text column it writes. So each thread keeps one and clears the slots it used, rather
        // than asking for a fresh megabyte of zeroes and faulting it in on every block.
        thread_local! {
            static COUNTS: RefCell<Option<Counts>> = const { RefCell::new(None) };
        }
        COUNTS.with(|held| match held.try_borrow_mut() {
            Ok(mut held) => Self::train_with(samples, held.get_or_insert_with(Counts::new)),
            Err(_) => Self::train_with(samples, &mut Counts::new()),
        })
    }

    fn train_with(samples: &[&[u8]], counts: &mut Counts) -> Self {
        let mut table = Self::empty();
        for _ in 0..GENERATIONS {
            counts.clear();
            for sample in samples {
                table.count(sample, counts);
            }
            let next = counts.best(&table);
            if next.is_empty() {
                break;
            }
            table = Self::build(next);
        }
        table
    }

    /// How many symbols are in the table.
    #[must_use]
    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    /// Whether the table has no symbols, in which case every byte of every string escapes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    /// How many bytes [`serialize`](Self::serialize) writes. At most 2049 for a full table, and
    /// that is the number section 6.4 is weighing when it says a shared symbol table is cheaper
    /// than a shared dictionary.
    #[must_use]
    pub fn serialized_len(&self) -> usize {
        1 + self.symbols.iter().map(|symbol| 1 + symbol.len()).sum::<usize>()
    }

    /// Writes the table itself, which has to travel with the data it compressed.
    pub fn serialize(&self, out: &mut Vec<u8>) {
        out.push(self.symbols.len() as u8);
        for symbol in &self.symbols {
            out.push(symbol.len);
            out.extend_from_slice(&symbol.bytes());
        }
    }

    /// Reads back what [`serialize`](Self::serialize) wrote, and says how many bytes it consumed.
    ///
    /// # Errors
    ///
    /// If the bytes are truncated or describe a symbol of zero or more than eight bytes.
    pub fn deserialize(bytes: &[u8]) -> Result<(Self, usize)> {
        let count = *bytes.first().ok_or_else(|| truncated("a symbol table header"))? as usize;
        let mut at = 1;
        let mut symbols = Vec::with_capacity(count);
        for _ in 0..count {
            let len = *bytes.get(at).ok_or_else(|| truncated("a symbol length"))? as usize;
            if len == 0 || len > MAX_SYMBOL_LEN {
                return Err(Error::internal(format!("a symbol of {len} bytes is not a symbol")));
            }
            at += 1;
            let end = at + len;
            if end > bytes.len() {
                return Err(truncated("a symbol"));
            }
            symbols.push(Symbol::new(&bytes[at..end]));
            at = end;
        }
        Ok((Self::build(symbols), at))
    }

    /// Compresses one string, appending to `out`.
    ///
    /// Strings are compressed one at a time against a shared table rather than as one stream,
    /// because that is what keeps random access, which is the first of the two reasons this encoding
    /// was chosen at all.
    pub fn compress(&self, input: &[u8], out: &mut Vec<u8>) {
        let mut at = 0;
        while at < input.len() {
            let (code, len) = self.match_at(input, at);
            if code == ESCAPE {
                out.push(ESCAPE);
                out.push(input[at]);
            } else {
                out.push(code);
            }
            at += len;
        }
    }

    /// Decompresses one string, appending to `out`.
    ///
    /// A symbol goes out as all eight of the bytes its `u64` holds, and then the cursor steps back
    /// over the ones that were not part of it. Eight is a length the compiler knows, so that is one
    /// store. The length a symbol really has is only known at run time, so copying exactly that
    /// many bytes is a call into `memcpy` for one to eight of them, and building a `Vec` to copy
    /// them out of, which is what this used to do, is a heap allocation and a free on top.
    ///
    /// That mattered more than anything else in the engine. `SELECT COUNT(*) FROM hits WHERE URL
    /// LIKE '%google%'` over ClickBench spends almost all of its time here, because the search
    /// itself runs once per distinct URL and finding those means decompressing the column, and the
    /// allocation, the free and the copy together were 41% of the query.
    ///
    /// # Errors
    ///
    /// If the input ends on an escape byte, or holds a code the table does not have.
    pub fn decompress(&self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        // What the table is trained to reach, plus room for the tail of the last symbol, so the
        // loop below mostly finds the space already there. Nothing here depends on the guess being
        // right: too small and the growth happens where it always did.
        out.reserve(input.len().saturating_mul(2).saturating_add(MAX_SYMBOL_LEN));
        let mut at = 0;
        while at < input.len() {
            let code = input[at];
            at += 1;
            if code == ESCAPE {
                let literal = *input.get(at).ok_or_else(|| truncated("an escaped byte"))?;
                out.push(literal);
                at += 1;
            } else {
                let symbol = *self
                    .symbols
                    .get(code as usize)
                    .ok_or_else(|| Error::internal(format!("code {code} is not in the table")))?;
                out.extend_from_slice(&symbol.value.to_le_bytes());
                out.truncate(out.len() - (MAX_SYMBOL_LEN - symbol.len()));
            }
        }
        Ok(())
    }

    /// The code and how many input bytes it covers. [`ESCAPE`] and 1 when nothing matches.
    fn match_at(&self, input: &[u8], at: usize) -> (u8, usize) {
        let remaining = input.len() - at;
        let word = load(input, at);
        // Written as a nested `if` rather than as a chained `if let` because the minimum supported
        // Rust version is 1.85 and let chains landed in 1.88.
        if remaining >= 3 {
            if let Some((symbol, code)) = self.probe(word, remaining) {
                return (code, symbol.len());
            }
        }
        if remaining >= 2 {
            let code = self.pair[(word & 0xffff) as usize];
            if code != u16::MAX {
                return (code as u8, 2);
            }
        }
        let code = self.single[(word & 0xff) as usize];
        if code == ESCAPE { (ESCAPE, 1) } else { (code, 1) }
    }

    /// The longest symbol of three bytes or more matching here, if any.
    ///
    /// Longest rather than first, because several symbols share a three byte prefix and taking
    /// whichever the probe reached first would make the compression ratio depend on the order the
    /// table was built in.
    fn probe(&self, word: u64, remaining: usize) -> Option<(Symbol, u8)> {
        let mut slot = hash_of(word);
        let mut best: Option<(Symbol, u8)> = None;
        for _ in 0..PROBE {
            match self.hash[slot] {
                None => break,
                Some((symbol, code)) => {
                    if symbol.len() <= remaining
                        && word & symbol.mask() == symbol.value
                        && best.is_none_or(|(found, _)| symbol.len() > found.len())
                    {
                        best = Some((symbol, code));
                    }
                }
            }
            slot = (slot + 1) & (HASH_SLOTS - 1);
        }
        best
    }

    /// Runs the matcher over a sample without producing output, recording what it used. This is the
    /// counting half of a training generation.
    fn count(&self, input: &[u8], counts: &mut Counts) {
        let mut at = 0;
        let mut previous: Option<u16> = None;
        while at < input.len() {
            let (code, len) = self.match_at(input, at);
            let id = if code == ESCAPE { 256 + u16::from(input[at]) } else { u16::from(code) };
            counts.one(id);
            if let Some(previous) = previous {
                counts.two(previous, id);
            }
            previous = Some(id);
            at += len;
        }
    }

    fn build(symbols: Vec<Symbol>) -> Self {
        let mut table = Self {
            symbols,
            single: vec![ESCAPE; 256],
            pair: vec![u16::MAX; 65536],
            hash: vec![None; HASH_SLOTS],
        };
        // Longest first, so that a short symbol never displaces a long one out of the probe window
        // and the flat tables get the lowest code for a duplicate.
        let mut order: Vec<(Symbol, u8)> =
            table.symbols.iter().enumerate().map(|(code, symbol)| (*symbol, code as u8)).collect();
        order.sort_by_key(|(symbol, code)| (std::cmp::Reverse(symbol.len()), *code));
        for (symbol, code) in order {
            match symbol.len() {
                1 => {
                    let index = (symbol.value & 0xff) as usize;
                    if table.single[index] == ESCAPE {
                        table.single[index] = code;
                    }
                }
                2 => {
                    let index = (symbol.value & 0xffff) as usize;
                    if table.pair[index] == u16::MAX {
                        table.pair[index] = u16::from(code);
                    }
                }
                _ => {
                    let mut slot = hash_of(symbol.value);
                    for _ in 0..PROBE {
                        if table.hash[slot].is_none() {
                            table.hash[slot] = Some((symbol, code));
                            break;
                        }
                        slot = (slot + 1) & (HASH_SLOTS - 1);
                    }
                }
            }
        }
        table
    }
}

/// The next eight bytes as a little endian word, zero padded at the end of the input.
///
/// The padding is why every match checks the remaining length as well as the mask. Without that
/// check a two byte symbol ending in a zero byte would match the last byte of a string.
fn load(input: &[u8], at: usize) -> u64 {
    if at + 8 <= input.len() {
        let bytes: [u8; 8] = input[at..at + 8].try_into().expect("eight bytes were checked");
        u64::from_le_bytes(bytes)
    } else {
        let mut word = 0u64;
        for (index, byte) in input[at..].iter().enumerate() {
            word |= u64::from(*byte) << (8 * index);
        }
        word
    }
}

/// Hashes the first three bytes. The multiply and shift is the standard Fibonacci hash, which
/// spreads a three byte key across the whole slot range where a mask of the low bits would put every
/// symbol starting with the same letter in the same neighbourhood.
fn hash_of(word: u64) -> usize {
    let key = word & 0xff_ffff;
    ((key.wrapping_mul(0x9e37_79b9_7f4a_7c15)) >> (64 - HASH_SLOTS.trailing_zeros())) as usize
}

/// What one training generation counts. Symbol ids below 256 are codes in the current table and ids
/// from 256 up are escaped literal bytes, which is how a generation learns single bytes it does not
/// have yet.
struct Counts {
    single: Vec<u32>,
    /// Indexed by `first * IDS + second`. Flat rather than hashed because every id is under 512, so
    /// every pair fits in a quarter million slots, and counting pairs is most of what training does.
    pairs: Vec<u32>,
    /// The pair slots that are not zero, so that reading and clearing them costs the pairs seen
    /// rather than the whole array.
    seen: Vec<u32>,
    /// The candidates of the generation being ranked, kept so a thread sizes them once.
    gains: Gains,
}

/// Every candidate symbol once with its summed gain, which [`Counts::best`] ranks.
#[derive(Default)]
struct Gains {
    gains: Vec<(Symbol, u64)>,
    /// Where each symbol sits in `gains`, open addressed on the symbol, `u32::MAX` where empty.
    places: Vec<u32>,
    /// The slots of `places` in use, so that emptying it costs the symbols rather than the table.
    taken: Vec<u32>,
}

impl Gains {
    /// Empties the candidates, with room for `most` of them.
    fn clear(&mut self, most: usize) {
        for place in self.taken.drain(..) {
            self.places[place as usize] = u32::MAX;
        }
        let wanted = (most * 2).next_power_of_two();
        if self.places.len() < wanted {
            self.places = vec![u32::MAX; wanted];
        }
        self.gains.clear();
    }

    /// Adds `count` uses of `symbol` to its gain, making it a candidate if it is not one yet.
    fn add(&mut self, symbol: Symbol, count: u64) {
        let gain = count * symbol.len() as u64;
        let mask = self.places.len() - 1;
        let mut place = gain_hash(symbol) & mask;
        loop {
            let at = self.places[place];
            if at == u32::MAX {
                self.places[place] = self.gains.len() as u32;
                self.taken.push(place as u32);
                self.gains.push((symbol, gain));
                return;
            }
            if self.gains[at as usize].0 == symbol {
                self.gains[at as usize].1 += gain;
                return;
            }
            place = (place + 1) & mask;
        }
    }
}

/// How many symbol ids there are: 256 codes and 256 escaped bytes.
const IDS: usize = 512;

impl Counts {
    fn new() -> Self {
        Self {
            single: vec![0; IDS],
            pairs: vec![0; IDS * IDS],
            seen: Vec::new(),
            gains: Gains::default(),
        }
    }

    fn clear(&mut self) {
        self.single.fill(0);
        for slot in self.seen.drain(..) {
            self.pairs[slot as usize] = 0;
        }
    }

    fn one(&mut self, id: u16) {
        self.single[id as usize] += 1;
    }

    fn two(&mut self, first: u16, second: u16) {
        let slot = first as usize * IDS + second as usize;
        if self.pairs[slot] == 0 {
            self.seen.push(slot as u32);
        }
        self.pairs[slot] += 1;
    }

    /// The 255 best symbols for the next generation.
    ///
    /// Gain is how many bytes of input a symbol accounts for, which is its length times how often it
    /// was used. A concatenation is scored on the length it would have, so a pair of four byte
    /// symbols scores as eight and a pair of six byte ones also scores as eight, because that is
    /// what it would be cut down to.
    fn best(&mut self, table: &SymbolTable) -> Vec<Symbol> {
        // Different ids can spell the same symbol, a code and the pair it was learned from for one,
        // and two pairs whose concatenation runs past eight bytes for another, so the gains are
        // summed per symbol before anything is ranked. This was a sort of every candidate by symbol
        // followed by a dedup, and on a ClickBench load that sort was a quarter of training.
        self.gains.clear(IDS + self.seen.len());
        for (id, count) in self.single.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            let symbol = symbol_of(table, id as u16);
            self.gains.add(symbol, u64::from(*count));
        }
        for slot in &self.seen {
            let slot = *slot as usize;
            let (first, second) = ((slot / IDS) as u16, (slot % IDS) as u16);
            let symbol = symbol_of(table, first).concat(symbol_of(table, second));
            self.gains.add(symbol, u64::from(self.pairs[slot]));
        }
        let gains = &mut self.gains.gains;
        // Gain first, then the symbol itself, so that two symbols with the same gain come out in the
        // same order on every host and the table is a function of the sample and nothing else. The
        // symbols are distinct by now, so the order is total and an unstable sort gives one answer.
        let order = |left: &(Symbol, u64), right: &(Symbol, u64)| {
            right.1.cmp(&left.1).then(left.0.cmp(&right.0))
        };
        if gains.len() > MAX_SYMBOLS {
            gains.select_nth_unstable_by(MAX_SYMBOLS - 1, order);
            gains.truncate(MAX_SYMBOLS);
        }
        gains.sort_unstable_by(order);
        gains.iter().map(|(symbol, _)| *symbol).collect()
    }
}

/// Where a symbol's search for its place in [`Gains::places`] starts.
fn gain_hash(symbol: Symbol) -> usize {
    ((symbol.value ^ u64::from(symbol.len)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 32) as usize
}

fn symbol_of(table: &SymbolTable, id: u16) -> Symbol {
    if (id as usize) < table.symbols.len() {
        table.symbols[id as usize]
    } else {
        Symbol::single((id.saturating_sub(256)) as u8)
    }
}

fn truncated(what: &str) -> Error {
    Error::internal(format!("the input ended in the middle of {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A few hundred URLs in the shape ClickBench `hits` has them, which is the workload this
    /// encoding was chosen for. Repetitive in the way real URLs are: a handful of hosts, a handful
    /// of path shapes, and query strings that differ in a number.
    fn urls() -> Vec<Vec<u8>> {
        let hosts = ["www.example.com", "shop.example.com", "news.other.example.org"];
        let paths = ["/index.html", "/catalog/item", "/search", "/user/profile/settings"];
        let mut out = Vec::new();
        for index in 0..600 {
            let host = hosts[index % hosts.len()];
            let path = paths[(index / 3) % paths.len()];
            out.push(
                format!("http://{host}{path}?session={}&ref=google&page={}", index * 7, index % 20)
                    .into_bytes(),
            );
        }
        out
    }

    fn borrow(strings: &[Vec<u8>]) -> Vec<&[u8]> {
        strings.iter().map(Vec::as_slice).collect()
    }

    fn round_trip(table: &SymbolTable, strings: &[Vec<u8>]) -> (usize, usize) {
        let mut raw = 0;
        let mut compressed = 0;
        for string in strings {
            let mut bytes = Vec::new();
            table.compress(string, &mut bytes);
            let mut back = Vec::new();
            table.decompress(&bytes, &mut back).unwrap();
            assert_eq!(back, *string, "{}", String::from_utf8_lossy(string));
            raw += string.len();
            compressed += bytes.len();
        }
        (raw, compressed)
    }

    #[test]
    fn urls_compress_by_more_than_half_and_come_back_unchanged() {
        // The number the paper reports on text is around 2x, and URLs are more repetitive than
        // text. Anything under 2x here means the trainer is not finding the long symbols.
        let strings = urls();
        let table = SymbolTable::train(&borrow(&strings));
        let (raw, compressed) = round_trip(&table, &strings);
        let ratio = raw as f64 / compressed as f64;
        assert!(ratio > 2.5, "{ratio:.2}x, {raw} to {compressed}");
        assert!(table.len() > 100, "{} symbols", table.len());
    }

    #[test]
    fn the_trainer_finds_the_long_repeated_pieces() {
        let strings = urls();
        let table = SymbolTable::train(&borrow(&strings));
        let found: Vec<String> = (0..table.len())
            .map(|code| String::from_utf8_lossy(&table.symbols[code].bytes()).into_owned())
            .collect();
        // Not a specific symbol, since which eight bytes win is a property of the sample, but the
        // table has to be mostly long symbols or it has not learned anything.
        let long = found.iter().filter(|symbol| symbol.len() >= 6).count();
        assert!(long > 60, "only {long} symbols of six bytes or more: {found:?}");
    }

    #[test]
    fn english_text_round_trips_and_shrinks() {
        let text: Vec<Vec<u8>> = "the quick brown fox jumps over the lazy dog while the other dog \
             watches the fox and the dog and the fox go over the hill together"
            .split(' ')
            .map(|word| word.as_bytes().to_vec())
            .collect();
        let table = SymbolTable::train(&borrow(&text));
        let (raw, compressed) = round_trip(&table, &text);
        assert!(compressed < raw, "{raw} to {compressed}");
    }

    #[test]
    fn incompressible_bytes_round_trip_and_cost_what_escaping_costs() {
        // The worst case, and it has to be a correct worst case. Every byte escapes at two bytes
        // each unless the trainer finds single byte symbols, which it will for the 255 most common
        // of the 256 values.
        let mut state = 0x1234_5678_9abc_def0u64;
        let strings: Vec<Vec<u8>> = (0..100)
            .map(|_| {
                (0..64)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state as u8
                    })
                    .collect()
            })
            .collect();
        let table = SymbolTable::train(&borrow(&strings));
        let (raw, compressed) = round_trip(&table, &strings);
        assert!(compressed < raw * 2, "{raw} to {compressed}");
    }

    #[test]
    fn an_empty_table_escapes_everything_and_still_round_trips() {
        let table = SymbolTable::empty();
        let strings = vec![b"hello".to_vec(), Vec::new(), b"x".to_vec()];
        let (raw, compressed) = round_trip(&table, &strings);
        assert_eq!(compressed, raw * 2);
    }

    #[test]
    fn an_empty_string_compresses_to_nothing() {
        let table = SymbolTable::train(&[b"abcabcabc"]);
        let mut out = Vec::new();
        table.compress(b"", &mut out);
        assert!(out.is_empty());
        let mut back = Vec::new();
        table.decompress(&out, &mut back).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn a_string_shorter_than_the_symbols_does_not_read_past_its_end() {
        // The load pads with zeros, so without the length check a two byte symbol whose second byte
        // is zero would match the last byte of a string and swallow a byte that is not there.
        let table = SymbolTable::train(&[b"ab\0ab\0ab\0ab\0", b"abcdefgh"]);
        for string in [b"a".to_vec(), b"ab".to_vec(), b"abc".to_vec()] {
            let mut bytes = Vec::new();
            table.compress(&string, &mut bytes);
            let mut back = Vec::new();
            table.decompress(&bytes, &mut back).unwrap();
            assert_eq!(back, string);
        }
    }

    #[test]
    fn a_table_survives_being_written_and_read_back() {
        let strings = urls();
        let table = SymbolTable::train(&borrow(&strings));
        let mut bytes = Vec::new();
        table.serialize(&mut bytes);
        assert_eq!(bytes.len(), table.serialized_len());
        let (read, consumed) = SymbolTable::deserialize(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(read.symbols, table.symbols);

        // And the read back table compresses to the same bytes, which is the property that matters,
        // since the lookup structures are rebuilt rather than stored.
        let mut first = Vec::new();
        let mut second = Vec::new();
        table.compress(&strings[7], &mut first);
        read.compress(&strings[7], &mut second);
        assert_eq!(first, second);
    }

    #[test]
    fn a_full_table_is_two_kilobytes_at_the_very_most() {
        let strings = urls();
        let table = SymbolTable::train(&borrow(&strings));
        assert!(table.serialized_len() <= 1 + MAX_SYMBOLS * (1 + MAX_SYMBOL_LEN));
        assert!(table.serialized_len() <= 2049);
    }

    #[test]
    fn a_truncated_symbol_table_is_an_error() {
        let strings = urls();
        let table = SymbolTable::train(&borrow(&strings));
        let mut bytes = Vec::new();
        table.serialize(&mut bytes);
        for len in 1..bytes.len().min(40) {
            let error = SymbolTable::deserialize(&bytes[..len]).unwrap_err();
            assert!(error.message().contains("ended in the middle"), "{error}");
        }
    }

    #[test]
    fn a_symbol_of_zero_bytes_is_an_error() {
        let error = SymbolTable::deserialize(&[1, 0]).unwrap_err();
        assert!(error.message().contains("is not a symbol"), "{error}");
    }

    #[test]
    fn a_short_symbol_does_not_drag_the_rest_of_its_word_out_with_it() {
        // Decompression writes all eight bytes of a symbol and steps back over the ones that were
        // not part of it, so a table of short symbols is where that would show. `ab` and `cd` are
        // two bytes each and sit in a `u64` with six zero bytes above them, and if the step back
        // were wrong those zeros would be in the answer. Appending twice checks it again at an
        // offset, since the second write lands where the first one left the cursor.
        let table = SymbolTable::train(&[b"abcdabcdabcdabcd"]);
        let mut compressed = Vec::new();
        table.compress(b"abcdabcd", &mut compressed);
        let mut out = Vec::new();
        table.decompress(&compressed, &mut out).expect("decompresses");
        table.decompress(&compressed, &mut out).expect("decompresses");
        assert_eq!(out, b"abcdabcdabcdabcd");
    }

    #[test]
    fn a_dangling_escape_is_an_error_and_not_a_panic() {
        let table = SymbolTable::train(&[b"abcabcabc"]);
        let error = table.decompress(&[ESCAPE], &mut Vec::new()).unwrap_err();
        assert!(error.message().contains("escaped byte"), "{error}");
    }

    #[test]
    fn a_code_the_table_does_not_have_is_an_error() {
        let table = SymbolTable::train(&[b"abcabcabc"]);
        let code = table.len() as u8;
        let error = table.decompress(&[code], &mut Vec::new()).unwrap_err();
        assert!(error.message().contains("not in the table"), "{error}");
    }

    #[test]
    fn training_twice_on_the_same_sample_gives_the_same_table() {
        // A table that differs run to run would make every size in the M1 report unreproducible.
        let strings = urls();
        let first = SymbolTable::train(&borrow(&strings));
        let second = SymbolTable::train(&borrow(&strings));
        assert_eq!(first.symbols, second.symbols);
    }

    #[test]
    fn the_longest_match_wins_rather_than_the_first_one_found() {
        let table = SymbolTable::build(vec![
            Symbol::new(b"abc"),
            Symbol::new(b"abcdef"),
            Symbol::new(b"abcd"),
        ]);
        let mut out = Vec::new();
        table.compress(b"abcdef", &mut out);
        assert_eq!(out, vec![1]);
    }

    #[test]
    fn a_symbol_longer_than_what_is_left_is_not_used() {
        let table = SymbolTable::build(vec![Symbol::new(b"abcdef"), Symbol::new(b"ab")]);
        let mut out = Vec::new();
        table.compress(b"abcd", &mut out);
        // "ab" then two escapes, rather than a six byte symbol over four bytes of input.
        assert_eq!(out, vec![1, ESCAPE, b'c', ESCAPE, b'd']);
    }

    #[test]
    fn concatenation_stops_at_eight_bytes() {
        let long = Symbol::new(b"abcdef");
        assert_eq!(long.concat(Symbol::new(b"ghijkl")).bytes(), b"abcdefgh");
        assert_eq!(Symbol::new(b"ab").concat(Symbol::new(b"cd")).bytes(), b"abcd");
    }

    /// The trainer the way it was written first, with the pairs and the gains in hash maps and one
    /// stable sort over everything, kept here to check the flat counts pick the same symbols.
    fn train_with_maps(samples: &[&[u8]]) -> SymbolTable {
        use std::collections::HashMap;
        let mut table = SymbolTable::empty();
        for _ in 0..GENERATIONS {
            let mut single = [0u32; IDS];
            let mut pairs: HashMap<(u16, u16), u32> = HashMap::new();
            for sample in samples {
                let mut at = 0;
                let mut previous: Option<u16> = None;
                while at < sample.len() {
                    let (code, len) = table.match_at(sample, at);
                    let id =
                        if code == ESCAPE { 256 + u16::from(sample[at]) } else { u16::from(code) };
                    single[id as usize] += 1;
                    if let Some(previous) = previous {
                        *pairs.entry((previous, id)).or_insert(0) += 1;
                    }
                    previous = Some(id);
                    at += len;
                }
            }
            let mut gains: HashMap<Symbol, u64> = HashMap::new();
            for (id, count) in single.iter().enumerate().filter(|(_, count)| **count > 0) {
                let symbol = symbol_of(&table, id as u16);
                *gains.entry(symbol).or_insert(0) += u64::from(*count) * symbol.len() as u64;
            }
            for ((first, second), count) in &pairs {
                let symbol = symbol_of(&table, *first).concat(symbol_of(&table, *second));
                *gains.entry(symbol).or_insert(0) += u64::from(*count) * symbol.len() as u64;
            }
            let mut ranked: Vec<(Symbol, u64)> = gains.into_iter().collect();
            ranked.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
            ranked.truncate(MAX_SYMBOLS);
            if ranked.is_empty() {
                break;
            }
            table = SymbolTable::build(ranked.into_iter().map(|(symbol, _)| symbol).collect());
        }
        table
    }

    #[test]
    fn flat_counts_train_the_same_table_as_hash_maps() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut shapes: Vec<Vec<Vec<u8>>> = vec![urls(), Vec::new(), vec![Vec::new(); 3]];
        // Few letters, so that many pairs tie on gain and the tie break decides the table.
        shapes.push(
            (0..400).map(|_| (0..12).map(|_| b"abc"[(next() % 3) as usize]).collect()).collect(),
        );
        // Every byte value, so that escapes of all 256 bytes are counted.
        shapes.push((0..300).map(|_| (0..40).map(|_| next() as u8).collect()).collect());
        // Long repeats, so that concatenations reach eight bytes and get cut.
        shapes.push(
            (0..200)
                .map(|index| format!("prefix-{}-suffix-{}", index % 7, index % 5).into_bytes())
                .collect(),
        );
        // One after another on one thread, so every table after the first is trained on the counts
        // the one before it left behind, which is what a load does block after block.
        for strings in &shapes {
            let samples = borrow(strings);
            assert_eq!(SymbolTable::train(&samples).symbols, train_with_maps(&samples).symbols);
        }
    }
}

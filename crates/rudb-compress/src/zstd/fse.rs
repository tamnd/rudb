//! Finite state entropy, which is what zstd uses everywhere a Huffman code would lose too much.
//!
//! FSE is an arithmetic coder in the shape of a state machine. A table of `1 << log` slots is laid
//! out so that a symbol occupies a number of slots proportional to how often it occurs, and the
//! current state is a slot. Reading a symbol is a table lookup, and moving to the next state costs
//! a number of bits that falls as the symbol gets more common, which is how a symbol ends up coded
//! in a fractional number of bits without any arithmetic.
//!
//! Three things here. Reading a normalized distribution out of a table description, building the
//! decoding table from one, and walking it. The distribution is what a compressed block carries;
//! the table is what the walk needs, and building it is where the format's one piece of real
//! cleverness lives, in [`Table::build`].

use rudb_common::{Error, Result};

use super::bits::{Backward, Forward};

/// One slot of a decoding table.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Slot {
    /// The symbol a decoder standing here reads.
    pub(crate) symbol: u8,
    /// How many bits it then takes from the stream to move on.
    pub(crate) bits: u8,
    /// What those bits are added to, to get the next slot.
    pub(crate) next: u16,
}

/// A built decoding table.
#[derive(Debug, Clone)]
pub(crate) struct Table {
    /// The base two logarithm of the slot count, which is also the width of an initial state.
    log: u32,
    slots: Vec<Slot>,
}

/// Where a decoder is standing in a table.
#[derive(Debug, Clone, Copy)]
pub(crate) struct State(u16);

impl Table {
    /// A table with one symbol in it and no bits between occurrences.
    ///
    /// What a section in RLE mode means: every sequence uses the same code, so there is nothing to
    /// read. It is a real table rather than a special case so the decoding loop does not have to
    /// know which of the four modes built the table it is walking.
    pub(crate) fn rle(symbol: u8) -> Self {
        Self { log: 0, slots: vec![Slot { symbol, bits: 0, next: 0 }] }
    }

    /// Builds a table from a normalized distribution.
    ///
    /// A count of -1 means the symbol appears in the source but less than once per table size, and
    /// those symbols are laid down at the top of the table from the last slot backwards. Everything
    /// else is spread by walking the table in a stride of just over five eighths of its size, which
    /// is co-prime with the size and so visits every slot exactly once. The stride is what gives
    /// each symbol slots scattered across the table rather than in a run, and scattered is what
    /// makes the bit cost of leaving a slot close to the ideal fractional cost.
    ///
    /// # Errors
    ///
    /// If the counts do not add up to the table size, which means the description and the table
    /// size disagree and nothing decoded from here would mean anything.
    pub(crate) fn build(counts: &[i32], log: u32) -> Result<Self> {
        if log > 15 {
            return Err(Error::io(format!("an FSE table of 2^{log} slots is not one zstd writes")));
        }
        let size = 1usize << log;
        let total: i64 = counts.iter().map(|&count| i64::from(count.abs())).sum();
        if total != size as i64 {
            return Err(Error::io(format!(
                "an FSE description accounting for {total} of the {size} slots in its table"
            )));
        }
        let mut symbols = vec![0u8; size];
        let mut seen = vec![0u32; counts.len()];
        let mut top = size;
        for (symbol, &count) in counts.iter().enumerate() {
            if count == -1 {
                if top == 0 {
                    return Err(Error::io("an FSE description with more symbols than slots"));
                }
                top -= 1;
                symbols[top] = symbol as u8;
                seen[symbol] = 1;
            } else if count > 0 {
                seen[symbol] = count as u32;
            }
        }
        let mask = size - 1;
        let stride = (size >> 1) + (size >> 3) + 3;
        let mut at = 0usize;
        for (symbol, &count) in counts.iter().enumerate() {
            for _ in 0..count.max(0) {
                symbols[at] = symbol as u8;
                at = (at + stride) & mask;
                while at >= top {
                    at = (at + stride) & mask;
                }
            }
        }
        if at != 0 {
            return Err(Error::io("an FSE description whose counts do not fill its table"));
        }
        let mut slots = Vec::with_capacity(size);
        for symbol in symbols {
            let rank = seen[symbol as usize];
            seen[symbol as usize] += 1;
            let bits = log - (31 - rank.leading_zeros());
            slots.push(Slot {
                symbol,
                bits: bits as u8,
                next: ((rank << bits) - size as u32) as u16,
            });
        }
        Ok(Self { log, slots })
    }

    /// Reads a table description and builds the table, returning how many bytes it took.
    ///
    /// The description is the only forward bitstream in the format. It carries an accuracy log and
    /// then one count per symbol, each in a width that shrinks as the counts left to account for
    /// shrink, which is why the widths are not in the file: both sides derive them from what they
    /// have read so far.
    ///
    /// # Errors
    ///
    /// If the accuracy is above what this section allows, if the description runs off its bytes, or
    /// if the counts do not add up.
    pub(crate) fn read(input: &[u8], top_symbol: usize, top_log: u32) -> Result<(Self, usize)> {
        let mut bits = Forward::new(input);
        let log = bits.take(4) as u32 + 5;
        if log > top_log {
            return Err(Error::io(format!(
                "an FSE table claims an accuracy of {log} where this section allows {top_log}"
            )));
        }
        let size = 1i32 << log;
        let mut counts = vec![0i32; top_symbol + 1];
        let mut left = size + 1;
        let mut threshold = size;
        let mut width = log + 1;
        let mut symbol = 0usize;
        let mut was_zero = false;
        while left > 1 && symbol <= top_symbol {
            if was_zero {
                // A zero count is followed by a run length of further zeros, two bits at a time,
                // with three meaning three more zeros and read again. Common because a table for
                // one block usually uses a short prefix of the symbol range.
                loop {
                    let pair = bits.take(2) as usize;
                    symbol += pair.min(3);
                    if pair != 3 {
                        break;
                    }
                    if symbol > top_symbol {
                        break;
                    }
                }
                if symbol > top_symbol {
                    break;
                }
            }
            let ceiling = 2 * threshold - 1 - left;
            let low = bits.peek(width - 1) as i32;
            let count = if low < ceiling {
                bits.skip(width - 1);
                low
            } else {
                let full = bits.take(width) as i32;
                if full >= threshold { full - ceiling } else { full }
            } - 1;
            left -= count.abs();
            counts[symbol] = count;
            symbol += 1;
            was_zero = count == 0;
            while left < threshold {
                width -= 1;
                threshold >>= 1;
            }
        }
        if bits.past() {
            return Err(Error::io("an FSE table description that runs off the end of its block"));
        }
        if left != 1 {
            return Err(Error::io("an FSE table description whose counts do not add up"));
        }
        Ok((Self::build(&counts, log)?, bits.used()))
    }

    /// Reads an initial state, which is the only place a state comes from the stream directly.
    pub(crate) fn start(&self, bits: &mut Backward<'_>) -> State {
        State(bits.take(self.log) as u16)
    }

    /// The symbol a state stands on, without moving.
    ///
    /// Separate from moving because a sequence reads all three of its symbols before it reads any
    /// of their extra bits, and the last sequence in a block never moves at all.
    pub(crate) fn symbol(&self, state: State) -> u8 {
        self.slots[state.0 as usize].symbol
    }

    /// Moves a state on, which is where an FSE stream is actually consumed.
    pub(crate) fn step(&self, state: &mut State, bits: &mut Backward<'_>) {
        let slot = self.slots[state.0 as usize];
        state.0 = slot.next + bits.take(u32::from(slot.bits)) as u16;
    }

    /// Reads the symbol and moves on.
    pub(crate) fn decode(&self, state: &mut State, bits: &mut Backward<'_>) -> u8 {
        let symbol = self.symbol(*state);
        self.step(state, bits);
        symbol
    }
}

#[cfg(test)]
mod tests {
    use super::Table;
    use crate::zstd::bits::Backward;

    #[test]
    fn an_rle_table_reads_its_one_symbol_forever_and_costs_nothing() {
        let table = Table::rle(7);
        let mut bits = Backward::new(&[0b0000_0001]).unwrap();
        let mut state = table.start(&mut bits);
        assert_eq!(table.decode(&mut state, &mut bits), 7);
        assert_eq!(table.decode(&mut state, &mut bits), 7);
    }

    #[test]
    fn a_table_gives_every_slot_to_exactly_one_symbol() {
        // Two symbols, one three times as common as the other, in a table of four slots.
        let table = Table::build(&[3, 1], 2).unwrap();
        let mut counted = [0usize; 2];
        for slot in &table.slots {
            counted[slot.symbol as usize] += 1;
        }
        assert_eq!(counted, [3, 1]);
    }

    #[test]
    fn a_rare_symbol_goes_at_the_top_of_the_table_where_it_costs_the_most_to_reach() {
        let table = Table::build(&[3, -1], 2).unwrap();
        assert_eq!(table.slots[3].symbol, 1);
        assert_eq!(table.slots[3].bits, 2, "the whole state has to be read again to leave it");
    }

    #[test]
    fn counts_that_do_not_fill_the_table_are_an_error_rather_than_a_short_table() {
        assert!(Table::build(&[2, 1], 2).is_err());
    }

    #[test]
    fn a_description_claiming_more_accuracy_than_its_section_allows_is_refused() {
        // The low four bits are the accuracy log less five, so 0b1111 asks for twenty.
        let error = Table::read(&[0x0F, 0, 0, 0], 35, 9).unwrap_err();
        assert!(error.message().contains("accuracy of 20"), "{}", error.message());
    }
}

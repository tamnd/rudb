//! LZ4 blocks, the framing Parquet calls `LZ4_RAW`.
//!
//! A block is a run of sequences. Each starts with a token whose high four bits are how many
//! literals follow and whose low four bits are how long the match after them is, less four, with
//! fifteen in either meaning more bytes of length follow. The literals come next, then the match's
//! distance back as two little endian bytes, and then the rest of its length. The last sequence
//! is literals alone.
//!
//! [`compress`] makes the same choices the pin's `LZ4_compress_default` makes, so a page comes out
//! byte for byte as the pin writes it: one hash table of the positions of four or five byte
//! sequences, a match taken as soon as one is found, and a search that steps further the longer it
//! goes without one.

use rudb_common::{Error, Result};

/// The shortest match a block can hold.
const MIN_MATCH: usize = 4;
/// How close to the end of a block the last match may start.
const MATCH_LIMIT: usize = 12;
/// How many bytes at the end of a block are always literals.
const LAST_LITERALS: usize = 5;
/// The furthest back a match may point.
const MAX_DISTANCE: usize = 65_535;
/// Below this many bytes a block's positions fit the smaller table, and above it the larger.
const SMALL_INPUT: usize = 64 * 1024 + MATCH_LIMIT - 1;
/// How many failed probes it takes for the search to start skipping ahead.
const SKIP_TRIGGER: u32 = 6;
/// The bits of a hash of the larger table. The smaller one has one more.
const HASH_LOG: u32 = 12;

/// Compresses `input` into one block.
#[must_use]
pub fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 255 + 16);
    let small = input.len() < SMALL_INPUT;
    let hash_log = if small { HASH_LOG + 1 } else { HASH_LOG };
    let mut table = vec![0_u32; 1 << hash_log];
    let hash = |at: usize| -> usize {
        if small {
            (read32(input, at).wrapping_mul(2_654_435_761) >> (32 - hash_log)) as usize
        } else {
            let sequence = u64::from_le_bytes(input[at..at + 8].try_into().unwrap_or_default());
            ((sequence << 24).wrapping_mul(889_523_592_379) >> (64 - hash_log)) as usize
        }
    };
    let mut anchor = 0;
    if input.len() > MATCH_LIMIT {
        let last_start = input.len() - MATCH_LIMIT + 1;
        let match_end = input.len() - LAST_LITERALS;
        let mut at = 0;
        table[hash(at)] = 0;
        at += 1;
        let mut next_hash = hash(at);
        'sequences: loop {
            // Look for a match, stepping further each time once the search has gone a while.
            let mut from;
            let mut ahead = at;
            let mut step = 1;
            let mut probes = 1_usize << SKIP_TRIGGER;
            loop {
                let h = next_hash;
                let candidate = table[h] as usize;
                at = ahead;
                ahead += step;
                step = probes >> SKIP_TRIGGER;
                probes += 1;
                if ahead > last_start {
                    break 'sequences;
                }
                from = candidate;
                next_hash = hash(ahead);
                table[h] = at as u32;
                if !small && candidate + MAX_DISTANCE < at {
                    continue;
                }
                if read32(input, from) == read32(input, at) {
                    break;
                }
            }
            // Take in the bytes before the match that match as well.
            while at > anchor && from > 0 && input[at - 1] == input[from - 1] {
                at -= 1;
                from -= 1;
            }
            let mut token = out.len();
            out.push(0);
            literals(&input[anchor..at], token, &mut out);
            loop {
                out.extend_from_slice(&((at - from) as u16).to_le_bytes());
                let mut length = MIN_MATCH;
                while at + length < match_end && input[at + length] == input[from + length] {
                    length += 1;
                }
                at += length;
                let extra = length - MIN_MATCH;
                if extra >= 15 {
                    out[token] += 15;
                    lengthen(extra - 15, &mut out);
                } else {
                    out[token] += extra as u8;
                }
                anchor = at;
                if at >= last_start {
                    break 'sequences;
                }
                table[hash(at - 2)] = (at - 2) as u32;
                // A match that starts where this one ends follows it with no literals between.
                let h = hash(at);
                let candidate = table[h] as usize;
                table[h] = at as u32;
                if (small || candidate + MAX_DISTANCE >= at)
                    && read32(input, candidate) == read32(input, at)
                {
                    from = candidate;
                    token = out.len();
                    out.push(0);
                    continue;
                }
                break;
            }
            at += 1;
            next_hash = hash(at);
        }
    }
    let token = out.len();
    out.push(0);
    literals(&input[anchor..], token, &mut out);
    out
}

/// Appends the literals of a sequence after its token at `token`, with their length in it.
fn literals(bytes: &[u8], token: usize, out: &mut Vec<u8>) {
    if bytes.len() >= 15 {
        out[token] = 15 << 4;
        lengthen(bytes.len() - 15, out);
    } else {
        out[token] = (bytes.len() as u8) << 4;
    }
    out.extend_from_slice(bytes);
}

/// Appends what is left of a length past the fifteen its token holds.
fn lengthen(mut left: usize, out: &mut Vec<u8>) {
    while left >= 255 {
        out.push(255);
        left -= 255;
    }
    out.push(left as u8);
}

fn read32(input: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(input[at..at + 4].try_into().unwrap_or_default())
}

/// Decompresses one block, which must come to exactly `expected` bytes.
///
/// # Errors
///
/// If the block runs out part way through a sequence, points a match back past its start or
/// comes to a length other than `expected`.
pub fn decompress(input: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected);
    decompress_onto(input, expected, &mut out)?;
    Ok(out)
}

/// Decompresses one block onto the end of `out`, which grows by exactly `expected` bytes.
///
/// # Errors
///
/// As for [`decompress`].
pub fn decompress_onto(input: &[u8], expected: usize, out: &mut Vec<u8>) -> Result<()> {
    let start = out.len();
    let limit = start + expected;
    let mut at = 0;
    loop {
        let token = *input.get(at).ok_or_else(broken)?;
        at += 1;
        let mut length = usize::from(token >> 4);
        if length == 15 {
            length += longer(input, &mut at)?;
        }
        let literals = input.get(at..at + length).ok_or_else(broken)?;
        if out.len() + length > limit {
            return Err(broken());
        }
        out.extend_from_slice(literals);
        at += length;
        if at == input.len() {
            break;
        }
        let distance = usize::from(u16::from_le_bytes(
            input.get(at..at + 2).ok_or_else(broken)?.try_into().unwrap_or_default(),
        ));
        at += 2;
        if distance == 0 || distance > out.len() - start {
            return Err(broken());
        }
        let mut length = usize::from(token & 15);
        if length == 15 {
            length += longer(input, &mut at)?;
        }
        length += MIN_MATCH;
        if out.len() + length > limit {
            return Err(broken());
        }
        let from = out.len() - distance;
        if distance >= length {
            out.extend_from_within(from..from + length);
        } else {
            for offset in 0..length {
                out.push(out[from + offset]);
            }
        }
    }
    if out.len() != limit {
        return Err(Error::io(format!(
            "the metadata says this LZ4_RAW block is {expected} bytes and it decompressed to {}",
            out.len() - start
        )));
    }
    Ok(())
}

/// Reads the bytes of a length past the fifteen its token holds.
fn longer(input: &[u8], at: &mut usize) -> Result<usize> {
    let mut length = 0;
    loop {
        let byte = *input.get(*at).ok_or_else(broken)?;
        *at += 1;
        length += usize::from(byte);
        if byte != 255 {
            return Ok(length);
        }
    }
}

fn broken() -> Error {
    Error::io("LZ4 decompression failure")
}

#[cfg(test)]
mod tests {
    use super::{compress, decompress};

    /// Bytes that repeat at a few distances, with some that do not.
    fn sample(len: usize) -> Vec<u8> {
        let mut state = 7_u32;
        (0..len)
            .map(|at| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                if (state >> 16).is_multiple_of(5) {
                    (state >> 8) as u8
                } else {
                    (at % 251 / 3) as u8
                }
            })
            .collect()
    }

    #[test]
    fn what_is_compressed_comes_back() {
        for len in [0, 1, 12, 13, 64, 1000, 65_546, 65_547, 300_000] {
            let input = sample(len);
            let block = compress(&input);
            assert_eq!(decompress(&block, input.len()).expect("decompresses"), input, "{len}");
        }
        let zeroes = vec![0_u8; 100_000];
        let block = compress(&zeroes);
        assert!(block.len() < 500, "{}", block.len());
        assert_eq!(decompress(&block, zeroes.len()).expect("decompresses"), zeroes);
    }

    #[test]
    fn a_short_block_is_its_literals() {
        assert_eq!(compress(b""), vec![0]);
        assert_eq!(compress(b"hello"), b"\x50hello".to_vec());
        let twelve = b"abababababab";
        assert_eq!(compress(twelve)[0], 12 << 4);
    }

    #[test]
    fn a_broken_block_is_refused() {
        let block = compress(&sample(1000));
        assert!(decompress(&block, 999).is_err());
        assert!(decompress(&block, 1001).is_err());
        assert!(decompress(&block[..block.len() - 1], 1000).is_err());
        // A match that points back before the block starts.
        assert!(decompress(&[0x00, 0x05, 0x00], 4).is_err());
    }
}

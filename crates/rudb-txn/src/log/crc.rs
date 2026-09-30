//! CRC-32C, the checksum every segment header and every record carries.
//!
//! The Castagnoli polynomial, reflected. On x86 with SSE 4.2 and on arm with the CRC extension it
//! is the processor's own instruction, eight bytes at a time. Anywhere else it is eight tables
//! built at compile time, at about a byte a cycle. Replay checks every byte of the log, and on a
//! 1 GiB log the tables were a larger share of the open than reading the disk.

/// The reflected Castagnoli polynomial.
const POLY: u32 = 0x82F6_3B78;

/// Table `k` advances a byte that is `k` bytes further from the end of an eight byte step.
static TABLES: [[u32; 256]; 8] = tables();

const fn tables() -> [[u32; 256]; 8] {
    let mut tables = [[0_u32; 256]; 8];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 0 { crc >> 1 } else { (crc >> 1) ^ POLY };
            bit += 1;
        }
        tables[0][byte] = crc;
        byte += 1;
    }
    let mut byte = 0;
    while byte < 256 {
        let mut table = 1;
        while table < 8 {
            let previous = tables[table - 1][byte];
            tables[table][byte] = (previous >> 8) ^ tables[0][(previous & 0xFF) as usize];
            table += 1;
        }
        byte += 1;
    }
    tables
}

/// The CRC-32C of `bytes` continued from `crc`, which is what an earlier call returned or a seed.
///
/// Continuing is the same as checksumming the two pieces joined, so a record's header and payload
/// are checksummed as one without being copied together first.
#[must_use]
pub(crate) fn extend(crc: u32, bytes: &[u8]) -> u32 {
    // The instruction does eight bytes in the time the tables do one, and replay checks every
    // byte of the log.
    rudb_io::crc32c_extend(crc, bytes).unwrap_or_else(|| software(crc, bytes))
}

/// [`extend`] eight bytes at a time through the tables, for a machine without the instruction.
fn software(crc: u32, bytes: &[u8]) -> u32 {
    let mut crc = !crc;
    let mut steps = bytes.chunks_exact(8);
    for step in &mut steps {
        let low = u32::from_le_bytes([step[0], step[1], step[2], step[3]]) ^ crc;
        let high = u32::from_le_bytes([step[4], step[5], step[6], step[7]]);
        crc = TABLES[7][(low & 0xFF) as usize]
            ^ TABLES[6][((low >> 8) & 0xFF) as usize]
            ^ TABLES[5][((low >> 16) & 0xFF) as usize]
            ^ TABLES[4][(low >> 24) as usize]
            ^ TABLES[3][(high & 0xFF) as usize]
            ^ TABLES[2][((high >> 8) & 0xFF) as usize]
            ^ TABLES[1][((high >> 16) & 0xFF) as usize]
            ^ TABLES[0][(high >> 24) as usize];
    }
    for &byte in steps.remainder() {
        crc = TABLES[0][((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// The CRC-32C of `bytes`.
#[must_use]
pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    extend(0, bytes)
}

#[cfg(test)]
mod tests {
    use super::{crc32c, extend};

    #[test]
    fn the_check_value_is_the_one_every_implementation_agrees_on() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
        // RFC 3720 appendix B.4: thirty two zero bytes and thirty two 0xFF bytes.
        assert_eq!(crc32c(&[0; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFF; 32]), 0x62A8_AB43);
    }

    #[test]
    fn the_instruction_and_the_tables_agree() {
        let bytes: Vec<u8> = (0..1000_u32).map(|n| (n * 131 + 17) as u8).collect();
        for len in [0, 1, 7, 8, 9, 15, 16, 17, 63, 64, 65, 999, 1000] {
            for seed in [0, 1, 0xDEAD_BEEF] {
                assert_eq!(extend(seed, &bytes[..len]), software(seed, &bytes[..len]), "{len}");
            }
        }
    }

    #[test]
    fn continuing_is_the_same_as_checksumming_the_pieces_joined() {
        let bytes: Vec<u8> = (0..300_u32).map(|n| (n * 7 + 3) as u8).collect();
        for cut in [0, 1, 7, 8, 9, 150, 299, 300] {
            let (left, right) = bytes.split_at(cut);
            assert_eq!(extend(extend(0, left), right), crc32c(&bytes), "cut at {cut}");
        }
    }
}

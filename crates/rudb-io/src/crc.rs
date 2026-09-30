//! CRC-32C through the processor's own instruction, for the log's checksums.
//!
//! Here rather than in `rudb-txn` because the intrinsics need `unsafe` and that crate forbids it.
//! The table version for a machine without the instruction stays in `rudb-txn`, which is why this
//! answers `None` rather than computing the checksum some other way.

/// `crc` continued over `bytes`, in the same convention as a checksum started at zero, or `None`
/// when this machine has no CRC-32C instruction.
#[must_use]
#[allow(unsafe_code, reason = "the CRC-32C instructions are only there once the check finds them")]
pub fn crc32c_extend(crc: u32, bytes: &[u8]) -> Option<u32> {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.2") {
        // SAFETY: `hardware` needs SSE 4.2 and nothing else, and the check above found it.
        return Some(unsafe { hardware(crc, bytes) });
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("crc") {
        // SAFETY: `hardware` needs the CRC extension and nothing else, and the check above found it.
        return Some(unsafe { hardware(crc, bytes) });
    }
    let _ = (crc, bytes);
    None
}

/// The SSE 4.2 `crc32` instruction, which is CRC-32C, eight bytes at a time.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
fn hardware(crc: u32, bytes: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let mut wide = u64::from(!crc);
    let mut steps = bytes.chunks_exact(8);
    for step in &mut steps {
        wide = _mm_crc32_u64(wide, u64::from_le_bytes(step.try_into().expect("eight bytes")));
    }
    // The instruction's 64-bit form keeps the checksum in the low half and zeroes the rest.
    let mut crc = wide as u32;
    for &byte in steps.remainder() {
        crc = _mm_crc32_u8(crc, byte);
    }
    !crc
}

/// The ARMv8 CRC-32C instructions, eight bytes at a time.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
fn hardware(crc: u32, bytes: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32cb, __crc32cd};
    let mut crc = !crc;
    let mut steps = bytes.chunks_exact(8);
    for step in &mut steps {
        crc = __crc32cd(crc, u64::from_le_bytes(step.try_into().expect("eight bytes")));
    }
    for &byte in steps.remainder() {
        crc = __crc32cb(crc, byte);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instruction_gives_the_known_answers() {
        let Some(zero) = crc32c_extend(0, b"") else { return };
        assert_eq!(zero, 0);
        // RFC 3720 appendix B.4: thirty two zero bytes, thirty two 0xFF bytes, and 0 to 31.
        assert_eq!(crc32c_extend(0, &[0; 32]), Some(0x8A91_36AA));
        assert_eq!(crc32c_extend(0, &[0xFF; 32]), Some(0x62A8_AB43));
        let ascending: Vec<u8> = (0..32).collect();
        assert_eq!(crc32c_extend(0, &ascending), Some(0x46DD_794E));
        let (left, right) = ascending.split_at(13);
        let first = crc32c_extend(0, left).expect("the same machine");
        assert_eq!(crc32c_extend(first, right), Some(0x46DD_794E));
    }
}

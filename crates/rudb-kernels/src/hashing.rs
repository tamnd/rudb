//! MD5, SHA-1 and SHA-256, which `md5`, `md5_number`, `sha1` and `sha256` answer with.
//!
//! These are the digests of RFC 1321 and FIPS 180-4 and nothing more, written here so the kernels
//! need no hashing crate. None of them is meant to protect anything, so they are written for being
//! read rather than for speed.

use rudb_common::Value;

/// `md5`, `md5_number`, `sha1` or `sha256` of the bytes of a string or a blob.
///
/// The digests are written in lower case hexadecimal, and `md5_number` reads the MD5 digest as a
/// UHUGEINT little endian, which is how the pin reads it.
pub(crate) fn hashed(name: &str, bytes: &[u8]) -> Value {
    match name {
        "md5" => Value::Varchar(lower_hex(&md5(bytes))),
        "md5_number" => Value::UHugeInt(u128::from_le_bytes(md5(bytes))),
        "sha1" => Value::Varchar(lower_hex(&sha1(bytes))),
        _ => Value::Varchar(lower_hex(&sha256(bytes))),
    }
}

/// Pads a message the way all three digests do, to a whole number of 64 byte blocks with the
/// message's length in bits in the last eight bytes, little endian for MD5 and big endian for the
/// other two.
fn padded(message: &[u8], little_endian: bool) -> Vec<u8> {
    let bits = (message.len() as u64).wrapping_mul(8);
    let mut out = message.to_vec();
    out.push(0x80);
    while out.len() % 64 != 56 {
        out.push(0);
    }
    out.extend_from_slice(&if little_endian { bits.to_le_bytes() } else { bits.to_be_bytes() });
    out
}

/// The amounts each of MD5's 64 steps rotates by.
const MD5_SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// The MD5 digest of `message`.
pub(crate) fn md5(message: &[u8]) -> [u8; 16] {
    // The constants are the integer part of 2^32 times the sine of 1 to 64, which is how RFC 1321
    // defines them. The sine of a small integer is exact enough in a double for every one of them.
    let constants: [u32; 64] =
        std::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32);
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in padded(message, true).chunks_exact(64) {
        let words: [u32; 16] = std::array::from_fn(|i| {
            u32::from_le_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]])
        });
        let [mut a, mut b, mut c, mut d] = state;
        for step in 0..64 {
            let (mixed, word) = match step / 16 {
                0 => ((b & c) | (!b & d), step),
                1 => ((d & b) | (!d & c), (5 * step + 1) % 16),
                2 => (b ^ c ^ d, (3 * step + 5) % 16),
                _ => (c ^ (b | !d), (7 * step) % 16),
            };
            let rotated = a
                .wrapping_add(mixed)
                .wrapping_add(constants[step])
                .wrapping_add(words[word])
                .rotate_left(MD5_SHIFTS[step]);
            (a, b, c, d) = (d, b.wrapping_add(rotated), b, c);
        }
        for (held, moved) in state.iter_mut().zip([a, b, c, d]) {
            *held = held.wrapping_add(moved);
        }
    }
    let mut out = [0; 16];
    for (at, word) in state.iter().enumerate() {
        out[4 * at..4 * at + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// The SHA-1 digest of `message`.
pub(crate) fn sha1(message: &[u8]) -> [u8; 20] {
    let mut state: [u32; 5] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0];
    for block in padded(message, false).chunks_exact(64) {
        let mut words = [0u32; 80];
        for i in 0..16 {
            words[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            words[i] = (words[i - 3] ^ words[i - 8] ^ words[i - 14] ^ words[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (i, word) in words.iter().enumerate() {
            let (mixed, constant) = match i / 20 {
                0 => ((b & c) | (!b & d), 0x5a82_7999),
                1 => (b ^ c ^ d, 0x6ed9_eba1),
                2 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(mixed)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            (a, b, c, d, e) = (next, a, b.rotate_left(30), c, d);
        }
        for (held, moved) in state.iter_mut().zip([a, b, c, d, e]) {
            *held = held.wrapping_add(moved);
        }
    }
    let mut out = [0; 20];
    for (at, word) in state.iter().enumerate() {
        out[4 * at..4 * at + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// The first 32 bits of the fractional parts of the cube roots of the first 64 primes.
const SHA256_CONSTANTS: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The SHA-256 digest of `message`.
pub(crate) fn sha256(message: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    for block in padded(message, false).chunks_exact(64) {
        let mut words = [0u32; 64];
        for i in 0..16 {
            words[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let low = words[i - 15].rotate_right(7)
                ^ words[i - 15].rotate_right(18)
                ^ (words[i - 15] >> 3);
            let high = words[i - 2].rotate_right(17)
                ^ words[i - 2].rotate_right(19)
                ^ (words[i - 2] >> 10);
            words[i] =
                words[i - 16].wrapping_add(low).wrapping_add(words[i - 7]).wrapping_add(high);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for (constant, word) in SHA256_CONSTANTS.iter().zip(words) {
            let chosen = (e & f) ^ (!e & g);
            let first = h
                .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                .wrapping_add(chosen)
                .wrapping_add(*constant)
                .wrapping_add(word);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let second = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add(majority);
            (h, g, f, e, d, c, b, a) =
                (g, f, e, d.wrapping_add(first), c, b, a, first.wrapping_add(second));
        }
        for (held, moved) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *held = held.wrapping_add(moved);
        }
    }
    let mut out = [0; 32];
    for (at, word) in state.iter().enumerate() {
        out[4 * at..4 * at + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// A digest in lower case hexadecimal, which is how the pin writes all three.
pub(crate) fn lower_hex(digest: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digests_of_the_standards_examples_are_the_standards_answers() {
        assert_eq!(lower_hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(lower_hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(lower_hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(lower_hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            lower_hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            lower_hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_message_that_crosses_a_block_is_padded_into_the_next_one() {
        let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(lower_hex(&md5(long)), "8215ef0796a20bcaaae116d3876c664a");
        assert_eq!(lower_hex(&sha1(long)), "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
        assert_eq!(
            lower_hex(&sha256(long)),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }
}

//! XXH64, which is the hash a zstd frame puts at its end.
//!
//! Here because a frame that says what its content hashes to and is not checked against it is a
//! frame that can be silently wrong, and the whole argument for decompressing a page at all is that
//! the bytes that come out are the bytes that went in. It is not a cryptographic hash and it does
//! not need to be: what it catches is a truncated read, a torn page and a decoder that got a
//! sequence wrong, and all three of those are accidents.
//!
//! This is the seeded sixty four bit variant with a seed of zero, which is what zstd writes, and
//! only the low thirty two bits go in the frame.

/// The five constants XXH64 is built out of.
const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

/// XXH64 of `data` with a seed of zero.
pub(crate) fn xxh64(data: &[u8]) -> u64 {
    let mut hash;
    let mut at = 0;
    if data.len() >= 32 {
        let mut lanes = [P1.wrapping_add(P2), P2, 0, 0u64.wrapping_sub(P1)];
        while at + 32 <= data.len() {
            for (which, lane) in lanes.iter_mut().enumerate() {
                *lane = round(*lane, eight(data, at + which * 8));
            }
            at += 32;
        }
        hash = lanes[0]
            .rotate_left(1)
            .wrapping_add(lanes[1].rotate_left(7))
            .wrapping_add(lanes[2].rotate_left(12))
            .wrapping_add(lanes[3].rotate_left(18));
        for lane in lanes {
            hash = (hash ^ round(0, lane)).wrapping_mul(P1).wrapping_add(P4);
        }
    } else {
        hash = P5;
    }
    hash = hash.wrapping_add(data.len() as u64);
    while at + 8 <= data.len() {
        hash = (hash ^ round(0, eight(data, at))).rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        at += 8;
    }
    if at + 4 <= data.len() {
        let four =
            u64::from(u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]));
        hash = (hash ^ four.wrapping_mul(P1)).rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        at += 4;
    }
    while at < data.len() {
        hash = (hash ^ u64::from(data[at]).wrapping_mul(P5)).rotate_left(11).wrapping_mul(P1);
        at += 1;
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(P3);
    hash ^ (hash >> 32)
}

/// One lane's step.
fn round(lane: u64, input: u64) -> u64 {
    lane.wrapping_add(input.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
}

/// Eight bytes little endian from an offset that is known to have them.
fn eight(data: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(word)
}

#[cfg(test)]
mod tests {
    use super::xxh64;

    /// The one published vector, which is the only thing here that says this is XXH64 and not a
    /// hash of my own that happens to be self consistent. The rest of the assurance comes from
    /// `tests/real_frames.rs`, where every frame a real zstd wrote carries its own answer.
    #[test]
    fn the_empty_input_hashes_the_way_the_specification_says() {
        assert_eq!(xxh64(b""), 0xEF46_DB37_51D8_E999);
    }

    #[test]
    fn every_length_takes_a_different_path_and_none_of_them_agree_by_accident() {
        // One input per branch: under thirty two bytes, the four lane loop, and each of the three
        // tails. A hash where two of these collide is one where a loop bound is off by one.
        let data: Vec<u8> = (0..=200u8).collect();
        let mut seen = std::collections::HashSet::new();
        for length in [0, 1, 4, 7, 8, 31, 32, 33, 39, 40, 100, 201] {
            assert!(seen.insert(xxh64(&data[..length])), "two lengths hash the same at {length}");
        }
    }
}

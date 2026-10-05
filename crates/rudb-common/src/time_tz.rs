//! The one number a `TIME WITH TIME ZONE` is held in, which is the pin's sort key for it.
//!
//! The pin packs a zoned time into 64 bits: the time of day in microseconds above 24 bits that hold
//! the offset in seconds, biased so that a larger offset is a smaller number. It compares two of
//! them by a sort key, which is those bits with the biased offset added to the time once more, so
//! that the high bits are the time at UTC plus a constant and the low bits break a tie between two
//! readings of the same instant. The key and the bits are one to one, since the low 24 bits are the
//! same in both. Holding the key rather than the bits means every place that orders, groups or
//! hashes a zoned time by its number gets the pin's order without knowing what the number means:
//! `13:00:00+01` sorts before `12:00:00+00`, which sorts before `11:00:00-01`, and none of them
//! equal.

/// The largest offset the pin takes, in seconds, which is `15:59:59` either side of UTC.
pub const MAX_OFFSET: i32 = 16 * 60 * 60 - 1;

/// The number of low bits that hold the biased offset.
const OFFSET_BITS: u32 = 24;

/// The low bits a key or the pin's bits keep the biased offset in.
const OFFSET_MASK: i64 = (1 << OFFSET_BITS) - 1;

/// Microseconds in a second, which is what the offset is scaled by before it is added to the time.
const MICROS_PER_SECOND: i64 = 1_000_000;

/// The microseconds in a day, the most a time of day can be, since `24:00:00` is a reading.
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

/// The key for a time of day in microseconds read at an offset in seconds east of UTC.
///
/// The time is taken as it is, so it should be in `0..=MICROS_PER_DAY`, and the offset should be
/// within [`MAX_OFFSET`] either side.
#[must_use]
pub const fn pack(micros: i64, offset: i32) -> i64 {
    let biased = (MAX_OFFSET - offset) as i64;
    ((micros + biased * MICROS_PER_SECOND) << OFFSET_BITS) | biased
}

/// The time of day a key was read at, in microseconds since midnight in its own offset.
#[must_use]
pub const fn micros(key: i64) -> i64 {
    (key >> OFFSET_BITS) - (key & OFFSET_MASK) * MICROS_PER_SECOND
}

/// The offset a key was read at, in seconds east of UTC.
#[must_use]
pub const fn offset(key: i64) -> i32 {
    let biased = (key & OFFSET_MASK) as i32;
    MAX_OFFSET - biased
}

/// The pin's own bits for a key, which is what it hashes and what its files hold.
#[must_use]
pub const fn bits(key: i64) -> i64 {
    (micros(key) << OFFSET_BITS) | (key & OFFSET_MASK)
}

/// The key for the pin's own bits.
#[must_use]
pub const fn from_bits(bits: i64) -> i64 {
    let biased = bits & OFFSET_MASK;
    (((bits >> OFFSET_BITS) + biased * MICROS_PER_SECOND) << OFFSET_BITS) | biased
}

/// The same instant read at UTC, wrapped into one day, which is what the pin calls normalizing.
#[must_use]
pub const fn at_utc(key: i64) -> i64 {
    (micros(key) - offset(key) as i64 * MICROS_PER_SECOND).rem_euclid(MICROS_PER_DAY)
}

/// Whether an offset in seconds is one the pin can hold.
#[must_use]
pub const fn holds(offset: i32) -> bool {
    -MAX_OFFSET <= offset && offset <= MAX_OFFSET
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600 * MICROS_PER_SECOND;

    #[test]
    fn a_key_gives_back_its_time_and_its_offset() {
        for shift in [-MAX_OFFSET, -19_800, 0, 3_600, MAX_OFFSET] {
            for time in [0, 12 * HOUR + 1, MICROS_PER_DAY] {
                let key = pack(time, shift);
                assert_eq!((micros(key), offset(key)), (time, shift));
                assert_eq!(from_bits(bits(key)), key);
            }
        }
    }

    #[test]
    fn keys_order_by_the_instant_and_then_the_larger_offset_first() {
        let keys = [
            pack(11 * HOUR + HOUR / 2, 0),
            pack(13 * HOUR, 3_600),
            pack(12 * HOUR, 0),
            pack(11 * HOUR, -3_600),
        ];
        assert!(keys.is_sorted() && keys.windows(2).all(|pair| pair[0] != pair[1]));
        assert_eq!(at_utc(pack(12 * HOUR, 5 * 3_600)), 7 * HOUR);
        assert_eq!(at_utc(pack(HOUR, 5 * 3_600)), 20 * HOUR);
    }
}

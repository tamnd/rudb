//! Whether a run of bytes is text, answered in one pass.
//!
//! Every string that enters this engine from a file is bytes until something says it is text, and
//! the something is here. A Parquet byte array column, a CSV field and a JSON string all arrive as a
//! range of a buffer somebody else owns, and all three want the same answer about it before it can
//! be handed out as a `&str`.
//!
//! That is already in the standard library as `str::from_utf8`, so this module owes an explanation
//! for existing.
//!
//! # Why this is written and not called
//!
//! The standard library's validator is a function call. It is a good one, and the cost that matters
//! at a million strings a query is not inside it. A URL column of ClickBench is strings of about
//! seventy bytes, most of which hold a run of Cyrillic somewhere in the query string, and the string
//! column was answering the question twice for each of them: `is_ascii` walked the bytes to the
//! first one over 127 and gave up, and then `from_utf8` walked the same bytes again from the start
//! with its own alignment prologue in front. A callgrind profile of `SELECT URL, COUNT(*) FROM hits
//! GROUP BY URL` put `core::str::converts::from_utf8` at 8.49 percent of the whole query, which
//! works out at about two hundred instructions for seventy bytes.
//!
//! One pass costs what one pass costs. [`valid`] runs the fast case and the slow case in the same
//! loop, so a string that is ASCII up to its last ten bytes is scanned a word at a time up to there
//! and byte at a time after it, rather than scanned twice and only one of those times quickly. It is
//! small enough to inline into the caller, which is the other half of where those instructions were
//! going.
//!
//! # What it accepts
//!
//! The encoding as the Unicode standard defines it and as Rust defines it, which are the same set:
//! no overlong form, no surrogate, nothing above U+10FFFF. Those three are the cases that make a
//! permissive validator a security bug rather than a slow one, because two decoders that disagree
//! about which bytes are a character disagree about what a string says. A caller that holds bytes
//! this accepts can hand them to `str::from_utf8_unchecked` and be right, and the tests in this
//! module are written against exactly that claim.
//!
//! # What could replace it
//!
//! The vectorised validators, which check sixteen or thirty two bytes of a multi byte sequence at
//! once rather than one. Lemire and Keiser's is the known one and it is about an order of magnitude
//! faster on text that is not ASCII. It wants SIMD intrinsics, which this workspace does not reach
//! for yet, and it wants a fallback for the tail either way. The shape here is what that would slot
//! into: one function, one answer, no state.

/// The high bit of each of eight bytes at once.
const HIGH: u64 = 0x8080_8080_8080_8080;

/// How many bytes wide the ASCII step is.
const STEP: usize = size_of::<u64>();

/// Whether `bytes` is valid UTF-8.
///
/// The same answer `str::from_utf8(bytes).is_ok()` gives, for every input. That equivalence is what
/// the caller is relying on when it goes on to treat the bytes as text, so it is a property test in
/// this module rather than a claim in this sentence.
#[must_use]
pub fn valid(bytes: &[u8]) -> bool {
    let len = bytes.len();
    let mut at = 0;
    while at < len {
        if bytes[at] < 0x80 {
            // A word at a time while the bytes are ASCII, which is most of them in most columns,
            // and then byte at a time to find where the run actually ended. The word test is one
            // and and one compare for eight bytes, so the byte loop after it only ever runs over
            // the seven bytes at the end of a word and the tail of the slice.
            // Taken eight at a time through a pattern rather than through `try_into`, which would
            // be a conversion that cannot fail and an `expect` to say so.
            while let Some(&[a, b, c, d, e, f, g, h]) = bytes.get(at..at + STEP) {
                if u64::from_le_bytes([a, b, c, d, e, f, g, h]) & HIGH != 0 {
                    break;
                }
                at += STEP;
            }
            while at < len && bytes[at] < 0x80 {
                at += 1;
            }
            continue;
        }
        let first = bytes[at];
        // How many bytes follow the first one. The ranges that are missing are the ones no first
        // byte is allowed to be: 0x80 to 0xBF is a continuation byte with nothing in front of it,
        // 0xC0 and 0xC1 only ever spell a character that has a shorter spelling, and 0xF5 and up
        // would spell one above U+10FFFF, which is not a character.
        let follows = match first {
            0xC2..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF4 => 3,
            _ => return false,
        };
        if at + follows >= len {
            return false;
        }
        // The second byte is the one that carries the rest of the rules, because the range it is
        // allowed to take depends on the first. 0xE0 with 0x80 is an overlong three byte form of
        // something that fits in two, 0xED with 0xA0 is half of a surrogate pair, 0xF0 with 0x80 is
        // an overlong four byte form and 0xF4 with 0x90 is over U+10FFFF. Everything the standard
        // rules out is ruled out right here.
        let second = bytes[at + 1];
        let allowed = match first {
            0xC2..=0xDF => 0x80..=0xBF,
            0xE0 => 0xA0..=0xBF,
            0xED => 0x80..=0x9F,
            0xE1..=0xEF => 0x80..=0xBF,
            0xF0 => 0x90..=0xBF,
            0xF4 => 0x80..=0x8F,
            _ => 0x80..=0xBF,
        };
        if !allowed.contains(&second) {
            return false;
        }
        // The third and the fourth have no rule beyond being continuation bytes, since whatever
        // they could have said about range the second one has already said.
        if follows >= 2 && bytes[at + 2] & 0xC0 != 0x80 {
            return false;
        }
        if follows >= 3 && bytes[at + 3] & 0xC0 != 0x80 {
            return false;
        }
        at += follows + 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::valid;

    /// Both answers, so a case that is wrong is wrong against the standard library rather than
    /// against what this test happened to expect.
    fn agree(bytes: &[u8]) -> bool {
        let ours = valid(bytes);
        assert_eq!(ours, std::str::from_utf8(bytes).is_ok(), "disagreed about {bytes:?}");
        ours
    }

    #[test]
    fn nothing_is_text() {
        assert!(agree(b""));
    }

    #[test]
    fn ascii_of_every_length_around_the_word_is_text() {
        let line = b"abcdefghijklmnopqrstuvwxyz0123456789";
        // row at a time: the point is every length from nothing to past two words, not a sample.
        for len in 0..line.len() {
            assert!(agree(&line[..len]), "{len} bytes of ASCII");
        }
    }

    #[test]
    fn a_control_byte_and_a_nul_are_still_ascii() {
        assert!(agree(b"a\0b\x7f\x01"));
    }

    #[test]
    fn the_sequence_lengths_are_all_text() {
        assert!(agree("é".as_bytes()));
        assert!(agree("Яндекс".as_bytes()));
        assert!(agree("日本語".as_bytes()));
        assert!(agree("😀".as_bytes()));
        assert!(agree("\u{10ffff}".as_bytes()));
    }

    #[test]
    fn a_character_that_straddles_the_end_of_a_word_is_still_read_whole() {
        // The word loop is what puts a sequence at an offset the byte path then has to pick up, so
        // every offset a four byte character can sit at gets one.
        // row at a time: each offset is a different path through the loop, so each one is a case.
        for pad in 0..16 {
            let mut bytes = vec![b'a'; pad];
            bytes.extend_from_slice("😀".as_bytes());
            bytes.extend_from_slice(b"tail");
            assert!(agree(&bytes), "padded by {pad}");
        }
    }

    #[test]
    fn a_continuation_byte_on_its_own_is_not_text() {
        assert!(!agree(&[0x80]));
        assert!(!agree(&[0xbf]));
        assert!(!agree(b"good\x80bytes"));
    }

    #[test]
    fn a_sequence_cut_short_by_the_end_is_not_text() {
        assert!(!agree(&[0xc3]));
        assert!(!agree(&[0xe6, 0x97]));
        assert!(!agree(&[0xf0, 0x9f, 0x98]));
    }

    #[test]
    fn a_sequence_cut_short_by_the_next_one_is_not_text() {
        assert!(!agree(&[0xc3, 0x28]));
        assert!(!agree(&[0xe6, 0x97, 0x28]));
        assert!(!agree(&[0xf0, 0x9f, 0x98, 0x28]));
    }

    #[test]
    fn an_overlong_spelling_is_not_text() {
        // Two bytes for a character that fits in one, three for one that fits in two, four for one
        // that fits in three. All three decode to something if you let them, which is the problem.
        assert!(!agree(&[0xc0, 0xaf]));
        assert!(!agree(&[0xc1, 0xbf]));
        assert!(!agree(&[0xe0, 0x80, 0xaf]));
        assert!(!agree(&[0xe0, 0x9f, 0xbf]));
        assert!(!agree(&[0xf0, 0x80, 0x80, 0xaf]));
        assert!(!agree(&[0xf0, 0x8f, 0xbf, 0xbf]));
    }

    #[test]
    fn half_of_a_surrogate_pair_is_not_text() {
        assert!(!agree(&[0xed, 0xa0, 0x80]));
        assert!(!agree(&[0xed, 0xbf, 0xbf]));
        assert!(agree(&[0xed, 0x9f, 0xbf]));
        assert!(agree(&[0xee, 0x80, 0x80]));
    }

    #[test]
    fn a_number_above_the_last_character_is_not_text() {
        assert!(!agree(&[0xf4, 0x90, 0x80, 0x80]));
        assert!(!agree(&[0xf5, 0x80, 0x80, 0x80]));
        assert!(!agree(&[0xfe]));
        assert!(!agree(&[0xff]));
    }

    #[test]
    fn every_byte_on_its_own_answers_what_the_standard_library_answers() {
        // row at a time: two hundred and fifty six is small enough to just do all of them.
        for byte in 0..=u8::MAX {
            agree(&[byte]);
            agree(&[b'a', byte]);
            agree(&[byte, 0x80]);
        }
    }

    #[test]
    fn a_run_of_every_two_byte_pair_answers_what_the_standard_library_answers() {
        // The exhaustive one. Every pair of bytes covers every first byte against every second,
        // which is where all of the range rules live, and it is sixty five thousand cases.
        // row at a time: an exhaustive check is the point, so there is nothing to batch.
        for first in 0..=u8::MAX {
            for second in 0..=u8::MAX {
                agree(&[first, second]);
            }
        }
    }

    /// One byte out of each range a continuation byte is ever tested against, plus the two either
    /// side of the range as a whole.
    const EDGES: [u8; 10] = [0x00, 0x7f, 0x80, 0x8f, 0x90, 0x9f, 0xa0, 0xbf, 0xc0, 0xff];

    #[test]
    fn a_three_byte_sequence_answers_what_the_standard_library_answers() {
        // Every byte that starts a sequence against every second byte, which is the pair the range
        // rules are about, and then a third byte from each side of each boundary.
        // row at a time: the point is the cross product, so there is nothing to batch.
        for first in 0xC0..=0xFF {
            for second in 0..=u8::MAX {
                for third in EDGES {
                    agree(&[first, second, third]);
                }
            }
        }
    }

    #[test]
    fn a_four_byte_sequence_answers_what_the_standard_library_answers() {
        // The same for the only four bytes that start a four byte sequence, and for the one either
        // side of them that looks as though it should.
        // row at a time: the point is the cross product, so there is nothing to batch.
        for first in 0xEF..=0xF5 {
            for second in 0..=u8::MAX {
                for third in EDGES {
                    for fourth in EDGES {
                        agree(&[first, second, third, fourth]);
                    }
                }
            }
        }
    }

    #[test]
    fn a_sequence_after_a_word_of_ascii_answers_what_the_standard_library_answers() {
        // The same cases again at every offset the word loop can hand the automaton, because the
        // two loops meeting in the middle of a character is the shape that breaks first.
        // row at a time: the point is the cross product, so there is nothing to batch.
        for pad in 0..9 {
            let ascii = vec![b'a'; pad];
            for first in 0xC0..=0xFF {
                for second in EDGES {
                    for third in EDGES {
                        let mut bytes = ascii.clone();
                        bytes.extend_from_slice(&[first, second, third]);
                        bytes.extend_from_slice(b"tail");
                        agree(&bytes);
                    }
                }
            }
        }
    }
}

//! The static dictionary and its transforms, RFC 7932 sections 8 and appendices A and B.
//!
//! A copy that reaches further back than the output goes is a word from this dictionary instead.
//! The distance past the end picks the word among those of the copy's length and one of 121
//! transforms, each a prefix, a change to the word and a suffix, so " the " and "The" and "the,"
//! all come from one word.

/// The words, 122,784 bytes grouped by length, whose SHA-256 is the
/// `20e42eb1b511c21806d4d227d07e5dd06877d8ce7b3a817f378f313653f35c70` that the RFC gives.
static WORDS: &[u8; 122_784] = include_bytes!("dictionary.bin");

/// How many bits of a word's number pick the word among those of its length, by length.
const NDBITS: [u32; 25] =
    [0, 0, 0, 0, 10, 10, 11, 11, 10, 10, 10, 10, 10, 9, 9, 8, 7, 7, 8, 7, 7, 6, 6, 5, 5];

/// Where the words of each length start.
const OFFSETS: [usize; 25] = {
    let mut offsets = [0; 25];
    let mut length = 4;
    while length < 24 {
        offsets[length + 1] = offsets[length] + (length << NDBITS[length]);
        length += 1;
    }
    offsets
};

/// The shortest word.
pub(super) const SHORTEST: usize = 4;
/// The longest word.
pub(super) const LONGEST: usize = 24;

/// What each transform puts before the word, does to it and puts after it. The change is 0 for
/// none, 1 to 9 for dropping that many bytes off the end, 10 for an upper case first letter, 11
/// for all upper case and 12 to 20 for dropping 1 to 9 bytes off the start.
const TRANSFORMS: [(&[u8], u8, &[u8]); 121] = [
    (b"", 0, b""),
    (b"", 0, b" "),
    (b" ", 0, b" "),
    (b"", 12, b""),
    (b"", 10, b" "),
    (b"", 0, b" the "),
    (b" ", 0, b""),
    (b"s ", 0, b" "),
    (b"", 0, b" of "),
    (b"", 10, b""),
    (b"", 0, b" and "),
    (b"", 13, b""),
    (b"", 1, b""),
    (b", ", 0, b" "),
    (b"", 0, b", "),
    (b" ", 10, b" "),
    (b"", 0, b" in "),
    (b"", 0, b" to "),
    (b"e ", 0, b" "),
    (b"", 0, b"\""),
    (b"", 0, b"."),
    (b"", 0, b"\">"),
    (b"", 0, b"\n"),
    (b"", 3, b""),
    (b"", 0, b"]"),
    (b"", 0, b" for "),
    (b"", 14, b""),
    (b"", 2, b""),
    (b"", 0, b" a "),
    (b"", 0, b" that "),
    (b" ", 10, b""),
    (b"", 0, b". "),
    (b".", 0, b""),
    (b" ", 0, b", "),
    (b"", 15, b""),
    (b"", 0, b" with "),
    (b"", 0, b"'"),
    (b"", 0, b" from "),
    (b"", 0, b" by "),
    (b"", 16, b""),
    (b"", 17, b""),
    (b" the ", 0, b""),
    (b"", 4, b""),
    (b"", 0, b". The "),
    (b"", 11, b""),
    (b"", 0, b" on "),
    (b"", 0, b" as "),
    (b"", 0, b" is "),
    (b"", 7, b""),
    (b"", 1, b"ing "),
    (b"", 0, b"\n\t"),
    (b"", 0, b":"),
    (b" ", 0, b". "),
    (b"", 0, b"ed "),
    (b"", 20, b""),
    (b"", 18, b""),
    (b"", 6, b""),
    (b"", 0, b"("),
    (b"", 10, b", "),
    (b"", 8, b""),
    (b"", 0, b" at "),
    (b"", 0, b"ly "),
    (b" the ", 0, b" of "),
    (b"", 5, b""),
    (b"", 9, b""),
    (b" ", 10, b", "),
    (b"", 10, b"\""),
    (b".", 0, b"("),
    (b"", 11, b" "),
    (b"", 10, b"\">"),
    (b"", 0, b"=\""),
    (b" ", 0, b"."),
    (b".com/", 0, b""),
    (b" the ", 0, b" of the "),
    (b"", 10, b"'"),
    (b"", 0, b". This "),
    (b"", 0, b","),
    (b".", 0, b" "),
    (b"", 10, b"("),
    (b"", 10, b"."),
    (b"", 0, b" not "),
    (b" ", 0, b"=\""),
    (b"", 0, b"er "),
    (b" ", 11, b" "),
    (b"", 0, b"al "),
    (b" ", 11, b""),
    (b"", 0, b"='"),
    (b"", 11, b"\""),
    (b"", 10, b". "),
    (b" ", 0, b"("),
    (b"", 0, b"ful "),
    (b" ", 10, b". "),
    (b"", 0, b"ive "),
    (b"", 0, b"less "),
    (b"", 11, b"'"),
    (b"", 0, b"est "),
    (b" ", 10, b"."),
    (b"", 11, b"\">"),
    (b" ", 0, b"='"),
    (b"", 10, b","),
    (b"", 0, b"ize "),
    (b"", 11, b"."),
    (b"\xc2\xa0", 0, b""),
    (b" ", 0, b","),
    (b"", 10, b"=\""),
    (b"", 11, b"=\""),
    (b"", 0, b"ous "),
    (b"", 11, b", "),
    (b"", 10, b"='"),
    (b" ", 10, b","),
    (b" ", 11, b"=\""),
    (b" ", 11, b", "),
    (b"", 11, b","),
    (b"", 11, b"("),
    (b"", 11, b". "),
    (b" ", 11, b"."),
    (b"", 11, b"='"),
    (b" ", 11, b". "),
    (b" ", 10, b"=\""),
    (b" ", 11, b"='"),
    (b" ", 10, b"='"),
];

/// Appends word `id` among those of `length` bytes, transformed, or returns `None` when there is
/// no such word.
pub(super) fn word(length: usize, id: usize, out: &mut Vec<u8>) -> Option<()> {
    if !(SHORTEST..=LONGEST).contains(&length) {
        return None;
    }
    let bits = NDBITS[length];
    let &(prefix, change, suffix) = TRANSFORMS.get(id >> bits)?;
    let start = OFFSETS[length] + (id & ((1 << bits) - 1)) * length;
    let mut word = &WORDS[start..start + length];
    match change {
        1..=9 => word = &word[..length.saturating_sub(usize::from(change))],
        12..=20 => word = &word[usize::from(change - 11).min(length)..],
        _ => {}
    }
    out.extend_from_slice(prefix);
    let at = out.len();
    out.extend_from_slice(word);
    match change {
        10 if !word.is_empty() => {
            upper(&mut out[at..]);
        }
        11 => {
            let mut at = at;
            while at < out.len() {
                at += upper(&mut out[at..]);
            }
        }
        _ => {}
    }
    out.extend_from_slice(suffix);
    Some(())
}

/// Upper cases the character `bytes` starts with the rough way the RFC does, returning how many
/// bytes it took it to be. A byte the character would have past the end of the word is left alone.
fn upper(bytes: &mut [u8]) -> usize {
    let first = bytes[0];
    if first < 0xc0 {
        if first.is_ascii_lowercase() {
            bytes[0] ^= 32;
        }
        return 1;
    }
    if first < 0xe0 {
        if let Some(byte) = bytes.get_mut(1) {
            *byte ^= 32;
        }
        return 2;
    }
    if let Some(byte) = bytes.get_mut(2) {
        *byte ^= 5;
    }
    3
}

#[cfg(test)]
mod tests {
    use super::{OFFSETS, WORDS, word};

    #[test]
    fn the_words_of_every_length_fill_the_dictionary() {
        assert_eq!(OFFSETS[5], 4096);
        assert_eq!(OFFSETS[24] + (24 << 5), WORDS.len());
    }

    #[test]
    fn a_transform_wraps_and_changes_its_word() {
        let mut out = Vec::new();
        assert_eq!(word(4, 0, &mut out), Some(()));
        assert_eq!(out, b"time");
        // Transform 4 upper cases the first letter and puts a space after it, and 9 upper cases
        // the first letter alone.
        out.clear();
        word(4, 4 << 10, &mut out);
        assert_eq!(out, b"Time ");
        out.clear();
        word(4, 9 << 10, &mut out);
        assert_eq!(out, b"Time");
        out.clear();
        assert_eq!(word(4, 121 << 10, &mut out), None);
        assert_eq!(word(3, 0, &mut out), None);
    }
}

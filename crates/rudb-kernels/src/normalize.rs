//! `nfc_normalize` and `strip_accents`, which are utf8proc's `utf8proc_NFC` and the pin's
//! `utf8proc_remove_accents` added to it.
//!
//! Both are `utf8proc_map` with `STABLE` and `COMPOSE`, and `strip_accents` adds `STRIPMARK`. This
//! is a port of the three loops that runs: every code point is decomposed canonically and
//! recursively, with a mark dropped at whatever depth it turns up when marks are stripped, the
//! result is put in canonical order with utf8proc's own swapping pass, and then each starter is
//! composed with what follows it. The tables are read out of the pin's copy of utf8proc, so the
//! Unicode version is the pin's, and the loops keep utf8proc's quirks, such as a lone U+11A7 after
//! an LV syllable being swallowed into it.
//!
//! A string with no byte above 0x7F is answered as it stands, as the pin answers it, without
//! running the loops.

use std::borrow::Cow;

mod table;

use table::{CLASSES, COMPOSITIONS, DECOMPOSITIONS};

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// `nfc_normalize`.
pub(crate) fn nfc(text: &str) -> Cow<'_, str> {
    mapped(text, false)
}

/// `strip_accents`.
pub(crate) fn strip_accents(text: &str) -> Cow<'_, str> {
    mapped(text, true)
}

fn mapped(text: &str, strip: bool) -> Cow<'_, str> {
    if text.is_ascii() {
        return Cow::Borrowed(text);
    }
    let mut buffer = Vec::with_capacity(text.len());
    for c in text.chars() {
        decompose(u32::from(c), strip, &mut buffer);
    }
    reorder(&mut buffer);
    compose(&mut buffer);
    // Every code point in the buffer came out of a string or out of utf8proc's tables, and none of
    // those is a surrogate or above U+10FFFF.
    Cow::Owned(buffer.into_iter().filter_map(char::from_u32).collect())
}

/// The combining class of `cp` and whether it is a mark.
fn class(cp: u32) -> (u8, bool) {
    let at = CLASSES.partition_point(|&(start, ..)| start <= cp);
    let (_, class, mark) = CLASSES[at.saturating_sub(1)];
    (class, mark)
}

/// utf8proc's `utf8proc_decompose_char` with `COMPOSE`, and `STRIPMARK` when `strip` is set.
fn decompose(cp: u32, strip: bool, into: &mut Vec<u32>) {
    let syllable = cp.wrapping_sub(S_BASE);
    if syllable < S_COUNT {
        into.push(L_BASE + syllable / N_COUNT);
        into.push(V_BASE + (syllable % N_COUNT) / T_COUNT);
        let trail = syllable % T_COUNT;
        if trail != 0 {
            into.push(T_BASE + trail);
        }
        return;
    }
    if strip && class(cp).1 {
        return;
    }
    match DECOMPOSITIONS.binary_search_by_key(&cp, |&(from, ..)| from) {
        Ok(at) => {
            let (_, first, second) = DECOMPOSITIONS[at];
            decompose(first, strip, into);
            if second != 0 {
                decompose(second, strip, into);
            }
        }
        Err(_) => into.push(cp),
    }
}

/// The canonical ordering pass at the end of `utf8proc_decompose_custom`, which swaps two
/// neighbours when the first has the higher class and the second is not a starter, and steps back
/// one after a swap.
fn reorder(buffer: &mut [u32]) {
    let mut at = 0;
    while at + 1 < buffer.len() {
        let (first, second) = (class(buffer[at]).0, class(buffer[at + 1]).0);
        if first > second && second > 0 {
            buffer.swap(at, at + 1);
            at = if at > 0 { at - 1 } else { at + 1 };
        } else {
            at += 1;
        }
    }
}

/// The `COMPOSE` loop in `utf8proc_normalize_utf32`.
fn compose(buffer: &mut Vec<u32>) {
    let mut starter: Option<usize> = None;
    let mut highest: i32 = -1;
    let mut written = 0;
    for read in 0..buffer.len() {
        let current = buffer[read];
        let current_class = i32::from(class(current).0);
        if let Some(at) = starter
            && current_class > highest
        {
            let lead = buffer[at];
            let leading = lead.wrapping_sub(L_BASE);
            let vowel = current.wrapping_sub(V_BASE);
            if leading < L_COUNT && vowel < V_COUNT {
                buffer[at] = S_BASE + (leading * V_COUNT + vowel) * T_COUNT;
                continue;
            }
            let syllable = lead.wrapping_sub(S_BASE);
            let trail = current.wrapping_sub(T_BASE);
            if syllable < S_COUNT && syllable % T_COUNT == 0 && trail < T_COUNT {
                buffer[at] += trail;
                continue;
            }
            let pair = (lead, current);
            if let Ok(found) = COMPOSITIONS.binary_search_by_key(&pair, |&(a, b, _)| (a, b)) {
                buffer[at] = COMPOSITIONS[found].2;
                continue;
            }
        }
        buffer[written] = current;
        if current_class == 0 {
            starter = Some(written);
            highest = -1;
        } else if current_class > highest {
            highest = current_class;
        }
        written += 1;
    }
    buffer.truncate(written);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decomposed_letter_is_composed_and_an_accent_is_stripped() {
        assert_eq!(nfc("e\u{301}"), "\u{e9}");
        assert_eq!(nfc("\u{e9}"), "\u{e9}");
        assert_eq!(strip_accents("\u{e9}t\u{e9}"), "ete");
        assert_eq!(strip_accents("M\u{fc}hleisen"), "Muhleisen");
        assert!(matches!(nfc("plain"), Cow::Borrowed("plain")));
    }

    #[test]
    fn marks_are_put_in_canonical_order_before_composing() {
        // A dot below has class 220 and a circumflex 230, so they swap and then both compose.
        assert_eq!(nfc("a\u{302}\u{323}"), "\u{1ead}");
        assert_eq!(nfc("a\u{323}\u{302}"), "\u{1ead}");
    }

    #[test]
    fn hangul_is_composed_by_arithmetic() {
        assert_eq!(nfc("\u{1100}\u{1161}\u{11a8}"), "\u{ac01}");
        assert_eq!(nfc("\u{ac00}"), "\u{ac00}");
    }

    #[test]
    fn a_composition_exclusion_is_not_composed() {
        // U+0958 decomposes to U+0915 U+093C and is excluded, so it stays decomposed.
        assert_eq!(nfc("\u{958}"), "\u{915}\u{93c}");
    }
}

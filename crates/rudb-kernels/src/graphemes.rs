//! The functions that count a character as an extended grapheme cluster rather than a code point:
//! `length_grapheme`, `left_grapheme`, `right_grapheme`, `substring_grapheme` and `reverse`.
//!
//! A cluster ends where utf8proc says it does, which is `utf8proc_grapheme_break_stateful` in the
//! copy the pin vendors, ported below with its state packed the same way. That function reads two
//! properties of every code point out of Unicode 15.1, and [`table`] is those two properties read
//! out of the pin's own data file, so a new emoji or an Indic conjunct splits here where it splits
//! there.
//!
//! Each function also keeps the pin's shortcut for ASCII, and the shortcut is not quite the rule.
//! It treats every byte as a cluster, so `length_grapheme(E'a\r\nb')` is 4 although a carriage
//! return and a line feed are one cluster, and it is 3 once an `é` anywhere in the string sends it
//! down the slow path. `substring_grapheme` looks one byte past the end of the window before it
//! decides, so an accent that follows the last character still joins it.

use std::ops::Range;

use rudb_common::{Error, Result};

mod table;

/// The boundclass values the break rules name, as `utf8proc_boundclass_t` numbers them.
const START: u8 = 0;
const OTHER: u8 = 1;
const CR: u8 = 2;
const LF: u8 = 3;
const CONTROL: u8 = 4;
const EXTEND: u8 = 5;
const L: u8 = 6;
const V: u8 = 7;
const T: u8 = 8;
const LV: u8 = 9;
const LVT: u8 = 10;
const REGIONAL_INDICATOR: u8 = 11;
const SPACINGMARK: u8 = 12;
const PREPEND: u8 = 13;
const ZWJ: u8 = 14;
const EXTENDED_PICTOGRAPHIC: u8 = 19;
const E_ZWG: u8 = 20;

/// The indic conjunct break values, as `utf8proc_indic_conjunct_break_t` numbers them.
const CONJUNCT_NONE: u8 = 0;
const CONJUNCT_LINKER: u8 = 1;
const CONJUNCT_CONSONANT: u8 = 2;
const CONJUNCT_EXTEND: u8 = 3;

/// The largest offset or length `substring_grapheme` takes, and one less than the smallest.
const SUPPORTED: i64 = u32::MAX as i64;

/// The boundclass and the indic conjunct break value of a code point.
fn class(character: char) -> (u8, u8) {
    let code = character as u32;
    let at = table::RUNS.partition_point(|&(start, _)| start <= code) - 1;
    let packed = table::RUNS[at].1;
    (packed & 0x1F, packed >> 5)
}

/// `grapheme_break_simple`: whether the rules allow a break between two boundclasses, leaving out
/// the ones that need to know what came before.
fn simple(left: u8, right: u8) -> bool {
    match (left, right) {
        (START, _) => true,
        (CR, LF) => false,
        (CR..=CONTROL, _) | (_, CR..=CONTROL) => true,
        (L, L | V | LV | LVT) => false,
        (LV | V, V | T) => false,
        (LVT | T, T) => false,
        (_, EXTEND | ZWJ | SPACINGMARK) | (PREPEND, _) => false,
        (E_ZWG, EXTENDED_PICTOGRAPHIC) => false,
        (REGIONAL_INDICATOR, REGIONAL_INDICATOR) => false,
        _ => true,
    }
}

/// `grapheme_break_extended` with a state, which is zero at the start of a cluster and otherwise
/// the boundclass so far in the low byte and the indic conjunct break value so far above it.
fn breaks(left: char, right: char, state: &mut u32) -> bool {
    let ((left_bound, left_conjunct), (right_bound, right_conjunct)) = (class(left), class(right));
    let (mut bound, mut conjunct) = if *state == 0 {
        let conjunct =
            if left_conjunct == CONJUNCT_CONSONANT { left_conjunct } else { CONJUNCT_NONE };
        (left_bound, conjunct)
    } else {
        ((*state & 0xFF) as u8, (*state >> 8) as u8)
    };
    let permitted = simple(bound, right_bound)
        && !(conjunct == CONJUNCT_LINKER && right_conjunct == CONJUNCT_CONSONANT);
    if right_conjunct == CONJUNCT_CONSONANT
        || conjunct == CONJUNCT_CONSONANT
        || conjunct == CONJUNCT_EXTEND
    {
        conjunct = right_conjunct;
    } else if conjunct == CONJUNCT_LINKER {
        conjunct = if right_conjunct == CONJUNCT_EXTEND { CONJUNCT_LINKER } else { right_conjunct };
    }
    // Two regional indicators make a flag and a third starts the next one, which the simple rules
    // cannot see, so the second one is remembered as OTHER to force the break after it.
    bound = if bound == right_bound && right_bound == REGIONAL_INDICATOR {
        OTHER
    } else if bound == EXTENDED_PICTOGRAPHIC {
        match right_bound {
            EXTEND => EXTENDED_PICTOGRAPHIC,
            ZWJ => E_ZWG,
            _ => right_bound,
        }
    } else {
        right_bound
    };
    *state = u32::from(bound) + (u32::from(conjunct) << 8);
    permitted
}

/// Where the cluster that starts at byte `from` of `text` ends.
fn next_cluster(text: &str, from: usize) -> usize {
    let mut characters = text[from..].char_indices();
    let Some((_, mut previous)) = characters.next() else {
        return text.len();
    };
    let mut state = 0;
    for (offset, next) in characters {
        if breaks(previous, next, &mut state) {
            return from + offset;
        }
        previous = next;
    }
    text.len()
}

/// The byte range of every cluster in `text`, in order.
pub(crate) fn clusters(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut at = 0;
    std::iter::from_fn(move || {
        (at < text.len()).then(|| {
            let start = at;
            at = next_cluster(text, at);
            start..at
        })
    })
}

/// `length_grapheme`, which is one per byte for ASCII and one per cluster otherwise.
pub(crate) fn count(text: &str) -> i64 {
    if text.is_ascii() { text.len() as i64 } else { clusters(text).count() as i64 }
}

/// `reverse`: the clusters in the opposite order, each of them left as it was.
pub(crate) fn reverse(text: &str) -> String {
    if text.is_ascii() {
        return text.chars().rev().collect();
    }
    let clusters: Vec<Range<usize>> = clusters(text).collect();
    let mut out = String::with_capacity(text.len());
    for cluster in clusters.into_iter().rev() {
        out.push_str(&text[cluster]);
    }
    out
}

/// `substring_grapheme(text, offset)`, which is the three argument form with the largest length.
pub(crate) fn substring_rest(text: &str, offset: i64) -> Result<&str> {
    substring(text, offset, SUPPORTED)
}

/// `substring_grapheme(text, offset, length)`, a port of `SubstringGrapheme`.
///
/// The window is worked out on the byte length first. If every byte up to one past its end is
/// ASCII it is cut there, and otherwise it is worked out again on the cluster count and found by
/// walking the clusters. The second answer is not checked for being empty the way the first is, and
/// the walk stops at the end only once it has passed the start, so a window that counting back
/// empties on clusters but not on bytes runs from its start to the end of the string, which is
/// what the pin answers to `substring_grapheme('🦆🦆', -5, 2)` where `substring` answers nothing.
/// That is tamnd/duckdb#26, kept here because it is what the pin does.
pub(crate) fn substring(text: &str, offset: i64, length: i64) -> Result<&str> {
    supported(text.len(), offset, length)?;
    let Some((mut start, mut end)) = bounds(text.len() as i64, offset, length) else {
        return Ok("");
    };
    let ascii_end = (end as usize + 1).min(text.len());
    if text.as_bytes()[..ascii_end].is_ascii() {
        return Ok(&text[start as usize..end as usize]);
    }
    if offset < 0 {
        let (from, to) = clamped(count(text), offset, length);
        (start, end) = (from, to);
    }
    let (mut from, mut to) = (None, text.len());
    for (at, cluster) in clusters(text).enumerate() {
        let at = at as i64;
        if at == start {
            from = Some(cluster.start);
        } else if at == end {
            to = cluster.start;
            break;
        }
    }
    Ok(from.map_or("", |from| &text[from..to]))
}

/// `left_grapheme`, which counts from the far end for a negative count.
pub(crate) fn left(text: &str, count_or_back: i64) -> Result<&str> {
    let kept =
        if count_or_back >= 0 { count_or_back } else { (count(text) + count_or_back).max(0) };
    substring(text, 1, kept)
}

/// `right_grapheme`, which keeps all but that many for a negative count.
pub(crate) fn right(text: &str, count_or_back: i64) -> Result<&str> {
    let total = count(text);
    let kept = if count_or_back >= 0 {
        total.min(count_or_back)
    } else if count_or_back == i64::MIN {
        0
    } else {
        total - total.min(-count_or_back)
    };
    substring(text, total - kept + 1, kept)
}

/// `AssertInSupportedRange`, in its words.
fn supported(size: usize, offset: i64, length: i64) -> Result<()> {
    let refused = |what: &str, side: &str, bound: i64| {
        Err(Error::out_of_range(format!(
            "Substring {what} outside of supported range ({side} {bound})"
        )))
    };
    if size as u64 > SUPPORTED as u64 {
        return Err(Error::out_of_range(format!(
            "Substring input size is too large (> {SUPPORTED})"
        )));
    }
    if offset < -SUPPORTED - 1 {
        return refused("offset", "<", -SUPPORTED - 1);
    }
    if offset > SUPPORTED {
        return refused("offset", ">", SUPPORTED);
    }
    if length < -SUPPORTED - 1 {
        return refused("length", "<", -SUPPORTED - 1);
    }
    if length > SUPPORTED {
        return refused("length", ">", SUPPORTED);
    }
    Ok(())
}

/// `SubstringASCIIBounds`: the zero based window over `size` characters, or `None` when it is
/// empty.
fn bounds(size: i64, offset: i64, length: i64) -> Option<(i64, i64)> {
    let (start, end) = start_end(offset, length)?;
    let (start, end) = clamp(size, offset, start, end);
    (start < end).then_some((start, end))
}

/// [`bounds`] without the check for an empty window, which is how `SubstringGrapheme` calls it the
/// second time. The first call has already said the length is not zero.
fn clamped(size: i64, offset: i64, length: i64) -> (i64, i64) {
    let (start, end) = start_end(offset, length).unwrap_or((0, 0));
    clamp(size, offset, start, end)
}

fn clamp(size: i64, offset: i64, start: i64, end: i64) -> (i64, i64) {
    let (start, end) = if offset < 0 { (start + size, end + size) } else { (start, end) };
    (start.clamp(0, size), end.clamp(0, size))
}

/// `SubstringStartEnd`: the window before it is clamped, with an offset of zero starting one
/// character before the string.
fn start_end(offset: i64, mut length: i64) -> Option<(i64, i64)> {
    if length == 0 {
        return None;
    }
    let start = match offset {
        1.. => offset - 1,
        ..0 => offset,
        0 => {
            length -= 1;
            if length <= 0 {
                return None;
            }
            0
        }
    };
    Some(if length > 0 { (start, start + length) } else { (start + length, start) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(text: &str) -> Vec<&str> {
        clusters(text).map(|cluster| &text[cluster]).collect()
    }

    #[test]
    fn clusters_are_split_where_utf8proc_splits_them() {
        assert_eq!(split("e\u{301}x"), ["e\u{301}", "x"]);
        assert_eq!(split("\r\n\n"), ["\r\n", "\n"]);
        assert_eq!(split("🇺🇸🇫🇷🇩"), ["🇺🇸", "🇫🇷", "🇩"]);
        assert_eq!(split("🤦🏼\u{200d}♂\u{fe0f}a"), ["🤦🏼\u{200d}♂\u{fe0f}", "a"]);
        assert_eq!(split("\u{1100}\u{1161}\u{11a8}"), ["\u{1100}\u{1161}\u{11a8}"]);
        // GB9c, which is new in Unicode 15.1: two consonants joined by a virama are one cluster.
        assert_eq!(split("क्षि"), ["क्षि"]);
        assert_eq!(split(""), Vec::<&str>::new());
    }

    #[test]
    fn the_table_covers_every_code_point_from_zero() {
        assert_eq!(table::RUNS[0].0, 0);
        assert!(table::RUNS.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(class('a'), (OTHER, CONJUNCT_NONE));
        assert_eq!(class('\r'), (CR, CONJUNCT_NONE));
        assert_eq!(class('\u{10FFFF}').0, OTHER);
    }

    #[test]
    fn a_substring_is_cut_on_clusters() {
        assert_eq!(substring("🦆🦆x🦆", 2, 2).unwrap(), "🦆x");
        assert_eq!(substring_rest("🦆🦆x🦆", -2).unwrap(), "x🦆");
        assert_eq!(substring("🦆🦆x🦆", 0, 2).unwrap(), "🦆");
        assert_eq!(substring("🦆🦆x🦆", 3, -2).unwrap(), "🦆🦆");
        assert_eq!(substring("a\r\nb", 2, 1).unwrap(), "\r");
        assert_eq!(substring("ae\u{301}b", 2, 1).unwrap(), "e\u{301}");
        assert_eq!(left("abc", -5).unwrap(), "");
        assert_eq!(right("abc", i64::MIN).unwrap(), "");
        assert_eq!(reverse("🤦🏼\u{200d}♂\u{fe0f}x🇺🇸"), "🇺🇸x🤦🏼\u{200d}♂\u{fe0f}");
    }
}

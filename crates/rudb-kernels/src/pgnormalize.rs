//! `normalize` and `IS NORMALIZED` of `pg_proc`, a port of `unicode_norm.c` over the tables of the
//! pin of PostgreSQL, so that the Unicode version is the one of PostgreSQL and not the one of the
//! utf8proc of the DuckDB pin that `nfc_normalize` uses.
//!
//! `unicode_is_normalized` is the comparison of the string with its normal form. The quick check
//! of PostgreSQL only makes the same answer faster.

mod table;

use std::borrow::Cow;

use rudb_common::{Error, Result, SqlState, Value};

use table::{CODEPOINTS, COMPOSITIONS, DECOMP_COMPAT, DECOMP_INLINE, DECOMPOSITIONS};

/// The C functions of this module, sorted.
pub(crate) const SOURCES: &[&str] = &["unicode_is_normalized", "unicode_normalize_func"];

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// The value of the C function `src` over `args`, or `None` for another function.
pub(crate) fn call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    use Value::{Boolean, Varchar};
    let value = match (src, args) {
        ("unicode_normalize_func", [Varchar(text), Varchar(form)]) => {
            Varchar(normalize(text, Form::from_name(form)?).into_owned())
        }
        ("unicode_is_normalized", [Varchar(text), Varchar(form)]) => {
            Boolean(normalize(text, Form::from_name(form)?) == text.as_str())
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// `UnicodeNormalizationForm`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Form {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

impl Form {
    /// `unicode_norm_form_from_string`.
    fn from_name(name: &str) -> Result<Form> {
        Ok(match name.to_ascii_uppercase().as_str() {
            "NFC" => Form::Nfc,
            "NFD" => Form::Nfd,
            "NFKC" => Form::Nfkc,
            "NFKD" => Form::Nfkd,
            _ => {
                return Err(Error::invalid_input(format!("invalid normalization form: {name}"))
                    .state(SqlState::INVALID_PARAMETER_VALUE)
                    .unplaced());
            }
        })
    }

    fn compat(self) -> bool {
        matches!(self, Form::Nfkc | Form::Nfkd)
    }

    fn recompose(self) -> bool {
        matches!(self, Form::Nfc | Form::Nfkc)
    }
}

/// `unicode_normalize`. No code point below U+00A0 has a decomposition or a combining class, so a
/// string of ASCII is its own normal form in each of the four.
fn normalize(text: &str, form: Form) -> Cow<'_, str> {
    if text.is_ascii() {
        return Cow::Borrowed(text);
    }
    let mut chars = Vec::with_capacity(text.len());
    for c in text.chars() {
        decompose(u32::from(c), form.compat(), &mut chars);
    }
    reorder(&mut chars);
    if form.recompose() {
        recompose(&mut chars);
    }
    // Each code point came from the text or from the tables, and none of them is a surrogate.
    Cow::Owned(chars.into_iter().filter_map(char::from_u32).collect())
}

/// `get_code_entry`: the combining class, the flags and the index of `code`.
fn entry(code: u32) -> Option<(u8, u8, u16)> {
    let at = DECOMPOSITIONS.binary_search_by_key(&code, |&(codepoint, ..)| codepoint).ok()?;
    let (_, class, flags, index) = DECOMPOSITIONS[at];
    Some((class, flags, index))
}

/// `get_canonical_class`.
fn class(code: u32) -> u8 {
    entry(code).map_or(0, |(class, ..)| class)
}

/// `decompose_code`: the full decomposition of `code`, canonical or with the compatibility
/// mappings too.
fn decompose(code: u32, compat: bool, into: &mut Vec<u32>) {
    let syllable = code.wrapping_sub(S_BASE);
    if syllable < S_COUNT {
        into.push(L_BASE + syllable / N_COUNT);
        into.push(V_BASE + (syllable % N_COUNT) / T_COUNT);
        let trail = syllable % T_COUNT;
        if trail != 0 {
            into.push(T_BASE + trail);
        }
        return;
    }
    let Some((_, flags, index)) = entry(code) else {
        into.push(code);
        return;
    };
    let size = usize::from(flags & 0x1F);
    if size == 0 || (!compat && flags & DECOMP_COMPAT != 0) {
        into.push(code);
    } else if flags & DECOMP_INLINE != 0 {
        decompose(u32::from(index), compat, into);
    } else {
        let start = usize::from(index);
        for &part in &CODEPOINTS[start..start + size] {
            decompose(part, compat, into);
        }
    }
}

/// The canonical ordering of `unicode_normalize`, which swaps two neighbours when both have a
/// class and the first has the higher one, and steps back one after a swap.
fn reorder(chars: &mut [u32]) {
    let mut count = 1;
    while count < chars.len() {
        let previous = class(chars[count - 1]);
        let next = class(chars[count]);
        if previous != 0 && next != 0 && previous > next {
            chars.swap(count - 1, count);
            if count > 1 {
                count -= 2;
            }
        }
        count += 1;
    }
}

/// `recompose_code`: the code point that `start` and `code` compose into.
fn composite(start: u32, code: u32) -> Option<u32> {
    let leading = start.wrapping_sub(L_BASE);
    let vowel = code.wrapping_sub(V_BASE);
    if leading < L_COUNT && vowel < V_COUNT {
        return Some(S_BASE + (leading * V_COUNT + vowel) * T_COUNT);
    }
    let syllable = start.wrapping_sub(S_BASE);
    if syllable < S_COUNT && syllable.is_multiple_of(T_COUNT) && code > T_BASE {
        let trail = code - T_BASE;
        if trail < T_COUNT {
            return Some(start + trail);
        }
    }
    let at = COMPOSITIONS.binary_search_by_key(&(start, code), |&(a, b, _)| (a, b)).ok()?;
    Some(COMPOSITIONS[at].2)
}

/// The recomposition of NFC and NFKC at the end of `unicode_normalize`.
fn recompose(chars: &mut Vec<u32>) {
    if chars.is_empty() {
        return;
    }
    let mut last_class: i32 = -1;
    let mut starter_at = 0;
    let mut starter = chars[0];
    let mut target = 1;
    for count in 1..chars.len() {
        let ch = chars[count];
        let ch_class = i32::from(class(ch));
        if last_class < ch_class
            && let Some(composed) = composite(starter, ch)
        {
            chars[starter_at] = composed;
            starter = composed;
        } else if ch_class == 0 {
            starter_at = target;
            starter = ch;
            last_class = -1;
            chars[target] = ch;
            target += 1;
        } else {
            last_class = ch_class;
            chars[target] = ch;
            target += 1;
        }
    }
    chars.truncate(target);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normal(text: &str, form: &str) -> String {
        normalize(text, Form::from_name(form).unwrap()).into_owned()
    }

    #[test]
    fn the_four_forms_are_the_forms_of_postgresql() {
        assert_eq!(normal("a\u{301}", "nfc"), "\u{e1}");
        assert_eq!(normal("\u{e1}", "NFD"), "a\u{301}");
        assert_eq!(normal("\u{fb01}", "NFKC"), "fi");
        assert_eq!(normal("\u{fb01}", "NFC"), "\u{fb01}");
        assert_eq!(normal("\u{1e9b}\u{323}", "NFKD"), "s\u{323}\u{307}");
        assert_eq!(normal("\u{1e9b}\u{323}", "NFC"), "\u{1e9b}\u{323}");
        assert_eq!(normal("\u{1e9b}\u{323}", "NFKC"), "\u{1e69}");
        // A dot below has class 220 and a circumflex 230, so they swap and then both compose.
        assert_eq!(normal("a\u{302}\u{323}", "NFC"), "\u{1ead}");
        assert_eq!(normal("\u{1100}\u{1161}\u{11a8}", "NFC"), "\u{ac01}");
        assert_eq!(normal("\u{ac01}", "NFD"), "\u{1100}\u{1161}\u{11a8}");
        // U+0958 is in the exclusion list, so it stays decomposed.
        assert_eq!(normal("\u{958}", "NFC"), "\u{915}\u{93c}");
        assert_eq!(normal("", "NFC"), "");
    }

    #[test]
    fn is_normalized_compares_with_the_normal_form_and_a_bad_form_is_an_error() {
        let is = |text: &str, form: &str| {
            let args = [Value::Varchar(text.into()), Value::Varchar(form.into())];
            call("unicode_is_normalized", &args)
        };
        assert_eq!(is("\u{e1}", "NFC").unwrap(), Some(Value::Boolean(true)));
        assert_eq!(is("a\u{301}", "NFC").unwrap(), Some(Value::Boolean(false)));
        assert_eq!(is("a\u{301}", "NFD").unwrap(), Some(Value::Boolean(true)));
        let error = is("a", "nfx").unwrap_err();
        assert_eq!(error.reported_state(), SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(error.message(), "invalid normalization form: nfx");
    }
}

//! The character classes and the case mapping that a regular expression takes from the collation
//! of the call, as `pg_set_regex_collation` picks them and `ctype_methods_builtin` in
//! `pg_locale_builtin.c` gives them for the builtin collations.
//!
//! The builtin collations read the classes from the properties and the general categories of
//! Unicode, by the functions of `unicode_category.c`, and map the case by the simple mappings.
//! `C.UTF-8` takes the POSIX form of `digit` and `punct`, and `PG_UNICODE_FAST` the standard form.
//! The pattern asks for the members of a class as ranges, so each class is worked out once over
//! the tables and kept.

use std::sync::OnceLock;

use rudb_common::Result;
use rudb_regex::{AsciiCtype, PgClass, PgCtype};

use super::table::{ALPHABETIC, CATEGORIES, Category, LOWERCASE, UPPERCASE, WHITE_SPACE};
use super::{Casing, Kind, case_of, category, in_ranges, is_alnum};

/// The classes in the order of [`PgClass`].
const CLASSES: [PgClass; 10] = [
    PgClass::Alnum,
    PgClass::Alpha,
    PgClass::Digit,
    PgClass::Graph,
    PgClass::Lower,
    PgClass::Print,
    PgClass::Punct,
    PgClass::Space,
    PgClass::Upper,
    PgClass::Word,
];

/// The classes and the case mapping of a builtin collation that is not `C`.
struct Builtin {
    /// Whether `digit` and `punct` take the POSIX form, which is `C.UTF-8`.
    posix: bool,
    /// The ranges of each class of [`CLASSES`], once a pattern has asked for them.
    classes: [OnceLock<Vec<(char, char)>>; CLASSES.len()],
}

static C_UTF8: Builtin = Builtin { posix: true, classes: [const { OnceLock::new() }; 10] };

static UNICODE_FAST: Builtin = Builtin { posix: false, classes: [const { OnceLock::new() }; 10] };

/// The classes and the case mapping of the collation with the OID `oid`.
pub(crate) fn ctype(oid: i64) -> Result<&'static dyn PgCtype> {
    Ok(match Casing::of(oid)? {
        Casing::Ascii => &AsciiCtype,
        Casing::Simple => &C_UTF8,
        Casing::Full => &UNICODE_FAST,
    })
}

impl Builtin {
    /// Whether the class holds the code point, as the `pg_u_is` function of the class decides it.
    fn holds(&self, class: PgClass, code: u32) -> bool {
        let punctuation = |category| {
            matches!(
                category,
                Category::DashPunctuation
                    | Category::OpenPunctuation
                    | Category::ClosePunctuation
                    | Category::ConnectorPunctuation
                    | Category::OtherPunctuation
                    | Category::InitialPunctuation
                    | Category::FinalPunctuation
            )
        };
        let symbol = |category| {
            matches!(
                category,
                Category::MathSymbol
                    | Category::CurrencySymbol
                    | Category::ModifierSymbol
                    | Category::OtherSymbol
            )
        };
        let graph = |code| {
            !matches!(
                category(code),
                Category::Control | Category::Surrogate | Category::Unassigned
            ) && !in_ranges(&WHITE_SPACE, code)
        };
        match class {
            PgClass::Alnum => is_alnum(code, self.posix),
            PgClass::Alpha => in_ranges(&ALPHABETIC, code),
            PgClass::Digit if self.posix => (u32::from('0')..=u32::from('9')).contains(&code),
            PgClass::Digit => category(code) == Category::DecimalNumber,
            PgClass::Graph => graph(code),
            PgClass::Lower => in_ranges(&LOWERCASE, code),
            PgClass::Print => {
                let blank = code == u32::from('\t') || category(code) == Category::SpaceSeparator;
                category(code) != Category::Control && (graph(code) || blank)
            }
            PgClass::Punct if self.posix => {
                !in_ranges(&ALPHABETIC, code) && {
                    let category = category(code);
                    punctuation(category) || symbol(category)
                }
            }
            PgClass::Punct => punctuation(category(code)),
            PgClass::Space => in_ranges(&WHITE_SPACE, code),
            PgClass::Upper => in_ranges(&UPPERCASE, code),
            // `regc_wc_isword`: `alnum` and the underscore.
            PgClass::Word => code == u32::from('_') || is_alnum(code, self.posix),
        }
    }

    /// The ranges of the class. The answer of a class changes only where a range of one of the
    /// tables begins or ends, or at one of the characters that a class names, so the class is
    /// asked once for each piece between two of those points.
    fn ranges(&self, class: PgClass) -> Vec<(char, char)> {
        let mut points: Vec<u32> = ['\t', '0', '_'].iter().map(|&c| u32::from(c)).collect();
        points.extend([u32::from('\t') + 1, u32::from('9') + 1, u32::from('_') + 1]);
        points.extend([0, 0x11_0000]);
        for &(first, last, _) in &CATEGORIES {
            points.extend([first, last + 1]);
        }
        for table in [&ALPHABETIC[..], &LOWERCASE, &UPPERCASE, &WHITE_SPACE] {
            for &(first, last) in table {
                points.extend([first, last + 1]);
            }
        }
        points.sort_unstable();
        points.dedup();
        let mut out: Vec<(char, char)> = Vec::new();
        for piece in points.windows(2) {
            let (first, last) = (piece[0], piece[1] - 1);
            let (Some(low), Some(high)) = (char::from_u32(first), char::from_u32(last)) else {
                continue;
            };
            if !self.holds(class, first) {
                continue;
            }
            match out.last_mut() {
                Some(previous) if u32::from(previous.1) + 1 == first => previous.1 = high,
                _ => out.push((low, high)),
            }
        }
        out
    }
}

impl PgCtype for Builtin {
    fn class(&self, class: PgClass) -> &[(char, char)] {
        let at = CLASSES.iter().position(|&held| held == class).unwrap_or(0);
        self.classes[at].get_or_init(|| self.ranges(class))
    }

    fn lower(&self, ch: char) -> char {
        simple(ch, Kind::Lower)
    }

    fn upper(&self, ch: char) -> char {
        simple(ch, Kind::Upper)
    }
}

/// The simple mapping of a character to the case `kind`, as `unicode_lowercase_simple` and
/// `unicode_uppercase_simple` give it.
fn simple(ch: char, kind: Kind) -> char {
    case_of(u32::from(ch)).and_then(|&(_, map, _)| char::from_u32(map[kind as usize])).unwrap_or(ch)
}

#[cfg(test)]
mod tests {
    use rudb_regex::{PgFlags, Regex};

    use super::*;

    /// The first match of `pattern` in `text` in the collation `collation`.
    fn first(pattern: &str, text: &str, flags: &str, collation: &str) -> Option<String> {
        let oid = rudb_pgtypes::collation(collation).map(|collation| collation.oid).unwrap();
        let flags = PgFlags::parse(flags).unwrap();
        let regex = Regex::postgres(pattern, flags, ctype(i64::from(oid)).unwrap()).unwrap();
        regex.find_at(text, 0).map(|found| text[found.start()..found.end()].to_owned())
    }

    fn pieces(pattern: &str, text: &str, collation: &str) -> String {
        first(pattern, text, "", collation).unwrap_or_default()
    }

    #[test]
    fn the_builtin_collations_read_the_classes_from_unicode() {
        for collation in ["pg_c_utf8", "pg_unicode_fast"] {
            assert_eq!(pieces("[[:alpha:]]+", "1ab\u{e9}\u{3b1}2", collation), "ab\u{e9}\u{3b1}");
            assert_eq!(pieces("[[:upper:]]+", "a\u{c9}\u{391}b", collation), "\u{c9}\u{391}");
            assert_eq!(pieces("\\s+", "a\u{a0}\u{2028} b", collation), "\u{a0}\u{2028} ");
            assert_eq!(pieces("\\w+", ",a_\u{e9},", collation), "a_\u{e9}");
            assert_eq!(pieces("[^[:alpha:]]+", "a\u{e9}12\u{3b1}", collation), "12");
            // The word boundaries look at the same word characters.
            assert_eq!(first("\\mb", "\u{e9}b", "", collation), None);
        }
    }

    #[test]
    fn c_utf8_takes_the_posix_form_of_digit_and_punct() {
        assert_eq!(pieces("[[:digit:]]+", "\u{663}12", "pg_c_utf8"), "12");
        assert_eq!(pieces("[[:digit:]]+", "a\u{663}12", "pg_unicode_fast"), "\u{663}12");
        assert_eq!(pieces("[[:punct:]]+", "a$+!b", "pg_c_utf8"), "$+!");
        assert_eq!(pieces("[[:punct:]]+", "a$+!b", "pg_unicode_fast"), "!");
        assert_eq!(pieces("\\w+", ".a\u{663}.", "pg_c_utf8"), "a");
        assert_eq!(pieces("\\w+", ".a\u{663}.", "pg_unicode_fast"), "a\u{663}");
    }

    #[test]
    fn case_folding_is_the_simple_lower_and_upper_case_and_not_the_character_itself() {
        for collation in ["pg_c_utf8", "pg_unicode_fast"] {
            assert_eq!(first("\u{e9}", "\u{c9}", "i", collation), Some("\u{c9}".into()));
            assert_eq!(
                first("[\u{e0}-\u{e9}]+", "\u{c0}\u{c9}", "i", collation).unwrap(),
                "\u{c0}\u{c9}"
            );
            // A title case letter folds to its lower and upper case and not to itself.
            assert_eq!(first("\u{1c5}", "\u{1c5}", "i", collation), None);
            assert_eq!(first("[\u{1c5}]", "\u{1c6}", "i", collation), Some("\u{1c6}".into()));
            // The simple mappings do not fold the sharp s to `ss`.
            assert_eq!(first("\u{df}", "ss", "i", collation), None);
            assert_eq!(first("\u{1e9e}", "\u{df}", "i", collation), Some("\u{df}".into()));
        }
        assert_eq!(first("\u{e9}", "\u{c9}", "i", "C"), None);
    }

    #[test]
    fn the_classes_are_sorted_and_hold_no_surrogate() {
        for builtin in [&C_UTF8, &UNICODE_FAST] {
            for class in CLASSES {
                let ranges = builtin.class(class);
                assert!(
                    ranges.windows(2).all(|pair| u32::from(pair[0].1) + 1 < u32::from(pair[1].0))
                );
                assert!(ranges.iter().all(|&(low, high)| low <= high));
            }
        }
    }
}

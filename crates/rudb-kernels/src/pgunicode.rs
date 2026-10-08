//! `upper`, `lower`, `initcap` and `casefold` of `pg_proc` by the collation of the call, a port of
//! `unicode_case.c` and of `str_toupper` and the functions near it in `formatting.c`, over the
//! tables of the pin of PostgreSQL.
//!
//! PostgreSQL gives each call of a function the collation of its arguments. The binder gives the
//! kernel of each function of [`COLLATED`] the OID of that collation as one more argument, after
//! the declared ones. A collation whose `ctype` is `C`, as `C`, `POSIX` and `ucs_basic` are, maps
//! only the ASCII letters. `pg_c_utf8` maps each code point by the simple mappings of Unicode, and
//! `pg_unicode_fast` by the full mappings, with the special mappings and the condition
//! `Final_Sigma`. The properties of a code point here are the ones of `unicode_category.c` that the
//! case mapping reads.

mod table;

use std::cmp::Ordering;

use rudb_common::{Error, Result, SqlState, Value};

use table::{
    ALPHABETIC, CASE_IGNORABLE, CASES, CATEGORIES, Category, FINAL_SIGMA, LOWERCASE, SPECIALS,
    UPPERCASE,
};

/// The C functions of this module, sorted.
pub(crate) const SOURCES: &[&str] = &["casefold", "initcap", "lower", "upper"];

/// The C functions whose kernels take the OID of the collation of the call as one more argument,
/// a `BIGINT`, as `PG_GET_COLLATION` gives it to the C function.
pub(crate) const COLLATED: &[&str] = SOURCES;

/// A kind of case, in the order of the maps of a row of [`CASES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Lower = 0,
    Title = 1,
    Upper = 2,
    Fold = 3,
}

/// How a collation maps the case of a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Casing {
    /// Only the ASCII letters, for a collation whose `ctype` is `C`.
    Ascii,
    /// The simple mappings of one code point to one code point, of the builtin `C.UTF-8`.
    Simple,
    /// The full mappings, with the special mappings, of the builtin `PG_UNICODE_FAST`.
    Full,
}

impl Casing {
    /// The case mapping of the collation with the OID `oid`, as `pg_newlocale_from_collation`
    /// makes it. rudb is a build without ICU.
    fn of(oid: i64) -> Result<Casing> {
        let collation = u32::try_from(oid).ok().and_then(rudb_pgtypes::collation_by_oid);
        match collation.map(|collation| (collation.provider, collation.locale)) {
            Some((b'c', "C" | "POSIX") | (b'b', "C")) => Ok(Casing::Ascii),
            Some((b'b', "C.UTF-8")) => Ok(Casing::Simple),
            Some((b'b', "PG_UNICODE_FAST")) => Ok(Casing::Full),
            Some((b'i', _)) => Err(Error::not_implemented("ICU is not supported in this build")
                .state(SqlState::FEATURE_NOT_SUPPORTED)
                .unplaced()),
            _ => Err(Error::internal(format!("cache lookup failed for collation {oid}"))),
        }
    }
}

/// The value of the C function `src` over `args`, or `None` for another function.
pub(crate) fn call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    let kind = match src {
        "lower" => Kind::Lower,
        "initcap" => Kind::Title,
        "upper" => Kind::Upper,
        "casefold" => Kind::Fold,
        _ => return Ok(None),
    };
    let [Value::Varchar(text), Value::BigInt(collation)] = args else { return Ok(None) };
    Ok(Some(Value::Varchar(convert(text, kind, Casing::of(*collation)?))))
}

/// The text in the case `kind` by the case mapping `casing`.
fn convert(text: &str, kind: Kind, casing: Casing) -> String {
    if casing == Casing::Ascii || text.is_ascii() {
        return ascii(text, kind);
    }
    let full = casing == Casing::Full;
    let mut out = String::with_capacity(text.len());
    let mut previous = None;
    for (at, c) in text.char_indices() {
        let code = u32::from(c);
        // `initcap` maps the first character of each word to title case, or to upper case
        // without the full mappings, and the others to lower case. A word ends where the result
        // of `pg_u_isalnum` changes, as `initcap_wbnext` finds it.
        let kind = match kind {
            Kind::Title => {
                let alnum = is_alnum(code, !full);
                let boundary = previous != Some(alnum);
                previous = Some(alnum);
                match (boundary, full) {
                    (true, true) => Kind::Title,
                    (true, false) => Kind::Upper,
                    (false, _) => Kind::Lower,
                }
            }
            kind => kind,
        };
        let Some(&(_, map, special)) = case_of(code) else {
            out.push(c);
            continue;
        };
        let (conditions, specials) = SPECIALS[usize::from(special)];
        if full && special != 0 && (conditions != FINAL_SIGMA || final_sigma(text, at, c)) {
            let points = specials[kind as usize].iter().take_while(|&&point| point != 0);
            out.extend(points.filter_map(|&point| char::from_u32(point)));
        } else {
            out.push(char::from_u32(map[kind as usize]).unwrap_or(c));
        }
    }
    out
}

/// The text in the case `kind` with only the ASCII letters mapped, as `asc_tolower`,
/// `asc_toupper` and `asc_initcap` map them. A text of ASCII has the same case in every
/// collation.
fn ascii(text: &str, kind: Kind) -> String {
    match kind {
        Kind::Lower | Kind::Fold => text.to_ascii_lowercase(),
        Kind::Upper => text.to_ascii_uppercase(),
        Kind::Title => {
            let mut after_alnum = false;
            text.chars()
                .map(|c| {
                    let c = match after_alnum {
                        true => c.to_ascii_lowercase(),
                        false => c.to_ascii_uppercase(),
                    };
                    after_alnum = c.is_ascii_alphanumeric();
                    c
                })
                .collect()
        }
    }
}

/// The row of [`CASES`] of the code point, if it has a case mapping.
fn case_of(code: u32) -> Option<&'static (u32, [u32; 4], u8)> {
    CASES.binary_search_by_key(&code, |row| row.0).ok().map(|at| &CASES[at])
}

/// Whether the sigma at `at` ends a word, as `check_final_sigma` finds it: a cased character is
/// before it and none is after it, with the characters that ignore case left out.
fn final_sigma(text: &str, at: usize, sigma: char) -> bool {
    let ignorable = |c: &char| in_ranges(&CASE_IGNORABLE, u32::from(*c));
    let before = text[..at].chars().rev().find(|c| !ignorable(c));
    let after = text[at + sigma.len_utf8()..].chars().find(|c| !ignorable(c));
    before.is_some_and(|c| is_cased(u32::from(c))) && !after.is_some_and(|c| is_cased(u32::from(c)))
}

/// Whether one of the sorted ranges has the code point.
fn in_ranges(ranges: &[(u32, u32)], code: u32) -> bool {
    ranges
        .binary_search_by(|&(first, last)| match () {
            () if last < code => Ordering::Less,
            () if first > code => Ordering::Greater,
            () => Ordering::Equal,
        })
        .is_ok()
}

/// The general category of the code point.
fn category(code: u32) -> Category {
    CATEGORIES
        .binary_search_by(|&(first, last, _)| match () {
            () if last < code => Ordering::Less,
            () if first > code => Ordering::Greater,
            () => Ordering::Equal,
        })
        .map_or(Category::Unassigned, |at| CATEGORIES[at].2)
}

/// `pg_u_prop_cased`: a letter of title case, or a code point of lower case or of upper case.
fn is_cased(code: u32) -> bool {
    category(code) == Category::TitlecaseLetter
        || in_ranges(&LOWERCASE, code)
        || in_ranges(&UPPERCASE, code)
}

/// `pg_u_isalnum`: an alphabetic code point or a digit, which is `0` to `9` in the POSIX form and
/// a decimal number in the standard form.
fn is_alnum(code: u32, posix: bool) -> bool {
    in_ranges(&ALPHABETIC, code)
        || match posix {
            true => (u32::from('0')..=u32::from('9')).contains(&code),
            false => category(code) == Category::DecimalNumber,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(src: &str, text: &str, collation: &str) -> String {
        let oid = rudb_pgtypes::collation(collation).map(|collation| collation.oid).unwrap();
        let args = [Value::Varchar(text.into()), Value::BigInt(i64::from(oid))];
        match call(src, &args).unwrap() {
            Some(Value::Varchar(text)) => text,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_c_collations_map_only_the_ascii_letters() {
        for collation in ["C", "POSIX", "ucs_basic"] {
            assert_eq!(case("upper", "abc é ß", collation), "ABC é ß");
            assert_eq!(case("lower", "ABC É", collation), "abc É");
            assert_eq!(case("casefold", "ABC ẞ", collation), "abc ẞ");
            assert_eq!(case("initcap", "hELLO wORLD 1abc éa", collation), "Hello World 1abc éA");
        }
    }

    #[test]
    fn c_utf8_maps_by_the_simple_mappings() {
        assert_eq!(case("upper", "abc é ß ǆ", "pg_c_utf8"), "ABC É ß Ǆ");
        assert_eq!(case("lower", "ΑΣ ẞ", "pg_c_utf8"), "ασ ß");
        assert_eq!(case("casefold", "ẞ ABC", "pg_c_utf8"), "ß abc");
        assert_eq!(
            case("initcap", "hello wORLD foo_bar 1abc ǆa", "pg_c_utf8"),
            "Hello World Foo_Bar 1abc Ǆa"
        );
        // The POSIX form of a digit leaves out the digits of other scripts.
        assert_eq!(case("initcap", "١a", "pg_c_utf8"), "١A");
    }

    #[test]
    fn pg_unicode_fast_maps_by_the_full_mappings() {
        assert_eq!(case("upper", "ß ŉ", "pg_unicode_fast"), "SS ʼN");
        assert_eq!(case("casefold", "ẞ ß", "pg_unicode_fast"), "ss ss");
        assert_eq!(case("lower", "ΑΣ ΑΣ.Α Σ", "pg_unicode_fast"), "ας ασ.α σ");
        assert_eq!(case("initcap", "ǆa ßa", "pg_unicode_fast"), "ǅa Ssa");
        assert_eq!(case("initcap", "١a", "pg_unicode_fast"), "١a");
    }

    #[test]
    fn icu_is_not_in_this_build() {
        let args = [Value::Varchar("a".into()), Value::BigInt(963)];
        let error = call("upper", &args).unwrap_err();
        assert_eq!(error.sqlstate(), Some(SqlState::FEATURE_NOT_SUPPORTED));
    }

    #[test]
    fn the_tables_agree_with_the_ascii_letters() {
        for code in 0..0x80u32 {
            let c = char::from_u32(code).unwrap();
            let row = case_of(code).unwrap();
            assert_eq!(row.1[Kind::Lower as usize], u32::from(c.to_ascii_lowercase()));
            assert_eq!(row.1[Kind::Upper as usize], u32::from(c.to_ascii_uppercase()));
            assert_eq!(is_cased(code), c.is_ascii_alphabetic());
            assert_eq!(is_alnum(code, false), c.is_ascii_alphanumeric());
        }
        assert!(in_ranges(&CASE_IGNORABLE, u32::from('\'')));
        assert_eq!(category(0x0378), Category::Unassigned);
    }
}

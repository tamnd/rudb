//! The locales of the operating system, for the checks of `CREATE DATABASE`: `check_locale` and
//! `pg_get_encoding_from_locale` of PostgreSQL.

use std::ffi::{CStr, CString};

/// The codesets of the C library and the encodings of PostgreSQL that they are,
/// `encoding_match_list` of `chklocale.c`.
const CODESETS: &[(&str, &str)] = &[
    ("EUC-JP", "EUC_JP"),
    ("eucJP", "EUC_JP"),
    ("IBM-eucJP", "EUC_JP"),
    ("sdeckanji", "EUC_JP"),
    ("CP20932", "EUC_JP"),
    ("EUC-CN", "EUC_CN"),
    ("eucCN", "EUC_CN"),
    ("IBM-eucCN", "EUC_CN"),
    ("GB2312", "EUC_CN"),
    ("dechanzi", "EUC_CN"),
    ("CP20936", "EUC_CN"),
    ("EUC-KR", "EUC_KR"),
    ("eucKR", "EUC_KR"),
    ("IBM-eucKR", "EUC_KR"),
    ("deckorean", "EUC_KR"),
    ("5601", "EUC_KR"),
    ("CP51949", "EUC_KR"),
    ("EUC-TW", "EUC_TW"),
    ("eucTW", "EUC_TW"),
    ("IBM-eucTW", "EUC_TW"),
    ("cns11643", "EUC_TW"),
    ("UTF-8", "UTF8"),
    ("utf8", "UTF8"),
    ("CP65001", "UTF8"),
    ("ISO-8859-1", "LATIN1"),
    ("ISO8859-1", "LATIN1"),
    ("iso88591", "LATIN1"),
    ("CP28591", "LATIN1"),
    ("ISO-8859-2", "LATIN2"),
    ("ISO8859-2", "LATIN2"),
    ("iso88592", "LATIN2"),
    ("CP28592", "LATIN2"),
    ("ISO-8859-3", "LATIN3"),
    ("ISO8859-3", "LATIN3"),
    ("iso88593", "LATIN3"),
    ("CP28593", "LATIN3"),
    ("ISO-8859-4", "LATIN4"),
    ("ISO8859-4", "LATIN4"),
    ("iso88594", "LATIN4"),
    ("CP28594", "LATIN4"),
    ("ISO-8859-9", "LATIN5"),
    ("ISO8859-9", "LATIN5"),
    ("iso88599", "LATIN5"),
    ("CP28599", "LATIN5"),
    ("ISO-8859-10", "LATIN6"),
    ("ISO8859-10", "LATIN6"),
    ("iso885910", "LATIN6"),
    ("ISO-8859-13", "LATIN7"),
    ("ISO8859-13", "LATIN7"),
    ("iso885913", "LATIN7"),
    ("ISO-8859-14", "LATIN8"),
    ("ISO8859-14", "LATIN8"),
    ("iso885914", "LATIN8"),
    ("ISO-8859-15", "LATIN9"),
    ("ISO8859-15", "LATIN9"),
    ("iso885915", "LATIN9"),
    ("CP28605", "LATIN9"),
    ("ISO-8859-16", "LATIN10"),
    ("ISO8859-16", "LATIN10"),
    ("iso885916", "LATIN10"),
    ("KOI8-R", "KOI8R"),
    ("CP20866", "KOI8R"),
    ("KOI8-U", "KOI8U"),
    ("CP21866", "KOI8U"),
    ("CP866", "WIN866"),
    ("CP874", "WIN874"),
    ("CP1250", "WIN1250"),
    ("CP1251", "WIN1251"),
    ("ansi-1251", "WIN1251"),
    ("CP1252", "WIN1252"),
    ("CP1253", "WIN1253"),
    ("CP1254", "WIN1254"),
    ("CP1255", "WIN1255"),
    ("CP1256", "WIN1256"),
    ("CP1257", "WIN1257"),
    ("CP1258", "WIN1258"),
    ("ISO-8859-5", "ISO_8859_5"),
    ("ISO8859-5", "ISO_8859_5"),
    ("iso88595", "ISO_8859_5"),
    ("CP28595", "ISO_8859_5"),
    ("ISO-8859-6", "ISO_8859_6"),
    ("ISO8859-6", "ISO_8859_6"),
    ("iso88596", "ISO_8859_6"),
    ("CP28596", "ISO_8859_6"),
    ("ISO-8859-7", "ISO_8859_7"),
    ("ISO8859-7", "ISO_8859_7"),
    ("iso88597", "ISO_8859_7"),
    ("CP28597", "ISO_8859_7"),
    ("ISO-8859-8", "ISO_8859_8"),
    ("ISO8859-8", "ISO_8859_8"),
    ("iso88598", "ISO_8859_8"),
    ("CP28598", "ISO_8859_8"),
    ("SJIS", "SJIS"),
    ("PCK", "SJIS"),
    ("CP932", "SJIS"),
    ("SHIFT_JIS", "SJIS"),
    ("BIG5", "BIG5"),
    ("BIG5HKSCS", "BIG5"),
    ("Big5-HKSCS", "BIG5"),
    ("CP950", "BIG5"),
    ("GBK", "GBK"),
    ("CP936", "GBK"),
    ("UHC", "UHC"),
    ("CP949", "UHC"),
    ("JOHAB", "JOHAB"),
    ("CP1361", "JOHAB"),
    ("GB18030", "GB18030"),
    ("CP54936", "GB18030"),
    ("SJIS_2004", "SHIFT_JIS_2004"),
    ("US-ASCII", "SQL_ASCII"),
];

/// The kind of locale that a check is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Category {
    Collate,
    Ctype,
}

impl Category {
    fn mask(self) -> libc::c_int {
        match self {
            Category::Collate => libc::LC_COLLATE_MASK,
            Category::Ctype => libc::LC_CTYPE_MASK,
        }
    }
}

/// The locale of the environment for an empty name, as `setlocale` finds it.
fn environment(category: Category) -> String {
    let variable = match category {
        Category::Collate => "LC_COLLATE",
        Category::Ctype => "LC_CTYPE",
    };
    ["LC_ALL", variable, "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "C".to_owned())
}

/// `check_locale`: the name of the locale when the C library knows it, `None` when it does not.
/// An empty name is the locale of the environment.
pub(crate) fn check(category: Category, name: &str) -> Option<String> {
    let name = if name.is_empty() { environment(category) } else { name.to_owned() };
    let text = CString::new(name.as_str()).ok()?;
    // SAFETY: `text` is a valid C string for the whole call, and a locale that the call gives is
    // freed once.
    unsafe {
        let locale = libc::newlocale(category.mask(), text.as_ptr(), std::ptr::null_mut());
        if locale.is_null() {
            return None;
        }
        let _ = libc::freelocale(locale);
    }
    Some(name)
}

/// What `pg_get_encoding_from_locale` finds for a locale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Codeset {
    /// The locale works with any encoding: `C`, `POSIX`, or a locale that the C library does not
    /// know.
    Any,
    /// The locale needs this encoding of PostgreSQL.
    Encoding(&'static str),
    /// The C library gives a codeset that PostgreSQL does not know. PostgreSQL warns and then
    /// allows any encoding.
    Unknown(String),
}

/// `pg_get_encoding_from_locale`: the encoding that the `LC_CTYPE` locale needs.
pub(crate) fn encoding(name: &str) -> Codeset {
    if name.eq_ignore_ascii_case("C") || name.eq_ignore_ascii_case("POSIX") {
        return Codeset::Encoding("SQL_ASCII");
    }
    let Ok(text) = CString::new(name) else {
        return Codeset::Any;
    };
    // SAFETY: `text` is a valid C string for the whole call. The locale of the thread changes
    // only between the two calls of `uselocale`, and `nl_langinfo` gives a string that stays
    // valid until the next call on the thread, which is after the copy.
    let codeset = unsafe {
        let locale = libc::newlocale(libc::LC_CTYPE_MASK, text.as_ptr(), std::ptr::null_mut());
        if locale.is_null() {
            return Codeset::Any;
        }
        let old = libc::uselocale(locale);
        let codeset = libc::nl_langinfo(libc::CODESET);
        let codeset = if codeset.is_null() {
            String::new()
        } else {
            CStr::from_ptr(codeset).to_string_lossy().into_owned()
        };
        libc::uselocale(old);
        let _ = libc::freelocale(locale);
        codeset
    };
    if let Some((_, encoding)) =
        CODESETS.iter().find(|(known, _)| known.eq_ignore_ascii_case(&codeset))
    {
        return Codeset::Encoding(encoding);
    }
    // On macOS the codeset of some locales is empty, and PostgreSQL takes them as UTF8.
    if cfg!(target_os = "macos") && codeset.is_empty() {
        return Codeset::Encoding("UTF8");
    }
    Codeset::Unknown(codeset)
}

/// True for a locale that compares text as rudb does, by bytes: `C`, `POSIX` and the `C.`
/// locales such as `C.UTF-8`.
pub(crate) fn bytewise(name: &str) -> bool {
    name.eq_ignore_ascii_case("C")
        || name.eq_ignore_ascii_case("POSIX")
        || name.get(..2).is_some_and(|head| head.eq_ignore_ascii_case("C."))
}

#[cfg(test)]
mod tests {
    use super::{Category, Codeset, bytewise, check, encoding};

    #[test]
    fn the_c_locales() {
        assert_eq!(check(Category::Collate, "C"), Some("C".to_owned()));
        assert_eq!(check(Category::Ctype, "POSIX"), Some("POSIX".to_owned()));
        assert_eq!(check(Category::Collate, "no_such_locale"), None);
        assert_eq!(encoding("C"), Codeset::Encoding("SQL_ASCII"));
        assert_eq!(encoding("posix"), Codeset::Encoding("SQL_ASCII"));
        assert_eq!(encoding("no_such_locale"), Codeset::Any);
        assert!(bytewise("C") && bytewise("c.utf8") && bytewise("POSIX"));
        assert!(!bytewise("en_US.UTF-8") && !bytewise("Ca"));
    }
}

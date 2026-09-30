//! `nfc_normalize` and `strip_accents`.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn rows(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect()
}

fn answered(sql: &str) -> String {
    rows(&Database::new(), sql).join("\n")
}

fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

#[test]
fn a_string_is_put_in_normal_form_c() {
    let cases = [
        (
            "SELECT nfc_normalize('e' || chr(769)) = chr(233), length(nfc_normalize('e' || chr(769))), nfc_normalize('abc'), nfc_normalize(NULL), typeof(nfc_normalize('x'))",
            "true|1|abc|NULL|VARCHAR",
        ),
        // The marks are put in canonical order before the letter composes with both.
        (
            "SELECT unicode(nfc_normalize('a' || chr(770) || chr(803))), unicode(nfc_normalize('a' || chr(803) || chr(770)))",
            "7853|7853",
        ),
        (
            "SELECT unicode(nfc_normalize(chr(4352) || chr(4449) || chr(4520))), length(nfc_normalize(chr(4352) || chr(4449) || chr(4520)))",
            "44033|1",
        ),
        // utf8proc takes U+11A7, which is not a trailing consonant, into the syllable before it.
        (
            "SELECT length(nfc_normalize(chr(44032) || chr(4519))), unicode(nfc_normalize(chr(44032) || chr(4519)))",
            "1|44032",
        ),
        // U+0958 is a composition exclusion, so it stays decomposed.
        ("SELECT length(nfc_normalize(chr(2392))), unicode(nfc_normalize(chr(2392)))", "2|2325"),
        (
            "SELECT length(nfc_normalize(chr(8491))), unicode(nfc_normalize(chr(8491))), length(nfc_normalize(chr(8486))), unicode(nfc_normalize(chr(8486)))",
            "1|197|1|937",
        ),
        (
            "SELECT length(nfc_normalize('q' || chr(775) || chr(803))), unicode(nfc_normalize(chr(7777) || chr(803)))",
            "3|7785",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
}

#[test]
fn accents_are_stripped_and_other_letters_are_kept() {
    let cases = [
        (
            "SELECT strip_accents('Mühleisen'), strip_accents('éèêë ÀÁÂÃÄÅ çñ'), strip_accents('abc'), strip_accents(NULL)",
            "Muhleisen|eeee AAAAAA cn|abc|NULL",
        ),
        (
            "SELECT strip_accents('Ångström São Paulo Kraków Đà Nẵng'), strip_accents('ﬁ œ ß ø ł')",
            "Angstrom Sao Paulo Krakow Đa Nang|ﬁ œ ß ø ł",
        ),
        (
            "SELECT strip_accents('日本語 한국어 Ελληνικά'), nfc_normalize('日本語 한국어'), length(strip_accents('ά'))",
            "日本語 한국어 Ελληνικα|日本語 한국어|1",
        ),
        (
            "SELECT length(strip_accents(chr(836))), length(strip_accents(chr(3635))), length(strip_accents(chr(2381)))",
            "0|1|0",
        ),
        (
            "SELECT strip_accents('😀👍🏽'), length(nfc_normalize('👍🏽')), strip_accents('ǅ ǈ ǆ'), strip_accents('ẞ Ǆ')",
            "😀👍🏽|2|ǅ ǈ ǆ|ẞ Ǆ",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
    assert_eq!(
        rows(
            &Database::new(),
            "SELECT nfc_normalize(s), strip_accents(s) FROM (VALUES ('café'), (NULL), ('naïve'), ('')) t(s)"
        ),
        ["café|cafe", "NULL|NULL", "naïve|naive", "|"]
    );
}

#[test]
fn a_call_that_is_not_one_string_is_refused_in_the_pins_words() {
    let candidates = "You might need to add explicit type casts.\n\tCandidate functions:\n\tnfc_normalize(col0 VARCHAR) -> VARCHAR\n";
    assert_eq!(
        refused("SELECT nfc_normalize()"),
        format!(
            "Binder Error: No function matches the given name and argument types 'nfc_normalize()'. {candidates}"
        )
    );
}

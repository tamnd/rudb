//! The character classes and the case mapping of the collation of a pattern, which are the
//! `ctype_methods` of the locale that `pg_set_regex_collation` picks.
//!
//! PostgreSQL asks the collation of the call what each class holds and what the other case of a
//! character is. The classes `ascii`, `blank`, `cntrl` and `xdigit` are the same in every collation,
//! as `cclasscvec` in `regc_locale.c` hardwires them, and the others come from a [`PgCtype`]. A
//! collation whose `ctype` is `C` keeps the classes and the case mapping to ASCII, which is
//! [`AsciiCtype`]. The tables of Unicode are in `rudb-kernels`, which gives the builtin collations
//! their own.

/// A class of characters whose members depend on the collation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgClass {
    Alnum,
    Alpha,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    /// `[[:alnum:]_]`, which `\w` is and which the word boundaries look at.
    Word,
}

/// What a collation says about the characters of a pattern.
pub trait PgCtype: Sync {
    /// The characters of the class, as ranges that are sorted and inclusive at both ends.
    fn class(&self, class: PgClass) -> &[(char, char)];
    /// The lower case of a character, or the character itself.
    fn lower(&self, ch: char) -> char;
    /// The upper case of a character, or the character itself.
    fn upper(&self, ch: char) -> char;
}

/// The classes and the case mapping of a collation whose `ctype` is `C`, which hold only ASCII
/// characters, as `pg_char_properties` gives them.
#[derive(Debug, Clone, Copy, Default)]
pub struct AsciiCtype;

impl PgCtype for AsciiCtype {
    fn class(&self, class: PgClass) -> &[(char, char)] {
        match class {
            PgClass::Alnum => &[('0', '9'), ('A', 'Z'), ('a', 'z')],
            PgClass::Alpha => &[('A', 'Z'), ('a', 'z')],
            PgClass::Digit => &[('0', '9')],
            PgClass::Graph => &[('!', '~')],
            PgClass::Lower => &[('a', 'z')],
            PgClass::Print => &[(' ', '~')],
            PgClass::Punct => &[('!', '/'), (':', '@'), ('[', '`'), ('{', '~')],
            PgClass::Space => &[('\t', '\r'), (' ', ' ')],
            PgClass::Upper => &[('A', 'Z')],
            PgClass::Word => &[('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')],
        }
    }

    fn lower(&self, ch: char) -> char {
        ch.to_ascii_lowercase()
    }

    fn upper(&self, ch: char) -> char {
        ch.to_ascii_uppercase()
    }
}

//! PostgreSQL's flavour of regular expression, which is Henry Spencer's engine as Tcl and
//! PostgreSQL ship it.
//!
//! The syntax is close to RE2's and the answers are not. Spencer's engine finds the leftmost match
//! and then the longest one from there, or the shortest where the pattern says so, and only then
//! works out where the groups are. The group answer comes from a tree the parser builds beside the
//! automaton, and the shape of that tree is the semantics: `(a*)+` against `aaa` captures the empty
//! string, because the parser turns `x+` into `x*x` and gives the last `x` everything that is left.
//! A leftmost first machine cannot give these answers, so this module ports the parts that decide
//! them, which are the lexer of `regc_lex.c`, the tree building of `parse`, `parsebranch` and
//! `parseqatom` in `regcomp.c`, and `find` and the dissect functions of `regexec.c`, and runs them
//! over the machines of this crate rather than over Spencer's DFA.
//!
//! Each node of the tree is compiled to a program of its own with no groups in it. The program
//! answers the one question the dissect functions ask, which is how far a node can match from a
//! position, by stepping the set of instructions it could be at, which is the DFA's answer reached
//! without building the DFA.
//!
//! Back references are not here yet, and a pattern that has one is refused rather than run with a
//! different answer.
//!
//! PostgreSQL also refuses a pattern whose automaton grows past a budget of states and arcs, with
//! "regular expression is too complex". There is no such automaton here, so the budget cannot be
//! matched exactly. The parser keeps PostgreSQL's limit on nesting depth, and a pattern that is
//! huge but shallow is run where PostgreSQL would refuse it. A sweep of 3500 random patterns
//! against PostgreSQL 19 found six of these and no other difference.

mod ctype;
mod lex;
mod run;
mod tree;

use std::sync::Arc;

use rudb_common::{Error, Result, SqlState};

use crate::compile::Set;
use crate::parse::Class;

pub use ctype::{AsciiCtype, PgClass, PgCtype};
pub(crate) use run::{Tree, find};

/// The syntax and matching flags, as PostgreSQL's `cflags` holds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cflags {
    /// `REG_EXTENDED`: the syntax is ERE, or ARE with `advf`, rather than BRE.
    pub(crate) extended: bool,
    /// `REG_ADVF`: the extensions of ARE on top of ERE.
    pub(crate) advf: bool,
    /// `REG_QUOTE`: the pattern is the characters it is made of.
    pub(crate) quote: bool,
    pub(crate) icase: bool,
    /// `REG_NLSTOP`: a dot and a negated bracket do not match a newline.
    pub(crate) nlstop: bool,
    /// `REG_NLANCH`: `^` and `$` also match after and before a newline.
    pub(crate) nlanch: bool,
    /// `REG_EXPANDED`: white space and `#` comments in the pattern are skipped.
    pub(crate) expanded: bool,
}

impl Cflags {
    /// `REG_ADVANCED`, which is what every SQL function starts from.
    const ADVANCED: Self = Self {
        extended: true,
        advf: true,
        quote: false,
        icase: false,
        nlstop: false,
        nlanch: false,
        expanded: false,
    };
}

/// The flags of a PostgreSQL regular expression, read from the letters a function takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PgFlags {
    pub(crate) cflags: Cflags,
    global: bool,
}

impl Default for PgFlags {
    fn default() -> Self {
        Self { cflags: Cflags::ADVANCED, global: false }
    }
}

impl PgFlags {
    /// Reads the letters, which is `parse_re_flags` in `regexp.c`.
    ///
    /// `e` clears the ERE bit it means to set, because it clears all of `REG_ADVANCED` after
    /// setting `REG_EXTENDED`, so the letter gives BRE syntax. The same letter inside the pattern
    /// gives ERE. Both are what PostgreSQL does.
    ///
    /// # Errors
    ///
    /// On a letter that is not a flag, with PostgreSQL's message.
    pub fn parse(letters: &str) -> Result<Self> {
        let mut flags = Self::default();
        let c = &mut flags.cflags;
        for letter in letters.chars() {
            match letter {
                'g' => flags.global = true,
                'b' => {
                    c.extended = false;
                    c.advf = false;
                    c.quote = false;
                }
                'c' => c.icase = false,
                'e' => {
                    c.extended = false;
                    c.advf = false;
                    c.quote = false;
                }
                'i' => c.icase = true,
                'm' | 'n' => {
                    c.nlstop = true;
                    c.nlanch = true;
                }
                'p' => {
                    c.nlstop = true;
                    c.nlanch = false;
                }
                'q' => {
                    c.quote = true;
                    c.extended = false;
                    c.advf = false;
                }
                's' => {
                    c.nlstop = false;
                    c.nlanch = false;
                }
                't' => c.expanded = false,
                'w' => {
                    c.nlstop = false;
                    c.nlanch = true;
                }
                'x' => c.expanded = true,
                other => {
                    return Err(Error::invalid_input(format!(
                        "invalid regular expression option: \"{other}\""
                    ))
                    .state(SqlState::INVALID_PARAMETER_VALUE));
                }
            }
        }
        Ok(flags)
    }

    /// The same flags with case folded, which is what `~*` adds to `~`.
    #[must_use]
    pub fn case_insensitive(mut self) -> Self {
        self.cflags.icase = true;
        self
    }

    /// Whether `g` was given, which is about how often to match rather than how.
    #[must_use]
    pub fn is_global(&self) -> bool {
        self.global
    }
}

/// The ways a pattern can be refused, which are the `REG_*` codes of `regex.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Code {
    BadPat,
    Collate,
    Ctype,
    Escape,
    Subreg,
    Brack,
    Paren,
    Brace,
    BadBr,
    Range,
    BadRpt,
    BadOpt,
    TooBig,
    Assert,
    /// Not a code of PostgreSQL's: a back reference, which this engine does not run yet.
    Backref,
}

impl Code {
    /// The error PostgreSQL raises, with the wording of `regerrs.h`.
    pub(crate) fn error(self) -> Error {
        let message = match self {
            Self::BadPat => "invalid regexp (reg version 0.8)",
            Self::Collate => "invalid collating element",
            Self::Ctype => "invalid character class",
            Self::Escape => "invalid escape \\ sequence",
            Self::Subreg => "invalid backreference number",
            Self::Brack => "brackets [] not balanced",
            Self::Paren => "parentheses () not balanced",
            Self::Brace => "braces {} not balanced",
            Self::BadBr => "invalid repetition count(s)",
            Self::Range => "invalid character range",
            Self::BadRpt => "quantifier operand invalid",
            Self::BadOpt => "invalid embedded option",
            Self::TooBig => "regular expression is too complex",
            Self::Assert => "\"can't happen\" -- you found a bug",
            Self::Backref => {
                return Error::invalid_input(
                    "back references in regular expressions are not supported",
                )
                .state(SqlState::FEATURE_NOT_SUPPORTED);
            }
        };
        Error::invalid_input(format!("invalid regular expression: {message}"))
            .state(SqlState::INVALID_REGULAR_EXPRESSION)
    }
}

/// Compiles a pattern with the classes and the case mapping of a collation.
///
/// # Errors
///
/// On a pattern PostgreSQL refuses, with PostgreSQL's message.
pub(crate) fn compile(pattern: &str, flags: PgFlags, ctype: &dyn PgCtype) -> Result<Tree> {
    let (node, groups) = tree::parse(pattern, flags.cflags, ctype).map_err(Code::error)?;
    // `\m`, `\M`, `\y` and `\Y` look at `[[:alnum:]_]` of the collation, as `wordchrs` does.
    let word = Set::new(&Class::of(ctype.class(PgClass::Word)));
    run::build(&node, groups, &Arc::new(word)).map_err(|_| Code::TooBig.error())
}

#[cfg(test)]
mod tests {
    use crate::{AsciiCtype, PgFlags, Regex};

    /// What `regexp_match` gives in the collation `C`: the groups, or the whole match for a
    /// pattern with none, with `None` for a group that took no part.
    fn groups(pattern: &str, text: &str, flags: &str) -> Option<Vec<Option<String>>> {
        let flags = PgFlags::parse(flags).expect("flags");
        let regex = Regex::postgres(pattern, flags, &AsciiCtype).expect("compiles");
        let found = regex.find_at(text, 0)?;
        let piece = |index| found.group(index).map(|(from, to)| text[from..to].to_string());
        if regex.groups() == 0 {
            return Some(vec![piece(0)]);
        }
        Some((1..=regex.groups()).map(piece).collect())
    }

    fn one(pattern: &str, text: &str, flags: &str) -> Vec<Option<String>> {
        groups(pattern, text, flags).expect("matches")
    }

    fn some(values: &[&str]) -> Vec<Option<String>> {
        values.iter().map(|value| Some((*value).to_string())).collect()
    }

    fn message(pattern: &str, flags: &str) -> String {
        let error = PgFlags::parse(flags)
            .and_then(|flags| Regex::postgres(pattern, flags, &AsciiCtype))
            .expect_err("refused");
        error.message().to_string()
    }

    /// Every answer here was read off PostgreSQL 19.
    #[test]
    fn a_plus_gives_its_last_iteration_what_is_left() {
        assert_eq!(one("(a*)+", "aaa", ""), some(&[""]));
        assert_eq!(one("(a*)*", "aaa", ""), some(&["aaa"]));
        assert_eq!(one("(a|)+b", "aab", ""), some(&[""]));
        assert_eq!(one("((a)|b)+", "ab", ""), vec![Some("b".into()), None]);
        assert_eq!(one("((a)|b)+", "ba", ""), some(&["a", "a"]));
        assert_eq!(one("(a{2,3})+", "aaaaaaa", ""), some(&["aa"]));
    }

    #[test]
    fn the_match_is_the_leftmost_and_then_the_longest() {
        assert_eq!(one("(a|ab)(c|bcd)(d*)", "abcd", ""), some(&["ab", "c", "d"]));
        assert_eq!(one("(foo|foobar)(bar)?", "foobar", ""), vec![Some("foobar".into()), None]);
        assert_eq!(one("(ab|a)(bc|c)", "abc", ""), some(&["ab", "c"]));
        assert_eq!(one("(a|b|c)+", "abc", ""), some(&["c"]));
        assert_eq!(one("(.*)(\\d+)", "abc123", ""), some(&["abc12", "3"]));
    }

    #[test]
    fn a_lazy_quantifier_makes_its_part_short_and_can_make_the_whole_match_short() {
        assert_eq!(one("(.*?)(\\d+)", "abc123", ""), some(&["abc", "1"]));
        assert_eq!(one("(.*?)(\\d+)$", "abc123", ""), some(&["abc", "123"]));
        assert_eq!(one("x*a*?(x*)", "xx", ""), some(&[""]));
        assert_eq!(one("a(b*?)", "abbb", ""), some(&[""]));
        assert_eq!(one("a(b*?)c", "abbbc", ""), some(&["bbb"]));
        assert_eq!(one("(a|b|c)+?", "abc", ""), some(&["a"]));
        assert_eq!(one("(a.*?)(b.*?)$", "abab", ""), some(&["a", "bab"]));
    }

    #[test]
    fn the_constraints_are_postgres_ones() {
        assert_eq!(one("(\\y\\w+\\y)", "  foo bar", ""), some(&["foo"]));
        assert_eq!(one("(\\mfo)", "afo fo", ""), some(&["fo"]));
        assert_eq!(one("(?<=a)(b)", "cbab", ""), some(&["b"]));
        assert_eq!(one("(?!foo)(f)", "foo fa", ""), some(&["f"]));
        assert_eq!(one("(?<=(a))b", "ab", ""), some(&["b"]), "a group in a lookaround is not one");
        assert_eq!(one("(^b)", "a\nb", "n"), some(&["b"]));
        assert_eq!(groups("(.)", "\n", "n"), None);
        assert_eq!(one("(.)", "\n", ""), some(&["\n"]));
    }

    #[test]
    fn the_flags_and_the_directors_choose_the_syntax() {
        assert_eq!(one("a+", "a+", "e"), some(&["a+"]), "the e flag gives BRE");
        assert_eq!(one("a\\{2\\}", "xaa", "e"), some(&["aa"]));
        assert_eq!(one("(?e)a+", "xaa", ""), some(&["aa"]), "and the e option gives ERE");
        assert_eq!(one("\\(a\\)", "x(a)", "b"), some(&["a"]));
        assert_eq!(one("a+", "a+", "q"), some(&["a+"]));
        assert_eq!(one("***=a+", "a+", ""), some(&["a+"]));
        assert_eq!(one("(a b)", "ab", "x"), some(&["ab"]));
    }

    #[test]
    fn the_collation_c_keeps_the_case_folding_to_ascii() {
        assert_eq!(one("([a-z]+)", "ABC", "i"), some(&["ABC"]));
        assert_eq!(one("([A-Z]+)", "abc", "i"), some(&["abc"]));
        assert_eq!(groups("\u{e9}", "\u{c9}", "i"), None);
        assert_eq!(one("([[:lower:]]+)", "aB\u{e9}", "i"), some(&["aB"]));
    }

    #[test]
    fn the_collation_c_keeps_the_classes_to_ascii() {
        assert_eq!(one("([[:alpha:]]+)", "ab\u{e9}", ""), some(&["ab"]));
        assert_eq!(one("([^[:alpha:]]+)", "\u{e9}\u{663}a", ""), some(&["\u{e9}\u{663}"]));
        assert_eq!(groups("\\w", "\u{e9}", ""), None);
        assert_eq!(groups("\\s", "\u{a0}", ""), None);
        assert_eq!(one("([^\\W]+)", ",.cd", ""), some(&["cd"]));
        assert_eq!(one("([[:cntrl:]]+)", "a\u{85}", ""), some(&["\u{85}"]));
        // The word boundaries look at the same word characters.
        assert_eq!(one("(\\mb)", "\u{e9}b", ""), some(&["b"]));
    }

    #[test]
    fn the_escapes_and_the_names_read_as_postgres_reads_them() {
        assert_eq!(one("\\x41", "A", ""), some(&["A"]));
        assert_eq!(one("\\101", "A", ""), some(&["A"]));
        assert_eq!(groups("\\0101", "A1", ""), None);
        assert_eq!(one("[[.space.]]", " ", ""), some(&[" "]));
        assert_eq!(one("[[=a=]]", "a", ""), some(&["a"]));
    }

    #[test]
    fn a_broken_pattern_is_refused_with_the_postgres_message() {
        let cases = [
            ("(", "parentheses () not balanced"),
            ("a)", "parentheses () not balanced"),
            ("a{2", "braces {} not balanced"),
            ("a{3,2}", "invalid repetition count(s)"),
            ("a{256}", "invalid repetition count(s)"),
            ("*a", "quantifier operand invalid"),
            ("a**", "quantifier operand invalid"),
            ("[a", "brackets [] not balanced"),
            ("[z-a]", "invalid character range"),
            ("[[:foo:]]", "invalid character class"),
            ("[[.foo.]]", "invalid collating element"),
            ("\\x", "invalid escape \\ sequence"),
            ("\\u12", "invalid escape \\ sequence"),
            ("\\1(a)", "invalid backreference number"),
            ("(?z)a", "invalid embedded option"),
            ("***?a", "invalid regexp (reg version 0.8)"),
            ("***xa", "quantifier operand invalid"),
        ];
        for (pattern, wanted) in cases {
            assert_eq!(
                message(pattern, ""),
                format!("invalid regular expression: {wanted}"),
                "{pattern}"
            );
        }
        assert_eq!(message("a", "z"), "invalid regular expression option: \"z\"");
        assert_eq!(
            message("(a)\\1", ""),
            "back references in regular expressions are not supported"
        );
    }
}

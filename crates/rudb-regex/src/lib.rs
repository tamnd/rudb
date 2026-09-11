//! A regular expression engine, written rather than depended on.
//!
//! Rank 1 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! Rank 1 and not higher for the same reason the codecs are there: this turns text into positions
//! and knows nothing about a vector, a column or a type. The kernel that wants it is at rank 3 and
//! everything in between is free to not know this crate exists.
//!
//! # Why this is written and not a dependency
//!
//! `spec/18-package-layout.md` says the published workspace has zero external dependencies, and the
//! reason is not compile times. An embedded database is a thing other people's binaries contain, so
//! every crate here is a crate their licence audit has to clear and their security team has to
//! account for. The other reason is specific to this one: DuckDB links RE2, and being compatible
//! with DuckDB means being compatible with RE2's syntax, RE2's leftmost first semantics and RE2's
//! error messages. A general purpose regular expression crate would have its own answers to all
//! three, and reconciling them is more work than writing the engine.
//!
//! # What is here
//!
//! A parser in `parse`, a compiler in `compile` and Pike's virtual machine in `vm`. The
//! machine runs every possibility at once rather than backtracking, so a match costs the length of
//! the text times the size of the program and no pattern can be made to take exponential time. That
//! matters in a database, where the pattern can come out of the data.
//!
//! The syntax is RE2's, less the parts of it nothing in SQL reaches: literals, `.`, character sets
//! with ranges, negation, the Perl classes and the POSIX names, the anchors `^`, `$`, `\A`, `\z`,
//! `\b` and `\B`, groups both capturing and not, named groups with the name read and dropped,
//! alternation, the three repetition operators, counted repetitions, laziness, and the inline flags
//! `(?i)`, `(?s)`, `(?m)` and `(?U)`.
//!
//! # Which answer is the right one
//!
//! Leftmost first, which is Perl's rule and RE2's default and therefore DuckDB's. `a|ab` against
//! `ab` matches `a`, because the first branch of an alternation wins where both could match. POSIX
//! asks for the longest instead and this engine deliberately does not do that.
//!
//! Every behaviour below was read off the DuckDB binary on server3 before it was written here,
//! including the option letters, what each one does, the exact error text for six kinds of broken
//! pattern, and what happens to a replacement that asks for a group the pattern does not have.
//!
//! # What is not here
//!
//! No DFA and no literal prefilter. RE2 has both and they are most of why it is fast: a search for a
//! pattern that must start with `https` should find the `h` with a memory scan rather than by
//! stepping the machine over every character. The machine is the correct answer to measure the fast
//! paths against, and it is what the kernel calls today, so the ClickBench number it produces is a
//! real number to improve on rather than an estimate.
//!
//! No `\p{...}` Unicode classes, so `\w` and the POSIX names are ASCII, which is what RE2 does by
//! default anyway. Case folding covers ASCII and every character whose fold is a single character,
//! which leaves out a handful of letters that fold to two.

#![deny(unsafe_code)]

mod compile;
mod parse;
mod vm;

use rudb_common::{Error, Result};

use crate::compile::Program;

/// The letters DuckDB accepts after the pattern.
///
/// The set is `c`, `i`, `l`, `m`, `n`, `p`, `s` and `g`, and anything else is refused with DuckDB's
/// own message. `m`, `n` and `p` are one option under three spellings: in RE2 they set `never_nl`,
/// and what that turns out to mean, measured rather than read, is that a dot stops matching a
/// newline again. `s` and `m` therefore cancel each other and the last one written wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// `i`, and `c` turns it back off.
    pub case_insensitive: bool,
    /// `s`, and `m`, `n` or `p` turn it back off.
    pub dot_matches_newline: bool,
    /// `l`, which takes the pattern as the characters it is made of.
    pub literal: bool,
    /// `g`, which is about the replacement rather than about the match.
    pub global: bool,
}

impl Options {
    /// Reads the option string.
    ///
    /// # Errors
    ///
    /// On a letter that is not one of the eight, with DuckDB's message.
    pub fn parse(spelling: &str) -> Result<Self> {
        let mut options = Self::default();
        for letter in spelling.chars() {
            match letter {
                'c' => options.case_insensitive = false,
                'i' => options.case_insensitive = true,
                'l' => options.literal = true,
                'm' | 'n' | 'p' => options.dot_matches_newline = false,
                's' => options.dot_matches_newline = true,
                'g' => options.global = true,
                other => {
                    return Err(Error::invalid_input(format!("Unrecognized Regex option {other}")));
                }
            }
        }
        Ok(options)
    }
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub struct Regex {
    program: Program,
}

impl Regex {
    /// Compiles a pattern with no options set.
    ///
    /// # Errors
    ///
    /// On a pattern RE2 would refuse, with RE2's message.
    pub fn new(pattern: &str) -> Result<Self> {
        Self::with_options(pattern, Options::default())
    }

    /// Compiles a pattern.
    ///
    /// # Errors
    ///
    /// On a pattern RE2 would refuse, with RE2's message.
    pub fn with_options(pattern: &str, options: Options) -> Result<Self> {
        let (ast, groups) = if options.literal {
            (parse::literal(pattern, options.case_insensitive), 0)
        } else {
            parse::parse(pattern, options.case_insensitive, options.dot_matches_newline)?
        };
        Ok(Self { program: compile::compile(&ast, groups)? })
    }

    /// How many capturing groups the pattern has, not counting the whole match.
    #[must_use]
    pub fn groups(&self) -> usize {
        self.program.groups
    }

    /// The first match at or after `start`.
    ///
    /// `start` is a byte offset and has to be one a character begins at. The anchors are still about
    /// the text and not about `start`, so a pattern with `^` in it matches nothing here unless
    /// `start` is zero, which is what makes a global replacement of an anchored pattern replace once.
    #[must_use]
    pub fn find_at(&self, text: &str, start: usize) -> Option<Captures> {
        vm::search(&self.program, text, start, false).map(|slots| Captures { slots })
    }

    /// Whether the pattern matches anywhere in the text, which is `regexp_matches`.
    #[must_use]
    pub fn is_match(&self, text: &str) -> bool {
        vm::search(&self.program, text, 0, false).is_some()
    }

    /// Whether the pattern matches the whole text, which is `regexp_full_match`.
    #[must_use]
    pub fn is_full_match(&self, text: &str) -> bool {
        vm::search(&self.program, text, 0, true).is_some()
    }

    /// The text of one group of the first match, which is `regexp_extract`.
    ///
    /// Group zero is the whole match. A group the pattern does not have, a group that took no part
    /// in the match and a pattern that did not match all come back as `None`, which the caller turns
    /// into the empty string DuckDB answers with.
    #[must_use]
    pub fn extract<'a>(&self, text: &'a str, group: usize) -> Option<&'a str> {
        let found = self.find_at(text, 0)?;
        let (from, to) = found.group(group)?;
        text.get(from..to)
    }

    /// Replaces the first match, or every match when `global` is set.
    ///
    /// The replacement is RE2's rewrite syntax: `\0` is the whole match, `\1` to `\9` are the
    /// groups, `\\` is a backslash, and everything else is itself. A rewrite that asks for a group
    /// the pattern does not have, or that carries an escape that is not one of those, makes the
    /// whole call give the text back unchanged, which is what RE2 does by returning false and what
    /// DuckDB then prints.
    #[must_use]
    pub fn replace(&self, text: &str, rewrite: &str, global: bool) -> String {
        let Some(rewrite) = Rewrite::parse(rewrite, self.groups()) else {
            return text.to_string();
        };
        if !global {
            let Some(found) = self.find_at(text, 0) else {
                return text.to_string();
            };
            let mut out = String::with_capacity(text.len());
            out.push_str(&text[..found.start()]);
            rewrite.apply(&mut out, text, &found);
            out.push_str(&text[found.end()..]);
            return out;
        }
        self.replace_all(text, &rewrite)
    }

    /// Every match replaced, which is RE2's global replacement down to how it steps over an empty
    /// one.
    ///
    /// An empty match is the awkward case and it is worth writing out. `regexp_replace('aaa', '',
    /// '-', 'g')` is `-a-a-a-` in DuckDB: the machine matches the empty string before every
    /// character and once at the end, and the rule that keeps it from matching the empty string
    /// twice in the same place is that an empty match where the last one ended is skipped over.
    fn replace_all(&self, text: &str, rewrite: &Rewrite) -> String {
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        let mut last_end: Option<usize> = None;
        while at <= text.len() {
            let Some(found) = self.find_at(text, at) else {
                break;
            };
            if at < found.start() {
                out.push_str(&text[at..found.start()]);
            }
            if found.start() == found.end() && last_end == Some(found.start()) {
                let Some(ch) = text[at..].chars().next() else {
                    break;
                };
                out.push(ch);
                at += ch.len_utf8();
                continue;
            }
            rewrite.apply(&mut out, text, &found);
            at = found.end();
            last_end = Some(at);
        }
        out.push_str(&text[at..]);
        out
    }
}

/// Where a match and its groups are, as byte offsets into the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captures {
    slots: Vec<Option<usize>>,
}

impl Captures {
    /// Where the whole match begins.
    #[must_use]
    pub fn start(&self) -> usize {
        self.slots.first().copied().flatten().unwrap_or(0)
    }

    /// Where the whole match ends.
    #[must_use]
    pub fn end(&self) -> usize {
        self.slots.get(1).copied().flatten().unwrap_or(0)
    }

    /// Where a group begins and ends, or `None` if it took no part in the match.
    #[must_use]
    pub fn group(&self, index: usize) -> Option<(usize, usize)> {
        let from = self.slots.get(index * 2).copied().flatten()?;
        let to = self.slots.get(index * 2 + 1).copied().flatten()?;
        Some((from, to))
    }
}

/// A replacement string, taken apart once rather than once per row.
#[derive(Debug, Clone)]
struct Rewrite {
    pieces: Vec<Piece>,
}

#[derive(Debug, Clone)]
enum Piece {
    Text(String),
    Group(usize),
}

impl Rewrite {
    /// Reads a replacement, or `None` if RE2 would refuse it.
    fn parse(rewrite: &str, groups: usize) -> Option<Self> {
        let mut pieces = Vec::new();
        let mut text = String::new();
        let mut chars = rewrite.chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                text.push(ch);
                continue;
            }
            match chars.next() {
                Some('\\') => text.push('\\'),
                Some(digit) if digit.is_ascii_digit() => {
                    let index = digit as usize - '0' as usize;
                    if index > groups {
                        return None;
                    }
                    if !text.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut text)));
                    }
                    pieces.push(Piece::Group(index));
                }
                // A backslash in front of anything else, and a trailing backslash, are both
                // rewrites RE2 will not run.
                _ => return None,
            }
        }
        if !text.is_empty() {
            pieces.push(Piece::Text(text));
        }
        Some(Self { pieces })
    }

    fn apply(&self, out: &mut String, text: &str, found: &Captures) {
        for piece in &self.pieces {
            match piece {
                Piece::Text(held) => out.push_str(held),
                Piece::Group(index) => {
                    if let Some((from, to)) = found.group(*index) {
                        out.push_str(&text[from..to]);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(pattern: &str, text: &str) -> Option<(usize, usize)> {
        let regex = Regex::new(pattern).expect("compiles");
        regex.find_at(text, 0).map(|found| (found.start(), found.end()))
    }

    fn replace(text: &str, pattern: &str, rewrite: &str, options: &str) -> String {
        let options = Options::parse(options).expect("options");
        let regex = Regex::with_options(pattern, options).expect("compiles");
        regex.replace(text, rewrite, options.global)
    }

    #[test]
    fn a_literal_is_found_where_it_is() {
        assert_eq!(find("bc", "abcd"), Some((1, 3)));
        assert_eq!(find("zz", "abcd"), None);
    }

    /// Leftmost first rather than leftmost longest, which is the one semantic choice in the engine
    /// that a user can see from the outside.
    #[test]
    fn the_first_branch_wins_rather_than_the_longest() {
        assert_eq!(find("a|ab", "ab"), Some((0, 1)));
        assert_eq!(find("ab|a", "ab"), Some((0, 2)));
    }

    #[test]
    fn a_greedy_repetition_takes_what_it_can_and_a_lazy_one_does_not() {
        assert_eq!(find("a+", "aaab"), Some((0, 3)));
        assert_eq!(find("a+?", "aaab"), Some((0, 1)));
        assert_eq!(find("<.*>", "<a><b>"), Some((0, 6)));
        assert_eq!(find("<.*?>", "<a><b>"), Some((0, 3)));
    }

    #[test]
    fn the_anchors_are_about_the_text_and_not_about_the_line() {
        assert!(Regex::new("^a").expect("compiles").is_match("ab"));
        assert!(!Regex::new("^b").expect("compiles").is_match("ab"));
        // RE2 is not Perl here: a dollar does not match in front of a trailing newline.
        assert!(!Regex::new("a$").expect("compiles").is_match("a\n"));
        assert!(Regex::new("(?m)^b").expect("compiles").is_match("a\nb"));
    }

    #[test]
    fn a_word_boundary_is_where_a_word_character_meets_something_else() {
        assert!(Regex::new("\\babc\\b").expect("compiles").is_match("abc"));
        assert!(!Regex::new("\\babc\\b").expect("compiles").is_match("xabcx"));
        assert!(Regex::new("\\Babc").expect("compiles").is_match("xabc"));
    }

    #[test]
    fn a_dot_stops_at_a_newline_unless_it_is_told_not_to() {
        assert!(!Regex::new("a.b").expect("compiles").is_match("a\nb"));
        let dotted = Options { dot_matches_newline: true, ..Options::default() };
        assert!(Regex::with_options("a.b", dotted).expect("compiles").is_match("a\nb"));
        assert!(Regex::new("(?s)a.b").expect("compiles").is_match("a\nb"));
    }

    #[test]
    fn a_group_records_where_it_matched() {
        let regex = Regex::new("([a-z]+)([0-9]+)").expect("compiles");
        let found = regex.find_at("xx abc123 yy", 0).expect("matches");
        assert_eq!(found.group(0), Some((3, 9)));
        assert_eq!(found.group(1), Some((3, 6)));
        assert_eq!(found.group(2), Some((6, 9)));
        assert_eq!(found.group(3), None);
    }

    #[test]
    fn a_group_that_took_no_part_has_no_position() {
        let regex = Regex::new("(a)|(b)").expect("compiles");
        let found = regex.find_at("b", 0).expect("matches");
        assert_eq!(found.group(1), None);
        assert_eq!(found.group(2), Some((0, 1)));
    }

    #[test]
    fn a_full_match_has_to_reach_the_end() {
        let regex = Regex::new("a.c").expect("compiles");
        assert!(regex.is_full_match("abc"));
        assert!(!regex.is_full_match("abcd"));
        assert!(!Regex::new("a").expect("compiles").is_full_match("abc"));
        // The shorter branch matches first and the longer one still has to be tried, which is the
        // reason a whole text match is a flag on the machine rather than a check on the answer.
        assert!(Regex::new("a|ab").expect("compiles").is_full_match("ab"));
    }

    /// The query that this engine exists for, with the pattern and the replacement exactly as
    /// ClickBench writes them.
    #[test]
    fn clickbench_query_twenty_nine_takes_the_host_out_of_a_url() {
        let pattern = "^https?://(?:www\\.)?([^/]+)/.*$";
        let regex = Regex::new(pattern).expect("compiles");
        assert_eq!(regex.replace("http://www.example.com/a/b", "\\1", false), "example.com");
        assert_eq!(regex.replace("https://example.com/", "\\1", false), "example.com");
        assert_eq!(
            regex.replace("http://example.com", "\\1", false),
            "http://example.com",
            "no trailing slash is no match, so the referer comes back whole"
        );
        assert!(regex.program.anchored, "the search starts once rather than once per character");
    }

    /// Every one of these was read off DuckDB on server3 before it was written here.
    #[test]
    fn the_replacements_are_the_ones_duckdb_prints() {
        assert_eq!(replace("aXbXc", "X", "-", ""), "a-bXc", "the first one, not all of them");
        assert_eq!(replace("aXbXc", "X", "-", "g"), "a-b-c");
        assert_eq!(replace("abc", "z", "-", ""), "abc");
        assert_eq!(replace("abc", "(a)(b)", "\\0|\\2\\1", ""), "ab|bac");
        assert_eq!(replace("abc", "b", "&", ""), "a&c", "an ampersand is a character here");
        assert_eq!(replace("ABC", "b", "x", "i"), "AxC");
        assert_eq!(replace("aaa", "", "-", "g"), "-a-a-a-");
        assert_eq!(replace("aaa", "a*", "-", "g"), "-");
        assert_eq!(replace("a.c", ".", "x", "l"), "axc");
        assert_eq!(replace("xaaay", "a+?", "-", ""), "x-aay");
        assert_eq!(replace("abc", "[^b]", "-", "g"), "-b-");
        assert_eq!(replace("abc", "(a)(?:b)(c)", "\\2\\1", ""), "ca");
    }

    /// A rewrite RE2 will not run leaves the text alone rather than raising, which is worth a test
    /// because it is the one place where a mistake in a query is silent.
    #[test]
    fn a_replacement_that_asks_for_a_group_that_is_not_there_changes_nothing() {
        assert_eq!(replace("abc", "(b)", "[\\2]", ""), "abc");
        assert_eq!(replace("abc", "b", "\\q", ""), "abc");
        assert_eq!(replace("abc", "b", "x\\\\y", ""), "ax\\yc");
    }

    #[test]
    fn an_option_letter_that_is_not_one_says_so_the_way_duckdb_does() {
        let error = Options::parse("q").expect_err("not an option");
        assert_eq!(error.to_string(), "Invalid Input Error: Unrecognized Regex option q");
    }

    /// `s` and `m` are the same setting from two directions and the last one written wins, which is
    /// the only way to explain that `sm` and `ms` give different answers in DuckDB.
    #[test]
    fn the_newline_options_cancel_each_other_in_the_order_they_are_written() {
        assert!(!Options::parse("sm").expect("options").dot_matches_newline);
        assert!(Options::parse("ms").expect("options").dot_matches_newline);
        assert!(Options::parse("ci").expect("options").case_insensitive);
        assert!(!Options::parse("ic").expect("options").case_insensitive);
    }

    #[test]
    fn extract_takes_a_group_out_of_the_first_match() {
        let regex = Regex::new("([a-z]+)([0-9]+)").expect("compiles");
        assert_eq!(regex.extract("abc123", 0), Some("abc123"));
        assert_eq!(regex.extract("abc123", 2), Some("123"));
        assert_eq!(regex.extract("abc123", 7), None);
        assert_eq!(regex.extract("...", 0), None);
    }

    #[test]
    fn a_search_can_start_after_the_beginning_and_the_anchor_still_means_the_beginning() {
        let regex = Regex::new("^a").expect("compiles");
        assert!(regex.find_at("aab", 0).is_some());
        assert!(regex.find_at("aab", 1).is_none());
    }

    /// The machine is linear in the length of the text, so the pattern that takes a backtracking
    /// engine the age of the universe takes it no time at all. This is the property the engine was
    /// chosen for and it is cheap to hold it to.
    #[test]
    fn the_pattern_that_kills_a_backtracking_engine_is_answered_at_once() {
        let regex = Regex::new("(a+)+b").expect("compiles");
        assert!(!regex.is_match(&"a".repeat(40)));
        assert!(regex.is_match(&format!("{}b", "a".repeat(40))));
    }

    #[test]
    fn a_pattern_over_characters_that_are_more_than_one_byte_keeps_its_offsets() {
        let regex = Regex::new("é+").expect("compiles");
        let found = regex.find_at("aéés", 0).expect("matches");
        assert_eq!((found.start(), found.end()), (1, 5));
        assert_eq!(regex.replace("aéés", "x", false), "axs");
    }

    #[test]
    fn a_counted_repetition_counts() {
        assert_eq!(find("a{2,3}", "aaaa"), Some((0, 3)));
        assert_eq!(find("a{2}", "aaaa"), Some((0, 2)));
        assert!(Regex::new("^a{3,}$").expect("compiles").is_match("aaaa"));
        assert!(!Regex::new("^a{3,}$").expect("compiles").is_match("aa"));
    }
}

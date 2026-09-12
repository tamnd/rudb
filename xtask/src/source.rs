//! The small amount of reading Rust source that the lints in here do.
//!
//! Two lints walk the same files looking for the same kinds of thing: a line of code with the
//! comments taken off it, a comment block sitting directly above a line, and the point where the
//! test module starts and the rules stop applying. They were written twice before this module
//! existed and the second copy was already a line out of step with the first, which is the usual
//! reason to have one of something.
//!
//! None of this is a parser and none of it is trying to be. A parser in the task runner is a
//! dependency the workspace does not have and a maintenance cost nobody asked for, and every rule
//! that reads source here is a heuristic that is allowed to be wrong as long as being wrong is
//! loud. A lint that cannot tell what it is looking at says so and fails, rather than guessing and
//! passing.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, in no particular order.
///
/// Skips hidden directories, build output, and any directory holding a `VENDOR` file, which is
/// somebody else's tree that `cargo xtask grammar` checks for being byte for byte theirs. Having
/// opinions about how that one is written is the opposite of what it is for.
pub(crate) fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("could not read {}: {e}", dir.display()))?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if path.is_dir() && path.join("VENDOR").is_file() {
            continue;
        }
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Where the part of a file worth checking stops, which is the test module.
///
/// A test is allowed to do the slow thing, because that is how a test says what the answer is and
/// `spec/16-testing.md` makes the slow path the oracle the fast path is checked against. A lint
/// that made the oracle illegal would be a lint that deleted the testing strategy.
pub(crate) fn body_ends_at(lines: &[&str]) -> usize {
    lines.iter().position(|line| line.trim() == "#[cfg(test)]").unwrap_or(lines.len())
}

/// Whether the comment block sitting directly above `line` carries `marker`.
///
/// The whole block and not just the line above it, because a reason worth reading is usually a
/// sentence or two and rustfmt will have wrapped it, and a rule that only looked at the last line
/// would be a rule that made the reason fit the checker rather than the reader. A blank line ends
/// the block, so prose about something else further up does not cover anything.
pub(crate) fn declares(lines: &[&str], line: usize, marker: &str) -> bool {
    let mut at = line;
    while let Some(above) = at.checked_sub(1).and_then(|above| lines.get(above)) {
        if !above.trim_start().starts_with("//") {
            return false;
        }
        if above.contains(marker) {
            return true;
        }
        at -= 1;
    }
    false
}

/// Whether the text uses `word` as a word rather than as part of a longer name, so that `row` does
/// not match `narrowed` and `index` does not match `indexed`.
pub(crate) fn names(text: &str, word: &str) -> bool {
    let mut rest = text;
    while let Some(at) = rest.find(word) {
        let before = rest[..at].chars().next_back();
        let after = rest[at + word.len()..].chars().next();
        let edge = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if edge(before) && edge(after) {
            return true;
        }
        rest = &rest[at + word.len()..];
    }
    false
}

/// The line with any trailing comment removed, so that prose about code is not code.
///
/// String literals are respected, and so is a character literal holding a brace, which is the one
/// thing that would put a brace count out by one for the rest of a file.
pub(crate) fn without_comment(line: &str) -> String {
    let bytes: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut in_string = false;
    let mut at = 0;
    while at < bytes.len() {
        let current = bytes[at];
        if in_string {
            out.push(current);
            if current == '\\' && at + 1 < bytes.len() {
                out.push(bytes[at + 1]);
                at += 2;
                continue;
            }
            if current == '"' {
                in_string = false;
            }
            at += 1;
            continue;
        }
        if current == '"' {
            in_string = true;
            out.push(current);
            at += 1;
            continue;
        }
        if current == '/' && bytes.get(at + 1) == Some(&'/') {
            break;
        }
        if current == '\'' && bytes.get(at + 2) == Some(&'\'') {
            at += 3;
            continue;
        }
        out.push(current);
        at += 1;
    }
    out
}

/// The line with every string literal taken out of it, quotes and all.
///
/// A rule that looks for a piece of syntax has to be able to tell the syntax from a piece of source
/// that talks about the syntax, and a string holding `.flatten()` is the second. Taking the quotes
/// out with the contents rather than leaving an empty pair means the rest of the line reads as if
/// the call had no argument, which is what a rule looking at what sits in front of a dot wants.
///
/// Run this after [`without_comment`], because an unbalanced quote inside a comment would otherwise
/// swallow the rest of the line.
pub(crate) fn without_strings(line: &str) -> String {
    let characters: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut at = 0;
    while at < characters.len() {
        if characters[at] != '"' {
            out.push(characters[at]);
            at += 1;
            continue;
        }
        at += 1;
        while at < characters.len() && characters[at] != '"' {
            at += if characters[at] == '\\' { 2 } else { 1 };
        }
        at += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{body_ends_at, declares, names, without_comment, without_strings};

    #[test]
    fn a_string_holding_code_is_not_code() {
        assert_eq!(without_strings(r#"rest.find(".flatten()")"#), "rest.find()");
        assert_eq!(
            without_strings(r#"let escaped = "a\"b"; let x = 1;"#),
            "let escaped = ; let x = 1;"
        );
        assert_eq!(without_strings("let x = 1;"), "let x = 1;");
    }

    #[test]
    fn a_trailing_comment_is_not_code_and_a_slash_in_a_string_is_not_a_comment() {
        assert_eq!(without_comment("let x = 1; // and a { here"), "let x = 1; ");
        assert_eq!(without_comment(r#"let x = "a // b";"#), r#"let x = "a // b";"#);
        assert_eq!(without_comment("let open = '{';"), "let open = ;");
    }

    #[test]
    fn the_block_directly_above_counts_and_one_with_a_gap_does_not() {
        let touching = ["// mark: a reason", "// wrapped onto a second line", "code"];
        assert!(declares(&touching, 2, "mark:"));
        let gap = ["// mark: about something else", "", "code"];
        assert!(!declares(&gap, 2, "mark:"));
    }

    #[test]
    fn a_word_is_matched_whole_or_not_at_all() {
        assert!(names("for row in", "row"));
        assert!(!names("for narrowed in", "row"));
        assert!(names("(index)", "index"));
        assert!(!names("indexed", "index"));
    }

    #[test]
    fn the_body_stops_where_the_tests_start() {
        let lines = ["fn f() {}", "", "#[cfg(test)]", "mod tests {"];
        assert_eq!(body_ends_at(&lines), 2);
        assert_eq!(body_ends_at(&["fn f() {}"]), 1);
    }
}

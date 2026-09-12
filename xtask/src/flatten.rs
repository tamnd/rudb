//! The rule that every place the engine copies a compact column out flat says why.
//!
//! `Vector::flatten` turns any form back into a plain array of values. It is a copy of the whole
//! column, it throws away whatever the encoding was buying, and it is the single most expensive
//! thing in `rudb-vector`. It also exists for good reasons, which is why it is not simply deleted:
//! a caller that hands data to somebody else's format, or a test checking a fast path against a
//! slow one, genuinely cannot do better.
//!
//! The problem with a function like that is not the calls that are there. It is the calls that
//! arrive later, one at a time, each of them the shortest way to make one thing compile, none of
//! them written by somebody who was thinking about the whole. Three call sites that each say why
//! are an engine with a boundary. Thirty that nobody looked at are an engine that decodes
//! everything and cannot say where.
//!
//! So every call carries a comment saying why, the gate fails on one that does not, and
//! `rudb_common::slow` counts the ones that run. The lint is the list being reviewed and the
//! counter is the list being measured, and F1 needs both, because a call site that is deliberate
//! and on the hot path is still on the hot path.
//!
//! # Telling this flatten from the other two
//!
//! `flatten` is also an iterator adapter and an `Option` method, and this workspace calls both far
//! more often than it calls the one that matters. The rule that separates them is what sits
//! directly in front of the dot. A column flatten is called on something with a name, so
//! `column.flatten()` or `self.values.flatten()`. The other two are the end of a chain, so
//! `children().into_iter().flatten()` or `slots.get(i).copied().flatten()`.
//!
//! That is a heuristic, and the important part is what it does when it does not know. A receiver
//! that ends in a call this module has not been told about is neither passed nor quietly treated as
//! a column flatten: it is reported, and whoever wrote it either adds the adapter to [`ADAPTERS`]
//! or marks the line. A heuristic that guesses is a lint that stops being true without anybody
//! noticing, and this rule only has value while it is exactly true.
//!
//! # How to say it is deliberate
//!
//! A comment reading `flatten:` and then a reason, on the call itself or anywhere in the comment
//! block directly above it. Same shape as the marker `cargo xtask rowloop` uses, and for the same
//! reason: the escape hatch has to exist and it has to be a line a reviewer sees.

use std::path::Path;

use crate::source::{body_ends_at, collect, declares, without_comment, without_strings};

/// The comment that makes a call deliberate.
const MARKER: &str = "flatten:";

/// The standard library methods a chain ends in when its `flatten` is not a column flatten.
///
/// Every one of these was read off a call that is in the workspace today rather than guessed at, so
/// the list is short and grows a line at a time when somebody writes a shape it does not cover. A
/// long speculative list would defeat the point, since the value of this rule is entirely in it
/// failing on something it has not seen before.
const ADAPTERS: [&str; 6] = ["iter", "into_iter", "copied", "cloned", "map", "filter_map"];

pub(crate) fn check(root: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    collect(&root.join("crates"), &mut files)?;
    collect(&root.join("xtask"), &mut files)?;
    files.sort();

    let mut problems = Vec::new();
    let mut reviewed = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file)
            .map_err(|e| format!("could not read {}: {e}", file.display()))?;
        let shown = file.strip_prefix(root).unwrap_or(file).display().to_string();
        let (found, marked) = check_one(&shown, &text);
        problems.extend(found);
        reviewed.extend(marked);
    }

    if problems.is_empty() {
        println!("{} call sites flatten a column, and every one of them says why", reviewed.len());
        for site in &reviewed {
            println!("  {site}");
        }
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("  {problem}");
        }
        eprintln!(
            "  a call that means to copy a column out flat says so in a comment reading \
             `{MARKER} <reason>`"
        );
        Err(format!("{} flatten call sites are not accounted for", problems.len()))
    }
}

/// What one `.flatten()` in the text turns out to be.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// Called on something with a name, so it is a column being copied out.
    Column,
    /// The end of a chain this module knows, so it is the iterator or the option one.
    Standard,
    /// The end of a chain this module does not know, which is the case worth failing on.
    Unknown,
}

fn check_one(name: &str, text: &str) -> (Vec<String>, Vec<String>) {
    let lines: Vec<&str> = text.lines().collect();
    let mut problems = Vec::new();
    let mut reviewed = Vec::new();

    for (index, raw) in lines.iter().enumerate().take(body_ends_at(&lines)) {
        let code = without_strings(&without_comment(raw));
        for kind in kinds(&code) {
            let at = format!("{name}:{}", index + 1);
            match kind {
                Kind::Standard => {}
                Kind::Column if raw.contains(MARKER) || declares(&lines, index, MARKER) => {
                    reviewed.push(at);
                }
                Kind::Column => {
                    problems
                        .push(format!("{at}: a column is copied out flat and nothing says why"));
                }
                Kind::Unknown => problems.push(format!(
                    "{at}: cannot tell whether this flattens a column, so it is reported rather \
                     than assumed"
                )),
            }
        }
    }
    (problems, reviewed)
}

/// What each `.flatten()` on this line is.
fn kinds(code: &str) -> Vec<Kind> {
    let mut out = Vec::new();
    let mut rest = code;
    let mut consumed = 0;
    while let Some(at) = rest.find(".flatten()") {
        let before = &code[..consumed + at];
        out.push(classify(before));
        consumed += at + ".flatten()".len();
        rest = &code[consumed..];
    }
    out
}

/// What the text in front of the dot says about which `flatten` this is.
fn classify(before: &str) -> Kind {
    let trimmed = before.trim_end();
    if !trimmed.ends_with(')') {
        // A name, a field or an index, which is what a column is reached through. An empty receiver
        // is the definition of the method itself rather than a call of it.
        return if trimmed.is_empty() { Kind::Standard } else { Kind::Column };
    }
    // `a.b(x).flatten()`, so find the name in front of the parentheses that just closed.
    let opened = match matching(trimmed) {
        Some(at) => at,
        None => return Kind::Unknown,
    };
    let head = trimmed[..opened].trim_end();
    let called: String =
        head.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    let called: String = called.chars().rev().collect();
    if ADAPTERS.contains(&called.as_str()) { Kind::Standard } else { Kind::Unknown }
}

/// Where the parenthesis that the last character closes was opened, counting only this line.
fn matching(text: &str) -> Option<usize> {
    let characters: Vec<char> = text.chars().collect();
    let mut depth = 0_i32;
    for (at, character) in characters.iter().enumerate().rev() {
        match character {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{Kind, check_one, classify};

    #[test]
    fn a_column_flatten_is_the_one_called_on_a_name() {
        assert_eq!(classify("        let flat = vector"), Kind::Column);
        assert_eq!(classify("            columns.push(column"), Kind::Column);
        assert_eq!(classify("        let chunk = self.held"), Kind::Column);
    }

    #[test]
    fn the_iterator_and_option_ones_are_the_end_of_a_chain_this_knows() {
        assert_eq!(classify("        for child in node.children().into_iter()"), Kind::Standard);
        assert_eq!(classify("        self.slots.get(index).copied()"), Kind::Standard);
        assert_eq!(classify("        if values.iter()"), Kind::Standard);
    }

    /// The case the rule is worth having. A chain ending in something nobody listed is not waved
    /// through, because a lint that guesses stops being true without anybody noticing.
    #[test]
    fn a_chain_ending_in_something_unlisted_is_reported_rather_than_guessed_at() {
        assert_eq!(classify("        let flat = decode(page)"), Kind::Unknown);
        let text = "fn f() {\n    let flat = decode(page).flatten()?;\n}\n";
        let (problems, reviewed) = check_one("t.rs", text);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("cannot tell"), "{problems:?}");
        assert!(reviewed.is_empty());
    }

    #[test]
    fn an_unmarked_column_flatten_fails_the_rule() {
        let text = "fn f() {\n    let flat = vector.flatten()?;\n}\n";
        let (problems, reviewed) = check_one("t.rs", text);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("nothing says why"), "{problems:?}");
        assert!(reviewed.is_empty());
    }

    #[test]
    fn the_marker_above_the_call_makes_it_reviewed_and_it_is_listed() {
        let text = "fn f() {\n    // flatten: the arrow bridge hands out plain buffers, so there\n    // is nothing compact on the far side to hand this to.\n    let flat = vector.flatten()?;\n}\n";
        let (problems, reviewed) = check_one("array.rs", text);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(reviewed, ["array.rs:4"]);
    }

    #[test]
    fn the_marker_on_the_call_itself_counts_too() {
        let text = "fn f() {\n    let flat = vector.flatten()?; // flatten: a one line reason\n}\n";
        assert!(check_one("t.rs", text).0.is_empty());
    }

    #[test]
    fn the_definition_of_the_method_is_not_a_call_of_it() {
        let text = "impl Vector {\n    pub fn flatten(&self) -> Result<Self> {\n        self.copied()\n    }\n}\n";
        let (problems, reviewed) = check_one("vector.rs", text);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(reviewed.is_empty());
    }

    #[test]
    fn the_test_module_is_not_checked() {
        let text = "fn f() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {\n        let flat = vector.flatten().unwrap();\n    }\n}\n";
        assert!(check_one("t.rs", text).0.is_empty());
    }

    #[test]
    fn a_string_that_holds_the_call_is_not_a_call() {
        let text = "fn f() {\n    while let Some(at) = rest.find(\".flatten()\") {}\n}\n";
        let (problems, reviewed) = check_one("t.rs", text);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(reviewed.is_empty());
    }

    #[test]
    fn a_comment_describing_a_call_is_not_a_call() {
        let text = "fn f() {\n    // a caller that wrote vector.flatten() here would be copying\n    let one = 1;\n}\n";
        assert!(check_one("t.rs", text).0.is_empty());
    }

    #[test]
    fn two_on_one_line_are_two_and_are_judged_separately() {
        let text = "fn f() {\n    let both = (rows.iter().flatten(), column.flatten()?);\n}\n";
        let (problems, _) = check_one("t.rs", text);
        assert_eq!(problems.len(), 1, "the iterator one is fine and the column one is not");
        assert!(problems[0].contains("nothing says why"), "{problems:?}");
    }
}

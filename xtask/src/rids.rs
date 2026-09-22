//! The rule that every plan node has a test saying whether its output carries a row id.
//!
//! `crates/rudb-plan/src/rid.rs` answers, for every operator, whether a row of its output is still a
//! row of a base table. `spec/graph/05-execution.md` section 5.1 says every wrong answer the graph
//! layer can produce is a row id used after the operator that invalidated it, so the answer for one
//! operator being wrong is not a slow query, it is a wrong one.
//!
//! The analysis itself is already forced: the `match` in that module is exhaustive over `Node`, so a
//! new operator does not compile until somebody answers the question for it. Nothing forces the
//! answer to be tested, and an answer nobody wrote a test for is an answer nobody read twice. G3's
//! second exit criterion asks for a test per plan node, and this is what keeps that true after the
//! day it was first true.
//!
//! # What counts as a test for a variant
//!
//! A `Node::<Variant>` in the test module of `rid.rs`, which means a test constructed one and ran
//! the pass over it. Naming the variant in a comment is not enough and is not meant to be. A helper
//! in that module counts, because a helper there exists to build a node for a test.
//!
//! The check is a search for a name rather than a parse, for the same reason `rowloop` is: the
//! alternative is a Rust parser in the task runner, and a variant name is not a thing that appears
//! in that file by accident.

use std::path::Path;

use crate::source::body_ends_at;

/// The operators.
const NODES: &str = "crates/rudb-plan/src/node.rs";

/// Where each of them has to be answered for and tested.
const RIDS: &str = "crates/rudb-plan/src/rid.rs";

pub(crate) fn check(root: &Path) -> Result<(), String> {
    let nodes = read(root, NODES)?;
    let rids = read(root, RIDS)?;
    let variants = variants(&nodes);
    if variants.is_empty() {
        return Err(format!("found no variants of enum Node in {NODES}"));
    }

    let tests = test_module(&rids);
    let untested =
        variants.iter().filter(|variant| !mentions(&tests, variant)).cloned().collect::<Vec<_>>();
    // The analysis is exhaustive by the compiler, and this says so rather than checking it, because
    // a variant missing from the `match` is a build failure before it is a lint failure.
    let unanswered =
        variants.iter().filter(|variant| !mentions(&rids, variant)).cloned().collect::<Vec<_>>();

    if untested.is_empty() && unanswered.is_empty() {
        println!(
            "each of the {} plan nodes says whether its output carries a rid, and each is tested",
            variants.len()
        );
        return Ok(());
    }
    for variant in &unanswered {
        eprintln!("  Node::{variant} is not named in {RIDS}");
    }
    for variant in &untested {
        eprintln!("  Node::{variant} has no test in {RIDS}");
    }
    eprintln!(
        "  a test constructs the node and asserts what rids_of says its output carries, which is \
         G3 exit criterion 2"
    );
    let missing = untested.len() + unanswered.len();
    Err(match missing {
        1 => "one plan node has no rid test".to_string(),
        many => format!("{many} plan nodes have no rid test"),
    })
}

fn read(root: &Path, path: &str) -> Result<String, String> {
    std::fs::read_to_string(root.join(path)).map_err(|e| format!("could not read {path}: {e}"))
}

/// The names of the variants of `enum Node`.
///
/// Read off the indentation rather than by parsing: a variant of that enum is a capitalised word at
/// one level of indentation between the `pub enum Node {` line and the brace that closes it, and the
/// file is `rustfmt` output, so the indentation is not a guess about style.
fn variants(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut inside = false;
    let mut depth = 0_i32;
    for line in text.lines() {
        if !inside {
            inside = line.starts_with("pub enum Node {");
            if inside {
                depth = 1;
            }
            continue;
        }
        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;
        if depth == 1
            && let Some(name) = variant(line)
        {
            found.push(name);
        }
        depth += opens - closes;
        if depth <= 0 {
            break;
        }
    }
    found
}

/// The name a variant line declares, when the line declares one.
///
/// `    Get {`, `    Dummy,` and a bare `    Dummy` are the three shapes a variant takes. An
/// attribute, a comment and a field line are the three this has to leave alone, and a field line is
/// told apart by its lower case first letter.
fn variant(line: &str) -> Option<String> {
    let rest = line.strip_prefix("    ")?;
    if rest.starts_with(' ') || rest.starts_with('/') || rest.starts_with('#') {
        return None;
    }
    let name: String = rest.chars().take_while(char::is_ascii_alphanumeric).collect();
    let after = &rest[name.len()..];
    let declares = after.is_empty() || after.starts_with(" {") || after.starts_with(',');
    (declares && name.starts_with(|first: char| first.is_ascii_uppercase())).then_some(name)
}

/// The test module of a file, which is everything from the `#[cfg(test)]` to the end.
fn test_module(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[body_ends_at(&lines).min(lines.len())..].join("\n")
}

/// Whether the text names that variant as a variant, rather than as part of a longer name.
///
/// `Node::Limit` is in `Node::LimitPercent`, and a file that tested only the second would otherwise
/// look like it had tested both.
fn mentions(text: &str, variant: &str) -> bool {
    let needle = format!("Node::{variant}");
    let mut from = 0;
    while let Some(found) = text[from..].find(&needle) {
        let at = from + found;
        from = at + needle.len();
        if !text[from..].starts_with(|next: char| next.is_ascii_alphanumeric() || next == '_') {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{mentions, test_module, variant, variants};

    #[test]
    fn the_three_shapes_of_a_variant_are_read_and_a_field_is_not() {
        assert_eq!(variant("    Get {").as_deref(), Some("Get"));
        assert_eq!(variant("    Dummy,").as_deref(), Some("Dummy"));
        assert_eq!(variant("    Dummy").as_deref(), Some("Dummy"));
        assert_eq!(variant("        input: NodeRef,"), None, "a field is indented further");
        assert_eq!(variant("    index: u32,"), None, "and starts in lower case");
        assert_eq!(variant("    /// What it is."), None);
        assert_eq!(variant("    #[derive(Debug)]"), None);
    }

    #[test]
    fn the_variants_of_the_enum_are_found_and_nothing_around_it_is() {
        let text = "pub enum Other {\n    Wrong,\n}\n\npub enum Node {\n    /// A scan.\n    \
                    Get {\n        index: u32,\n    },\n    Dummy,\n}\n\npub enum After {\n    \
                    Later,\n}\n";
        assert_eq!(variants(text), vec!["Get".to_string(), "Dummy".to_string()]);
    }

    #[test]
    fn a_longer_name_does_not_stand_in_for_a_shorter_one() {
        let text = "Node::LimitPercent { input }";
        assert!(mentions(text, "LimitPercent"));
        assert!(!mentions(text, "Limit"), "a prefix of a variant name is not that variant");
        assert!(mentions("Node::Limit { input }", "Limit"));
    }

    #[test]
    fn only_the_test_module_counts_as_a_test() {
        let text = "fn compute() {\n    Node::Sort { .. } => none(),\n}\n\n#[cfg(test)]\nmod \
                    tests {\n    Node::Filter { input }\n}\n";
        let tests = test_module(text);
        assert!(mentions(&tests, "Filter"));
        assert!(!mentions(&tests, "Sort"), "answering for a node is not testing it");
    }
}

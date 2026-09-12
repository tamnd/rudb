//! The rule that a seam is crossed once per chunk and never once per row.
//!
//! `rudb-seam` opens by saying it: every seam trait's methods take a whole chunk, a whole column, a
//! whole morsel, a whole partition or a decision made at plan time, and none of them take a row, a
//! value or a single key. An indirect call every hundred thousand rows costs nothing anybody can
//! measure and an indirect call per row costs an order of magnitude, and that one difference is why
//! a design with twenty seven swappable parts can be fast at all.
//!
//! A rule that lives only in a paragraph is a rule that holds until the first afternoon somebody is
//! in a hurry. The interface that breaks it is not obviously wrong when you write it: a hash table
//! seam with a `probe_one(key)` reads fine, passes its tests, and gives away everything the seam was
//! supposed to be affordable enough to buy. This is here so that the first one fails the build
//! rather than a benchmark six months later.
//!
//! # The two rules
//!
//! A method of a seam trait that takes parameters has to take something that is not a scalar. A
//! chunk, a column, a selection, a slice of anything, a plan, a context. A method that takes only
//! numbers, booleans, strings and values is a method that is being called per row, whatever the
//! name on it says. A method that takes nothing but `&self` is an accessor and is left alone, which
//! is what `name` and `describe` on `Strategy` are.
//!
//! A function that crosses a seam and also loops over the rows of a vector is reported, because the
//! two of them in one body is what crossing per row looks like from the outside. The honest version
//! of that function does its loop and then crosses once, and it says so with a comment reading
//! `seam once per chunk:` and a reason, the same escape hatch the other two lints have.
//!
//! # What it reads
//!
//! A trait whose supertrait list names `Strategy`, which is what makes a trait a seam, and every
//! `fn` inside it. Then every function in the workspace, looking for a call to one of those methods
//! or to `choose`, which is how a strategy is got out of a registry in the first place. Test files
//! are skipped on the way in, because the toy seam a test declares to check the machinery is not a
//! seam of the engine and its method names are the short ones that collide with everything.
//!
//! None of this is a parser, for the reason `crate::source` gives. A seam method whose name is also
//! an ordinary method name somewhere else will be reported where it is not crossed, and the fix for
//! that is a better name on the seam rather than a cleverer lint.

use std::path::Path;

use crate::rowloop::is_row_loop;
use crate::source::{body_ends_at, collect, declares, without_comment, without_strings};

/// The comment that says a function crosses its seam outside its loop.
const MARKER: &str = "seam once per chunk:";

/// The types a seam method is not allowed to be made of on its own.
///
/// Everything else counts as bulk, which is the permissive direction on purpose: a new encoding, a
/// new morsel type or a new statistics struct is something this list has never heard of, and a lint
/// that failed on every type it did not recognise would be a lint somebody turns off.
const SCALARS: &[&str] = &[
    "usize",
    "isize",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "f32",
    "f64",
    "bool",
    "char",
    "str",
    "String",
    "Value",
    "LogicalType",
    "PhysicalType",
    "SeamId",
    "Comparison",
    "Connective",
    "Ordering",
    "Determinism",
    "Provenance",
];

pub(crate) fn check(root: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    collect(&root.join("crates"), &mut files)?;
    files.sort();

    let mut read = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file)
            .map_err(|e| format!("could not read {}: {e}", file.display()))?;
        let shown = file.strip_prefix(root).unwrap_or(file).display().to_string();
        read.push((shown, text));
    }

    let mut problems = Vec::new();
    let mut methods: Vec<String> = Vec::new();
    let mut seams = 0;
    for (shown, text) in &read {
        if is_a_test(shown) {
            continue;
        }
        let found = traits_in(shown, text);
        problems.extend(found.problems);
        methods.extend(found.methods);
        seams += found.traits;
    }

    let mut declared = 0;
    for (shown, text) in &read {
        let found = crossings(shown, text, &methods);
        problems.extend(found.0);
        declared += found.1;
    }

    if problems.is_empty() {
        if seams == 0 {
            // Which is where F1 finds it. The seam registry is empty until the compaction seam
            // lands, so today this says the rule is armed rather than that it caught anything, and
            // that is worth printing rather than a line that reads like a check of something.
            println!(
                "no seam trait is declared outside the tests yet, so the rule that a seam is \
                 crossed once per chunk is waiting for the first one"
            );
        } else {
            println!(
                "the {} methods of {seams} seam traits all take something bigger than a value, and \
                 no function crosses a seam and loops over rows, {declared} say they mean to",
                methods.len()
            );
        }
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("  {problem}");
        }
        eprintln!(
            "  a seam is crossed once per chunk, so its methods take a chunk or a column, and a \
             function that crosses one outside its loop says so in a comment reading \
             `{MARKER} <reason>`"
        );
        Err(format!("{} places cross a seam a row at a time", problems.len()))
    }
}

/// Whether a file is a test file, whose toy seams are not the engine's.
fn is_a_test(shown: &str) -> bool {
    shown.ends_with("tests.rs") || shown.contains("/tests/")
}

/// What one file's seam traits came to.
struct Found {
    problems: Vec<String>,
    methods: Vec<String>,
    traits: usize,
}

/// Every seam trait in a file, and every method of one that takes only scalars.
fn traits_in(name: &str, text: &str) -> Found {
    let lines: Vec<&str> = text.lines().collect();
    let end = body_ends_at(&lines);
    let mut found = Found { problems: Vec::new(), methods: Vec::new(), traits: 0 };
    let mut at = 0;
    while at < end {
        let code = without_strings(&without_comment(lines[at]));
        let Some(trait_name) = declares_a_seam(&code) else {
            at += 1;
            continue;
        };
        found.traits += 1;
        let (body, ends) = block(&lines, at, end);
        for method in methods_of(&body) {
            if method.only_scalars {
                found.problems.push(format!(
                    "{name}:{}: {trait_name}::{} takes only scalars, so it is called per row",
                    at + 1,
                    method.name
                ));
            }
            found.methods.push(method.name);
        }
        at = ends;
    }
    found
}

/// The name of the trait this line declares, when the trait is a seam.
///
/// A seam trait is one whose supertrait list names `Strategy`, which is the whole of what the seam
/// machinery asks of it and therefore the only thing there is to look for.
fn declares_a_seam(code: &str) -> Option<String> {
    let at = code.find("trait ")?;
    let rest = &code[at + "trait ".len()..];
    let (head, supers) = rest.split_once(':')?;
    if !crate::source::names(supers, "Strategy") {
        return None;
    }
    let name: String =
        head.trim().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    if name.is_empty() { None } else { Some(name) }
}

/// The lines of the block that opens on `from`, joined, and the line the block ends after.
fn block(lines: &[&str], from: usize, end: usize) -> (String, usize) {
    let mut body = String::new();
    let mut depth = 0_i32;
    let mut open = false;
    for (index, raw) in lines.iter().enumerate().take(end).skip(from) {
        let code = without_strings(&without_comment(raw));
        for character in code.chars() {
            match character {
                '{' => {
                    depth += 1;
                    open = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        body.push_str(&code);
        body.push('\n');
        if open && depth <= 0 {
            return (body, index + 1);
        }
    }
    (body, end)
}

/// One method of a seam trait.
struct Method {
    name: String,
    only_scalars: bool,
}

/// Every `fn` in a trait body, with its parameters read.
fn methods_of(body: &str) -> Vec<Method> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find("fn ") {
        rest = &rest[at + "fn ".len()..];
        let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        let Some(open) = rest.find('(') else {
            break;
        };
        let Some(close) = closing(rest, open) else {
            break;
        };
        let params = parameters(&rest[open + 1..close]);
        let only_scalars = !params.is_empty() && params.iter().all(|param| is_a_scalar(param));
        if !name.is_empty() {
            out.push(Method { name, only_scalars });
        }
        rest = &rest[close..];
    }
    out
}

/// Where the parenthesis opened at `open` closes.
fn closing(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0_i32;
    for (at, character) in text.char_indices().skip(open) {
        match character {
            '(' => depth += 1,
            ')' => {
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

/// The types of a parameter list, with the receiver dropped.
///
/// Split at the commas that are not inside a generic, a slice or a tuple, because `&[(u32, u32)]`
/// is one parameter and a rule that read it as two would read the half of it that is a scalar and
/// report a method that takes a whole column.
fn parameters(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0_i32;
    let mut current = String::new();
    for character in list.chars() {
        match character {
            '<' | '[' | '(' => depth += 1,
            '>' | ']' | ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(character);
    }
    out.push(current);
    out.into_iter()
        .filter_map(|param| {
            let param = param.trim();
            if param.is_empty() || param.contains("self") {
                return None;
            }
            Some(param.split_once(':').map_or(param, |(_, ty)| ty).trim().to_string())
        })
        .collect()
}

/// Whether a parameter type is one value rather than a run of them.
fn is_a_scalar(ty: &str) -> bool {
    let ty = ty.trim();
    if ty.contains('[') || ty.contains("Vec<") || ty.contains("Iterator") {
        return false;
    }
    let bare = ty.trim_start_matches('&').trim_start_matches("mut ").trim();
    // An `Option` of one value is still one value, and so is a `Range`, which is two numbers naming
    // a piece of one column rather than the column.
    let inner = bare
        .strip_prefix("Option<")
        .or_else(|| bare.strip_prefix("Range<"))
        .and_then(|held| held.strip_suffix('>'))
        .map_or(bare, str::trim);
    let inner = inner.trim_start_matches('&').trim_start_matches("mut ").trim();
    SCALARS.contains(&inner)
}

/// The functions that cross a seam and loop over rows, and how many said they meant to.
fn crossings(name: &str, text: &str, methods: &[String]) -> (Vec<String>, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let end = body_ends_at(&lines);
    let mut problems = Vec::new();
    let mut declared = 0;
    let mut at = 0;
    while at < end {
        let code = without_strings(&without_comment(lines[at]));
        if !opens_a_function(&code) {
            at += 1;
            continue;
        }
        let (body, ends) = block(&lines, at, end);
        let marked = lines[at..ends].iter().any(|line| line.contains(MARKER))
            || declares(&lines, at, MARKER);
        let crossed = crosses(&body, methods);
        let looped = body.lines().any(is_row_loop);
        if let Some(crossed) = crossed {
            if looped {
                if marked {
                    declared += 1;
                } else {
                    problems.push(format!(
                        "{name}:{}: this function loops over rows and crosses a seam at {crossed}",
                        at + 1
                    ));
                }
            }
        }
        at = ends;
    }
    (problems, declared)
}

/// A line that opens a function body, which is a `fn` header ending in a brace.
///
/// A signature rustfmt wrapped ends on a later line, and the brace is there rather than on the `fn`,
/// so a wrapped signature is not seen. That is a hole and it is the cheap kind: it makes the lint
/// miss a function rather than report one that is fine, and a seam crossed inside a function whose
/// signature is four lines long is a seam crossed inside a function that is worth reading anyway.
fn opens_a_function(code: &str) -> bool {
    let trimmed = code.trim();
    trimmed.contains("fn ") && trimmed.ends_with('{')
}

/// What this body calls that is a seam crossing, if it calls anything.
fn crosses(body: &str, methods: &[String]) -> Option<String> {
    if body.contains(".choose(") {
        return Some("choose".to_string());
    }
    methods.iter().find(|method| body.contains(&format!(".{method}("))).cloned()
}

#[cfg(test)]
mod tests {
    use super::{crossings, traits_in};

    #[test]
    fn a_seam_method_that_takes_a_chunk_is_fine() {
        let text =
            "trait Compactor: Strategy {\n    fn compact(&self, chunk: &Chunk) -> Chunk;\n}\n";
        let found = traits_in("t.rs", text);
        assert!(found.problems.is_empty());
        assert_eq!(found.traits, 1);
        assert_eq!(found.methods, vec!["compact".to_string()]);
    }

    #[test]
    fn a_seam_method_that_takes_a_key_is_caught() {
        let text =
            "trait Table: Strategy {\n    fn probe_one(&self, key: u64) -> Option<usize>;\n}\n";
        assert_eq!(traits_in("t.rs", text).problems.len(), 1);
    }

    /// An accessor takes nothing, which is `name` and `describe` on every strategy there will ever
    /// be, and reporting those would mean the lint fails on the seam machinery itself.
    #[test]
    fn a_method_that_takes_nothing_is_an_accessor() {
        let text = "trait Toy: Strategy {\n    fn run(&self) -> &'static str;\n}\n";
        assert!(traits_in("t.rs", text).problems.is_empty());
    }

    /// A slice of keys is a batch of work, which is the whole point of the rule, and the comma
    /// inside the tuple is what a split on commas gets wrong.
    #[test]
    fn a_slice_of_scalars_is_a_batch_and_not_a_scalar() {
        let text =
            "trait Table: Strategy {\n    fn probe(&self, keys: &[(u64, u32)]) -> Selection;\n}\n";
        assert!(traits_in("t.rs", text).problems.is_empty());
    }

    /// A trait that is not a seam is somebody else's interface and none of this applies to it.
    #[test]
    fn a_trait_that_is_not_a_seam_is_left_alone() {
        let text = "trait Operator: Debug {\n    fn push(&self, row: usize);\n}\n";
        let found = traits_in("t.rs", text);
        assert_eq!(found.traits, 0);
        assert!(found.problems.is_empty());
    }

    #[test]
    fn a_seam_crossed_in_a_function_that_loops_over_rows_is_caught() {
        let methods = vec!["compact".to_string()];
        let text = "fn run(&self, chunk: &Chunk) {\n    for row in 0..chunk.len() {\n        total += row;\n    }\n    self.compact(chunk);\n}\n";
        assert_eq!(crossings("t.rs", text, &methods).0.len(), 1);
    }

    #[test]
    fn the_marker_says_the_crossing_is_outside_the_loop() {
        let methods = vec!["compact".to_string()];
        let text = "// seam once per chunk: the loop fills the chunk and the crossing is after it\nfn run(&self, chunk: &Chunk) {\n    for row in 0..chunk.len() {\n        total += row;\n    }\n    self.compact(chunk);\n}\n";
        let (problems, declared) = crossings("t.rs", text, &methods);
        assert!(problems.is_empty());
        assert_eq!(declared, 1);
    }

    #[test]
    fn a_function_that_crosses_a_seam_and_does_not_loop_is_fine() {
        let methods = vec!["compact".to_string()];
        let text = "fn run(&self, chunk: &Chunk) {\n    self.compact(chunk);\n}\n";
        assert!(crossings("t.rs", text, &methods).0.is_empty());
    }

    #[test]
    fn choosing_an_implementation_inside_a_row_loop_is_a_crossing() {
        let text = "fn run(&self, chunk: &Chunk) {\n    for row in 0..chunk.len() {\n        let chosen = registry.choose(&context);\n    }\n}\n";
        assert_eq!(crossings("t.rs", text, &[]).0.len(), 1);
    }
}

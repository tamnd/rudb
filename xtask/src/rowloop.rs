//! The rule that a loop over the rows of a vector does not build a `Value`.
//!
//! `Vector::value_at` says in its own documentation that an operator calling it per row has already
//! lost the argument the vector interface exists to win, and `spec/engine/03-data-plane.md` names
//! that as the finding that sets layer one's agenda. It has been true twice since: `Vector::flatten`
//! was collecting a `Vec<Value>` of a whole vector and pushing each value back into the layout it
//! had just taken it out of, which was 2.6 times the result path of a returning query, and the
//! comparison kernel was building two owned `Value`s a row before it was rewritten around a typed
//! loop. Both were found by reading rather than by the gate, which is the reason this exists.
//!
//! # What counts as a violation
//!
//! A `for` loop whose header looks like it runs once per row, containing a `Value::` construction, a
//! call to `value_at` or a call to `from_values`. That is a heuristic and it is meant to be: the
//! alternative is a Rust parser in the task runner, and the heuristic found every one of the twelve
//! loops this repository had when it was written and nothing that was not one.
//!
//! A loop header looks like it runs once per row when it is a range, an iterator or an enumeration,
//! and either its source names a count of rows or its binding names a row. Both halves are needed.
//! Without the first, every `for` in the workspace is a row loop. Without the second, a loop over
//! three comparison operators or over the columns of a chunk is one, and a loop over the columns of
//! a chunk is exactly where building one `Value` is the right thing to do.
//!
//! # How to say it is deliberate
//!
//! A comment reading `row at a time:` and then a reason, on the loop itself or anywhere in the
//! comment block directly above it, so that a reason long enough to be worth reading can be a
//! paragraph rather than a line that has to end where the checker looks. The
//! marker covers the whole loop, because a loop that is deliberately row at a time is row at a time
//! all the way down. It is a comment rather than an attribute for the same reason `unsafe_code` is
//! denied rather than forbidden in `rudb-vector`: the escape hatch has to exist, and it has to be a
//! line a reviewer sees.
//!
//! Two kinds of reason are honest. One is a path that is meant to be slow, which is every kernel's
//! fallback, and those say so and increment a counter. The other is a path that should not be slow
//! and has not been fixed yet, and those name the issue that fixes them, which turns the debt from
//! something you find by reading into something you find with `grep`.

use std::path::Path;

use crate::source::{body_ends_at, collect, declares, names, without_comment};

/// The comment that makes a loop deliberate.
const MARKER: &str = "row at a time:";

pub(crate) fn check(root: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    collect(&root.join("crates"), &mut files)?;
    files.sort();

    let mut problems = Vec::new();
    let mut declared = 0;
    for file in &files {
        let text = std::fs::read_to_string(file)
            .map_err(|e| format!("could not read {}: {e}", file.display()))?;
        let shown = file.strip_prefix(root).unwrap_or(file).display().to_string();
        let (found, marked) = check_one(&shown, &text);
        problems.extend(found);
        declared += marked;
    }

    if problems.is_empty() {
        println!(
            "no row loop builds a Value across {} source files, {declared} say they mean to",
            files.len()
        );
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("  {problem}");
        }
        eprintln!(
            "  a loop that means to work a row at a time says so in a comment reading \
             `{MARKER} <reason>`"
        );
        Err(format!("{} row loops build a Value", problems.len()))
    }
}

/// A loop being tracked: where its header was, the brace depth outside it, and whether it is
/// declared deliberate.
struct Open {
    line: usize,
    outer: i32,
    declared: bool,
}

fn check_one(name: &str, text: &str) -> (Vec<String>, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let mut problems = Vec::new();
    let mut declared = 0;
    let mut open: Vec<Open> = Vec::new();
    let mut depth = 0_i32;

    for (index, raw) in lines.iter().enumerate().take(body_ends_at(&lines)) {
        let code = without_comment(raw);
        if is_row_loop(&code) {
            let marked = raw.contains(MARKER) || declares(&lines, index, MARKER);
            if marked {
                declared += 1;
            }
            open.push(Open { line: index + 1, outer: depth, declared: marked });
        }
        if let Some(what) = builds_a_value(&code) {
            if let Some(loop_at) = open.iter().find(|entry| !entry.declared) {
                problems.push(format!(
                    "{name}:{}: a loop over rows, and {} on line {}",
                    loop_at.line,
                    what,
                    index + 1
                ));
            }
        }
        depth += braces(&code);
        open.retain(|entry| depth > entry.outer);
    }
    (problems, declared)
}

/// A loop header that looks like it runs once per row.
///
/// A range, an iterator or an enumeration, and then either a source that names a count of rows or a
/// binding that names a row. Both halves are needed. Without the first, every `for` in the workspace
/// is a row loop; without the second, a loop over three comparison operators or a handful of column
/// names is one, and those are the loops where a `Value` is the right thing to build.
pub(crate) fn is_row_loop(code: &str) -> bool {
    if !code.trim_start().starts_with("for ") {
        return false;
    }
    let Some((binding, source)) = code.split_once(" in ") else {
        return false;
    };
    let iterating =
        source.contains("0..") || source.contains(".iter()") || source.contains(".enumerate()");
    let counted = ["rows", "len", "length", "count", "VECTOR_SIZE", "indices", "codes", "views"]
        .iter()
        .any(|word| source.contains(word));
    // `row` and `index` and not `position`, because a loop binding a position is usually a loop
    // over the columns of a chunk, and building one `Value` per column is what a column loop is for.
    let bound = ["row", "index"].iter().any(|word| names(binding, word));
    iterating && (counted || bound)
}

/// What the line does that a loop over rows should not, if it does anything.
fn builds_a_value(code: &str) -> Option<&'static str> {
    if code.contains(".value_at(") {
        return Some("a call to value_at");
    }
    if code.contains("from_values(") {
        return Some("a call to from_values");
    }
    if constructs_a_value(code) {
        return Some("a Value construction");
    }
    None
}

/// `Value::Something(`, which is a construction, as opposed to `Value::Something =>`, which is a
/// pattern, or `Value::Null`, which is a constant the compiler hoists.
fn constructs_a_value(code: &str) -> bool {
    let mut rest = code;
    while let Some(at) = rest.find("Value::") {
        let after = &rest[at + "Value::".len()..];
        let name: String = after.chars().take_while(char::is_ascii_alphanumeric).collect();
        if !name.is_empty() && after[name.len()..].starts_with('(') {
            return true;
        }
        rest = &rest[at + "Value::".len()..];
    }
    false
}

fn braces(code: &str) -> i32 {
    let mut depth = 0;
    for character in code.chars() {
        match character {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    depth
}

#[cfg(test)]
mod tests {
    use super::check_one;

    #[test]
    fn a_value_built_in_a_row_loop_is_caught() {
        let text = "fn f() {\n    for row in 0..chunk.len() {\n        out.push(input.value_at(row));\n    }\n}\n";
        assert_eq!(check_one("t.rs", text).0.len(), 1);
    }

    #[test]
    fn the_marker_on_the_loop_makes_it_deliberate() {
        let text = "fn f() {\n    // row at a time: the fallback, which counts itself\n    for row in 0..chunk.len() {\n        out.push(input.value_at(row));\n    }\n}\n";
        let (problems, declared) = check_one("t.rs", text);
        assert!(problems.is_empty());
        assert_eq!(declared, 1);
    }

    /// A reason worth writing is a sentence or two, and rustfmt wraps it, so the marker ends up
    /// some lines above the loop rather than on the one directly above it.
    #[test]
    fn the_marker_anywhere_in_the_comment_block_above_counts() {
        let text = "fn f() {\n    // row at a time: 2f (#60) gives this a table that hashes a\n    // column at a time, and the key stops being a Vec<Value> then.\n    for row in 0..chunk.len() {\n        out.push(input.value_at(row));\n    }\n}\n";
        let (problems, declared) = check_one("t.rs", text);
        assert!(problems.is_empty());
        assert_eq!(declared, 1);
    }

    /// A block of prose with a blank line between it and the loop is prose about something else.
    #[test]
    fn a_comment_block_that_does_not_touch_the_loop_does_not_cover_it() {
        let text = "fn f() {\n    // row at a time: this is about the function below, not the loop.\n\n    for row in 0..chunk.len() {\n        out.push(input.value_at(row));\n    }\n}\n";
        assert_eq!(check_one("t.rs", text).0.len(), 1);
    }

    #[test]
    fn the_loop_stops_being_a_loop_at_its_closing_brace() {
        let text = "fn f() {\n    for row in 0..chunk.len() {\n        total += row;\n    }\n    let one = Value::Integer(1);\n}\n";
        assert!(check_one("t.rs", text).0.is_empty());
    }

    #[test]
    fn a_loop_over_something_that_is_not_rows_is_left_alone() {
        let text = "fn f() {\n    for op in [Comparison::Less, Comparison::Greater] {\n        out.push(Value::Boolean(true));\n    }\n}\n";
        assert!(check_one("t.rs", text).0.is_empty());
    }

    #[test]
    fn a_pattern_is_not_a_construction() {
        let text = "fn f() {\n    for row in 0..rows {\n        let held = match seen {\n            Value::Boolean => 1,\n        };\n    }\n}\n";
        assert!(check_one("t.rs", text).0.is_empty());
    }

    #[test]
    fn a_comment_describing_a_loop_is_not_a_loop() {
        let text = "fn f() {\n    // for row in 0..rows this would build a Value::Integer(3)\n    let one = 1;\n}\n";
        assert!(check_one("t.rs", text).0.is_empty());
    }

    #[test]
    fn the_test_module_is_not_checked() {
        let text = "fn f() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {\n        for row in 0..rows {\n            assert_eq!(v.value_at(row), Value::Integer(1));\n        }\n    }\n}\n";
        assert!(check_one("t.rs", text).0.is_empty());
    }

    /// A brace in a character literal would put the depth out by one for the rest of the file, which
    /// would make every loop after it look like it never closed.
    #[test]
    fn a_brace_in_a_character_literal_does_not_move_the_depth() {
        let text = "fn f() {\n    let open = '{';\n    for row in 0..rows {\n        total += row;\n    }\n    let one = Value::Integer(1);\n}\n";
        assert!(check_one("t.rs", text).0.is_empty());
    }
}

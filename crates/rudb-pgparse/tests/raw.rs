//! The raw parse trees of `parse` against those of PostgreSQL 19.
//!
//! `raw.txt` has a case for each statement: a line `> ` and the text, then the tree that
//! PostgreSQL logs for the text with `debug_print_raw_parse` on and `debug_pretty_print` off, on
//! one line, then an empty line. For a text that the raw parser of PostgreSQL rejects, the line
//! after the text is `ERROR`, the SQLSTATE, the position and the message. The trees and the errors
//! come from the oracle server of `tamnd/rudb-postgres`.

use rudb_pgparse::nodes::list_text;
use rudb_pgparse::parse;

#[test]
fn raw_trees() {
    let cases = include_str!("raw.txt");
    let mut failures = Vec::new();
    let mut count = 0;
    for case in cases.split("\n\n").filter(|case| !case.trim().is_empty()) {
        let Some((text, expected)) = case.trim_start_matches('\n').split_once('\n') else {
            panic!("a case of raw.txt has no tree: {case}");
        };
        let text = text.strip_prefix("> ").expect("a case of raw.txt starts with `> `");
        count += 1;
        let actual = match parse(text) {
            Ok((tree, _)) => list_text(&tree),
            Err(error) => {
                let position = error.position(text).unwrap_or(0);
                format!("ERROR {} {position} {}", error.code, error.message)
            }
        };
        if actual != expected {
            failures.push(format!("{text}\n  expected {expected}\n  actual   {actual}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {count} cases differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

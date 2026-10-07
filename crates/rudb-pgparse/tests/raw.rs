//! The raw parse trees of `parse` against those of PostgreSQL 19.
//!
//! `raw.txt` has a case for each statement: a line `> ` and the text, then the tree that
//! PostgreSQL logs for the text with `debug_print_raw_parse` on and `debug_pretty_print` off, on
//! one line, then an empty line. The trees come from the oracle server of `tamnd/rudb-postgres`.

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
        match parse(text) {
            Ok((tree, _)) if list_text(&tree) == expected => {}
            Ok((tree, _)) => failures
                .push(format!("{text}\n  expected {expected}\n  actual   {}", list_text(&tree))),
            Err(error) => {
                failures.push(format!("{text}\n  error {} {}", error.code, error.message))
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {count} cases differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

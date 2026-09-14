//! What the fuzz target does to one input, so that the test and the target cannot drift apart.
//!
//! `fuzz/fuzz_targets/tokenize.rs` includes this file rather than copying it. That direction is
//! deliberate: the fuzz crate is excluded from the workspace and is never published, so it is
//! allowed to reach into this one, and a file under `tests/` here is in the published package
//! while a file under `fuzz/` is not. Two copies of the checks would be two sets of checks within
//! a month.
//!
//! A subdirectory of `tests/` is not a test target, so this compiles once as part of whatever
//! includes it and never on its own.

/// Run the three front end entry points over a query and check what the tokenizer promises.
///
/// A query that does not parse is the ordinary outcome and not a failure, so each stage stops on
/// its own error. What is being asked is whether any of them can be made to panic instead.
pub(crate) fn exercise(query: &str) {
    let Ok(tokens) = rudb_parse::tokenize(query) else {
        return;
    };
    // The spans are a promise the tokenizer makes to everything above it: they arrive in order,
    // they do not overlap, they stay inside the query and both ends land on a character boundary.
    // An offset that breaks one of those is a panic in whichever caller slices the query with it,
    // so checking it here turns a crash somewhere else into a failure at the input that caused it.
    let mut previous = 0;
    for token in &tokens {
        let (start, end) = (token.start as usize, token.end as usize);
        assert!(start >= previous, "a token starts before the one before it ended");
        assert!(start <= end, "a token ends before it starts");
        assert!(end <= query.len(), "a token runs past the end of the query");
        assert!(query.is_char_boundary(start), "a token starts in the middle of a character");
        assert!(query.is_char_boundary(end), "a token ends in the middle of a character");
        previous = end;
    }
    if rudb_parse::parse(query).is_err() {
        return;
    }
    let _ = rudb_parse::parse_ast(query);
}

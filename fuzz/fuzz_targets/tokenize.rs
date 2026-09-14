//! Tokenize, match and transform whatever bytes arrive, and look for a panic.
//!
//! This target is not looking for a wrong answer. Every other generator in `spec/sql/duckdb` is,
//! and a fuzzer is a different job: what counts as a failure here is a panic, an assertion, a hang
//! past libFuzzer's timeout, or memory growth past its limit.
//!
//! The input has to be valid UTF-8 to get past the first line, because a `&str` is what the
//! tokenizer takes and the conversion is the caller's job rather than the parser's. That throws
//! away most random byte strings early, which is why the seed corpus is real SQL: the mutator does
//! far better starting from something that already parses than from nothing.
//!
//! The work itself lives next to the test that replays the corpus, so that the two cannot drift.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/rudb-parse/tests/support/front_end.rs"]
mod front_end;

fuzz_target!(|data: &[u8]| {
    if let Ok(query) = std::str::from_utf8(data) {
        front_end::exercise(query);
    }
});

# Fuzz targets

This directory is a separate cargo workspace on purpose. It needs a nightly toolchain for the sanitizer and it pulls in `libfuzzer-sys`, and neither of those belongs in the build that has to keep working on 1.85.0. The root `Cargo.toml` excludes it, so `cargo test` and `cargo clippy` at the top of the repo never see it and `./scripts/gate` never builds it.

## Running one

```
cargo install cargo-fuzz
cargo +nightly fuzz run tokenize fuzz/seeds/tokenize
```

The seed directory is passed on the command line rather than being the corpus itself, because libFuzzer writes back into the first directory it is given and the seeds are meant to stay as they are in git. Give it a real corpus directory first if you want the growth kept between runs.

## What counts as a failure

A fuzzer is looking for a different thing than every other generator in `spec/sql/duckdb`. Those look for a wrong answer, by asking two engines the same question or by asking one engine two questions that have to agree. This looks for a panic, an assertion that fires, a hang past the libFuzzer timeout, or memory growth past its limit. A query that does not parse is the expected outcome for nearly every input and is not a finding.

## The targets

`tokenize` takes raw bytes, turns them into a `&str` or gives up, and then runs the three front end entry points in order: `tokenize`, `parse` and `parse_ast`. All three run rather than only the first, because the tokenizer is a byte scan with few branches in it while the matcher walks 1088 rules and the transformer builds an arena out of the tree, and a target that stopped at the tokenizer would spend its whole budget on the cheapest of the three.

It also checks the promise the tokenizer makes to everything above it, which is that spans are in order, do not overlap, stay inside the query and land on character boundaries. An offset that breaks one of those is a panic in whichever caller slices the query with it, so checking it in the target turns a crash somewhere else into a crash at the input that caused it.

## The seeds

`seeds/tokenize` holds the 65 entries from `crates/rudb-parse/src/corpus.rs`, one file each in array order, plus sixteen shapes chosen by hand for the things a mutator is bad at finding on its own: deep nesting of parentheses, subqueries, list brackets and casts, the four unterminated forms, a dollar quote, a null byte, a number with 160 digits in it, and a few very wide statements. Real SQL matters here more than it does for most targets, because the input has to be valid UTF-8 and has to reach the end of the tokenizer before any of the interesting code runs, and a mutator starting from nothing spends a very long time before it gets there.

## Crashers

Anything the fuzzer finds gets committed to `crates/rudb-parse/tests/crashers/` and replayed by an ordinary test in `crates/rudb-parse/tests/crashers.rs`, so the case stays checked on every machine with no nightly toolchain and no fuzzer installed. The artifact in `fuzz/artifacts` is a working file and is not committed.

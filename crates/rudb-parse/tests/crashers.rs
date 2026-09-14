//! Replay every input the fuzzer has ever been given or has ever found.
//!
//! The fuzzer itself needs nightly, a sanitizer and a corpus that grows for hours, which is not
//! something every machine that runs `cargo test` has. What every machine can do is take the
//! inputs that matter and run them once. The seed corpus goes through because a seed that stops
//! parsing is worth knowing about, and the crashers go through because a bug that a fuzzer found
//! once is a bug that belongs in the ordinary test suite from then on.

#[path = "support/front_end.rs"]
mod front_end;

use std::path::{Path, PathBuf};

#[test]
fn every_fuzzer_seed_survives_the_front_end() {
    let seeds = root().join("fuzz/seeds/tokenize");
    let files = files_in(&seeds);
    assert!(
        files.len() > 60,
        "the seed corpus at {} has shrunk to {} files",
        seeds.display(),
        files.len()
    );
    for file in files {
        let bytes = std::fs::read(&file)
            .unwrap_or_else(|why| panic!("cannot read {}: {why}", file.display()));
        let query =
            String::from_utf8(bytes).unwrap_or_else(|_| panic!("{} is not UTF-8", file.display()));
        front_end::exercise(&query);
    }
}

#[test]
fn every_input_the_fuzzer_found_stays_fixed() {
    // Empty until the fuzzer finds something, which is the state this starts in and the state it
    // is meant to stay in. The directory is read rather than listed in code so that committing an
    // artifact is the whole of the work of adding a regression case.
    for file in files_in(&root().join("crates/rudb-parse/tests/crashers")) {
        let bytes = std::fs::read(&file)
            .unwrap_or_else(|why| panic!("cannot read {}: {why}", file.display()));
        // A crasher arrives as bytes and not as text, because the target takes bytes. One that is
        // not UTF-8 never reached the parser and is not a case this can replay.
        if let Ok(query) = std::str::from_utf8(&bytes) {
            front_end::exercise(query);
        }
    }
}

/// Every file in a directory, sorted, so a failure names the same file on every machine.
fn files_in(directory: &Path) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|why| panic!("cannot read {}: {why}", directory.display()));
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.expect("cannot read a directory entry").path())
        .filter(|path| path.is_file() && path.file_name().is_some_and(|name| name != ".gitkeep"))
        .collect();
    files.sort();
    files
}

/// The top of the repository, two levels up from this crate.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

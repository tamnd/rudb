//! The Parquet mirror of document 33 in `spec/storage-v3`, through the shell a person runs.
//!
//! A mirror is found by the environment the process starts with, so every test here runs the
//! binary in a child with its own mirror directory rather than changing this process's environment
//! under the tests that share it.
//!
//! What is checked is the promise the document makes: a query answers the same with the mirror off,
//! on the run that builds it and on the run that reuses it, a file that is written again is read
//! again, and nothing is written when mirroring is off or the file is below the threshold.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The queries, the same file the ClickBench test reads.
const SQL: &str = include_str!("../../rudb/testdata/clickbench.sql");

/// The benchmark fixture, ten thousand rows.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../rudb/testdata/hits.parquet");

/// A second, different Parquet file, to write over the first.
const OTHER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../rudb-parquet/testdata/mixed.parquet");

/// How much of each query's answer is settled, from the recorded answers: `exact`, `last` or
/// `count`, which the ClickBench test's documentation explains.
const ANSWERS: &str = include_str!("../../rudb/testdata/clickbench-answers.txt");

/// A directory of its own for one test, emptied first.
fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("rudb-mirror-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch directory");
    directory
}

/// Runs `script` through the shell in CSV with `environment` set, and returns what it printed.
fn shell(script: &str, environment: &[(&str, &str)]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rudb"));
    command.arg("-csv").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    command.env_remove("RUDB_PARQUET_MIRROR").env_remove("RUDB_MIRROR_ROWS");
    for (name, value) in environment {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("the shell starts");
    child.stdin.take().expect("stdin").write_all(script.as_bytes()).expect("script written");
    let output = child.wait_with_output().expect("the shell finishes");
    let (out, err) =
        (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success() && err.is_empty(), "the shell failed: {err}\n{out}");
    out.into_owned()
}

/// The mirrors in `directory`.
fn mirrors(directory: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(directory) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The ClickBench queries, each preceded by a line that names it, over a view of `file`.
fn suite(file: &str) -> String {
    let mut script = format!(
        "CREATE VIEW hits AS SELECT * FROM read_parquet('{file}', binary_as_string=True);\n"
    );
    let mut sql = String::new();
    let mut name = String::new();
    for line in SQL.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("--") {
            let rest = rest.trim();
            if rest.starts_with('q') && rest[1..].chars().all(|c| c.is_ascii_digit()) {
                name = rest.to_string();
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        sql.push_str(line);
        sql.push(' ');
        if line.ends_with(';') {
            if !name.is_empty() {
                script.push_str(&format!("SELECT '#{name}' AS marker;\n{sql}\n"));
            }
            sql.clear();
        }
    }
    script
}

/// The part of the answer to `name` that any right answer agrees on: every row for `exact`, the last
/// column for `last` and the number of rows for `count`.
fn settled(name: &str, lines: &[String]) -> Vec<String> {
    let mode = ANSWERS
        .lines()
        .find_map(|line| {
            let mut words = line.strip_prefix("-- ")?.split(' ');
            (words.next()? == name).then(|| words.next().map(str::to_string)).flatten()
        })
        .unwrap_or_else(|| panic!("{name} has no recorded answer"));
    let mut kept: Vec<String> = match mode.as_str() {
        "exact" => lines.to_vec(),
        "last" => {
            lines.iter().map(|line| line.rsplit(',').next().unwrap_or("").to_string()).collect()
        }
        _ => vec![lines.len().to_string()],
    };
    kept.sort();
    kept
}

/// The output split at the markers, each query's lines sorted, since a query without an `ORDER BY`
/// may put its rows in any order and still be right.
fn answers(output: &str) -> Vec<(String, Vec<String>)> {
    let mut found: Vec<(String, Vec<String>)> = Vec::new();
    for line in output.lines() {
        if line == "marker" {
            continue;
        }
        if let Some(name) = line.strip_prefix('#') {
            found.push((name.to_string(), Vec::new()));
        } else if let Some((_, lines)) = found.last_mut() {
            lines.push(line.to_string());
        }
    }
    found
        .into_iter()
        .map(|(name, lines)| {
            let kept = settled(&name, &lines);
            (name, kept)
        })
        .collect()
}

/// Every ClickBench query answers the same from the file, from the run that builds the mirror and
/// from the run that reuses it, and one mirror is built for the one file.
#[test]
fn a_mirror_answers_what_the_file_does() {
    let directory = scratch("agree");
    let spelled = directory.to_str().expect("utf8");
    let script = suite(FIXTURE);
    let off = shell(&script, &[("RUDB_MIRROR_DIR", spelled), ("RUDB_PARQUET_MIRROR", "0")]);
    assert!(mirrors(&directory).is_empty(), "mirroring off wrote {:?}", mirrors(&directory));
    let on = [("RUDB_MIRROR_DIR", spelled), ("RUDB_MIRROR_ROWS", "0")];
    let built = shell(&script, &on);
    assert_eq!(mirrors(&directory).len(), 1, "one file, one mirror: {:?}", mirrors(&directory));
    let reused = shell(&script, &on);
    assert_eq!(mirrors(&directory).len(), 1);
    let expected = answers(&off);
    assert_eq!(expected.len(), 43, "every query ran");
    for (label, got) in [("built", answers(&built)), ("reused", answers(&reused))] {
        assert_eq!(got.len(), expected.len());
        for ((name, want), (_, have)) in expected.iter().zip(&got) {
            assert_eq!(want, have, "{name} answers differently from the {label} mirror");
        }
    }
    let _ = std::fs::remove_dir_all(&directory);
}

/// A file written again is a new file: the next read answers from what is there now.
#[test]
fn a_rewritten_file_is_read_again() {
    let directory = scratch("rewrite");
    let spelled = directory.to_str().expect("utf8");
    let file = directory.join("data.parquet");
    let path = file.to_str().expect("utf8");
    let on = [("RUDB_MIRROR_DIR", spelled), ("RUDB_MIRROR_ROWS", "0")];
    let count = format!("SELECT count(*) FROM '{path}';\n");
    std::fs::copy(FIXTURE, &file).expect("copied");
    assert_eq!(shell(&count, &on), "count_star()\n10000\n");
    std::fs::copy(OTHER, &file).expect("copied over");
    assert_eq!(shell(&count, &on), "count_star()\n4096\n");
    let built = mirrors(&directory).iter().filter(|name| name.ends_with(".rudb")).count();
    assert_eq!(built, 2, "each version of the file has its own mirror");
    let _ = std::fs::remove_dir_all(&directory);
}

/// A file below the threshold is read where it is, and nothing is written for it.
#[test]
fn a_small_file_is_not_mirrored() {
    let directory = scratch("small");
    let spelled = directory.join("mirrors");
    let count = format!("SELECT count(*) FROM '{FIXTURE}';\n");
    let out = shell(&count, &[("RUDB_MIRROR_DIR", spelled.to_str().expect("utf8"))]);
    assert_eq!(out, "count_star()\n10000\n");
    assert!(!spelled.exists(), "a mirror was written for a small file");
    let _ = std::fs::remove_dir_all(&directory);
}

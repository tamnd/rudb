//! The forty three ClickBench queries through rudb and through duckdb, over one file.
//!
//! Exit criterion 2 of E0 is that every one of those queries matches DuckDB, and the committed test
//! in `crates/rudb-cli/tests/clickbench.rs` checks exactly that over a ten thousand row fixture. Ten
//! thousand rows is the right size for a file in the repository and it is the wrong size for
//! believing the claim, because a million rows is where a group by spills, where a hash table
//! resizes, where a dictionary stops fitting and where a sum stops being exact. Every difference
//! found so far was found at a million rows and none of them were visible at ten thousand.
//!
//! So this is the same comparison pointed at a file somebody downloaded. It is not a test and it is
//! not a gate, because the file is not in the repository and cannot be. It is the command that
//! produces the table that goes in the milestone, so that the table is something anybody with the
//! file can reproduce rather than something they have to take on trust.
//!
//! Timing comes out of it as well, one run each, which is enough to see a twenty times gap and is
//! not enough to see a ten percent one. `tamnd/rudb-bench` is where a number worth quoting comes
//! from and `cargo xtask bench clickbench` is how to get one.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::compare::{binary, build_rudb};

/// How the two engines ended up on one query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Byte for byte the same, which is the only one of these that is a pass.
    Same,
    /// Both answered and the answers are not the same.
    Differ,
    /// rudb answered and duckdb refused.
    RudbOnly,
    /// duckdb answered and rudb refused.
    DuckdbOnly,
    /// Neither would run it, which on this file is a property of the file rather than of either
    /// engine and is still worth counting separately from a pass.
    BothRefuse,
}

impl Verdict {
    /// What the table calls it.
    fn text(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Differ => "differ",
            Self::RudbOnly => "rudb only",
            Self::DuckdbOnly => "duckdb only",
            Self::BothRefuse => "both refuse",
        }
    }
}

/// What one engine did with one query.
struct Went {
    /// What it printed, or what it complained about when it would not run.
    output: String,
    /// Whether it ran at all.
    ok: bool,
    /// Wall clock around the whole process, so a process start is in here and is in both.
    took: Duration,
}

/// Run every query through both engines over `args[0]` and print the comparison.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let Some(first) = args.first() else {
        return Err("usage: cargo xtask differential <file.parquet>".into());
    };
    let file = PathBuf::from(first);
    if !file.is_file() {
        return Err(format!("{} is not a file", file.display()));
    }
    let file =
        file.canonicalize().map_err(|e| format!("could not resolve {}: {e}", file.display()))?;

    let rudb = match std::env::var_os("RUDB_BENCH_RUDB") {
        // The same variable the benchmark harness takes, because it means the same thing here and a
        // second name for it would be a second thing to remember. What it is for is asking an older
        // rudb the same questions, which is how a claim that something got better is checked.
        Some(set) => PathBuf::from(set),
        None => build_rudb(root)?,
    };
    if !rudb.is_file() {
        return Err(format!("no rudb at {}", rudb.display()));
    }
    let duckdb = duckdb()?;
    let init = quiet(root)?;
    let queries = queries(root, &file)?;

    println!();
    println!("file    {}", file.display());
    println!("rudb    {}", rudb.display());
    println!("duckdb  {}", duckdb.display());
    println!();
    println!("{:<5} {:>8} {:>8} {:>7}  verdict", "query", "rudb", "duckdb", "ratio");

    let mut counts = [0usize; 5];
    let mut differences = Vec::new();
    for (name, sql) in &queries {
        let ours = went(Command::new(&rudb).arg("-c").arg(sql));
        let theirs =
            went(Command::new(&duckdb).arg("-batch").arg("-init").arg(&init).arg("-c").arg(sql));
        let verdict = judge(&ours, &theirs);
        counts[verdict as usize] += 1;
        let ratio = match (ours.ok, theirs.ok) {
            (true, true) => format!("{:>7.1}", ours.took.as_secs_f64() / theirs.took.as_secs_f64()),
            _ => format!("{:>7}", "-"),
        };
        println!(
            "{name:<5} {:>8} {:>8} {ratio}  {}",
            seconds(&ours),
            seconds(&theirs),
            verdict.text()
        );
        if verdict == Verdict::Differ {
            differences.push((name.clone(), difference(&ours.output, &theirs.output)));
        }
    }

    println!();
    for verdict in [
        Verdict::Same,
        Verdict::Differ,
        Verdict::RudbOnly,
        Verdict::DuckdbOnly,
        Verdict::BothRefuse,
    ] {
        println!("{:>3}  {}", counts[verdict as usize], verdict.text());
    }

    for (name, lines) in &differences {
        println!();
        println!("=== {name}");
        for (ours, theirs) in lines {
            println!("  rudb    {ours}");
            println!("  duckdb  {theirs}");
        }
    }

    // Not an error. A difference is a finding and this command is how findings are found, so it
    // should print them and exit zero rather than make the caller read an exit code to learn that
    // something it already printed in full went wrong.
    Ok(())
}

/// The queries, with the table name replaced by the file so the replacement scan reads it.
///
/// The DDL at the top of the file is skipped rather than run. Neither engine needs it here: both
/// take the columns and the types from the Parquet file, which is the whole point of pointing them
/// at one. It also means the comparison is over the types the file really has rather than the types
/// the benchmark wishes it had, and that difference is real. `EventDate` in the published file is an
/// unsigned sixteen bit integer and the DDL calls it a DATE, so the eight queries that compare it
/// against a date are refused by both engines and counted as such.
fn queries(root: &Path, file: &Path) -> Result<Vec<(String, String)>, String> {
    let path = root.join("crates").join("rudb").join("testdata").join("clickbench.sql");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let from = format!("FROM '{}'", file.display());

    let mut found = Vec::new();
    let mut name = String::new();
    let mut sql = String::new();
    for line in text.lines() {
        let line = line.trim();
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
        if !sql.is_empty() {
            sql.push(' ');
        }
        sql.push_str(line);
        if let Some(statement) = sql.strip_suffix(';') {
            if !name.is_empty() {
                found.push((name.clone(), statement.replace("FROM hits", &from)));
            }
            sql.clear();
        }
    }
    if found.len() != 43 {
        return Err(format!("{} holds {} queries rather than 43", path.display(), found.len()));
    }
    Ok(found)
}

/// Run one engine on one query and keep what it said, what it cost and whether it worked.
///
/// Standard error is folded into the output on purpose. An engine that refuses a query is a result
/// and the sentence it refused with is the interesting half of it, so throwing that away would make
/// every refusal look the same as every other.
fn went(command: &mut Command) -> Went {
    let at = Instant::now();
    let out = command.output();
    let took = at.elapsed();
    match out {
        Ok(out) => {
            let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
            if !out.status.success() {
                output.push_str(&String::from_utf8_lossy(&out.stderr));
            }
            Went { output, ok: out.status.success(), took }
        }
        Err(e) => Went { output: format!("could not run it: {e}"), ok: false, took },
    }
}

/// Which of the five outcomes this pair is.
fn judge(ours: &Went, theirs: &Went) -> Verdict {
    match (ours.ok, theirs.ok) {
        (true, true) if ours.output == theirs.output => Verdict::Same,
        (true, true) => Verdict::Differ,
        (true, false) => Verdict::RudbOnly,
        (false, true) => Verdict::DuckdbOnly,
        (false, false) => Verdict::BothRefuse,
    }
}

/// The first few lines where two answers stopped agreeing, paired up.
///
/// Three pairs at most, because the useful thing about a difference is which line it starts on and
/// a result that went wrong at the top has gone wrong on every line after it as well.
fn difference(ours: &str, theirs: &str) -> Vec<(String, String)> {
    let mine: Vec<&str> = ours.lines().collect();
    let yours: Vec<&str> = theirs.lines().collect();
    let mut found = Vec::new();
    for at in 0..mine.len().max(yours.len()) {
        let a = mine.get(at).copied().unwrap_or("<nothing>");
        let b = yours.get(at).copied().unwrap_or("<nothing>");
        if a != b {
            found.push((a.to_string(), b.to_string()));
            if found.len() == 3 {
                break;
            }
        }
    }
    found
}

/// How long a run took, to the millisecond, or a dash when it did not run.
fn seconds(went: &Went) -> String {
    if went.ok { format!("{:.3}", went.took.as_secs_f64()) } else { "err".to_string() }
}

/// An empty file to hand duckdb as its startup script.
///
/// Without it duckdb reads whatever `.duckdbrc` the machine has, and a `.mode` in somebody's rc file
/// would make every query differ at once. That failure is loud rather than quiet, so this is belt
/// and braces, but it costs one empty file and it means the comparison is of the two engines rather
/// than of one engine and one configuration.
fn quiet(root: &Path) -> Result<PathBuf, String> {
    let path = root.join("target").join("differential-init.sql");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not make {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, "").map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path)
}

/// Where duckdb is, which is `RUDB_DUCKDB` or the path.
fn duckdb() -> Result<PathBuf, String> {
    if let Some(set) = std::env::var_os("RUDB_DUCKDB") {
        let path = PathBuf::from(set);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("RUDB_DUCKDB is {}, which is not a file", path.display()));
    }
    let name = binary("duckdb");
    let Some(path) = std::env::var_os("PATH") else {
        return Err("there is no PATH to look for duckdb on".into());
    };
    std::env::split_paths(&path)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            "no duckdb on PATH, install one or set RUDB_DUCKDB to where it is".to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::{Verdict, Went, difference, judge};
    use std::time::Duration;

    fn went(output: &str, ok: bool) -> Went {
        Went { output: output.to_string(), ok, took: Duration::from_millis(1) }
    }

    #[test]
    fn two_engines_that_printed_the_same_thing_agree_and_nothing_else_does() {
        assert_eq!(judge(&went("a\n", true), &went("a\n", true)), Verdict::Same);
        assert_eq!(judge(&went("a\n", true), &went("b\n", true)), Verdict::Differ);
        assert_eq!(judge(&went("a\n", true), &went("no", false)), Verdict::RudbOnly);
        assert_eq!(judge(&went("no", false), &went("a\n", true)), Verdict::DuckdbOnly);
        assert_eq!(judge(&went("no", false), &went("no", false)), Verdict::BothRefuse);
    }

    #[test]
    fn a_difference_names_the_lines_it_starts_on_and_stops_after_three() {
        let found = difference("1\n2\n3\n4\n5\n", "1\nx\ny\nz\nw\n");
        assert_eq!(found.len(), 3);
        assert_eq!(found[0], ("2".to_string(), "x".to_string()));
        assert_eq!(found[2], ("4".to_string(), "z".to_string()));
    }

    #[test]
    fn a_side_that_ran_out_of_lines_says_so_rather_than_being_dropped() {
        let found = difference("1\n2\n", "1\n");
        assert_eq!(found, vec![("2".to_string(), "<nothing>".to_string())]);
    }
}
